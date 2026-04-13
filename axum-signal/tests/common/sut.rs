use anyhow::{Result, anyhow};
use axum_signal::{HubClient, JsonCodec};
use std::time::Duration;
use tokio::sync::mpsc;

use super::server::{TestCommand, TestReply, TestServer};

/// How long to wait for an expected message before failing.
const RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait when asserting that no message arrives.
const NO_MSG_GRACE: Duration = Duration::from_millis(150);

/// System under test. Owns a [`TestServer`] and acts as a factory for [`TestClient`] connections.
pub struct Sut {
    server: TestServer,
}

impl Sut {
    pub async fn new() -> Result<Self> {
        let _ = tracing_subscriber::fmt::try_init();
        Ok(Self {
            server: TestServer::start().await?,
        })
    }

    pub async fn connect(&self) -> Result<TestClient> {
        TestClient::connect(&format!("ws://{}/ws", self.server.addr)).await
    }
}

/// A connected WebSocket client with a typed message queue.
pub struct TestClient {
    client: HubClient<TestCommand, TestReply, JsonCodec>,
    rx: mpsc::UnboundedReceiver<TestReply>,
}

impl TestClient {
    async fn connect(url: &str) -> Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut client = HubClient::builder(url)
            .with_in_message::<TestCommand>()
            .with_out_message::<TestReply>()
            .with_codec::<JsonCodec>()
            .build();
        client.on_message(move |reply| {
            let _ = tx.send(reply);
        });
        client.connect().await?;
        Ok(Self { client, rx })
    }

    /// Sends a command to the server.
    pub fn send(&self, cmd: TestCommand) -> Result<()> {
        self.client.send(cmd).map_err(|e| anyhow!("{e}"))
    }

    /// Waits up to [`RECV_TIMEOUT`] for the next message.
    pub async fn recv(&mut self) -> Result<TestReply> {
        tokio::time::timeout(RECV_TIMEOUT, self.rx.recv())
            .await
            .map_err(|_| anyhow!("timeout: no message arrived within {RECV_TIMEOUT:?}"))?
            .ok_or_else(|| anyhow!("channel closed unexpectedly"))
    }

    /// Asserts that no message arrives within [`NO_MSG_GRACE`].
    pub async fn assert_no_message(&mut self) -> Result<()> {
        tokio::select! {
            msg = self.rx.recv() => anyhow::bail!("expected no message but received {:?}", msg),
            _ = tokio::time::sleep(NO_MSG_GRACE) => Ok(()),
        }
    }

    /// Returns the server-assigned connection id for this client.
    pub async fn connection_id(&mut self) -> Result<String> {
        self.send(TestCommand::GetConnectionId)?;
        Ok(self.recv().await?.payload)
    }

    /// Adds this connection to `group` and waits for the server ack.
    pub async fn add_to_group(&mut self, group: &str) -> Result<()> {
        self.send(TestCommand::AddToGroup {
            group: group.into(),
        })?;
        self.recv().await?;
        Ok(())
    }

    /// Removes this connection from `group` and waits for the server ack.
    pub async fn remove_from_group(&mut self, group: &str) -> Result<()> {
        self.send(TestCommand::RemoveFromGroup {
            group: group.into(),
        })?;
        self.recv().await?;
        Ok(())
    }

    /// Performs a clean WebSocket closing handshake.
    pub async fn disconnect(&mut self) {
        self.client.disconnect().await;
    }
}
