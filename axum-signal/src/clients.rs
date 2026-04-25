use axum::extract::ws::Message;
use dashmap::{DashMap, DashSet};
use futures::future::BoxFuture;
use futures::stream::{self, StreamExt};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::codec::WsCodec;
use crate::policy::BroadcastPolicy;

trait IntoStream: IntoIterator + Sized {
    fn stream_iter(self) -> stream::Iter<Self::IntoIter> {
        stream::iter(self)
    }
}

impl<T: IntoIterator> IntoStream for T {}

type OnDropFn = Arc<dyn Fn(Arc<str>) + Send + Sync>;

/// Per-connection state tracked by [`InMemoryWsClients`].
struct ConnectionState {
    id: Arc<str>,
    sender: mpsc::Sender<Arc<Message>>,
    cancel: CancellationToken,
    on_drop: OnDropFn,
    /// Number of consecutive dropped messages under the active [`BroadcastPolicy`].
    drops: AtomicU32,
    /// Rolling window of recent RTT samples (oldest first), used by [`BroadcastPolicy::DropOnHighRtt`].
    rtt_samples: RwLock<VecDeque<Duration>>,
}

/// Manages the set of active WebSocket connections for a hub.
///
/// Each connection is identified by a unique string `connection_id`. The trait provides
/// both unicast (point-to-point) and broadcast (one-to-all) delivery, encoded via codec `C`.
pub trait WsClients<M, C>: Send + Sync + 'static
where
    M: serde::Serialize + Send + Sync + 'static,
    C: WsCodec,
{
    /// Registers a new connection.
    ///
    /// `cancel` is the same token used by `serve` to shut down the connection's tasks;
    /// [`BroadcastPolicy::DropConnection`] will cancel it to close the connection.
    ///
    /// `on_drop` is called whenever a message cannot be delivered to this connection due to the
    /// active [`BroadcastPolicy`]. The connection id is passed as the argument.
    fn add(
        &self,
        connection_id: Arc<str>,
        sender: mpsc::Sender<Arc<Message>>,
        cancel: CancellationToken,
        on_drop: Arc<dyn Fn(Arc<str>) + Send + Sync>,
    ) -> BoxFuture<'_, ()>;

    /// Removes the connection identified by `connection_id` from the registry.
    fn remove<'a>(&'a self, connection_id: &'a str) -> BoxFuture<'a, ()>;

    /// Returns a new [`broadcast::Receiver`] that will receive all future broadcast messages.
    fn subscribe(&self) -> broadcast::Receiver<Message>;

    /// Encodes `msg` and sends it only to the connection identified by `connection_id`.
    fn unicast<'a>(&'a self, connection_id: &'a str, msg: M) -> BoxFuture<'a, ()>;

    /// Encodes `msg` and delivers it to **all** currently subscribed connections in O(1).
    fn broadcast(&self, msg: M) -> BoxFuture<'_, ()>;

    /// Encodes `msg` and sends it to all connections **except** those listed in `excluded`.
    ///
    /// Applies the active [`BroadcastPolicy`] for each recipient.
    fn broadcast_except<'a>(&'a self, excluded: &'a [&'a str], msg: M) -> BoxFuture<'a, ()>;

    /// Adds `connection_id` to the named `group`.
    fn add_to_group<'a>(&'a self, connection_id: &'a str, group: &'a str) -> BoxFuture<'a, ()>;

    /// Removes `connection_id` from the named `group`.
    fn remove_from_group<'a>(&'a self, connection_id: &'a str, group: &'a str)
    -> BoxFuture<'a, ()>;

    /// Encodes `msg` and sends it to all connections in `group`.
    fn broadcast_group<'a>(&'a self, group: &'a str, msg: M) -> BoxFuture<'a, ()>;

    /// Encodes `msg` and sends it to all connections in `group` except those in `excluded`.
    fn broadcast_group_except<'a>(
        &'a self,
        group: &'a str,
        excluded: &'a [&'a str],
        msg: M,
    ) -> BoxFuture<'a, ()>;

    /// Encodes `msg` and sends it to all connections in any of the listed `groups`.
    ///
    /// Each connection receives the message at most once even if it belongs to multiple groups.
    fn broadcast_groups<'a>(&'a self, groups: &'a [&'a str], msg: M) -> BoxFuture<'a, ()>;

    /// Records the latest RTT measurement for a connection.
    ///
    /// Called by the serve layer after each ping/pong exchange. Used by
    /// [`BroadcastPolicy::DropOnHighRtt`] to gate delivery before sending.
    fn update_rtt<'a>(&'a self, connection_id: &'a str, rtt: Duration) -> BoxFuture<'a, ()>;
}

/// In-memory implementation of [`WsClients`].
///
/// Stores per-connection [`ConnectionState`] for unicast delivery and a single
/// `broadcast` channel for fan-out delivery.
///
/// # Configuration
///
/// Use the builder methods to customise delivery behaviour:
///
/// ```rust,no_run
/// use axum_signal::{InMemoryWsClients, WsHubConfig, JsonCodec};
///
/// # #[derive(serde::Serialize)] struct MyMsg;
/// let config = WsHubConfig::default();
/// let clients = InMemoryWsClients::<MyMsg, JsonCodec>::new(
///     config.policy,
///     config.broadcast_channel_capacity,
/// );
/// ```
pub struct InMemoryWsClients<M, C> {
    connections: DashMap<Arc<str>, Arc<ConnectionState>>,
    broadcast_tx: broadcast::Sender<Message>,
    group_members: DashMap<Arc<str>, DashSet<Arc<str>>>,
    connection_groups: DashMap<Arc<str>, DashSet<Arc<str>>>,
    policy: BroadcastPolicy,
    _phantom: std::marker::PhantomData<(M, C)>,
}

impl<M, C> InMemoryWsClients<M, C> {
    pub fn new(policy: BroadcastPolicy, broadcast_channel_capacity: usize) -> Self {
        let (broadcast_tx, _) = broadcast::channel(broadcast_channel_capacity);
        Self {
            connections: DashMap::new(),
            broadcast_tx,
            group_members: DashMap::new(),
            connection_groups: DashMap::new(),
            policy,
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<M, C> WsClients<M, C> for InMemoryWsClients<M, C>
where
    M: serde::Serialize + Send + Sync + 'static,
    C: WsCodec,
{
    fn add(
        &self,
        connection_id: Arc<str>,
        sender: mpsc::Sender<Arc<Message>>,
        cancel: CancellationToken,
        on_drop: Arc<dyn Fn(Arc<str>) + Send + Sync>,
    ) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.connections.insert(
                connection_id.clone(),
                Arc::new(ConnectionState {
                    id: connection_id,
                    sender,
                    cancel,
                    on_drop,
                    drops: AtomicU32::new(0),
                    rtt_samples: RwLock::new(VecDeque::new()),
                }),
            );
        })
    }

    fn remove<'a>(&'a self, connection_id: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.cleanup_groups(connection_id);
            self.connections.remove(connection_id);
        })
    }

    fn subscribe(&self) -> broadcast::Receiver<Message> {
        self.broadcast_tx.subscribe()
    }

    fn unicast<'a>(&'a self, connection_id: &'a str, msg: M) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            match C::encode(msg) {
                Ok(ws_msg) => {
                    let sender = self
                        .connections
                        .get(connection_id)
                        .map(|s| s.sender.clone());
                    if let Some(sender) = sender {
                        let _ = sender.send(Arc::new(ws_msg)).await;
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
                    let _ = self.broadcast_tx.send(ws_msg);
                }
                Err(e) => tracing::error!("encode error: {e}"),
            }
        })
    }

    fn broadcast_except<'a>(&'a self, excluded: &'a [&'a str], msg: M) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            match C::encode(msg) {
                Ok(ws_msg) => {
                    let arc_msg = Arc::new(ws_msg);
                    self.connections
                        .iter()
                        .filter(|e| !excluded.contains(&e.key().as_ref()))
                        .map(|e| e.value().clone())
                        .collect::<Vec<_>>()
                        .stream_iter()
                        .for_each(|s| {
                            let msg = Arc::clone(&arc_msg);
                            async move { self.send_with_policy(&s, msg).await }
                        })
                        .await;
                }
                Err(e) => tracing::error!("encode error: {e}"),
            }
        })
    }

    fn add_to_group<'a>(&'a self, connection_id: &'a str, group: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let conn: Arc<str> = Arc::from(connection_id);
            let grp: Arc<str> = Arc::from(group);
            self.group_members
                .entry(grp.clone())
                .or_default()
                .insert(conn.clone());
            self.connection_groups.entry(conn).or_default().insert(grp);
        })
    }

    fn remove_from_group<'a>(
        &'a self,
        connection_id: &'a str,
        group: &'a str,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Some(members) = self.group_members.get(group) {
                members.remove(connection_id);
            }
            if let Some(groups) = self.connection_groups.get(connection_id) {
                groups.remove(group);
            }
        })
    }

    fn broadcast_group<'a>(&'a self, group: &'a str, msg: M) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            match C::encode(msg) {
                Ok(ws_msg) => {
                    let Some(members) = self.group_members.get(group) else {
                        return;
                    };
                    let arc_msg = Arc::new(ws_msg);
                    members
                        .iter()
                        .filter_map(|id| {
                            self.connections.get(id.as_ref()).map(|s| s.value().clone())
                        })
                        .collect::<Vec<_>>()
                        .stream_iter()
                        .for_each(|s| {
                            let msg = Arc::clone(&arc_msg);
                            async move { self.send_with_policy(&s, msg).await }
                        })
                        .await;
                }
                Err(e) => tracing::error!("encode error: {e}"),
            }
        })
    }

    fn broadcast_group_except<'a>(
        &'a self,
        group: &'a str,
        excluded: &'a [&'a str],
        msg: M,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            match C::encode(msg) {
                Ok(ws_msg) => {
                    let Some(members) = self.group_members.get(group) else {
                        return;
                    };
                    let arc_msg = Arc::new(ws_msg);
                    members
                        .iter()
                        .filter(|id| !excluded.contains(&id.as_ref()))
                        .filter_map(|id| {
                            self.connections.get(id.as_ref()).map(|s| s.value().clone())
                        })
                        .collect::<Vec<_>>()
                        .stream_iter()
                        .for_each(|s| {
                            let msg = Arc::clone(&arc_msg);
                            async move { self.send_with_policy(&s, msg).await }
                        })
                        .await;
                }
                Err(e) => tracing::error!("encode error: {e}"),
            }
        })
    }

    fn broadcast_groups<'a>(&'a self, groups: &'a [&'a str], msg: M) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            match C::encode(msg) {
                Ok(ws_msg) => {
                    let arc_msg = Arc::new(ws_msg);
                    let mut seen: HashSet<Arc<str>> = HashSet::new();
                    groups
                        .iter()
                        .flat_map(|g| {
                            self.group_members
                                .get(*g)
                                .map(|m| m.iter().map(|id| id.clone()).collect::<Vec<_>>())
                                .unwrap_or_default()
                        })
                        .filter(|id| seen.insert(id.clone()))
                        .filter_map(|id| {
                            self.connections.get(id.as_ref()).map(|s| s.value().clone())
                        })
                        .collect::<Vec<_>>()
                        .stream_iter()
                        .for_each(|s| {
                            let msg = Arc::clone(&arc_msg);
                            async move { self.send_with_policy(&s, msg).await }
                        })
                        .await;
                }
                Err(e) => tracing::error!("encode error: {e}"),
            }
        })
    }

    fn update_rtt<'a>(&'a self, connection_id: &'a str, rtt: Duration) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let capacity = match &self.policy {
                BroadcastPolicy::DropOnHighRtt { rtt_samples, .. } => *rtt_samples,
                _ => return,
            };
            let state = self
                .connections
                .get(connection_id)
                .map(|s| s.value().clone());
            if let Some(state) = state {
                let mut samples = state.rtt_samples.write().unwrap();
                if samples.len() == capacity {
                    samples.pop_front();
                }
                samples.push_back(rtt);
            }
        })
    }
}

impl<M, C> InMemoryWsClients<M, C>
where
    M: serde::Serialize + Send + Sync + 'static,
    C: WsCodec,
{
    /// Delivers `msg` to the connection according to the active policy.
    async fn send_with_policy(&self, state: &ConnectionState, msg: Arc<Message>) {
        match &self.policy {
            BroadcastPolicy::Block => {
                let _ = state.sender.send(msg).await;
            }
            BroadcastPolicy::DropMessage { timeout: dur } => {
                let dropped = timeout(*dur, state.sender.send(msg))
                    .await
                    .map_or(true, |r| r.is_err());
                if dropped {
                    (state.on_drop)(state.id.clone());
                    tracing::warn!("message dropped for connection {}", state.id);
                }
            }
            BroadcastPolicy::DropConnection {
                timeout: dur,
                max_drops,
            } => {
                let dropped = timeout(*dur, state.sender.send(msg))
                    .await
                    .map_or(true, |r| r.is_err());
                if dropped {
                    (state.on_drop)(state.id.clone());
                    tracing::warn!("message dropped for connection {}", state.id);

                    let drops = state.drops.fetch_add(1, Ordering::Relaxed) + 1;
                    if drops >= *max_drops {
                        tracing::info!(
                            "disconnecting connection {} after {} consecutive drops",
                            state.id,
                            max_drops
                        );
                        if let Some((_, cs)) = self.connections.remove(state.id.as_ref()) {
                            cs.cancel.cancel();
                        }
                        self.cleanup_groups(&state.id);
                    }
                } else {
                    state.drops.store(0, Ordering::Relaxed);
                }
            }
            BroadcastPolicy::DropOnHighRtt { max_rtt, .. } => {
                let avg_rtt = {
                    let samples = state.rtt_samples.read().unwrap();
                    if samples.is_empty() {
                        None
                    } else {
                        let sum: Duration = samples.iter().sum();
                        Some(sum / samples.len() as u32)
                    }
                };
                if avg_rtt.is_some_and(|avg| avg > *max_rtt) {
                    tracing::warn!(
                        "dropping connection {}: avg RTT {:?} exceeds max {:?}",
                        state.id,
                        avg_rtt.unwrap(),
                        max_rtt
                    );
                    (state.on_drop)(state.id.clone());
                    if let Some((_, cs)) = self.connections.remove(state.id.as_ref()) {
                        cs.cancel.cancel();
                    }
                    self.cleanup_groups(&state.id);
                } else {
                    let _ = state.sender.send(msg).await;
                }
            }
        }
    }

    fn cleanup_groups(&self, connection_id: &str) {
        if let Some((_, groups)) = self.connection_groups.remove(connection_id) {
            for group in groups.iter() {
                if let Some(members) = self.group_members.get(group.as_ref()) {
                    members.remove(connection_id);
                    if members.is_empty() {
                        drop(members);
                        self.group_members.remove(group.as_ref());
                    }
                }
            }
        }
    }
}
