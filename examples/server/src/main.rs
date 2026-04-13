use anyhow::Result;
use server::app_config::AppConfig;
use server::service::Service;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let service = Service::new(&AppConfig {
        api_listener_address: "127.0.0.1:8020".to_string(),
    })
    .await?;

    service.run().await
}
