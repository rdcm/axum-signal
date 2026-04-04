use axum::extract::ws::{Message, WebSocket};
use dashmap::DashMap;
use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use std::any::TypeId;
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio::time::{interval, timeout};
use uuid::Uuid;

#[derive(Clone)]
pub struct WsHubConfig {
    pub idle_timeout: Duration,
    pub heartbeat_interval: Duration,
}

impl Default for WsHubConfig {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(60),
            heartbeat_interval: Duration::from_secs(15),
        }
    }
}

static CLIENT_REGISTRY: OnceLock<DashMap<TypeId, Box<dyn std::any::Any + Send + Sync>>> =
    OnceLock::new();

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

pub trait WsCodec: Send + Sync + 'static {
    fn decode<T: serde::de::DeserializeOwned>(msg: Message) -> anyhow::Result<T>;
    fn encode<T: serde::Serialize>(msg: T) -> anyhow::Result<Message>;
}

pub struct JsonCodec;

impl WsCodec for JsonCodec {
    fn decode<T: serde::de::DeserializeOwned>(msg: Message) -> anyhow::Result<T> {
        match msg {
            Message::Text(text) => Ok(serde_json::from_str(&text)?),
            Message::Binary(data) => Ok(serde_json::from_slice(&data)?),
            _ => Err(anyhow::anyhow!("unexpected message type")),
        }
    }

    fn encode<T: serde::Serialize>(value: T) -> anyhow::Result<Message> {
        Ok(Message::text(serde_json::to_string(&value)?))
    }
}

pub struct BinaryCodec;

impl WsCodec for BinaryCodec {
    fn decode<T: serde::de::DeserializeOwned>(msg: Message) -> anyhow::Result<T> {
        match msg {
            Message::Binary(data) => Ok(postcard::from_bytes(&data)?),
            _ => Err(anyhow::anyhow!("expected binary")),
        }
    }

    fn encode<T: serde::Serialize>(value: T) -> anyhow::Result<Message> {
        Ok(Message::binary(postcard::to_stdvec(&value)?))
    }
}

// --- WsClients ---

pub trait WsClients<M, C>: Send + Sync + 'static
where
    M: serde::Serialize + Send + Sync + 'static,
    C: WsCodec,
{
    fn add(&self, connection_id: Arc<str>, sender: mpsc::Sender<Message>) -> BoxFuture<'_, ()>;
    fn remove<'a>(&'a self, connection_id: &'a str) -> BoxFuture<'a, ()>;
    fn subscribe(&self) -> broadcast::Receiver<Message>;
    fn unicast<'a>(&'a self, connection_id: &'a str, msg: M) -> BoxFuture<'a, ()>;
    fn broadcast(&self, msg: M) -> BoxFuture<'_, ()>;
}

pub struct InMemoryWsClients<M, C> {
    // для unicast — mpsc sender на каждое соединение
    unicast_senders: DashMap<String, mpsc::Sender<Message>>,
    // для broadcast — один канал на всех
    broadcast_tx: broadcast::Sender<Message>,
    _phantom: std::marker::PhantomData<(M, C)>,
}

impl<M, C> Default for InMemoryWsClients<M, C> {
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
    fn add(&self, connection_id: Arc<str>, sender: mpsc::Sender<Message>) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.unicast_senders
                .insert(connection_id.to_string(), sender);
        })
    }

    fn remove<'a>(&'a self, connection_id: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.unicast_senders.remove(connection_id);
        })
    }

    fn subscribe(&self) -> broadcast::Receiver<Message> {
        self.broadcast_tx.subscribe()
    }

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

    fn broadcast(&self, msg: M) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            match C::encode(msg) {
                Ok(ws_msg) => {
                    // O(1) — один send, все подписчики получат сами
                    let _ = self.broadcast_tx.send(ws_msg);
                }
                Err(e) => tracing::error!("encode error: {e}"),
            }
        })
    }
}

// --- Requests ---

pub struct ConnectionRequest {
    pub connection_id: Arc<str>,
}

pub struct DisconnectRequest {
    pub connection_id: Arc<str>,
}

// --- MessageContext ---

pub struct MessageContext<M, C>
where
    M: serde::Serialize + Send + Sync + 'static,
    C: WsCodec,
{
    pub connection_id: Arc<str>,
    clients: Arc<dyn WsClients<M, C>>,
}

impl<M, C> MessageContext<M, C>
where
    M: serde::Serialize + Send + Sync + 'static,
    C: WsCodec,
{
    pub async fn unicast(&self, msg: M) {
        self.clients.unicast(&self.connection_id, msg).await;
    }

    pub async fn broadcast(&self, msg: M) {
        self.clients.broadcast(msg).await;
    }
}

// --- WsHub ---

pub trait WsHub: Send + Sync + 'static {
    type Codec: WsCodec;
    type InMessage: serde::de::DeserializeOwned + Send;
    type OutMessage: serde::Serialize + Send + Sync + 'static;

    fn on_connect(&self, req: ConnectionRequest) -> impl Future<Output = ()> + Send {
        async move {
            tracing::info!("connected: {}", req.connection_id);
        }
    }

    fn on_disconnect(&self, req: DisconnectRequest) -> impl Future<Output = ()> + Send {
        async move {
            tracing::info!("disconnected: {}", req.connection_id);
        }
    }

    fn on_message(
        &self,
        req: Self::InMessage,
        ctx: MessageContext<Self::OutMessage, Self::Codec>,
    ) -> impl Future<Output = ()> + Send;
}

// --- serve_hub ---

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

    let (unicast_tx, mut unicast_rx) = mpsc::channel::<Message>(32);
    let heartbeat_tx = unicast_tx.clone();
    let mut broadcast_rx = clients.subscribe();

    clients.add(connection_id.clone(), unicast_tx).await;

    let hub = Arc::new(hub);
    let clients: Arc<dyn WsClients<H::OutMessage, H::Codec>> = clients;
    let cancel = tokio_util::sync::CancellationToken::new();

    let writer_cancel = cancel.clone();
    let writer = tokio::spawn(async move {
        let mut sink = sink;
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
                _ = writer_cancel.cancelled() => break,
            }
        }
    });

    hub.on_connect(ConnectionRequest {
        connection_id: connection_id.clone(),
    })
    .await;

    let mut heartbeat = interval(config.heartbeat_interval);

    let disconnect_reason = loop {
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
                        match msg {
                            Message::Close(_) => break "client closed",
                            Message::Ping(p) => {
                                if heartbeat_tx.send(Message::Pong(p)).await.is_err() {
                                    break "pong send error";
                                }
                            }
                            Message::Pong(_) => {}
                            raw => match H::Codec::decode::<H::InMessage>(raw) {
                                Err(e) => {
                                    tracing::warn!("failed to parse message from {}: {e:?}", connection_id);
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
                                }
                            },
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
    };

    tracing::info!("connection {} closed: {}", connection_id, disconnect_reason);

    cancel.cancel();
    clients.remove(&connection_id).await;
    writer.await.ok();

    hub.on_disconnect(DisconnectRequest { connection_id }).await;
}
