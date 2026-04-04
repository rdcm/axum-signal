use anyhow::Result;
use broadcast::app_config::AppConfig;
use broadcast::service::Service;

#[tokio::main]
async fn main() -> Result<()> {
    let service = Service::new(&AppConfig {
        api_listener_address: "127.0.0.1:8020".to_string(),
    })
    .await?;

    service.run().await
}
