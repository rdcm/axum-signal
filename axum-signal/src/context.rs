use std::sync::Arc;

use crate::clients::WsClients;
use crate::codec::WsCodec;

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
    pub(crate) clients: Arc<dyn WsClients<M, C>>,
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

    /// Sends `msg` to all connections **except** those listed in `excluded`.
    pub async fn broadcast_except(&self, excluded: &[&str], msg: M) {
        self.clients.broadcast_except(excluded, msg).await;
    }

    /// Adds the current connection to the named `group`.
    pub async fn add_to_group(&self, group: &str) {
        self.clients.add_to_group(&self.connection_id, group).await;
    }

    /// Removes the current connection from the named `group`.
    pub async fn remove_from_group(&self, group: &str) {
        self.clients
            .remove_from_group(&self.connection_id, group)
            .await;
    }

    /// Sends `msg` to all connections in `group`.
    pub async fn broadcast_group(&self, group: &str, msg: M) {
        self.clients.broadcast_group(group, msg).await;
    }

    /// Sends `msg` to all connections in `group` except those listed in `excluded`.
    pub async fn broadcast_group_except(&self, group: &str, excluded: &[&str], msg: M) {
        self.clients
            .broadcast_group_except(group, excluded, msg)
            .await;
    }

    /// Sends `msg` to all connections in any of the listed `groups`.
    ///
    /// Each connection receives the message at most once even if it belongs to multiple groups.
    pub async fn broadcast_groups(&self, groups: &[&str], msg: M) {
        self.clients.broadcast_groups(groups, msg).await;
    }
}
