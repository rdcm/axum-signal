# axum-signal

SignalR-inspired WebSocket hub abstraction for Axum - broadcast, unicast, JSON/Binary codecs. Passes 10k connections tests.

## About

Working with raw WebSocket in Axum requires a lot of boilerplate - managing connections, handling heartbeats, implementing broadcast logic, dealing with serialization. `axum-signal` takes inspiration from SignalR and brings a clean hub abstraction on top of Axum's WebSocket support.

You define your message types and handle logic. The library handles everything else.

## Installation

```toml
[dependencies]
axum-signal = "0.1"
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

    async fn on_message(&self, msg: HelloMessage, ctx: MessageContext<HelloReply, JsonCodec>) {
        ctx.broadcast(HelloReply::Ok(msg.text)).await
    }
}
```

Wire it up in your Axum router:

```rust
use axum::{Router, extract::{State, WebSocketUpgrade}, response::IntoResponse, routing::get};
use axum_signal::serve_hub;

pub fn router() -> Router<AppState> {
    Router::new().route("/ws", get(handler))
}

async fn handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| serve_hub(socket, HelloHub::new(state)))
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

**Broadcast and unicast** - send to all connected clients or just one:

```rust
async fn on_message(&self, msg: MyMessage, ctx: MessageContext<MyReply, JsonCodec>) {
    // to all clients
    ctx.broadcast(MyReply::Ok(msg.text.clone())).await;

    // to sender only
    ctx.unicast(MyReply::Ok(msg.text)).await;
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

    async fn on_message(&self, msg: HelloMessage, ctx: MessageContext<HelloReply, JsonCodec>) {
        ctx.unicast(HelloReply::Ok(msg.text)).await;
    }
}
```

## Benchmarks

Tested with [k6](https://k6.io) on AMD Ryzen 9 (32 cores), 32GB RAM.

### Unicast - 10k connections

```
        /\      Grafana   /‾‾/  
    /\  /  \     |\  __   /  /   
   /  \/    \    | |/ /  /   ‾‾\ 
  /          \   |   (  |  (‾)  |
 / __________ \  |_|\_\  \_____/ 


     execution: local
        script: ws_test.js
        output: -

     scenarios: (100.00%) 1 scenario, 10000 max VUs, 1m30s max duration (incl. graceful stop):
              * default: 10000 looping VUs for 1m0s (gracefulStop: 30s)

WARN[0090] No script iterations fully finished, consider making the test duration longer 


  █ TOTAL RESULTS 

    checks_total.......: 170000  1888.320672/s
    checks_succeeded...: 100.00% 170000 out of 170000
    checks_failed......: 0.00%   0 out of 170000

    ✓ got response

    EXECUTION
    vus................: 10000  min=10000     max=10000
    vus_max............: 10000  min=10000     max=10000

    NETWORK
    data_received......: 4.1 MB 45 kB/s
    data_sent..........: 5.4 MB 60 kB/s

    WEBSOCKET
    ws_connecting......: avg=9.12ms min=32.57µs med=96.65µs max=130.43ms p(90)=20.95ms p(95)=68.57ms
    ws_msgs_received...: 170000 1888.320672/s
    ws_msgs_sent.......: 170000 1888.320672/s
    ws_sessions........: 10000  111.077687/s




running (1m30.0s), 00000/10000 VUs, 0 complete and 10000 interrupted iterations
default ✓ [======================================] 10000 VUs  1m0s
```

### Broadcast - 10k connections

```

ulimit -n 65535 && k6 run ws_test.js

         /\      Grafana   /‾‾/  
    /\  /  \     |\  __   /  /   
   /  \/    \    | |/ /  /   ‾‾\ 
  /          \   |   (  |  (‾)  |
 / __________ \  |_|\_\  \_____/ 


     execution: local
        script: ws_test.js
        output: -

     scenarios: (100.00%) 1 scenario, 10000 max VUs, 1m30s max duration (incl. graceful stop):
              * default: 10000 looping VUs for 1m0s (gracefulStop: 30s)

WARN[0090] No script iterations fully finished, consider making the test duration longer 


  █ TOTAL RESULTS 

    checks_total.......: 38676856 429435.867125/s
    checks_succeeded...: 100.00%  38676856 out of 38676856
    checks_failed......: 0.00%    0 out of 38676856

    ✓ got response

    EXECUTION
    vus................: 10000    min=10000       max=10000
    vus_max............: 10000    min=10000       max=10000

    NETWORK
    data_received......: 559 MB   6.2 MB/s
    data_sent..........: 5.4 MB   60 kB/s

    WEBSOCKET
    ws_connecting......: avg=17.86ms min=36.58µs med=129.39µs max=123.92ms p(90)=49.59ms p(95)=52.91ms
    ws_msgs_received...: 38681727 429489.950686/s
    ws_msgs_sent.......: 170000   1887.539603/s
    ws_sessions........: 10000    111.031741/s




running (1m30.1s), 00000/10000 VUs, 0 complete and 10000 interrupted iterations
default ✓ [======================================] 10000 VUs  1m0s
```

## TODO

- `broaddcast_except` - broadcast to all connections excluding specific connection
- `broadcast_group` - broadcast to a named group of connections
- `broadcast_group_except` - broadcast to a group excluding specific connection