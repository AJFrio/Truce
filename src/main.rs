use anyhow::Context;
use clap::Parser;
use truce::cli::Cli;

#[tokio::main]
async fn main() {
    if let Err(err) = try_main().await {
        let text = format!("{err:#}");
        if truce::ci::env_true("GITHUB_ACTIONS") {
            eprintln!("::error title=truce::{}", truce::ci::escape_data(&text));
        }
        eprintln!("error: {text}");
        std::process::exit(2);
    }
}

async fn try_main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let code = truce::run(cli).await.context("truce failed")?;
    std::process::exit(code);
}
