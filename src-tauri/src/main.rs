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

    /// Export an identity backup (Reticulum identity seed, or the owner
    /// master seed via `owner-mnemonic`).
    Export {
        #[command(subcommand)]
        format: ExportFormat,
    },

    /// Restore a Reticulum identity from a backup (mnemonic or recovery
    /// file). No owner-master-seed restore path exists yet — ZEB-439
    /// tracks re-adopting an owner identity from its exported mnemonic.
    Restore {
        #[command(subcommand)]
        format: RestoreFormat,

        /// Overwrite an existing identity (destructive).
        #[arg(long, global = true)]
        force: bool,

        /// Skip auto-detection of the `<PATH>.state` sidecar (identity-only restore).
        #[arg(long, global = true)]
        ignore_state: bool,
    },

    /// Run a windowless node exposing the localhost HTTP+WS control surface
    /// (ZEB-445). Token + bound port are written to <data-dir>/api/.
    Serve {
        /// Port for the API server (default 7420; 0 = OS-assigned).
        #[arg(long, value_name = "PORT")]
        api_port: Option<u16>,
    },
}

#[derive(Subcommand, Debug)]
enum ExportFormat {
    /// Print the RETICULUM IDENTITY seed (node keypair) as 24 BIP39 words
    /// (bare to stdout, warning + identity-hash to stderr). Does NOT back up
    /// the owner identity (friends/communities) — use `owner-mnemonic`.
    Mnemonic,
    /// Print the OWNER master seed (friends, communities, device
    /// enrollments) as 24 BIP39 words (bare to stdout, warning + owner-id
    /// to stderr).
    OwnerMnemonic,
    /// Write a passphrase-encrypted recovery file for the Reticulum
    /// identity seed. Requires
    /// HARMONY_RECOVERY_PASSPHRASE / HARMONY_RECOVERY_PASSPHRASE_FILE.
    RecoveryFile {
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
        #[arg(long, value_name = "STRING")]
        comment: Option<String>,
        /// Skip the owner-state sidecar (identity-only backup).
        #[arg(long)]
        no_state: bool,
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
            Some(Command::RotatePassphrase {
                new_passphrase_file,
            }) => {
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
                    ExportFormat::OwnerMnemonic => {
                        harmony_app::recovery_cli::export_owner_mnemonic_cli(&plaintext_path)
                    }
                    ExportFormat::RecoveryFile {
                        out,
                        comment,
                        no_state,
                    } => harmony_app::recovery_cli::export_recovery_file_cli(
                        &plaintext_path,
                        &out,
                        comment.as_deref(),
                        /*include_state=*/ !no_state,
                        /*force=*/ false,
                    ),
                };
                match result {
                    Ok(()) => std::process::exit(0),
                    Err(e) => {
                        eprintln!("Error: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Some(Command::Restore {
                format,
                force,
                ignore_state,
            }) => {
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
                            ignore_state,
                        )
                    }
                };
                match result {
                    Ok(()) => std::process::exit(0),
                    Err(e) => {
                        eprintln!("Error: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Some(Command::Serve { api_port }) => {
                // No init_tracing() here: serve_cli installs its own
                // subscriber (stderr console + rolling file, ZEB-445).
                std::process::exit(harmony_app::serve_cli(api_port));
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
    // try_init (not init) so a second subscriber install never panics; the GUI
    // path (lib.rs run()) installs its own via app_tracing. ZEB-379.
    //
    // Writer is stderr, NOT the fmt default of stdout: CLI subcommands promise
    // machine-readable stdout (`export mnemonic > backup.txt` must capture the
    // words and nothing else), so log lines must never interleave. ZEB-430.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
}
