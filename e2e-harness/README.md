# e2e-harness (ZEB-447)

Standalone harness that spawns two real `harmony-app serve` nodes under named
profiles and drives them over the live HTTP/WS API.

## Run

```bash
# 1. Build the binary the harness drives:
cd src-tauri && cargo build --bin harmony-app && cd ..

# 2. Run the scenario suite (slow, real transport):
cd e2e-harness && cargo nextest run --features e2e
```

Set `HARMONY_APP_BIN=/path/to/harmony-app` to override binary discovery.
Set `HARMONY_E2E_KEEP=1` to retain run artifacts on success
(`e2e-harness/target/e2e-runs/<scenario>-<runid>/`).
