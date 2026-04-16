use axum::extract::ws::Message;
use dashmap::{DashMap, DashSet};
use futures::future::BoxFuture;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::codec::WsCodec;
use crate::policy::BroadcastPolicy;

type OnDropFn = Arc<dyn Fn(Arc<str>) + Send + Sync>;

/// Per-connection state tracked by [`InMemoryWsClients`].
struct ConnectionState {
    sender: mpsc::Sender<Message>,
    cancel: CancellationToken,
    on_drop: OnDropFn,
    /// Number of consecutive dropped messages under the active [`BroadcastPolicy`].
    drops: u32,
    /// Rolling window of recent RTT samples (oldest first), used by [`BroadcastPolicy::DropOnHighRtt`].
    rtt_samples: VecDeque<Duration>,
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
        sender: mpsc::Sender<Message>,
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
    connections: DashMap<Arc<str>, ConnectionState>,
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
        sender: mpsc::Sender<Message>,
        cancel: CancellationToken,
        on_drop: Arc<dyn Fn(Arc<str>) + Send + Sync>,
    ) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.connections.insert(
                connection_id,
                ConnectionState {
                    sender,
                    cancel,
                    on_drop,
                    drops: 0,
                    rtt_samples: VecDeque::new(),
                },
            );
        })
    }

    fn remove<'a>(&'a self, connection_id: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
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
                        .map(|r| r.sender.clone());
                    if let Some(sender) = sender {
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
                    let ids: Vec<Arc<str>> = self
                        .connections
                        .iter()
                        .filter(|e| !excluded.contains(&e.key().as_ref()))
                        .map(|e| e.key().clone())
                        .collect();
                    for id in ids {
                        self.send_with_policy(&id, ws_msg.clone()).await;
                    }
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
                    let ids: Vec<Arc<str>> = match self.group_members.get(group) {
                        Some(members) => members.iter().map(|id| id.clone()).collect(),
                        None => return,
                    };
                    for id in ids {
                        self.send_with_policy(&id, ws_msg.clone()).await;
                    }
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
                    let ids: Vec<Arc<str>> = match self.group_members.get(group) {
                        Some(members) => members
                            .iter()
                            .filter(|id| !excluded.contains(&id.as_ref()))
                            .map(|id| id.clone())
                            .collect(),
                        None => return,
                    };
                    for id in ids {
                        self.send_with_policy(&id, ws_msg.clone()).await;
                    }
                }
                Err(e) => tracing::error!("encode error: {e}"),
            }
        })
    }

    fn broadcast_groups<'a>(&'a self, groups: &'a [&'a str], msg: M) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            match C::encode(msg) {
                Ok(ws_msg) => {
                    use std::collections::HashSet;
                    let mut seen: HashSet<Arc<str>> = HashSet::new();
                    let mut ids: Vec<Arc<str>> = Vec::new();
                    for group in groups {
                        let conn_ids: Vec<Arc<str>> = match self.group_members.get(*group) {
                            Some(members) => members.iter().map(|id| id.clone()).collect(),
                            None => continue,
                        };
                        for id in conn_ids {
                            if seen.insert(id.clone()) {
                                ids.push(id);
                            }
                        }
                    }
                    for id in ids {
                        self.send_with_policy(&id, ws_msg.clone()).await;
                    }
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
            if let Some(mut state) = self.connections.get_mut(connection_id) {
                if state.rtt_samples.len() == capacity {
                    state.rtt_samples.pop_front();
                }
                state.rtt_samples.push_back(rtt);
            }
        })
    }
}

impl<M, C> InMemoryWsClients<M, C>
where
    M: serde::Serialize + Send + Sync + 'static,
    C: WsCodec,
{
    /// Delivers `msg` to the connection identified by `id` according to the active policy.
    async fn send_with_policy(&self, id: &Arc<str>, msg: Message) {
        match &self.policy {
            BroadcastPolicy::Block => {
                if let Some(state) = self.connections.get(id) {
                    let sender = state.sender.clone();
                    drop(state);
                    let _ = sender.send(msg).await;
                }
            }
            BroadcastPolicy::DropMessage { timeout: dur } => {
                let (sender, on_drop) = match self.connections.get(id) {
                    Some(s) => (s.sender.clone(), s.on_drop.clone()),
                    None => return,
                };
                let dropped = timeout(*dur, sender.send(msg))
                    .await
                    .map_or(true, |r| r.is_err());
                if dropped {
                    on_drop(id.clone());
                    tracing::warn!("message dropped for connection {id}");
                }
            }
            BroadcastPolicy::DropConnection {
                timeout: dur,
                max_drops,
            } => {
                let (sender, on_drop) = match self.connections.get(id) {
                    Some(s) => (s.sender.clone(), s.on_drop.clone()),
                    None => return,
                };
                let dropped = timeout(*dur, sender.send(msg))
                    .await
                    .map_or(true, |r| r.is_err());
                if dropped {
                    on_drop(id.clone());
                    tracing::warn!("message dropped for connection {id}");

                    let should_disconnect = self
                        .connections
                        .get_mut(id)
                        .map(|mut s| {
                            s.drops += 1;
                            s.drops >= *max_drops
                        })
                        .unwrap_or(false);

                    if should_disconnect {
                        tracing::info!(
                            "disconnecting connection {id} after {} consecutive drops",
                            max_drops
                        );
                        if let Some((_, state)) = self.connections.remove(id.as_ref()) {
                            state.cancel.cancel();
                        }
                        // Clean up group membership
                        if let Some((_, groups)) = self.connection_groups.remove(id.as_ref()) {
                            for group in groups.iter() {
                                if let Some(members) = self.group_members.get(group.as_ref()) {
                                    members.remove(id.as_ref());
                                    if members.is_empty() {
                                        drop(members);
                                        self.group_members.remove(group.as_ref());
                                    }
                                }
                            }
                        }
                    }
                } else if let Some(mut state) = self.connections.get_mut(id) {
                    // Successful send resets the consecutive drop counter.
                    state.drops = 0;
                }
            }
            BroadcastPolicy::DropOnHighRtt { max_rtt, .. } => {
                let (sender, on_drop, avg_rtt) = match self.connections.get(id) {
                    Some(s) => {
                        let avg = if s.rtt_samples.is_empty() {
                            None
                        } else {
                            let sum: Duration = s.rtt_samples.iter().sum();
                            Some(sum / s.rtt_samples.len() as u32)
                        };
                        (s.sender.clone(), s.on_drop.clone(), avg)
                    }
                    None => return,
                };
                // No samples yet — treat the connection as healthy.
                if avg_rtt.is_some_and(|avg| avg > *max_rtt) {
                    tracing::warn!(
                        "dropping connection {id}: avg RTT {:?} exceeds max {:?}",
                        avg_rtt.unwrap(),
                        max_rtt
                    );
                    on_drop(id.clone());
                    if let Some((_, state)) = self.connections.remove(id.as_ref()) {
                        state.cancel.cancel();
                    }
                    if let Some((_, groups)) = self.connection_groups.remove(id.as_ref()) {
                        for group in groups.iter() {
                            if let Some(members) = self.group_members.get(group.as_ref()) {
                                members.remove(id.as_ref());
                                if members.is_empty() {
                                    drop(members);
                                    self.group_members.remove(group.as_ref());
                                }
                            }
                        }
                    }
                } else {
                    let _ = sender.send(msg).await;
                }
            }
        }
    }
}
