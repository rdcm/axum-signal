use axum::extract::ws::Message;
use dashmap::{DashMap, DashSet};
use futures::future::BoxFuture;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

use crate::codec::WsCodec;

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

    /// Encodes `msg` and sends it to all connections **except** those listed in `excluded`.
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
}

/// In-memory implementation of [`WsClients`].
///
/// Stores per-connection `mpsc` senders for unicast delivery and a single
/// `broadcast` channel for fan-out delivery. Both channels hold pre-encoded
/// [`Message`] values so encoding happens once regardless of the number of receivers.
pub struct InMemoryWsClients<M, C> {
    // mpsc sender per connection — used for unicast
    unicast_senders: DashMap<Arc<str>, mpsc::Sender<Message>>,
    // single broadcast channel shared by all connections
    broadcast_tx: broadcast::Sender<Message>,
    // group name -> set of connection ids
    group_members: DashMap<Arc<str>, DashSet<Arc<str>>>,
    // connection id -> set of group names (reverse index for cleanup on disconnect)
    connection_groups: DashMap<Arc<str>, DashSet<Arc<str>>>,
    _phantom: std::marker::PhantomData<(M, C)>,
}

impl<M, C> Default for InMemoryWsClients<M, C> {
    /// Creates an empty `InMemoryWsClients` with a broadcast channel capacity of 1024 messages.
    fn default() -> Self {
        let (broadcast_tx, _) = broadcast::channel(1024);
        Self {
            unicast_senders: DashMap::new(),
            broadcast_tx,
            group_members: DashMap::new(),
            connection_groups: DashMap::new(),
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
            self.unicast_senders.insert(connection_id, sender);
        })
    }

    /// Removes the entry for `connection_id` from the unicast map and all groups it belongs to.
    fn remove<'a>(&'a self, connection_id: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Some((_, groups)) = self.connection_groups.remove(connection_id) {
                for group in groups.iter() {
                    if let Some(members) = self.group_members.get(group.as_ref()) {
                        members.remove(connection_id);
                    }
                }
            }
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
                    // Clone the sender before dropping the DashMap guard — holding a DashMap Ref
                    // across an `.await` keeps the shard's read lock live and causes contention.
                    let sender = self.unicast_senders.get(connection_id).map(|r| r.clone());
                    if let Some(sender) = sender {
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

    fn broadcast_except<'a>(&'a self, excluded: &'a [&'a str], msg: M) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            match C::encode(msg) {
                Ok(ws_msg) => {
                    let senders: Vec<_> = self
                        .unicast_senders
                        .iter()
                        .filter(|e| !excluded.contains(&e.key().as_ref()))
                        .map(|e| e.value().clone())
                        .collect();
                    for sender in senders {
                        let _ = sender.send(ws_msg.clone()).await;
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
                    let conn_ids: Vec<Arc<str>> = match self.group_members.get(group) {
                        Some(members) => members.iter().map(|id| id.clone()).collect(),
                        None => return,
                    };
                    let senders: Vec<_> = conn_ids
                        .iter()
                        .filter_map(|id| self.unicast_senders.get(id.as_ref()).map(|s| s.clone()))
                        .collect();
                    for sender in senders {
                        let _ = sender.send(ws_msg.clone()).await;
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
                    let conn_ids: Vec<Arc<str>> = match self.group_members.get(group) {
                        Some(members) => members
                            .iter()
                            .filter(|id| !excluded.contains(&id.as_ref()))
                            .map(|id| id.clone())
                            .collect(),
                        None => return,
                    };
                    let senders: Vec<_> = conn_ids
                        .iter()
                        .filter_map(|id| self.unicast_senders.get(id.as_ref()).map(|s| s.clone()))
                        .collect();
                    for sender in senders {
                        let _ = sender.send(ws_msg.clone()).await;
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
                    let mut senders: Vec<mpsc::Sender<Message>> = Vec::new();
                    for group in groups {
                        let conn_ids: Vec<Arc<str>> = match self.group_members.get(*group) {
                            Some(members) => members.iter().map(|id| id.clone()).collect(),
                            None => continue,
                        };
                        for id in conn_ids {
                            if seen.insert(id.clone())
                                && let Some(s) = self.unicast_senders.get(id.as_ref())
                            {
                                senders.push(s.clone());
                            }
                        }
                    }
                    for sender in senders {
                        let _ = sender.send(ws_msg.clone()).await;
                    }
                }
                Err(e) => tracing::error!("encode error: {e}"),
            }
        })
    }
}
