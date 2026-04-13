use crate::app_config::AppConfig;
use crate::app_state::AppState;
use crate::router::api_router;
use anyhow::{Context, Result};
use futures::{TryFutureExt, future};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;
use tokio::net::TcpListener;

pub struct Service {
    _app_config: AppConfig,
    api_listener: TcpListener,
}

impl Service {
    pub async fn new(app_config: &AppConfig) -> Result<Self> {
        let addr: SocketAddr = app_config.api_listener_address.parse()?;

        let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
        socket.set_reuse_address(true)?;
        socket.set_reuse_port(true)?;
        socket.set_nonblocking(true)?;
        socket.bind(&addr.into())?;
        socket.listen(8192)?;

        let api_listener = TcpListener::from_std(socket.into())?;
        Ok(Self {
            _app_config: app_config.clone(),
            api_listener,
        })
    }

    pub async fn run(self) -> Result<()> {
        let app_state = AppState;

        let api_handle = tokio::spawn(
            axum::serve(
                self.api_listener,
                api_router(app_state).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .into_future()
            .map_err(anyhow::Error::from),
        );

        let (result, number, _) = future::select_all(vec![api_handle]).await;
        let context = format!("Error from call ai handle #{number}");
        result?.context(context)?;

        Ok(())
    }
}
