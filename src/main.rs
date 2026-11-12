use anyhow::Context;
use clap::Parser;
use truce::cli::Cli;

#[tokio::main]
async fn main() {
    // `git rebase --continue` needs a non-interactive editor. Sync points
    // GIT_EDITOR at this binary and sets the variable only on that child
    // process. The editor inherits it and exits without changing the message.
    if std::env::var("TRUCE_INTERNAL_NOOP_EDITOR").ok().as_deref() == Some("1") {
        std::process::exit(0);
    }
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
