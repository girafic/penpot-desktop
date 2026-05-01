use std::fs;
use std::io::Cursor;
use std::path::PathBuf;

use serde::Serialize;
use uuid::Uuid;

use crate::backend::binfile;
use crate::backend::model::{File as PenpotFile, LOCAL_PROJECT_ID};
use crate::backend::store::Store as BackendStore;
use crate::config::{save_config, AppMode, SharedConfig};

#[tauri::command]
pub fn save_download(data: Vec<u8>, path: String) -> Result<String, String> {
    fs::write(&path, &data).map_err(|e| e.to_string())?;
    Ok(path)
}

#[tauri::command]
pub fn get_proxy_url(state: tauri::State<SharedConfig>) -> String {
    let port = state
        .inner()
        .try_read()
        .map(|c| c.proxy_port)
        .unwrap_or(7080);
    format!("http://127.0.0.1:{port}")
}

// ────────────────────── Offline file workflow ──────────────────────

#[derive(Serialize, Clone)]
pub struct OfflineFileSummary {
    pub id: Uuid,
    pub name: String,
    #[serde(rename = "projectId")]
    pub project_id: Uuid,
    pub revn: i64,
    #[serde(rename = "modifiedAt")]
    pub modified_at: String,
}

impl From<PenpotFile> for OfflineFileSummary {
    fn from(f: PenpotFile) -> Self {
        OfflineFileSummary {
            id: f.id,
            name: f.name,
            project_id: f.project_id,
            revn: f.revn,
            modified_at: f.modified_at.to_rfc3339(),
        }
    }
}

/// Import a `.penpot` archive from disk into the offline store.
/// Returns the imported file IDs (one per file in the archive).
#[tauri::command]
pub fn open_penpot_file(
    store: tauri::State<BackendStore>,
    path: String,
) -> Result<Vec<Uuid>, String> {
    let p = PathBuf::from(&path);
    let bytes = fs::read(&p).map_err(|e| format!("failed to read {path}: {e}"))?;
    import_bytes(&store, &bytes, LOCAL_PROJECT_ID)
}

/// Same as [`open_penpot_file`] but takes the bytes directly — useful when
/// the frontend has already loaded the file via a drag-drop or a file picker.
#[tauri::command]
pub fn import_penpot_file(
    store: tauri::State<BackendStore>,
    data: Vec<u8>,
    project_id: Option<String>,
) -> Result<Vec<Uuid>, String> {
    let project_id = project_id
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|e| format!("invalid projectId: {e}"))?
        .unwrap_or(LOCAL_PROJECT_ID);
    import_bytes(&store, &data, project_id)
}

fn import_bytes(
    store: &BackendStore,
    data: &[u8],
    project_id: Uuid,
) -> Result<Vec<Uuid>, String> {
    let mut cursor = Cursor::new(data);
    let format = binfile::detect(&mut cursor).map_err(|e| e.to_string())?;
    if !matches!(format, binfile::Format::BinfileV3) {
        return Err(format!(
            "unsupported .penpot format: {format:?} — \
             only binfile-v3 archives are supported in this build"
        ));
    }
    let cursor = Cursor::new(data);
    let imp = binfile::import_binfile_v3(cursor).map_err(|e| e.to_string())?;
    let mut ids = Vec::with_capacity(imp.files.len());
    for imported in imp.files {
        let media_blobs = imported.media.clone();
        let file = binfile::imported_to_file(imported, project_id);
        let file_id = file.id;
        ids.push(file_id);
        store.put_file(file);
        // Persist media bytes under their original storage-object IDs so
        // the URLs already baked into file.data.media[*].id resolve.
        for (id_str, bytes) in media_blobs {
            let Ok(storage_id) = Uuid::parse_str(&id_str) else { continue };
            let mime = guess_mime(&bytes);
            if let Err(e) = store.store_media(&bytes, &mime, Some(storage_id)) {
                eprintln!("[import] store_media({storage_id}) failed: {e}");
            }
        }
    }
    Ok(ids)
}

/// Best-effort MIME sniff from the first few bytes — covers the
/// formats Penpot accepts as image uploads. Fonts and unknown blobs
/// fall back to `application/octet-stream`.
fn guess_mime(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return "image/png".into();
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return "image/jpeg".into();
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return "image/gif".into();
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return "image/webp".into();
    }
    if bytes.starts_with(b"<svg") || bytes.starts_with(b"<?xml") {
        return "image/svg+xml".into();
    }
    "application/octet-stream".into()
}

/// Write an offline file out as a `.penpot` archive.
#[tauri::command]
pub fn save_penpot_file(
    store: tauri::State<BackendStore>,
    file_id: String,
    path: String,
) -> Result<String, String> {
    let bytes = export_bytes(&store, &file_id)?;
    fs::write(&path, &bytes).map_err(|e| format!("write {path}: {e}"))?;
    Ok(path)
}

/// Same as [`save_penpot_file`] but returns the raw archive bytes —
/// useful for hand-off via the dialog plugin's save-as flow.
#[tauri::command]
pub fn export_penpot_file(
    store: tauri::State<BackendStore>,
    file_id: String,
) -> Result<Vec<u8>, String> {
    export_bytes(&store, &file_id)
}

fn export_bytes(store: &BackendStore, file_id: &str) -> Result<Vec<u8>, String> {
    let id = Uuid::parse_str(file_id).map_err(|e| format!("invalid file id: {e}"))?;
    let file = store
        .get_file(id)
        .ok_or_else(|| format!("file {id} not found in offline store"))?;
    // Pull media bytes back out of the local store. Storage IDs in
    // file.data.media[*].id already match what was written on import or
    // upload, so the closure just forwards them.
    let media_provider = store.media_provider();
    binfile::export_to_bytes(&file, &media_provider).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn list_offline_files(store: tauri::State<BackendStore>) -> Vec<OfflineFileSummary> {
    store
        .list_project_files(LOCAL_PROJECT_ID)
        .into_iter()
        .map(OfflineFileSummary::from)
        .collect()
}

#[tauri::command]
pub fn delete_offline_file(
    store: tauri::State<BackendStore>,
    file_id: String,
) -> Result<bool, String> {
    let id = Uuid::parse_str(&file_id).map_err(|e| format!("invalid file id: {e}"))?;
    Ok(store.delete_file(id).is_some())
}

#[tauri::command]
pub async fn switch_mode(
    state: tauri::State<'_, SharedConfig>,
    mode: String,
) -> Result<String, String> {
    let new_mode = match mode.as_str() {
        "online" => AppMode::Online,
        "offline" => AppMode::Offline,
        other => return Err(format!("unknown mode: {other}")),
    };
    let mut cfg = state.write().await;
    cfg.mode = new_mode;
    save_config(&cfg);
    Ok(mode)
}
