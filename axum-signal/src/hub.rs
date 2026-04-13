use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::codec::WsCodec;
use crate::context::MessageContext;

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
