//! Backend-owned save-dialog IPC. The renderer never names a write path
//! directly: it asks this command to open the OS save dialog, and gets back
//! an opaque UUID that resolves to the user-confirmed PathBuf on commit.
//!
//! Pair this with `crate::owner_state::take_path_token` in any command that
//! used to take a renderer-supplied path. See ZEB-194 design doc:
//! `docs/specs/2026-04-29-zeb-194-export-path-capability-token-design.md`.

use crate::identity_commands::run_blocking;
use crate::owner_state::insert_path_token;
use tauri_plugin_dialog::DialogExt;

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportPathRequest {
    pub title: Option<String>,
    pub default_filename: String,
    pub filter_name: String,
    pub filter_extensions: Vec<String>,
}

/// Open the OS save dialog and cache the user-confirmed path under a UUID.
/// Returns `Ok(Some(uuid))` on confirm, `Ok(None)` on cancel,
/// `Err(_)` if the OS dialog returns a non-filesystem path
/// (e.g., URI on mobile) or the blocking task fails to join.
///
/// The dialog hints (title/filename/filter) are renderer-supplied UX-only
/// values — the user explicitly confirms the final path in the OS dialog
/// and can rename or redirect freely. Default filename is a hint, not a
/// security control.
#[tauri::command]
pub async fn request_export_save_path(
    app: tauri::AppHandle,
    request: ExportPathRequest,
) -> Result<Option<String>, String> {
    run_blocking(move || {
        // tauri_plugin_dialog's blocking save_file API: synchronous on a
        // worker thread, returns Option<FilePath>. None == user cancelled.
        //
        // `add_filter` wants `&[&str]`, but the IPC payload arrives as
        // `Vec<String>`. Materialize a temporary `Vec<&str>` borrowing from
        // `request.filter_extensions` so the slice has a stable lifetime
        // that outlives the builder call.
        let extensions: Vec<&str> = request
            .filter_extensions
            .iter()
            .map(String::as_str)
            .collect();
        let mut builder = app
            .dialog()
            .file()
            .set_file_name(&request.default_filename)
            .add_filter(&request.filter_name, &extensions);
        if let Some(t) = request.title.as_deref() {
            builder = builder.set_title(t);
        }
        let chosen = builder.blocking_save_file();
        match chosen {
            Some(file_path) => {
                // Tauri 2 wraps the path in `FilePath`; `into_path()` extracts
                // the PathBuf. On platforms where the dialog can return URI-
                // style paths (mobile), this errors — we surface that as an
                // IPC error rather than silently dropping the user's choice.
                let path = file_path
                    .into_path()
                    .map_err(|e| format!("dialog returned non-filesystem path: {e}"))?;
                let token = insert_path_token(path);
                Ok(Some(token.to_string()))
            }
            None => Ok(None),
        }
    })
    .await
}
