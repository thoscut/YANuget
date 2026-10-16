use std::process::ExitCode;

use clap::Parser;
use yanuget_internalize::cli::{self, Cli};
use yanuget_internalize::outcome::Outcome;

#[tokio::main]
async fn main() -> ExitCode {
    // Logs go to stderr, so stdout carries only what a pipeline reads.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    match cli::run(Cli::parse()).await {
        Ok(outcome) => outcome.into(),
        Err(e) => {
            eprintln!("error: {e:#}");
            Outcome::Failed.into()
        }
    }
}
