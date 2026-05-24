# Harmony

Federated, polycentric social fabric. Desktop client built on Tauri 2 + Svelte 5 + Rust.

## Status: v0.1.0-alpha (invite-only)

Harmony is in private alpha. If you have an invite, install via the docs below and use your
`harmony://invite/...` URL to join.

## Install

- **macOS** (Apple Silicon or Intel): see [docs/install-macos.md](docs/install-macos.md)
- **Windows** (x64): see [docs/install-windows.md](docs/install-windows.md)
- **Linux** (x86_64 AppImage): see [docs/install-linux.md](docs/install-linux.md)
- **Server / headless**: see [docs/headless-install.md](docs/headless-install.md)

Builds are unsigned during alpha; install docs cover the Gatekeeper / SmartScreen workaround
for your OS.

## Development

Run tests:
- Rust: `cd src-tauri && cargo nextest run --locked --workspace --all-targets --features test-fixtures`
- Frontend: `npx vitest run`

See [CLAUDE.md](CLAUDE.md) for the full developer guide.

## License

ISC.
