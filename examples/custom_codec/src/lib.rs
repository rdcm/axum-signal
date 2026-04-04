use axum::extract::ws::Message;
use axum_signal::WsCodec;

pub struct MsgpackCodec;

impl WsCodec for MsgpackCodec {
    fn decode<T: serde::de::DeserializeOwned>(msg: Message) -> anyhow::Result<T> {
        match msg {
            Message::Binary(data) => Ok(rmp_serde::from_slice(&data)?),
            _ => Err(anyhow::anyhow!("expected binary")),
        }
    }

    fn encode<T: serde::Serialize>(value: T) -> anyhow::Result<Message> {
        Ok(Message::binary(rmp_serde::to_vec_named(&value)?))
    }
}
