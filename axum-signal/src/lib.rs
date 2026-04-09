use axum::extract::ws::{Message, WebSocket};
use dashmap::DashMap;
use futures::future::BoxFuture;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use std::any::TypeId;
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Configuration for a [`WsHub`] instance.
///
/// Controls connection health parameters such as idle timeout and heartbeat cadence.
#[derive(Clone)]
pub struct WsHubConfig {
    /// How long a connection may remain idle (no incoming messages) before it is dropped.
    pub idle_timeout: Duration,
    /// Interval between outgoing WebSocket ping frames used to keep the connection alive.
    pub heartbeat_interval: Duration,
}

impl Default for WsHubConfig {
    /// Returns a `WsHubConfig` with sensible production defaults:
    /// - `idle_timeout`: 60 seconds
    /// - `heartbeat_interval`: 15 seconds
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(60),
            heartbeat_interval: Duration::from_secs(15),
        }
    }
}

static CLIENT_REGISTRY: OnceLock<DashMap<TypeId, Box<dyn std::any::Any + Send + Sync>>> =
    OnceLock::new();

/// Returns the shared [`InMemoryWsClients`] instance for hub type `H`.
///
/// The registry is a global `DashMap` keyed by [`TypeId`]. On the first call for a given `H`
/// the entry is created; subsequent calls return the same `Arc`-wrapped value.
///
/// Returns `None` only when the downcast fails, which should never happen in practice because
/// the entry is always inserted with the correct concrete type.
fn get_clients<H: WsHub>() -> Option<Arc<InMemoryWsClients<H::OutMessage, H::Codec>>> {
    let registry = CLIENT_REGISTRY.get_or_init(DashMap::new);
    let type_id = TypeId::of::<H>();

    registry
        .entry(type_id)
        .or_insert_with(|| {
            Box::new(Arc::new(
                InMemoryWsClients::<H::OutMessage, H::Codec>::default(),
            ))
        })
        .downcast_ref::<Arc<InMemoryWsClients<H::OutMessage, H::Codec>>>()
        .cloned()
}

// --- Codec ---

/// Encodes and decodes WebSocket messages for a specific wire format.
///
/// Implement this trait to support custom serialization formats (e.g. MessagePack, Protobuf).
/// Two built-in implementations are provided: [`JsonCodec`] and [`BinaryCodec`].
pub trait WsCodec: Send + Sync + 'static {
    /// The concrete error type produced by [`decode`](WsCodec::decode) and [`encode`](WsCodec::encode).
    type Error: std::error::Error + Send + Sync + 'static;

    /// Deserializes a WebSocket [`Message`] into `T`.
    ///
    /// Returns an error if the message type is not supported by this codec or if
    /// deserialization fails.
    fn decode<T: serde::de::DeserializeOwned>(msg: Message) -> Result<T, Self::Error>;

    /// Serializes `msg` into a WebSocket [`Message`].
    ///
    /// Returns an error if serialization fails.
    fn encode<T: serde::Serialize>(msg: T) -> Result<Message, Self::Error>;
}

// --- JsonCodec ---

/// Error produced by [`JsonCodec`].
#[derive(Debug)]
pub enum JsonCodecError {
    /// The WebSocket frame variant is not supported (e.g. `Ping`, `Pong`, `Close`).
    UnexpectedMessageType,
    /// JSON serialization or deserialization failed.
    Serde(serde_json::Error),
}

impl std::fmt::Display for JsonCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedMessageType => f.write_str("unexpected message type"),
            Self::Serde(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for JsonCodecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Serde(e) => Some(e),
            Self::UnexpectedMessageType => None,
        }
    }
}

impl From<serde_json::Error> for JsonCodecError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serde(e)
    }
}

/// A [`WsCodec`] that serializes messages as JSON text frames.
///
/// `decode` accepts both `Text` and `Binary` frames (the binary variant is parsed as UTF-8 JSON).
/// `encode` always produces `Text` frames.
pub struct JsonCodec;

impl WsCodec for JsonCodec {
    type Error = JsonCodecError;

    /// Deserializes `msg` from a JSON text or binary WebSocket frame into `T`.
    ///
    /// Returns [`JsonCodecError::UnexpectedMessageType`] for any other frame variant.
    fn decode<T: serde::de::DeserializeOwned>(msg: Message) -> Result<T, Self::Error> {
        match msg {
            Message::Text(text) => Ok(serde_json::from_str(&text)?),
            Message::Binary(data) => Ok(serde_json::from_slice(&data)?),
            _ => Err(JsonCodecError::UnexpectedMessageType),
        }
    }

    /// Serializes `value` to a JSON string and wraps it in a `Text` WebSocket frame.
    fn encode<T: serde::Serialize>(value: T) -> Result<Message, Self::Error> {
        Ok(Message::text(serde_json::to_string(&value)?))
    }
}

// --- BinaryCodec ---

/// Error produced by [`BinaryCodec`].
#[derive(Debug)]
pub enum BinaryCodecError {
    /// The WebSocket frame is not a `Binary` variant.
    ExpectedBinary,
    /// Postcard serialization or deserialization failed.
    Postcard(postcard::Error),
}

impl std::fmt::Display for BinaryCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExpectedBinary => f.write_str("expected binary frame"),
            Self::Postcard(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BinaryCodecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Postcard(e) => Some(e),
            Self::ExpectedBinary => None,
        }
    }
}

impl From<postcard::Error> for BinaryCodecError {
    fn from(e: postcard::Error) -> Self {
        Self::Postcard(e)
    }
}

/// A [`WsCodec`] that serializes messages as binary frames using [`postcard`].
///
/// Both `decode` and `encode` operate exclusively on `Binary` WebSocket frames.
pub struct BinaryCodec;

impl WsCodec for BinaryCodec {
    type Error = BinaryCodecError;

    /// Deserializes `msg` from a binary WebSocket frame using `postcard`.
    ///
    /// Returns [`BinaryCodecError::ExpectedBinary`] if the frame is not a `Binary` variant.
    fn decode<T: serde::de::DeserializeOwned>(msg: Message) -> Result<T, Self::Error> {
        match msg {
            Message::Binary(data) => Ok(postcard::from_bytes(&data)?),
            _ => Err(BinaryCodecError::ExpectedBinary),
        }
    }

    /// Serializes `value` with `postcard` and wraps the result in a `Binary` WebSocket frame.
    fn encode<T: serde::Serialize>(value: T) -> Result<Message, Self::Error> {
        Ok(Message::binary(postcard::to_stdvec(&value)?))
    }
}

// --- WsClients ---

/// Manages the set of active WebSocket connections for a hub.
///
/// Each connection is identified by a unique string `connection_id`. The trait provides
/// both unicast (point-to-point) and broadcast (one-to-all) delivery, encoded via codec `C`.
pub trait WsClients<M, C>: Send + Sync + 'static
where
    M: serde::Serialize + Send + Sync + 'static,
    C: WsCodec,
{
    /// Registers a new connection with the given `connection_id` and outgoing message `sender`.
    ///
    /// The `sender` is an `mpsc` channel half that forwards encoded frames to the connection's
    /// writer task.
    fn add(&self, connection_id: Arc<str>, sender: mpsc::Sender<Message>) -> BoxFuture<'_, ()>;

    /// Removes the connection identified by `connection_id` from the registry.
    ///
    /// After this call, unicast messages addressed to `connection_id` will be silently dropped.
    fn remove<'a>(&'a self, connection_id: &'a str) -> BoxFuture<'a, ()>;

    /// Returns a new [`broadcast::Receiver`] that will receive all future broadcast messages.
    ///
    /// Each connection should call this once during setup to avoid missing early broadcasts.
    fn subscribe(&self) -> broadcast::Receiver<Message>;

    /// Encodes `msg` and sends it only to the connection identified by `connection_id`.
    ///
    /// If the connection no longer exists or the channel is full the message is silently dropped.
    fn unicast<'a>(&'a self, connection_id: &'a str, msg: M) -> BoxFuture<'a, ()>;

    /// Encodes `msg` and delivers it to **all** currently subscribed connections in O(1).
    ///
    /// Uses the internal `broadcast` channel; receivers that lag beyond the channel capacity
    /// will observe a [`broadcast::error::RecvError::Lagged`] error.
    fn broadcast(&self, msg: M) -> BoxFuture<'_, ()>;
}

/// In-memory implementation of [`WsClients`].
///
/// Stores per-connection `mpsc` senders for unicast delivery and a single
/// `broadcast` channel for fan-out delivery. Both channels hold pre-encoded
/// [`Message`] values so encoding happens once regardless of the number of receivers.
pub struct InMemoryWsClients<M, C> {
    // mpsc sender per connection — used for unicast
    unicast_senders: DashMap<String, mpsc::Sender<Message>>,
    // single broadcast channel shared by all connections
    broadcast_tx: broadcast::Sender<Message>,
    _phantom: std::marker::PhantomData<(M, C)>,
}

impl<M, C> Default for InMemoryWsClients<M, C> {
    /// Creates an empty `InMemoryWsClients` with a broadcast channel capacity of 1024 messages.
    fn default() -> Self {
        let (broadcast_tx, _) = broadcast::channel(1024);
        Self {
            unicast_senders: DashMap::new(),
            broadcast_tx,
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<M, C> WsClients<M, C> for InMemoryWsClients<M, C>
where
    M: serde::Serialize + Send + Sync + 'static,
    C: WsCodec,
{
    /// Inserts `sender` into the unicast map under `connection_id`.
    fn add(&self, connection_id: Arc<str>, sender: mpsc::Sender<Message>) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.unicast_senders
                .insert(connection_id.to_string(), sender);
        })
    }

    /// Removes the entry for `connection_id` from the unicast map.
    fn remove<'a>(&'a self, connection_id: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.unicast_senders.remove(connection_id);
        })
    }

    /// Subscribes to the internal broadcast channel, returning a new receiver.
    fn subscribe(&self) -> broadcast::Receiver<Message> {
        self.broadcast_tx.subscribe()
    }

    /// Encodes `msg` with codec `C` and sends the resulting frame to the connection
    /// identified by `connection_id` via its `mpsc` sender.
    ///
    /// Encoding errors are logged at `error` level; missing or full channels are silently ignored.
    fn unicast<'a>(&'a self, connection_id: &'a str, msg: M) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            match C::encode(msg) {
                Ok(ws_msg) => {
                    if let Some(sender) = self.unicast_senders.get(connection_id) {
                        let _ = sender.send(ws_msg).await;
                    }
                }
                Err(e) => tracing::error!("encode error: {e}"),
            }
        })
    }

    /// Encodes `msg` with codec `C` and publishes the resulting frame to the broadcast channel.
    ///
    /// This is an O(1) operation: the frame is sent once and all active receivers retrieve it
    /// independently. Encoding errors are logged at `error` level.
    fn broadcast(&self, msg: M) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            match C::encode(msg) {
                Ok(ws_msg) => {
                    // O(1) — single send; all subscribers receive independently
                    let _ = self.broadcast_tx.send(ws_msg);
                }
                Err(e) => tracing::error!("encode error: {e}"),
            }
        })
    }
}

// --- Requests ---

/// Carries context for a new WebSocket connection event.
pub struct ConnectionRequest {
    /// Unique identifier assigned to this connection (UUID v4).
    pub connection_id: Arc<str>,
}

/// Carries context for a WebSocket disconnection event.
pub struct DisconnectRequest {
    /// Unique identifier of the connection that was closed.
    pub connection_id: Arc<str>,
}

// --- MessageContext ---

/// Provides message-sending capabilities to a hub's `on_message` handler.
///
/// `MessageContext` is created per incoming message and gives the handler access to both
/// unicast (reply to sender) and broadcast (fan-out to all) delivery.
pub struct MessageContext<M, C>
where
    M: serde::Serialize + Send + Sync + 'static,
    C: WsCodec,
{
    /// The connection ID of the client that sent the triggering message.
    pub connection_id: Arc<str>,
    clients: Arc<dyn WsClients<M, C>>,
}

impl<M, C> MessageContext<M, C>
where
    M: serde::Serialize + Send + Sync + 'static,
    C: WsCodec,
{
    /// Sends `msg` only to the connection that triggered the current handler invocation.
    pub async fn unicast(&self, msg: M) {
        self.clients.unicast(&self.connection_id, msg).await;
    }

    /// Sends `msg` to **all** active connections, including the sender.
    pub async fn broadcast(&self, msg: M) {
        self.clients.broadcast(msg).await;
    }
}

// --- WsHub ---

/// The core trait for defining WebSocket hub behaviour.
///
/// Implement `WsHub` to handle connection lifecycle events and incoming messages.
/// The hub is run by [`serve_hub`] or [`serve_hub_with_clients`].
///
/// # Associated types
/// - `Codec` — wire format used for encoding/decoding messages.
/// - `InMessage` — deserialized type of incoming client messages.
/// - `OutMessage` — type of messages sent back to clients.
pub trait WsHub: Send + Sync + 'static {
    /// The codec used to encode outgoing messages and decode incoming ones.
    type Codec: WsCodec;
    /// The Rust type that incoming WebSocket frames are deserialized into.
    type InMessage: serde::de::DeserializeOwned + Send;
    /// The Rust type of messages sent to clients (via unicast or broadcast).
    type OutMessage: serde::Serialize + Send + Sync + 'static;

    /// Called once when a new WebSocket connection is established.
    ///
    /// The default implementation logs the connection ID at `info` level.
    fn on_connect(&self, req: ConnectionRequest) -> impl Future<Output = ()> + Send {
        async move {
            tracing::info!("connected: {}", req.connection_id);
        }
    }

    /// Called once after a WebSocket connection has been fully torn down.
    ///
    /// The default implementation logs the connection ID at `info` level.
    fn on_disconnect(&self, req: DisconnectRequest) -> impl Future<Output = ()> + Send {
        async move {
            tracing::info!("disconnected: {}", req.connection_id);
        }
    }

    /// Called for every successfully decoded incoming message.
    ///
    /// `ctx` exposes `unicast` and `broadcast` helpers so the handler can reply to
    /// the sender or push updates to all connected clients.
    fn on_message(
        &self,
        req: Self::InMessage,
        ctx: MessageContext<Self::OutMessage, Self::Codec>,
    ) -> impl Future<Output = ()> + Send;
}

// --- serve_hub ---

/// Drives a WebSocket connection using the global in-memory client registry.
///
/// This is the high-level entry point. It looks up (or lazily creates) the shared
/// [`InMemoryWsClients`] for hub type `H` and then delegates to [`serve_hub_with_clients`]
/// with the default [`WsHubConfig`].
///
/// # Errors
/// If the global registry entry cannot be downcast (which should never happen), the connection
/// is dropped and an error is logged.
pub async fn serve_hub<H>(socket: WebSocket, hub: H)
where
    H: WsHub,
{
    match get_clients::<H>() {
        Some(clients) => {
            serve_hub_with_clients(socket, hub, clients, &WsHubConfig::default()).await
        }
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

    let (unicast_tx, unicast_rx) = mpsc::channel::<Message>(32);
    let heartbeat_tx = unicast_tx.clone();
    let broadcast_rx = clients.subscribe();
    let cancel = CancellationToken::new();

    clients.add(connection_id.clone(), unicast_tx).await;

    let hub = Arc::new(hub);
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
                        tracing::error!("ws read error on {}: {e}", connection_id);
                        break "read error";
                    }
                    Ok(Some(Ok(msg))) => {
                        heartbeat.reset();
                        if let Some(reason) =
                            handle_frame::<H>(msg, hub, clients, connection_id, cancel, heartbeat_tx).await
                        {
                            break reason;
                        }
                    }
                }
            }
            _ = heartbeat.tick() => {
                if heartbeat_tx.send(Message::Ping(vec![].into())).await.is_err() {
                    break "ping send error";
                }
            }
        }
    }
}

/// Handles a single incoming WebSocket frame.
///
/// Returns `Some(reason)` to signal that the connection should be closed, or `None` to continue
/// the read loop. Data frames are decoded and dispatched to [`WsHub::on_message`] in a separate
/// task that is cancelled when `cancel` fires.
async fn handle_frame<H>(
    msg: Message,
    hub: &Arc<H>,
    clients: &Arc<dyn WsClients<H::OutMessage, H::Codec>>,
    connection_id: &Arc<str>,
    cancel: &CancellationToken,
    heartbeat_tx: &mpsc::Sender<Message>,
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
        Message::Pong(_) => None,
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
