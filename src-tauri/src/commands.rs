use crate::config::SharedConfig;
use crate::updater::{self, UpdateInfo};

#[tauri::command]
pub fn save_download(data: Vec<u8>, path: String) -> Result<String, String> {
    std::fs::write(&path, &data).map_err(|e| e.to_string())?;
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

#[tauri::command]
pub async fn check_for_updates(
    app: tauri::AppHandle,
    state: tauri::State<'_, SharedConfig>,
) -> Result<UpdateInfo, String> {
    let current = app.package_info().version.to_string();
    let result = updater::check(&current).await?;
    updater::record_check(state.inner()).await;
    Ok(result)
}

#[tauri::command]
pub fn open_update_page(app: tauri::AppHandle, url: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(&url, None::<&str>)
        .map_err(|e| e.to_string())
}
