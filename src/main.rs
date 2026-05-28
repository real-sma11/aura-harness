//! Phase 10 carve-out 1: thin `aura` binary entrypoint.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    aura_surface_cli::run().await
}
