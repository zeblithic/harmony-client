use serde::Serialize;

/// Vine video descriptor returned to the frontend.
///
/// Mirrors `harmony_content::vine::VineDescriptor` but uses hex-encoded
/// strings for CIDs and addresses (easier to consume from TypeScript).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VineVideoDto {
    pub id: String,
    pub creator_address: String,
    pub creator_name: String,
    pub created_at: u64,
    pub video_cid: String,
    pub title: Option<String>,
    pub reshare_of: Option<String>,
    pub viewed: bool,
}

#[tauri::command]
fn list_vine_videos() -> Vec<VineVideoDto> {
    // Stub — returns empty until real content transport is wired up.
    // The frontend uses mock data in the meantime.
    Vec::new()
}

#[tauri::command]
fn follow_vine_creator(_address: String) -> bool {
    // Stub — will subscribe to vine announce key expression via zenoh.
    true
}

#[tauri::command]
fn unfollow_vine_creator(_address: String) -> bool {
    // Stub — will unsubscribe from vine announce key expression.
    true
}

#[tauri::command]
fn mark_vine_viewed(_vine_id: String) -> bool {
    // Stub — will update viewed state in VineFeed state machine.
    true
}

pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            list_vine_videos,
            follow_vine_creator,
            unfollow_vine_creator,
            mark_vine_viewed,
        ])
        .run(tauri::generate_context!())
        .expect("error while running harmony");
}
