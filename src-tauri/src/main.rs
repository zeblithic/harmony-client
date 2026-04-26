#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "harmony-app", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Re-encrypt ~/.harmony/identity.enc with a new passphrase.
    ///
    /// The OLD passphrase is read from HARMONY_PASSPHRASE or
    /// HARMONY_PASSPHRASE_FILE (the same env vars used at startup). The NEW
    /// passphrase is read from --new-passphrase-file. Refuses to rotate if the
    /// identity is currently in the OS keychain; in that case the OS handles
    /// re-encryption when you change your login password.
    RotatePassphrase {
        /// Path to a file containing the new passphrase.
        #[arg(long, value_name = "PATH")]
        new_passphrase_file: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::RotatePassphrase { new_passphrase_file }) => {
            // Initialize tracing for CLI subcommands so warnings show up.
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .init();

            match harmony_app::rotate_passphrase_cli(&new_passphrase_file) {
                Ok(()) => {
                    println!("Passphrase rotated. Update your systemd unit / Docker secret to point at the new file.");
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("Rotation failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        None => {
            // Default path — launch the Tauri runtime.
            harmony_app::run();
        }
    }
}
