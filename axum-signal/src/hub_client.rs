use axum::extract::ws::Message as AxumMessage;
use futures::{SinkExt, StreamExt};
use std::marker::PhantomData;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message as TungMessage};

use crate::codec::WsCodec;

/// Marker type used in [`HubClientBuilder`] typestate before a type parameter has been set.
#[doc(hidden)]
pub struct Unset;

/// Error type for [`HubClient`] operations.
#[derive(Debug)]
pub enum ClientError {
    /// Failed to establish the WebSocket connection.
    Connect(tokio_tungstenite::tungstenite::Error),
    /// Failed to encode an outgoing message.
    Encode(Box<dyn std::error::Error + Send + Sync + 'static>),
    /// The send channel is closed, or [`connect`](HubClient::connect) has not been called yet.
    Disconnected,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "connection error: {e}"),
            Self::Encode(e) => write!(f, "encode error: {e}"),
            Self::Disconnected => f.write_str("client disconnected"),
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Connect(e) => Some(e),
            Self::Encode(e) => Some(e.as_ref()),
            Self::Disconnected => None,
        }
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for ClientError {
    fn from(e: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::Connect(e)
    }
}

/// Typestate builder for [`HubClient`]. Obtained via [`HubClient::builder`].
///
/// Use [`with_in_message`](Self::with_in_message), [`with_out_message`](Self::with_out_message),
/// and [`with_codec`](Self::with_codec) to pin the type parameters, then call
/// [`build`](Self::build) to get a [`HubClient`].
pub struct HubClientBuilder<S = Unset, R = Unset, C = Unset> {
    url: String,
    _phantom: PhantomData<(S, R, C)>,
}

impl<R, C> HubClientBuilder<Unset, R, C> {
    /// Sets the outgoing (sent-to-server) message type.
    pub fn with_in_message<S>(self) -> HubClientBuilder<S, R, C> {
        HubClientBuilder {
            url: self.url,
            _phantom: PhantomData,
        }
    }
}

impl<S, C> HubClientBuilder<S, Unset, C> {
    /// Sets the incoming (received-from-server) message type.
    pub fn with_out_message<R>(self) -> HubClientBuilder<S, R, C> {
        HubClientBuilder {
            url: self.url,
            _phantom: PhantomData,
        }
    }
}

impl<S, R> HubClientBuilder<S, R, Unset> {
    /// Sets the codec — must match the codec used by the server hub.
    pub fn with_codec<C>(self) -> HubClientBuilder<S, R, C> {
        HubClientBuilder {
            url: self.url,
            _phantom: PhantomData,
        }
    }
}

impl<S, R, C> HubClientBuilder<S, R, C>
where
    S: serde::Serialize + Send + 'static,
    R: serde::de::DeserializeOwned + Send + 'static,
    C: WsCodec,
{
    /// Builds the [`HubClient`]. The client is not yet connected; call
    /// [`on_message`](HubClient::on_message) then [`connect`](HubClient::connect).
    pub fn build(self) -> HubClient<S, R, C> {
        HubClient {
            url: self.url,
            handler: None,
            tx: None,
            writer: None,
            reader: None,
            _phantom: PhantomData,
        }
    }
}

/// Typed WebSocket client symmetric to [`WsHub`](crate::WsHub).
///
/// - `S` — messages sent to the server (mirrors the hub's `InMessage`).
/// - `R` — messages received from the server (mirrors the hub's `OutMessage`).
/// - `C` — codec, must match the codec used by the server hub.
///
/// # Example
/// ```no_run
/// # use axum_signal::{HubClient, JsonCodec};
/// # #[derive(serde::Serialize)] struct HelloMessage { text: String }
/// # #[derive(serde::Deserialize, Debug)] struct HelloReply {}
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let mut client = HubClient::builder("ws://localhost:3000/ws")
///     .with_in_message::<HelloMessage>()
///     .with_out_message::<HelloReply>()
///     .with_codec::<JsonCodec>()
///     .build();
///
/// client.on_message(|reply: HelloReply| println!("{reply:?}"));
/// client.connect().await?;
///
/// client.send(HelloMessage { text: "hello".into() })?;
/// client.disconnect().await;
/// # Ok(())
/// # }
/// ```
pub struct HubClient<S = Unset, R = Unset, C = Unset> {
    url: String,
    handler: Option<Arc<dyn Fn(R) + Send + Sync + 'static>>,
    tx: Option<mpsc::UnboundedSender<TungMessage>>,
    writer: Option<JoinHandle<()>>,
    reader: Option<JoinHandle<()>>,
    _phantom: PhantomData<(S, C)>,
}

impl<S, R, C> Drop for HubClient<S, R, C> {
    /// Sends a Close frame if [`disconnect`](HubClient::disconnect) was not called explicitly.
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(TungMessage::Close(None));
        }
    }
}

impl HubClient {
    /// Creates a builder for a [`HubClient`] that will connect to `url`.
    pub fn builder(url: impl Into<String>) -> HubClientBuilder {
        HubClientBuilder {
            url: url.into(),
            _phantom: PhantomData,
        }
    }
}

impl<S, R, C> HubClient<S, R, C>
where
    S: serde::Serialize + Send + 'static,
    R: serde::de::DeserializeOwned + Send + 'static,
    C: WsCodec,
{
    /// Registers a handler for messages received from the server.
    ///
    /// Must be called before [`connect`](Self::connect).
    pub fn on_message<F>(&mut self, f: F)
    where
        F: Fn(R) + Send + Sync + 'static,
    {
        self.handler = Some(Arc::new(f));
    }

    /// Connects to the hub and starts the reader/writer background tasks.
    ///
    /// After this returns `Ok(())`, [`send`](Self::send) can be used to deliver messages.
    pub async fn connect(&mut self) -> Result<(), ClientError> {
        let (ws_stream, _) = connect_async(&self.url).await?;
        let (mut sink, mut stream) = ws_stream.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<TungMessage>();

        // writer task: drains the outgoing channel into the WebSocket sink
        let writer = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
        });

        // reader task: decodes each incoming frame and calls the handler
        let handler = self.handler.clone();
        let reader = tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                if let Some(axum_msg) = tung_to_axum(msg) {
                    let Some(ref h) = handler else { continue };
                    match C::decode::<R>(axum_msg) {
                        Ok(decoded) => h(decoded),
                        Err(e) => tracing::warn!("decode error: {e}"),
                    }
                }
            }
        });

        self.tx = Some(tx);
        self.writer = Some(writer);
        self.reader = Some(reader);
        Ok(())
    }

    /// Performs a clean WebSocket closing handshake and waits for both background tasks to exit.
    ///
    /// Sends a Close frame, then blocks until the server echoes it back and the connection is
    /// fully closed. After this returns, all in-flight messages have been flushed.
    pub async fn disconnect(&mut self) {
        // dropping tx closes the channel — writer task sends Close then exits
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(TungMessage::Close(None));
        }
        // wait for writer to flush, then for reader to receive the server's Close echo
        if let Some(h) = self.writer.take() {
            let _ = h.await;
        }
        if let Some(h) = self.reader.take() {
            let _ = h.await;
        }
    }

    /// Encodes `msg` and queues it for delivery to the server.
    ///
    /// Non-blocking — the message is placed on an internal channel and sent by the writer task.
    /// Returns [`ClientError::Disconnected`] if [`connect`](Self::connect) has not been called
    /// or the background tasks have exited.
    pub fn send(&self, msg: S) -> Result<(), ClientError> {
        let tx = self.tx.as_ref().ok_or(ClientError::Disconnected)?;
        let axum_msg = C::encode(msg).map_err(|e| ClientError::Encode(Box::new(e)))?;
        tx.send(axum_to_tung(axum_msg))
            .map_err(|_| ClientError::Disconnected)
    }
}

/// Converts an outgoing axum [`AxumMessage`] to a tungstenite [`TungMessage`].
fn axum_to_tung(msg: AxumMessage) -> TungMessage {
    match msg {
        AxumMessage::Text(t) => TungMessage::Text(t.as_str().into()),
        AxumMessage::Binary(b) => TungMessage::Binary(b),
        AxumMessage::Ping(b) => TungMessage::Ping(b),
        AxumMessage::Pong(b) => TungMessage::Pong(b),
        AxumMessage::Close(_) => TungMessage::Close(None),
    }
}

/// Converts an incoming tungstenite [`TungMessage`] to an axum [`AxumMessage`] for codec decoding.
///
/// Returns `None` for control frames (Ping, Pong, Close, Frame).
fn tung_to_axum(msg: TungMessage) -> Option<AxumMessage> {
    match msg {
        TungMessage::Text(t) => Some(AxumMessage::text(t.as_str())),
        TungMessage::Binary(b) => Some(AxumMessage::Binary(b)),
        _ => None,
    }
}
