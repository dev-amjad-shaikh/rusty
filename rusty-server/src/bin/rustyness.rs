//! The `rustyness` CLI — packaging, deployment, and operations tooling.
//!
//! Subcommands:
//! - `verify-log <path>` — verify a journal snapshot's integrity and
//!   per-session invariants (EP-13-S10 AC 2).
//! - `verify-log <path> --artifacts <dir>` — also verify that every
//!   `PayloadRef::Artifact` in the event stream resolves in the artifact
//!   store or the snapshot's embedded artifact map (EP-13-S10 AC 5).

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "rustyness")]
#[command(about = "Rusty platform operations tooling")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Verify a journal snapshot for gap-free positions, paired turn
    /// events, structural integrity, and (optionally) artifact locator
    /// resolution.
    VerifyLog {
        /// Path to the journal snapshot JSON file.
        path: PathBuf,
        /// Path to a file artifact store directory. When provided, every
        /// `PayloadRef::Artifact` reference in the event stream is checked
        /// for existence in the store or in the snapshot's embedded
        /// `artifacts` map.
        #[arg(long)]
        artifacts: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::VerifyLog { path, artifacts } => {
            let code =
                rusty_agent_server::verify::verify_log_file(&path, artifacts.as_deref()).await;
            std::process::exit(code);
        }
    }
}
