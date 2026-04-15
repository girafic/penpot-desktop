use crate::config::SharedConfig;

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
