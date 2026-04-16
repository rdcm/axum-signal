use axum::Router;
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use axum_signal::{
    BroadcastPolicy, InMemoryWsClients, JsonCodec, MessageContext, WsHub, WsHubConfig,
    serve_hub_with_clients,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Message sent from test client to the hub.
#[derive(Serialize, Deserialize)]
pub enum TestCommand {
    GetConnectionId,
    Unicast {
        payload: String,
    },
    Broadcast {
        payload: String,
    },
    BroadcastExcept {
        excluded: Vec<String>,
        payload: String,
    },
    AddToGroup {
        group: String,
    },
    RemoveFromGroup {
        group: String,
    },
    BroadcastGroup {
        group: String,
        payload: String,
    },
    BroadcastGroupExcept {
        group: String,
        excluded: Vec<String>,
        payload: String,
    },
    BroadcastGroups {
        groups: Vec<String>,
        payload: String,
    },
}

/// Message sent from the hub back to test clients.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TestReply {
    pub payload: String,
}

impl TestReply {
    pub fn new(payload: impl Into<String>) -> Self {
        Self {
            payload: payload.into(),
        }
    }
}

/// Test hub that dispatches `TestCommand` to the corresponding `MessageContext` operation.
///
/// State-mutating commands (`AddToGroup`, `RemoveFromGroup`) send a unicast `"ack"` after
/// the mutation so that test clients can synchronise before issuing follow-up commands.
pub struct TestHub;

impl WsHub for TestHub {
    type Codec = JsonCodec;
    type InMessage = TestCommand;
    type OutMessage = TestReply;

    async fn on_message(&self, msg: TestCommand, ctx: MessageContext<TestReply, JsonCodec>) {
        match msg {
            TestCommand::GetConnectionId => {
                ctx.unicast(TestReply::new(ctx.connection_id.as_ref()))
                    .await;
            }
            TestCommand::Unicast { payload } => {
                ctx.unicast(TestReply::new(payload)).await;
            }
            TestCommand::Broadcast { payload } => {
                ctx.broadcast(TestReply::new(payload)).await;
            }
            TestCommand::BroadcastExcept { excluded, payload } => {
                let excluded: Vec<&str> = excluded.iter().map(String::as_str).collect();
                ctx.broadcast_except(&excluded, TestReply::new(payload))
                    .await;
            }
            TestCommand::AddToGroup { group } => {
                ctx.add_to_group(&group).await;
                ctx.unicast(TestReply::new("ack")).await;
            }
            TestCommand::RemoveFromGroup { group } => {
                ctx.remove_from_group(&group).await;
                ctx.unicast(TestReply::new("ack")).await;
            }
            TestCommand::BroadcastGroup { group, payload } => {
                ctx.broadcast_group(&group, TestReply::new(payload)).await;
            }
            TestCommand::BroadcastGroupExcept {
                group,
                excluded,
                payload,
            } => {
                let excluded: Vec<&str> = excluded.iter().map(String::as_str).collect();
                ctx.broadcast_group_except(&group, &excluded, TestReply::new(payload))
                    .await;
            }
            TestCommand::BroadcastGroups { groups, payload } => {
                let groups: Vec<&str> = groups.iter().map(String::as_str).collect();
                ctx.broadcast_groups(&groups, TestReply::new(payload)).await;
            }
        }
    }
}

#[derive(Clone)]
struct ServerState {
    clients: Arc<InMemoryWsClients<TestReply, JsonCodec>>,
    config: Arc<WsHubConfig>,
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<ServerState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        let config = state.config.clone();
        serve_hub_with_clients(socket, TestHub, state.clients, &config).await
    })
}

/// An isolated test server instance with its own client registry.
///
/// Each instance binds to `0.0.0.0:0` so tests never conflict on ports.
pub struct TestServer {
    pub addr: SocketAddr,
    _handle: JoinHandle<()>,
}

impl TestServer {
    /// Starts a server with a custom [`BroadcastPolicy`].
    pub async fn start_with_policy(policy: BroadcastPolicy) -> anyhow::Result<Self> {
        let config = Arc::new(WsHubConfig {
            policy,
            ..WsHubConfig::default()
        });
        let clients = Arc::new(InMemoryWsClients::<TestReply, JsonCodec>::new(
            config.policy.clone(),
            config.broadcast_channel_capacity,
        ));
        let state = ServerState { clients, config };

        let app = Router::new()
            .route("/ws", get(ws_handler))
            .with_state(state);

        let listener = TcpListener::bind("0.0.0.0:0").await?;
        let addr = listener.local_addr()?;

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        Ok(Self {
            addr,
            _handle: handle,
        })
    }
}
