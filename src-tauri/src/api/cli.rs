// src-tauri/src/api/cli.rs — ZEB-452: `harmony-app api` thin client.
//
// Stdout purity (PR #231 discipline): the RPC result JSON / event frames
// are the ONLY stdout; every diagnostic goes to stderr. Exit codes:
//   0 — success (HTTP 200 / clean stream close)
//   1 — server-reported error (any non-200 RPC response; body → stderr)
//   2 — local failure (discovery files missing, connect refused, bad usage)
// Agents that prefer curl/websocat ignore this entirely — same wire
// contract (spec §Thin CLI).

use std::path::Path;

#[derive(Clone, Debug)]
pub struct Discovery {
    pub port: u16,
    pub token: String,
}

/// Read `<data-dir>/api/{port,token}`. Error text names the likely cause:
/// only a live server writes these files (and removes them on shutdown).
pub fn read_discovery(data_dir: &Path) -> Result<Discovery, String> {
    let api_dir = data_dir.join("api");
    let port_path = api_dir.join("port");
    let port_raw = std::fs::read_to_string(&port_path).map_err(|e| {
        format!(
            "read {}: {e} — is `harmony-app serve` (or a GUI with HARMONY_API_PORT) running?",
            port_path.display()
        )
    })?;
    let port: u16 = port_raw.trim().parse().map_err(|e| {
        format!(
            "port file {} is corrupt ({e}): {port_raw:?}",
            port_path.display()
        )
    })?;
    let token_path = api_dir.join("token");
    let token = std::fs::read_to_string(&token_path)
        .map_err(|e| format!("read {}: {e}", token_path.display()))?
        .trim()
        .to_string();
    if token.is_empty() {
        return Err(format!("token file {} is empty", token_path.display()));
    }
    Ok(Discovery { port, token })
}

/// One-shot RPC POST. `Ok((status, body))` for ANY HTTP response — the
/// caller maps non-200 to exit 1. `Err` is transport/local-validation
/// failure (exit 2). Args are validated as JSON locally so a typo'd shell
/// quote fails fast with a useful message instead of a server 400.
pub async fn rpc_call(
    d: &Discovery,
    command: &str,
    args_json: Option<&str>,
) -> Result<(u16, String), String> {
    let body: serde_json::Value = match args_json {
        None => serde_json::json!({}),
        Some(raw) => {
            serde_json::from_str(raw).map_err(|e| format!("args are not valid JSON: {e}"))?
        }
    };
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/rpc/{}", d.port, command))
        .bearer_auth(&d.token)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("POST /v1/rpc/{command}: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("read response body: {e}"))?;
    Ok((status, text))
}

/// Stream `/v1/events` frames into `on_frame` until the server closes the
/// socket (`Ok`) or the connection fails (`Err`). Interruption is the
/// process-level ctrl-c default — no special handling needed for a
/// read-only stream.
pub async fn stream_events(d: &Discovery, mut on_frame: impl FnMut(&str)) -> Result<(), String> {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = format!("ws://127.0.0.1:{}/v1/events", d.port)
        .into_client_request()
        .map_err(|e| format!("build WS request: {e}"))?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", d.token)
            .parse()
            .map_err(|e| format!("auth header: {e}"))?,
    );
    let (ws, _resp) = tokio_tungstenite::connect_async(req)
        .await
        .map_err(|e| format!("connect /v1/events: {e}"))?;
    let (_write, mut read) = ws.split();
    while let Some(msg) = read.next().await {
        match msg {
            Ok(tokio_tungstenite::tungstenite::Message::Text(txt)) => on_frame(txt.as_str()),
            Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => return Ok(()),
            Ok(_) => {} // ping/pong/binary — not part of the frame contract
            Err(e) => return Err(format!("events stream: {e}")),
        }
    }
    Ok(())
}

/// Blocking CLI entry (called from main.rs). Validates the mode, resolves
/// discovery, runs the client on a current-thread runtime, prints, and
/// returns the process exit code.
pub fn api_cli(command: Option<String>, args_json: Option<String>, events: bool) -> i32 {
    if events && command.is_some() {
        eprintln!("api: --events takes no command (use one or the other)");
        return 2;
    }
    if !events && command.is_none() {
        eprintln!("api: missing command (or --events). Example: harmony-app api get_owner_state");
        return 2;
    }
    let data_dir = match crate::resolve_app_data_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("api: {e}");
            return 2;
        }
    };
    let d = match read_discovery(&data_dir) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("api: {e}");
            return 2;
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("api: cannot build tokio runtime: {e}");
            return 2;
        }
    };
    rt.block_on(async move {
        if events {
            use std::io::Write;
            match stream_events(&d, |frame| {
                // Explicit flush: stdout is block-buffered when piped, and
                // agents tail this stream live.
                let mut out = std::io::stdout();
                let _ = writeln!(out, "{frame}");
                let _ = out.flush();
            })
            .await
            {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("api: {e}");
                    2
                }
            }
        } else {
            let command = command.expect("mode validated above");
            match rpc_call(&d, &command, args_json.as_deref()).await {
                Ok((200, body)) => {
                    println!("{body}");
                    0
                }
                Ok((status, body)) => {
                    eprintln!("api: HTTP {status}: {body}");
                    1
                }
                Err(e) => {
                    eprintln!("api: {e}");
                    2
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_discovery_missing_files_names_the_server() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = read_discovery(dir.path()).expect_err("no files must err");
        assert!(
            err.contains("harmony-app serve"),
            "error must hint at the server: {err}"
        );
    }

    #[test]
    fn read_discovery_rejects_corrupt_port() {
        let dir = tempfile::tempdir().expect("tempdir");
        let api = dir.path().join("api");
        std::fs::create_dir_all(&api).expect("mkdir");
        std::fs::write(api.join("port"), "not-a-port").expect("write port");
        std::fs::write(api.join("token"), "deadbeef").expect("write token");
        let err = read_discovery(dir.path()).expect_err("corrupt port must err");
        assert!(err.contains("corrupt"), "{err}");
    }

    #[test]
    fn read_discovery_happy_path_trims_whitespace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let api = dir.path().join("api");
        std::fs::create_dir_all(&api).expect("mkdir");
        std::fs::write(api.join("port"), "7420\n").expect("write port");
        std::fs::write(api.join("token"), "abc123\n").expect("write token");
        let d = read_discovery(dir.path()).expect("happy path");
        assert_eq!(d.port, 7420);
        assert_eq!(d.token, "abc123");
    }

    #[test]
    fn read_discovery_rejects_empty_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        let api = dir.path().join("api");
        std::fs::create_dir_all(&api).expect("mkdir");
        std::fs::write(api.join("port"), "7420").expect("write port");
        std::fs::write(api.join("token"), "\n").expect("write token");
        let err = read_discovery(dir.path()).expect_err("empty token must err");
        assert!(err.contains("empty"), "{err}");
    }

    /// Mode validation happens before any filesystem/network access, so
    /// these run hermetically.
    #[test]
    fn api_cli_rejects_bad_mode_combinations() {
        assert_eq!(api_cli(Some("x".into()), None, true), 2);
        assert_eq!(api_cli(None, None, false), 2);
    }
}
