//! The `rustyness` CLI — packaging, deployment, and operations tooling.
//!
//! Subcommands:
//! - `verify-log <path>` — verify a journal snapshot's integrity and
//!   per-session invariants (EP-13-S10 AC 2).

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
    /// events, and structural integrity.
    VerifyLog {
        /// Path to the journal snapshot JSON file.
        path: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::VerifyLog { path } => {
            let code = rusty_agent_server::verify::verify_log_file(&path);
            std::process::exit(code);
        }
    }
}
