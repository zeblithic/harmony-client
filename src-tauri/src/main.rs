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

    /// Export the identity for backup.
    Export {
        #[command(subcommand)]
        format: ExportFormat,
    },

    /// Restore an identity from a backup.
    Restore {
        #[command(subcommand)]
        format: RestoreFormat,

        /// Overwrite an existing identity (destructive).
        #[arg(long, global = true)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ExportFormat {
    /// Print 24-word BIP39 mnemonic (bare to stdout, warning + identity-hash to stderr).
    Mnemonic,
    /// Write a passphrase-encrypted recovery file. Requires
    /// HARMONY_RECOVERY_PASSPHRASE / HARMONY_RECOVERY_PASSPHRASE_FILE.
    RecoveryFile {
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
        #[arg(long, value_name = "STRING")]
        comment: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum RestoreFormat {
    /// Read a 24-word mnemonic from a file (whitespace-tolerant, case-insensitive).
    Mnemonic {
        #[arg(long, value_name = "PATH")]
        mnemonic_file: PathBuf,
    },
    /// Read a passphrase-encrypted recovery file. Requires
    /// HARMONY_RECOVERY_PASSPHRASE / HARMONY_RECOVERY_PASSPHRASE_FILE.
    RecoveryFile {
        #[arg(long = "in", value_name = "PATH")]
        in_path: PathBuf,
    },
}

fn main() {
    // Cli::parse() exits the process on any unrecognized flag / positional,
    // which would block GUI launch on hosts that pass OS-injected argv:
    // - macOS file-open Apple Events translated to argv (`-psn_X_Y` and friends)
    // - Linux desktop environments injecting GTK / accessibility flags
    // - File-association invocations passing a path as an unexpected positional
    // Use try_parse and fall through to the GUI on any non-help failure so a
    // misbehaving desktop integration can't keep the app from starting. CLI
    // typos still print the parse error to stderr before falling through.
    match Cli::try_parse() {
        Ok(cli) => match cli.command {
            Some(Command::RotatePassphrase { new_passphrase_file }) => {
                init_tracing();
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
            Some(Command::Export { format }) => {
                init_tracing();
                let plaintext_path = match harmony_app::identity::resolve_path(None) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("Error: {e}");
                        std::process::exit(1);
                    }
                };
                let result = match format {
                    ExportFormat::Mnemonic => {
                        harmony_app::recovery_cli::export_mnemonic_cli(&plaintext_path)
                    }
                    ExportFormat::RecoveryFile { out, comment } => {
                        harmony_app::recovery_cli::export_recovery_file_cli(
                            &plaintext_path,
                            &out,
                            comment.as_deref(),
                        )
                    }
                };
                match result {
                    Ok(()) => std::process::exit(0),
                    Err(e) => {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
                }
            }
            Some(Command::Restore { format, force }) => {
                init_tracing();
                let plaintext_path = match harmony_app::identity::resolve_path(None) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("Error: {e}");
                        std::process::exit(1);
                    }
                };
                let result = match format {
                    RestoreFormat::Mnemonic { mnemonic_file } => {
                        harmony_app::recovery_cli::restore_mnemonic_cli(
                            &plaintext_path,
                            &mnemonic_file,
                            force,
                        )
                    }
                    RestoreFormat::RecoveryFile { in_path } => {
                        harmony_app::recovery_cli::restore_recovery_file_cli(
                            &plaintext_path,
                            &in_path,
                            force,
                        )
                    }
                };
                match result {
                    Ok(()) => std::process::exit(0),
                    Err(e) => {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
                }
            }
            None => {
                // Default path — launch the Tauri runtime.
                harmony_app::run();
            }
        },
        Err(err) => {
            use clap::error::ErrorKind;
            // --help / --version: clap-default behavior (print + exit 0).
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                err.exit();
            }
            // All other parse failures: leave a stderr breadcrumb and launch
            // the GUI. The breadcrumb means a CLI typo isn't fully silent
            // (operator sees `harmony-app: argv parsing failed ...` in their
            // terminal / journalctl) while still letting OS-injected args
            // pass through to the Tauri runtime.
            eprintln!("harmony-app: argv parsing failed ({err}); launching GUI");
            eprintln!("(run `harmony-app help` for the CLI subcommand list)");
            harmony_app::run();
        }
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}
