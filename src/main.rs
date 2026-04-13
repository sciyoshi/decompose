use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    decompose::run_cli().await
}
