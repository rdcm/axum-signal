use crate::app_state::AppState;
use axum_signal::{JsonCodec, MessageContext, WsHub};
use contracts::{HelloMessage, HelloReply};

pub struct HelloHub {
    _state: AppState,
}

impl HelloHub {
    pub fn new(state: AppState) -> Self {
        Self { _state: state }
    }
}

impl WsHub for HelloHub {
    type Codec = JsonCodec;
    type InMessage = HelloMessage;
    type OutMessage = HelloReply;

    async fn on_message(
        &self,
        msg: Self::InMessage,
        ctx: MessageContext<Self::OutMessage, Self::Codec>,
    ) {
        ctx.broadcast(HelloReply::Ok(msg.text)).await
    }
}
