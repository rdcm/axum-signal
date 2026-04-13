use axum::extract::ws::Message;

/// Encodes and decodes WebSocket messages for a specific wire format.
///
/// Implement this trait to support custom serialization formats (e.g. MessagePack, Protobuf).
/// Two built-in implementations are provided: [`JsonCodec`] and [`BinaryCodec`].
pub trait WsCodec: Send + Sync + 'static {
    /// The concrete error type produced by [`decode`](WsCodec::decode) and [`encode`](WsCodec::encode).
    type Error: std::error::Error + Send + Sync + 'static;

    /// Deserializes a WebSocket [`Message`] into `T`.
    ///
    /// Returns an error if the message type is not supported by this codec or if
    /// deserialization fails.
    fn decode<T: serde::de::DeserializeOwned>(msg: Message) -> Result<T, Self::Error>;

    /// Serializes `msg` into a WebSocket [`Message`].
    ///
    /// Returns an error if serialization fails.
    fn encode<T: serde::Serialize>(msg: T) -> Result<Message, Self::Error>;
}

// --- JsonCodec ---

/// Error produced by [`JsonCodec`].
#[derive(Debug)]
pub enum JsonCodecError {
    /// The WebSocket frame variant is not supported (e.g. `Ping`, `Pong`, `Close`).
    UnexpectedMessageType,
    /// JSON serialization or deserialization failed.
    Serde(serde_json::Error),
}

impl std::fmt::Display for JsonCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedMessageType => f.write_str("unexpected message type"),
            Self::Serde(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for JsonCodecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Serde(e) => Some(e),
            Self::UnexpectedMessageType => None,
        }
    }
}

impl From<serde_json::Error> for JsonCodecError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serde(e)
    }
}

/// A [`WsCodec`] that serializes messages as JSON text frames.
///
/// `decode` accepts both `Text` and `Binary` frames (the binary variant is parsed as UTF-8 JSON).
/// `encode` always produces `Text` frames.
pub struct JsonCodec;

impl WsCodec for JsonCodec {
    type Error = JsonCodecError;

    /// Deserializes `msg` from a JSON text or binary WebSocket frame into `T`.
    ///
    /// Returns [`JsonCodecError::UnexpectedMessageType`] for any other frame variant.
    fn decode<T: serde::de::DeserializeOwned>(msg: Message) -> Result<T, Self::Error> {
        match msg {
            Message::Text(text) => Ok(serde_json::from_str(&text)?),
            Message::Binary(data) => Ok(serde_json::from_slice(&data)?),
            _ => Err(JsonCodecError::UnexpectedMessageType),
        }
    }

    /// Serializes `value` to a JSON string and wraps it in a `Text` WebSocket frame.
    fn encode<T: serde::Serialize>(value: T) -> Result<Message, Self::Error> {
        Ok(Message::text(serde_json::to_string(&value)?))
    }
}

// --- BinaryCodec ---

/// Error produced by [`BinaryCodec`].
#[derive(Debug)]
pub enum BinaryCodecError {
    /// The WebSocket frame is not a `Binary` variant.
    ExpectedBinary,
    /// Postcard serialization or deserialization failed.
    Postcard(postcard::Error),
}

impl std::fmt::Display for BinaryCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExpectedBinary => f.write_str("expected binary frame"),
            Self::Postcard(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for BinaryCodecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Postcard(e) => Some(e),
            Self::ExpectedBinary => None,
        }
    }
}

impl From<postcard::Error> for BinaryCodecError {
    fn from(e: postcard::Error) -> Self {
        Self::Postcard(e)
    }
}

/// A [`WsCodec`] that serializes messages as binary frames using [`postcard`].
///
/// Both `decode` and `encode` operate exclusively on `Binary` WebSocket frames.
pub struct BinaryCodec;

impl WsCodec for BinaryCodec {
    type Error = BinaryCodecError;

    /// Deserializes `msg` from a binary WebSocket frame using `postcard`.
    ///
    /// Returns [`BinaryCodecError::ExpectedBinary`] if the frame is not a `Binary` variant.
    fn decode<T: serde::de::DeserializeOwned>(msg: Message) -> Result<T, Self::Error> {
        match msg {
            Message::Binary(data) => Ok(postcard::from_bytes(&data)?),
            _ => Err(BinaryCodecError::ExpectedBinary),
        }
    }

    /// Serializes `value` with `postcard` and wraps the result in a `Binary` WebSocket frame.
    fn encode<T: serde::Serialize>(value: T) -> Result<Message, Self::Error> {
        Ok(Message::binary(postcard::to_stdvec(&value)?))
    }
}
