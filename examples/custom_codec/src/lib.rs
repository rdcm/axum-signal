use axum::extract::ws::Message;
use axum_signal::WsCodec;

/// Error produced by [`MsgpackCodec`].
#[derive(Debug)]
pub enum MsgpackCodecError {
    /// The WebSocket frame is not a `Binary` variant.
    ExpectedBinary,
    /// MessagePack serialization or deserialization failed.
    Rmp(rmp_serde::encode::Error),
    /// MessagePack deserialization failed.
    RmpDecode(rmp_serde::decode::Error),
}

impl std::fmt::Display for MsgpackCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExpectedBinary => f.write_str("expected binary frame"),
            Self::Rmp(e) => write!(f, "{e}"),
            Self::RmpDecode(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for MsgpackCodecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rmp(e) => Some(e),
            Self::RmpDecode(e) => Some(e),
            Self::ExpectedBinary => None,
        }
    }
}

impl From<rmp_serde::encode::Error> for MsgpackCodecError {
    fn from(e: rmp_serde::encode::Error) -> Self {
        Self::Rmp(e)
    }
}

impl From<rmp_serde::decode::Error> for MsgpackCodecError {
    fn from(e: rmp_serde::decode::Error) -> Self {
        Self::RmpDecode(e)
    }
}

pub struct MsgpackCodec;

impl WsCodec for MsgpackCodec {
    type Error = MsgpackCodecError;

    fn decode<T: serde::de::DeserializeOwned>(msg: Message) -> Result<T, Self::Error> {
        match msg {
            Message::Binary(data) => Ok(rmp_serde::from_slice(&data)?),
            _ => Err(MsgpackCodecError::ExpectedBinary),
        }
    }

    fn encode<T: serde::Serialize>(value: T) -> Result<Message, Self::Error> {
        Ok(Message::binary(rmp_serde::to_vec_named(&value)?))
    }
}
