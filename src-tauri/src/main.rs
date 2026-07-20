#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "harmony-app", version)]
struct Cli {
    /// Named storage profile (ZEB-446): scopes ~/.harmony and the app-data
    /// dir to profiles/<NAME> so a second instance (e.g. the pinned headless
    /// coordination node) can run beside the default GUI. Falls back to
    /// HARMONY_PROFILE; omit for the default profile.
    #[arg(long, global = true, value_name = "NAME")]
    profile: Option<String>,

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

    /// Restore an identity from a backup: a Reticulum identity (mnemonic or
    /// recovery file) or the owner master seed (`owner-mnemonic`, ZEB-439 —
    /// re-adopts the owner identity from its exported mnemonic).
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

    /// Drive a running node's localhost API (ZEB-445): POST one RPC
    /// command (result JSON on stdout) or stream the event firehose with
    /// --events. Reads <data-dir>/api/{port,token} written by `serve` or
    /// a GUI launched with HARMONY_API_PORT.
    Api {
        /// RPC command name, e.g. get_owner_state (surface list:
        /// docs/headless-install.md).
        command: Option<String>,
        /// JSON object with the command's camelCase args, e.g.
        /// '{"name":"my community","isInviteOnly":true}'.
        args: Option<String>,
        /// Stream /v1/events as one JSON frame per line until interrupted.
        #[arg(long)]
        events: bool,
    },

    /// Watch a running node's channel(s) for new messages, emitting one JSON
    /// line per message on stdout (ZEB-480): filtered, resumable, self-healing
    /// — the push analog of `api --events`. Reads <data-dir>/api/{port,token}.
    Watch {
        /// Community id (hex) the watched channels belong to.
        #[arg(long, value_name = "HEX")]
        community: String,
        /// Channel id (hex) to watch; repeatable (>=1).
        #[arg(long = "channel", value_name = "HEX", required = true)]
        channels: Vec<String>,
        /// Resume cursor `wallMs:logical:deviceId`; seeds all channels.
        #[arg(long, value_name = "HLC")]
        since: Option<String>,
        /// Self-persist + auto-resume per-channel cursors at this path.
        #[arg(long, value_name = "PATH")]
        cursor_file: Option<PathBuf>,
        /// Emit untouched firehose frames instead of the normalized projection.
        #[arg(long)]
        raw: bool,
        /// Exit on disconnect (code 3) instead of reconnecting.
        #[arg(long)]
        no_retry: bool,
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
    /// Read a 24-word RETICULUM identity mnemonic from a file
    /// (whitespace-tolerant, case-insensitive).
    Mnemonic {
        #[arg(long, value_name = "PATH")]
        mnemonic_file: PathBuf,
    },
    /// Read a 24-word OWNER master-seed mnemonic from a file and re-adopt the
    /// owner identity (same owner-id, fresh device key). Refuses to overwrite
    /// an existing owner without --force (and refuses a different owner even
    /// with --force).
    OwnerMnemonic {
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

/// Minimum per-thread stack (bytes) this process needs so Zenoh's internal
/// tokio runtime survives the large `add_space`/DM async state machines. 8 MiB
/// matches `.cargo/config.toml`'s dev `[env]` net and the explicit
/// `.thread_stack_size` on our own runtime in `lib.rs`. See ZEB-503 / ZEB-149.
const MIN_STACK_BYTES: usize = 8 * 1024 * 1024;

/// Parse a raw `RUST_MIN_STACK` value into bytes, whitespace-tolerant. Returns
/// `None` if absent OR malformed — std itself silently falls back to the 2 MiB
/// default on an unparseable value, so a malformed value behaves like "unset"
/// and must be raised. Split out (and pure) so the trim/parse is unit-testable.
fn parse_min_stack(raw: Option<&str>) -> Option<usize> {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
}

/// Pure decision for [`raise_min_stack`]: given the currently-set
/// `RUST_MIN_STACK` (parsed bytes; `None` if unset or unparseable), return
/// `Some(target)` if it should be raised, else `None` (the operator's value is
/// already adequate). Split out so it is unit-testable without mutating the
/// process-global environment.
fn min_stack_target(current: Option<usize>) -> Option<usize> {
    match current {
        Some(cur) if cur >= MIN_STACK_BYTES => None,
        _ => Some(MIN_STACK_BYTES),
    }
}

/// Raise `RUST_MIN_STACK` to at least [`MIN_STACK_BYTES`] unless the operator
/// already set a larger value. MUST be called before any thread is spawned —
/// std caches `thread::min_stack()` on first read, and the value is only
/// consulted when a thread (including Zenoh's internal runtime workers) is
/// created.
fn raise_min_stack() {
    let raw = std::env::var("RUST_MIN_STACK").ok();
    let current = parse_min_stack(raw.as_deref());
    // Present but unparseable: std would silently fall back to the 2 MiB default
    // (the very overflow this guards against). Warn before overriding so a
    // formatting mistake isn't swallowed. Tracing isn't up yet → stderr.
    if current.is_none() {
        if let Some(bad) = raw.as_deref() {
            eprintln!(
                "harmony-app: ignoring malformed RUST_MIN_STACK={bad:?}; using the \
                 {MIN_STACK_BYTES}-byte floor (Zenoh's internal runtime needs it; ZEB-503)"
            );
        }
    }
    if let Some(target) = min_stack_target(current) {
        std::env::set_var("RUST_MIN_STACK", target.to_string());
    }
}

fn main() {
    // ZEB-503 / ZEB-149: raise the per-thread stack floor before anything
    // spawns a thread. Zenoh 1.x builds its OWN internal tokio runtime
    // (`event_loop.rs` `RuntimeBuilder`) whose worker threads we cannot size via
    // our own runtime builder; the `add_space` -> `DmInvite` path's large async
    // state machines overflow the std 2 MiB default and abort the process
    // (STATUS_STACK_OVERFLOW / SIGSEGV). The mitigations in `.cargo/config.toml`
    // do NOT cover a directly-run binary: the `[env] RUST_MIN_STACK` only reaches
    // cargo-spawned launches (cargo run / tauri dev / nextest), and the MSVC
    // `/STACK` image default is overridden by the explicit per-thread stack size
    // std passes to the OS. Setting it here — before the first thread spawn — is
    // the single place that reaches every launch, including a `tauri build`
    // artifact and the headless `serve` the agent harness runs directly.
    raise_min_stack();

    // Cli::parse() exits the process on any unrecognized flag / positional,
    // which would block GUI launch on hosts that pass OS-injected argv:
    // - macOS file-open Apple Events translated to argv (`-psn_X_Y` and friends)
    // - Linux desktop environments injecting GTK / accessibility flags
    // - File-association invocations passing a path as an unexpected positional
    // Use try_parse and fall through to the GUI on any non-help failure so a
    // misbehaving desktop integration can't keep the app from starting. CLI
    // typos still print the parse error to stderr before falling through.
    match Cli::try_parse() {
        Ok(cli) => {
            // ZEB-446: activate the storage profile BEFORE tracing init or
            // any path resolution (log dirs and both storage roots depend
            // on it). Invalid names are a hard startup error — silently
            // landing in the default profile's data is the worst outcome.
            if let Err(e) = harmony_app::profile::set_active_profile(cli.profile.as_deref()) {
                eprintln!("harmony-app: {e}");
                std::process::exit(2);
            }
            match cli.command {
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
                        RestoreFormat::OwnerMnemonic { mnemonic_file } => {
                            harmony_app::recovery_cli::restore_owner_mnemonic_cli(
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
                Some(Command::Api {
                    command,
                    args,
                    events,
                }) => {
                    init_tracing();
                    std::process::exit(harmony_app::api_cli(command, args, events));
                }
                Some(Command::Watch {
                    community,
                    channels,
                    since,
                    cursor_file,
                    raw,
                    no_retry,
                }) => {
                    init_tracing();
                    let since = match since.as_deref().map(harmony_app::api::watch::parse_since) {
                        Some(Ok(h)) => Some(h),
                        Some(Err(e)) => {
                            eprintln!("watch: {e}");
                            std::process::exit(2);
                        }
                        None => None,
                    };
                    let cfg = harmony_app::WatchConfig {
                        community_id: community,
                        channels,
                        since,
                        cursor_file,
                        raw,
                        no_retry,
                    };
                    std::process::exit(harmony_app::api_watch(cfg));
                }
                None => {
                    // Default path — launch the Tauri runtime.
                    harmony_app::run();
                }
            }
        }
        Err(err) => {
            use clap::error::ErrorKind;
            // --help / --version: clap-default behavior (print + exit 0).
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                err.exit();
            }
            // ZEB-446: an argv set containing an explicit --profile is an
            // operator-typed CLI invocation, not OS-injected launch argv —
            // falling through to the GUI here could put the session on the
            // wrong profile's data. Exit loudly instead (PR #245 round 1).
            if std::env::args().any(|a| a == "--profile" || a.starts_with("--profile=")) {
                eprintln!(
                    "harmony-app: argv parsing failed ({err}); refusing GUI fall-through \
                     because --profile was given"
                );
                std::process::exit(2);
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

#[cfg(test)]
mod tests {
    use super::{min_stack_target, parse_min_stack, MIN_STACK_BYTES};

    #[test]
    fn parse_min_stack_trims_and_rejects_malformed() {
        assert_eq!(parse_min_stack(None), None);
        assert_eq!(parse_min_stack(Some("")), None);
        assert_eq!(parse_min_stack(Some("   ")), None);
        assert_eq!(parse_min_stack(Some("not-a-number")), None);
        assert_eq!(parse_min_stack(Some("8388608")), Some(8 * 1024 * 1024));
        // Whitespace (e.g. a stray newline in a systemd unit) is tolerated, so a
        // valid operator value is preserved rather than silently overwritten.
        assert_eq!(
            parse_min_stack(Some("  33554432\n")),
            Some(32 * 1024 * 1024)
        );
    }

    #[test]
    fn raises_when_unset_or_below_floor() {
        // Unset / unparseable -> raise to the floor.
        assert_eq!(min_stack_target(None), Some(MIN_STACK_BYTES));
        // The std 2 MiB default that overflows the add_space path -> raise.
        assert_eq!(
            min_stack_target(Some(2 * 1024 * 1024)),
            Some(MIN_STACK_BYTES)
        );
        assert_eq!(min_stack_target(Some(0)), Some(MIN_STACK_BYTES));
    }

    #[test]
    fn preserves_adequate_or_higher_operator_value() {
        // Exactly the floor -> leave it.
        assert_eq!(min_stack_target(Some(MIN_STACK_BYTES)), None);
        // A larger operator-set value (e.g. the 32 MiB harness workaround) wins.
        assert_eq!(min_stack_target(Some(32 * 1024 * 1024)), None);
    }
}
