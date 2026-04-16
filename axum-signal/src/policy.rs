use std::time::Duration;

/// Strategy for delivering messages to connections that are slow to consume them.
///
/// Applies to all broadcast operations that iterate over per-connection mpsc senders:
/// [`broadcast_except`](crate::WsClients::broadcast_except), [`broadcast_group`](crate::WsClients::broadcast_group),
/// [`broadcast_group_except`](crate::WsClients::broadcast_group_except), [`broadcast_groups`](crate::WsClients::broadcast_groups).
///
/// Plain [`broadcast`](crate::WsClients::broadcast) uses a `tokio::broadcast` channel and handles
/// lagged receivers separately via [`RecvError::Lagged`](tokio::sync::broadcast::error::RecvError::Lagged).
#[derive(Clone, Debug, Default)]
pub enum BroadcastPolicy {
    /// Wait indefinitely until the connection's send buffer has space.
    ///
    /// Guarantees delivery as long as the TCP connection is alive.
    /// A single slow client will block the broadcast loop for all subsequent clients.
    #[default]
    Block,

    /// Attempt to send within `timeout`. If the deadline is exceeded, the message is dropped
    /// for that connection and [`WsHub::on_message_drop`](crate::WsHub::on_message_drop) is called.
    ///
    /// The connection is never closed regardless of how many messages are dropped.
    DropMessage { timeout: Duration },

    /// Attempt to send within `timeout`. If the deadline is exceeded, the drop is counted.
    /// Once `max_drops` consecutive drops accumulate for a connection, it is forcibly closed.
    ///
    /// Subsumes `DropMessage`: every dropped message triggers
    /// [`WsHub::on_message_drop`](crate::WsHub::on_message_drop) before the disconnect check.
    DropConnection { timeout: Duration, max_drops: u32 },

    /// Skip delivery and close the connection if its rolling-average RTT exceeds `max_rtt`.
    ///
    /// ## How RTT is measured
    ///
    /// The serve layer sends a WebSocket `Ping` frame every `heartbeat_interval` and records the
    /// elapsed time until the matching `Pong` arrives. Each measurement is appended to a
    /// per-connection ring buffer; once it holds `rtt_samples` samples the oldest is evicted on every
    /// new arrival.
    ///
    /// ## Delivery decision
    ///
    /// Before each send the rolling average of all buffered samples is compared against
    /// `max_rtt`. Until `rtt_samples` samples have been collected the average is computed over
    /// however many samples exist, so the policy becomes effective after the first pong.
    /// Before any pong is received the connection is treated as healthy and messages are
    /// delivered normally.
    ///
    /// ## Choosing `rtt_samples`
    ///
    /// `rtt_samples` controls the trade-off between responsiveness and noise resistance:
    ///
    /// - **Small rtt_samples (e.g. 3)** — reacts quickly to a sudden latency jump, but a single
    ///   delayed pong (e.g. caused by a GC pause on the client) can trigger a disconnect.
    /// - **Large rtt_samples (e.g. 16)** — tolerates transient spikes, but takes longer to act on a
    ///   genuinely degraded link.
    ///
    /// With the default `heartbeat_interval` of 15 s, `rtt_samples = 8` keeps ~2 minutes of history.
    /// A good starting point for most applications.
    ///
    /// ## On drop
    ///
    /// [`WsHub::on_message_drop`](crate::WsHub::on_message_drop) is called before the connection
    /// is removed, the same as [`DropConnection`].
    DropOnHighRtt {
        max_rtt: Duration,
        rtt_samples: usize,
    },
}
