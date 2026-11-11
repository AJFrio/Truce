use anyhow::Context;
use clap::Parser;
use truce::cli::Cli;

fn main() {
    if let Err(err) = try_main() {
        eprintln!("error: {err:#}");
        std::process::exit(2);
    }
}

fn try_main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let code = truce::run(cli).context("truce failed")?;
    std::process::exit(code);
}
