//! `crewd`: the Crew runtime daemon and its lifecycle CLI.
//!
//! The entry point is deliberately thin: it parses arguments and dispatches
//! into [`cli::run`], which drives the `batman_runtime::lifecycle` library.
//! Commands: `serve`, `status`, `stop`, `version`, `schema`, `monitor`,
//! `audit`, `doctor`, and `coordination-mcp`.

mod cli;

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    cli::run().await
}
