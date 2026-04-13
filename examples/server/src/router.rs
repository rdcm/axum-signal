use crate::app_state::AppState;
use crate::hello_hub::HelloHub;
use axum::Router;
use axum::extract::{State, WebSocketUpgrade};
use axum::routing::get;
use axum_signal::serve_hub;

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
