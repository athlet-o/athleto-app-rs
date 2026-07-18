#[tokio::main]
async fn main() -> anyhow::Result<()> {
    athleto_app_rs::startup::run().await
}
