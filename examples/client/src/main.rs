use anyhow::Result;
use axum_signal::{HubClient, JsonCodec};
use contracts::{HelloMessage, HelloReply};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut client = HubClient::builder("ws://127.0.0.1:8020/ws")
        .with_in_message::<HelloMessage>()
        .with_out_message::<HelloReply>()
        .with_codec::<JsonCodec>()
        .build();

    client.on_message(|reply: HelloReply| match reply {
        HelloReply::Ok(text) => tracing::info!("received: {text}"),
        HelloReply::Err(err) => tracing::warn!("error from server: {err}"),
    });

    client.connect().await?;
    tracing::info!("connected to ws://127.0.0.1:8020/ws");

    for i in 1..=5 {
        let msg = HelloMessage {
            text: format!("hello #{i}"),
        };
        tracing::info!("sending: {}", msg.text);
        client.send(msg)?;
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    client.disconnect().await;

    Ok(())
}
