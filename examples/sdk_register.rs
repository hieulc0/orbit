//! Run with a configured worker token: cargo run --example sdk_register
use orbit::sdk::{Client, Recovery};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = Client::new(std::env::var("ORBIT_URL")?, std::env::var("ORBIT_TOKEN")?)?;
    let receipt = client
        .register(
            vec!["repository.code".into()],
            vec![Recovery::RestartFromInputs],
        )
        .await?;
    println!("{receipt}");
    Ok(())
}
