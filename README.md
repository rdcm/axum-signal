# axum-signal

SignalR-inspired WebSocket hub abstraction for Axum - broadcast, unicast, JSON/Binary codecs. Passes 10k connections tests.

## About

Working with raw WebSocket in Axum requires a lot of boilerplate - managing connections, handling heartbeats, implementing broadcast logic, dealing with serialization. `axum-signal` takes inspiration from SignalR and brings a clean hub abstraction on top of Axum's WebSocket support.

You define your message types and handle logic. The library handles everything else.

## Installation

```toml
[dependencies]
axum-signal = "0.1.2"
```

Or directly from GitHub:

```toml
[dependencies]
axum-signal = { git = "https://github.com/rdcm/axum-signal" }
```

## Quick Start

Define your messages and implement `WsHub`:

```rust
use axum_signal::{WsHub, MessageContext, JsonCodec};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct HelloMessage {
    pub text: String,
}

#[derive(Serialize)]
pub enum HelloReply {
    Ok(String),
    Err(String),
}

pub struct HelloHub;

impl WsHub for HelloHub {
    type Codec = JsonCodec;
    type InMessage = HelloMessage;
    type OutMessage = HelloReply;

    async fn on_message(&self, msg: Self::InMessage, ctx: MessageContext<Self::OutMessage, Self::Codec>) {
        ctx.broadcast(HelloReply::Ok(msg.text)).await
    }
}
```

Wire it up in your Axum router:

```rust
pub fn api_router(state: AppState) -> Router {
    Router::new()
        .merge(hello_router())
        .with_state(state.clone())
}

pub fn hello_router() -> Router<AppState> {
    Router::new().route(
        "/ws",
        get(
            |ws: WebSocketUpgrade, State(state): State<AppState>| async move {
                ws.on_upgrade(move |socket| serve_hub(socket, HelloHub::new(state)))
            },
        ),
    )
}
```

To pass data from Axum extractors into your hub:
```rust
pub fn hello_router() -> Router<AppState> {
    Router::new().route(
        "/ws",
        get(
            |ws: WebSocketUpgrade,
             State(state): State<AppState>,
             Query(params): Query<WsQueryParams>| async move {
                // Extractors are resolved here, before the upgrade.
                // Pass the extracted data into the hub constructor.
                ws.on_upgrade(move |socket| serve_hub(socket, HelloHub::new(state, params)))
            },
        ),
    )
}
```


That's it. No connection management, no heartbeat setup, no serialization boilerplate.

## Features

**Typed messages** - define your `InMessage` and `OutMessage` as plain Rust structs. Serialization is handled automatically.

**Pluggable codecs** - switch between JSON and Binary with a single type change:

```rust
// JSON over text frames
type Codec = JsonCodec;

// Binary via postcard
type Codec = BinaryCodec;
```

**Broadcast, unicast, groups** - flexible targeting:

```rust
async fn on_message(&self, msg: Self::InMessage, ctx: MessageContext<Self::OutMessage, Self::Codec>) {
    // to all clients
    ctx.broadcast(MyReply::Ok(msg.text.clone())).await;

    // to sender only
    ctx.unicast(MyReply::Ok(msg.text.clone())).await;

    // to all except specific connections
    ctx.broadcast_except(&["conn-id-1", "conn-id-2"], MyReply::Ok(msg.text.clone())).await;

    // group management
    ctx.add_to_group("room-1").await;
    ctx.remove_from_group("room-1").await;

    // to all in a group
    ctx.broadcast_group("room-1", MyReply::Ok(msg.text.clone())).await;

    // to a group except specific connections
    ctx.broadcast_group_except("room-1", &["conn-id-1"], MyReply::Ok(msg.text.clone())).await;

    // to all in multiple groups (each connection receives at most once)
    ctx.broadcast_groups(&["room-1", "room-2"], MyReply::Ok(msg.text)).await;
}
```

**Default lifecycle hooks** - override only what you need:

```rust
impl WsHub for HelloHub {
    type Codec = JsonCodec;
    type InMessage = HelloMessage;
    type OutMessage = HelloReply;

    // optional - default logs connection id
    async fn on_connect(&self, req: ConnectionRequest) {
        tracing::info!("new connection: {}", req.connection_id);
    }

    // optional - default logs connection id
    async fn on_disconnect(&self, req: DisconnectRequest) {
        tracing::info!("disconnected: {}", req.connection_id);
    }

    async fn on_message(&self, msg: Self::InMessage, ctx: MessageContext<Self::OutMessage, Self::Codec>) {
        ctx.unicast(HelloReply::Ok(msg.text)).await;
    }
}
```

## Rust client

Enable the `client` feature:

```toml
[dependencies]
axum-signal = { version = "0.1.2", features = ["client"] }
```

`HubClient` mirrors the hub's type parameters — `S` is the message type sent to the server (`InMessage`), `R` is the message type received from the server (`OutMessage`), and `C` is the codec. All three must match the server hub.

```rust
use axum_signal::{HubClient, JsonCodec};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub struct HelloMessage {
    pub text: String,
}

#[derive(Deserialize, Debug)]
pub enum HelloReply {
    Ok(String),
    Err(String),
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = HubClient::builder("ws://localhost:3000/ws")
        .with_in_message::<HelloMessage>()
        .with_out_message::<HelloReply>()
        .with_codec::<JsonCodec>()
        .build();

    client.on_message(|reply: HelloReply| println!("{reply:?}"));
    client.connect().await?;

    client.send(HelloMessage { text: "hello".into() })?;

    // sends a Close frame and waits for the server to echo it back
    client.disconnect().await;
    Ok(())
}
```

Calling `disconnect()` performs the WebSocket closing handshake — it sends a Close frame, waits for the writer task to flush it, then waits for the reader task to receive the server's Close response. This avoids the `Connection reset without closing handshake` warning on the server side.

If `disconnect()` is not called, `Drop` will still send the Close frame on a best-effort basis, but without waiting for the handshake to complete.

## Benchmarks

Tested with [k6](https://k6.io) on AMD Ryzen 9 (32 cores), 32GB RAM.

### Unicast - 10k connections

```
ulimit -n 65535 && k6 run benchmarks/10k_connections.js

         /\      Grafana   /‾‾/  
    /\  /  \     |\  __   /  /   
   /  \/    \    | |/ /  /   ‾‾\ 
  /          \   |   (  |  (‾)  |
 / __________ \  |_|\_\  \_____/ 


     execution: local
        script: benchmarks/10k_connections.js
        output: -

     scenarios: (100.00%) 1 scenario, 10000 max VUs, 1m30s max duration (incl. graceful stop):
              * default: 10000 looping VUs for 1m0s (gracefulStop: 30s)


  █ TOTAL RESULTS 

    checks_total.......: 170000  1888.350122/s
    checks_succeeded...: 100.00% 170000 out of 170000
    checks_failed......: 0.00%   0 out of 170000

    ✓ got response

    EXECUTION
    vus................: 10000  min=10000     max=10000
    vus_max............: 10000  min=10000     max=10000

    NETWORK
    data_received......: 5.4 MB 60 kB/s
    data_sent..........: 6.8 MB 75 kB/s

    WEBSOCKET
    ws_connecting......: avg=156.49ms min=454.96µs med=137.77ms max=401.14ms p(90)=259.56ms p(95)=288.33ms
    ws_msgs_received...: 170000 1888.350122/s
    ws_msgs_sent.......: 170001 1888.36123/s
    ws_sessions........: 10000  111.079419/s




running (1m30.0s), 00000/10000 VUs, 0 complete and 10000 interrupted iterations
default ✓ [======================================] 10000 VUs  1m0s
```

### Broadcast - 10k connections

```
ulimit -n 65535 && k6 run benchmarks/10k_connections.js

         /\      Grafana   /‾‾/  
    /\  /  \     |\  __   /  /   
   /  \/    \    | |/ /  /   ‾‾\ 
  /          \   |   (  |  (‾)  |
 / __________ \  |_|\_\  \_____/ 


     execution: local
        script: benchmarks/10k_connections.js
        output: -

     scenarios: (100.00%) 1 scenario, 10000 max VUs, 1m30s max duration (incl. graceful stop):
              * default: 10000 looping VUs for 1m0s (gracefulStop: 30s)


  █ TOTAL RESULTS 

    checks_total.......: 40295409 447473.026684/s
    checks_succeeded...: 100.00%  40295409 out of 40295409
    checks_failed......: 0.00%    0 out of 40295409

    ✓ got response

    EXECUTION
    vus................: 10000    min=10000       max=10000
    vus_max............: 10000    min=10000       max=10000

    NETWORK
    data_received......: 911 MB   10 MB/s
    data_sent..........: 6.8 MB   75 kB/s

    WEBSOCKET
    ws_connecting......: avg=155.42ms min=55.24ms med=143.95ms max=317.78ms p(90)=225.3ms p(95)=230.77ms
    ws_msgs_received...: 40300506 447529.627922/s
    ws_msgs_sent.......: 170000   1887.8184/s
    ws_sessions........: 10000    111.048141/s




running (1m30.1s), 00000/10000 VUs, 0 complete and 10000 interrupted iterations
default ✓ [======================================] 10000 VUs  1m0s
```