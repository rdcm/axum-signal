use axum::extract::ws::{Message, WebSocket};
use dashmap::DashMap;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use std::any::TypeId;
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::clients::{InMemoryWsClients, WsClients};
use crate::codec::WsCodec;
use crate::context::MessageContext;
use crate::hub::{ConnectionRequest, DisconnectRequest, WsHub, WsHubConfig};

static CLIENT_REGISTRY: OnceLock<DashMap<TypeId, Box<dyn std::any::Any + Send + Sync>>> =
    OnceLock::new();

/// Returns the shared [`InMemoryWsClients`] instance for hub type `H`.
///
/// The registry is a global `DashMap` keyed by [`TypeId`]. On the first call for a given `H`
/// the entry is created; subsequent calls return the same `Arc`-wrapped value.
///
/// Returns `None` only when the downcast fails, which should never happen in practice because
/// the entry is always inserted with the correct concrete type.
fn get_clients<H: WsHub>(
    config: &WsHubConfig,
) -> Option<Arc<InMemoryWsClients<H::OutMessage, H::Codec>>> {
    let registry = CLIENT_REGISTRY.get_or_init(DashMap::new);
    let type_id = TypeId::of::<H>();

    registry
        .entry(type_id)
        .or_insert_with(|| {
            Box::new(Arc::new(InMemoryWsClients::<H::OutMessage, H::Codec>::new(
                config.policy.clone(),
                config.broadcast_channel_capacity,
            )))
        })
        .downcast_ref::<Arc<InMemoryWsClients<H::OutMessage, H::Codec>>>()
        .cloned()
}

/// Drives a WebSocket connection using the global in-memory client registry.
///
/// This is the high-level entry point. It looks up (or lazily creates) the shared
/// [`InMemoryWsClients`] for hub type `H` and then delegates to [`serve_hub_with_clients`].
/// The policy from `config` is applied when the registry entry is first created.
///
/// # Errors
/// If the global registry entry cannot be downcast (which should never happen), the connection
/// is dropped and an error is logged.
pub async fn serve_hub<H>(socket: WebSocket, hub: H, config: &WsHubConfig)
where
    H: WsHub,
{
    match get_clients::<H>(config) {
        Some(clients) => serve_hub_with_clients(socket, hub, clients, config).await,
        None => tracing::error!("failed to get clients registry for hub — connection dropped"),
    }
}

/// Drives a single WebSocket connection end-to-end using the provided `clients` store and `config`.
///
/// Orchestrates connection setup and teardown:
/// 1. Assigns a UUID v4 `connection_id` and registers the connection with `clients`.
/// 2. Spawns a writer task via [`spawn_writer`].
/// 3. Calls [`WsHub::on_connect`], runs [`run_read_loop`], then calls [`WsHub::on_disconnect`].
///
/// # Disconnect reasons (logged at `info` level)
/// `"idle timeout"`, `"stream closed"`, `"read error"`, `"client closed"`,
/// `"pong send error"`, `"ping send error"`.
pub async fn serve_hub_with_clients<H, C>(
    socket: WebSocket,
    hub: H,
    clients: Arc<C>,
    config: &WsHubConfig,
) where
    H: WsHub,
    C: WsClients<H::OutMessage, H::Codec>,
{
    let (sink, mut stream) = socket.split();
    let connection_id: Arc<str> = Uuid::new_v4().to_string().into();

    let (unicast_tx, unicast_rx) = mpsc::channel::<Message>(config.unicast_channel_capacity);
    let heartbeat_tx = unicast_tx.clone();
    let broadcast_rx = clients.subscribe();
    let cancel = CancellationToken::new();
    let hub = Arc::new(hub);

    let hub_for_drop = hub.clone();
    clients
        .add(
            connection_id.clone(),
            unicast_tx,
            cancel.clone(),
            Arc::new(move |id| {
                let hub = hub_for_drop.clone();
                tokio::spawn(async move { hub.on_message_drop(id).await });
            }),
        )
        .await;
    let clients: Arc<dyn WsClients<H::OutMessage, H::Codec>> = clients;
    let writer = spawn_writer(sink, unicast_rx, broadcast_rx, cancel.clone());

    hub.on_connect(ConnectionRequest {
        connection_id: connection_id.clone(),
    })
    .await;

    let disconnect_reason = run_read_loop(
        &mut stream,
        &hub,
        &clients,
        &connection_id,
        &cancel,
        &heartbeat_tx,
        config,
    )
    .await;

    tracing::info!("connection {} closed: {}", connection_id, disconnect_reason);

    cancel.cancel();
    clients.remove(&connection_id).await;
    writer.await.ok();

    hub.on_disconnect(DisconnectRequest { connection_id }).await;
}

/// Spawns the writer task that forwards outgoing messages to the WebSocket sink.
///
/// Merges two sources into the sink:
/// - `unicast_rx` — point-to-point messages addressed to this specific connection.
/// - `broadcast_rx` — fan-out messages shared across all connections.
///
/// The task exits when `cancel` is triggered, either source closes, or a send error occurs.
fn spawn_writer(
    mut sink: SplitSink<WebSocket, Message>,
    mut unicast_rx: mpsc::Receiver<Message>,
    mut broadcast_rx: broadcast::Receiver<Message>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(msg) = unicast_rx.recv() => {
                    if sink.send(msg).await.is_err() { break; }
                }
                result = broadcast_rx.recv() => {
                    match result {
                        Ok(msg) => { if sink.send(msg).await.is_err() { break; } }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("broadcast lagged by {n} messages");
                        }
                        Err(_) => break,
                    }
                }
                _ = cancel.cancelled() => break,
            }
        }
    })
}

/// Runs the read loop for a single connection until it disconnects.
///
/// On each iteration:
/// - Applies `config.idle_timeout` to the next incoming frame.
/// - Sends a `Ping` frame every `config.heartbeat_interval`; resets the idle timer on any
///   received frame.
/// - Delegates each received frame to [`handle_frame`].
///
/// Returns the disconnect reason string, which is logged by the caller.
async fn run_read_loop<H>(
    stream: &mut SplitStream<WebSocket>,
    hub: &Arc<H>,
    clients: &Arc<dyn WsClients<H::OutMessage, H::Codec>>,
    connection_id: &Arc<str>,
    cancel: &CancellationToken,
    heartbeat_tx: &mpsc::Sender<Message>,
    config: &WsHubConfig,
) -> &'static str
where
    H: WsHub,
{
    let mut heartbeat = interval(config.heartbeat_interval);
    let mut ping_sent_at: Option<Instant> = None;
    loop {
        tokio::select! {
            result = timeout(config.idle_timeout, stream.next()) => {
                match result {
                    Err(_) => {
                        tracing::warn!("connection {} idle timeout", connection_id);
                        break "idle timeout";
                    }
                    Ok(None) => break "stream closed",
                    Ok(Some(Err(e))) => {
                        tracing::warn!("ws read error on {}: {e}", connection_id);
                        break "read error";
                    }
                    Ok(Some(Ok(msg))) => {
                        heartbeat.reset();
                        if let Some(reason) =
                            handle_frame::<H>(msg, hub, clients, connection_id, cancel, heartbeat_tx, &mut ping_sent_at).await
                        {
                            break reason;
                        }
                    }
                }
            }
            _ = heartbeat.tick() => {
                ping_sent_at = Some(Instant::now());
                if heartbeat_tx.send(Message::Ping(axum::body::Bytes::new())).await.is_err() {
                    break "ping send error";
                }
            }
            _ = cancel.cancelled() => break "server disconnected",
        }
    }
}

/// Handles a single incoming WebSocket frame.
///
/// Returns `Some(reason)` to signal that the connection should be closed, or `None` to continue
/// the read loop. Data frames are decoded and dispatched to [`WsHub::on_message`] in a separate
/// task that is cancelled when `cancel` fires.
///
/// `ping_sent_at` tracks when the most recent outgoing Ping was sent. It is cleared after a Pong
/// is received and the RTT is recorded via [`WsClients::update_rtt`].
async fn handle_frame<H>(
    msg: Message,
    hub: &Arc<H>,
    clients: &Arc<dyn WsClients<H::OutMessage, H::Codec>>,
    connection_id: &Arc<str>,
    cancel: &CancellationToken,
    heartbeat_tx: &mpsc::Sender<Message>,
    ping_sent_at: &mut Option<Instant>,
) -> Option<&'static str>
where
    H: WsHub,
{
    match msg {
        Message::Close(_) => Some("client closed"),
        Message::Ping(p) => {
            if heartbeat_tx.send(Message::Pong(p)).await.is_err() {
                Some("pong send error")
            } else {
                None
            }
        }
        Message::Pong(_) => {
            if let Some(sent) = ping_sent_at.take() {
                let rtt = sent.elapsed();
                clients.update_rtt(connection_id, rtt).await;
                tracing::debug!("RTT for {connection_id}: {rtt:?}");
            }
            None
        }
        raw => match H::Codec::decode::<H::InMessage>(raw) {
            Err(e) => {
                tracing::warn!("failed to parse message from {}: {e:?}", connection_id);
                None
            }
            Ok(req) => {
                let hub = hub.clone();
                let cancel = cancel.clone();
                let ctx = MessageContext {
                    connection_id: connection_id.clone(),
                    clients: clients.clone(),
                };
                tokio::spawn(async move {
                    tokio::select! {
                        _ = hub.on_message(req, ctx) => {}
                        _ = cancel.cancelled() => {
                            tracing::debug!("on_message cancelled");
                        }
                    }
                });
                None
            }
        },
    }
}
