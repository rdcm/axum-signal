use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::codec::WsCodec;
use crate::context::MessageContext;
use crate::policy::BroadcastPolicy;

/// Configuration for a [`WsHub`] instance.
///
/// Controls connection health parameters, heartbeat cadence, broadcast delivery policy,
/// and channel buffer sizes.
#[derive(Clone)]
pub struct WsHubConfig {
    /// How long a connection may remain idle (no incoming messages) before it is dropped.
    pub idle_timeout: Duration,
    /// Interval between outgoing WebSocket ping frames used to keep the connection alive.
    pub heartbeat_interval: Duration,
    /// Delivery policy applied when broadcasting to a slow or unresponsive connection.
    pub policy: BroadcastPolicy,

    /// Capacity of the per-connection send buffer between the hub and the WebSocket writer task.
    ///
    /// The buffer fills when WebSocket write latency exceeds `capacity / message_rate`:
    ///
    /// | Message rate | Buffer fills in (capacity = 32) |
    /// |---|---|
    /// | 10 msg/s    | 3.2 s   |
    /// | 100 msg/s   | 320 ms  |
    /// | 1 000 msg/s | 32 ms   |
    /// | 10 000 msg/s| 3.2 ms  |
    ///
    /// With [`BroadcastPolicy::DropMessage`] or [`BroadcastPolicy::DropConnection`] the policy
    /// timeout fires before the buffer fills. Increase for high-throughput unicast streams
    /// (audio chunks, LLM token streaming) or when using [`BroadcastPolicy::Block`].
    pub unicast_channel_capacity: usize,

    /// Capacity of the shared fan-out broadcast channel.
    ///
    /// A subscriber receives a `Lagged` error when it falls behind by more than this many
    /// messages, at which point it skips forward to the latest message.
    ///
    /// | Broadcast rate | Time to `Lagged` (capacity = 1024) |
    /// |---|---|
    /// | 10 msg/s    | 102 s  |
    /// | 100 msg/s   | 10 s   |
    /// | 1 000 msg/s | 1 s    |
    /// | 10 000 msg/s| 100 ms |
    ///
    /// Increase for high-frequency broadcasts: game state at 60 fps exhausts 1024 in ~17 s,
    /// but a player lagging 17 s is already lost — consider 4096 to detect slow clients faster
    /// while still tolerating short network hiccups.
    pub broadcast_channel_capacity: usize,
}

impl Default for WsHubConfig {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(60),
            heartbeat_interval: Duration::from_secs(15),
            policy: BroadcastPolicy::default(),
            unicast_channel_capacity: 32,
            broadcast_channel_capacity: 1024,
        }
    }
}

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

/// The core trait for defining WebSocket hub behaviour.
///
/// Implement `WsHub` to handle connection lifecycle events and incoming messages.
/// The hub is run by [`serve_hub`](crate::serve_hub) or [`serve_hub_with_clients`](crate::serve_hub_with_clients).
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

    /// Called when a message could not be delivered to a connection due to the active
    /// [`BroadcastPolicy`](crate::BroadcastPolicy).
    ///
    /// The default implementation logs at `warn` level. Override to record metrics, e.g.
    /// increment a Prometheus counter.
    ///
    /// For [`BroadcastPolicy::DropConnection`](crate::BroadcastPolicy::DropConnection), this is
    /// called on every dropped message — including the final one that triggers the disconnect.
    fn on_message_drop(&self, connection_id: Arc<str>) -> impl Future<Output = ()> + Send {
        async move {
            tracing::warn!("message dropped for connection {}", connection_id);
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
