use anyhow::Context;
use clap::Parser;
use truce::cli::Cli;

#[tokio::main]
async fn main() {
    if let Err(err) = try_main().await {
        eprintln!("error: {err:#}");
        std::process::exit(2);
    }
}

async fn try_main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let code = truce::run(cli).await.context("truce failed")?;
    std::process::exit(code);
}
