use crate::app_state::AppState;
use crate::messages::{HelloMessage, HelloReply};
use axum_signal::{JsonCodec, MessageContext, WsHub};

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

    async fn on_message(&self, msg: HelloMessage, ctx: MessageContext<HelloReply, JsonCodec>) {
        ctx.broadcast(HelloReply::Ok(msg.text)).await
    }
}
