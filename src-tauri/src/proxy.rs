use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tauri::Manager;
use tokio::sync::Mutex;
use warp::Filter;

use crate::backend::{flags as backend_flags, rpc as backend_rpc, store as backend_store};
use crate::config::{
    desktop_to_penpot_locale, save_config, AppMode, SharedConfig,
    DESKTOP_CONFIG_JS, IFRAME_SHIM_JS,
};
#[cfg(target_os = "macos")]
use crate::config::desktop_to_apple_locale;
use crate::i18n;
use crate::menu::{build_menu, update_selection_items};
#[cfg(target_os = "macos")]
use crate::menu::{register_help_menu, register_window_menu};
use crate::state::{
    focused_window_mode, get_window_mode, set_window_mode, track_tab_title, track_tab_url,
    update_plugins, PluginInfo, APP_HANDLE, CURRENT_LANG,
};
use crate::windows::create_tab_window;

#[cfg(target_os = "macos")]
fn set_apple_languages(lang: &str) {
    unsafe {
        use objc2::runtime::{AnyClass, AnyObject};
        use std::ffi::CString;
        let defaults: *mut AnyObject = objc2::msg_send![
            AnyClass::get(c"NSUserDefaults").unwrap(),
            standardUserDefaults
        ];
        let lang_cstr = CString::new(lang).unwrap();
        let ns_lang: *mut AnyObject = objc2::msg_send![
            AnyClass::get(c"NSString").unwrap(),
            stringWithUTF8String: lang_cstr.as_ptr()
        ];
        let arr: *mut AnyObject = objc2::msg_send![
            AnyClass::get(c"NSArray").unwrap(),
            arrayWithObject: ns_lang
        ];
        let key = c"AppleLanguages";
        let ns_key: *mut AnyObject = objc2::msg_send![
            AnyClass::get(c"NSString").unwrap(),
            stringWithUTF8String: key.as_ptr()
        ];
        let _: () = objc2::msg_send![defaults, setObject: arr, forKey: ns_key];
        let _: () = objc2::msg_send![defaults, synchronize];
    }
}

// ── Error deduplication ─────────────────────────────────────
// Suppresses repeated identical proxy errors to avoid log spam
// when the backend is unreachable.

struct ErrorTracker {
    last_errors: HashMap<String, (String, Instant, u64)>, // key → (message, first_seen, suppressed_count)
}

impl ErrorTracker {
    fn new() -> Self {
        Self {
            last_errors: HashMap::new(),
        }
    }

    /// Log an error only if it's new or enough time has passed (5 s).
    /// Returns true if the message was printed.
    fn log(&mut self, key: &str, message: &str) -> bool {
        let now = Instant::now();
        if let Some((prev_msg, last_time, count)) = self.last_errors.get_mut(key) {
            if prev_msg == message && now.duration_since(*last_time).as_secs() < 5 {
                *count += 1;
                return false;
            }
            // Different error or cooldown expired — flush suppressed count
            let suppressed = *count;
            if suppressed > 0 {
                eprintln!("[proxy] … {suppressed} identical error(s) suppressed for {key}");
            }
        }
        eprintln!("{message}");
        self.last_errors
            .insert(key.to_string(), (message.to_string(), now, 0));
        true
    }
}

// ── Reverse Proxy Server ─────────────────────────────────────

/// Extract the Penpot frontend version from the built index.html.
/// Looks for the first `?version=X.Y.Z` query string emitted by the build.
fn read_penpot_version(penpot_dir: &PathBuf) -> String {
    std::fs::read_to_string(penpot_dir.join("index.html"))
        .ok()
        .and_then(|html| {
            html.split("?version=").nth(1).map(|tail| {
                tail.chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect::<String>()
            })
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

pub async fn start_proxy(config: SharedConfig, penpot_dir: PathBuf) {
    start_proxy_with(config, penpot_dir, backend_store::Store::seeded()).await
}

pub async fn start_proxy_with(
    config: SharedConfig,
    penpot_dir: PathBuf,
    backend_store: backend_store::Store,
) {
    let port = config.read().await.proxy_port;
    let penpot_version = read_penpot_version(&penpot_dir);
    println!("📦 Penpot frontend version: {penpot_version}");

    let config_for_api = config.clone();
    let config_for_assets = config.clone();
    let config_for_internal = config.clone();
    let config_for_ws = config.clone();
    let error_tracker = Arc::new(Mutex::new(ErrorTracker::new()));
    let error_tracker_api = error_tracker.clone();
    let error_tracker_assets = error_tracker.clone();
    let error_tracker_internal = error_tracker.clone();
    let config_for_cfg = config.clone();
    let config_for_set = config.clone();

    // ── GET/POST /__penpot_desktop/config → return current config as JSON
    let get_config = warp::path!("__penpot_desktop" / "config")
        .and(warp::get())
        .and_then(move || {
            let cfg = config_for_cfg.clone();
            async move {
                let c = cfg.read().await;
                Ok::<_, warp::Rejection>(warp::reply::json(&*c))
            }
        });

    // ── POST /__penpot_desktop/set-backend → update backend URL
    let set_backend = warp::path!("__penpot_desktop" / "set-backend")
        .and(warp::post())
        .and(warp::body::json())
        .and_then(move |body: serde_json::Value| {
            let cfg = config_for_set.clone();
            async move {
                if let Some(url) = body.get("url").and_then(|v| v.as_str()) {
                    let mut c = cfg.write().await;
                    c.backend_url = url.to_string();
                    if let Some(renderer) = body.get("renderer").and_then(|v| v.as_str()) {
                        c.renderer = renderer.to_string();
                    }
                    if !c.recent_urls.contains(&url.to_string()) {
                        c.recent_urls.insert(0, url.to_string());
                        if c.recent_urls.len() > 10 {
                            c.recent_urls.truncate(10);
                        }
                    }
                    save_config(&c);

                    // Close all other tabs when switching backends
                    if let Some(app) = APP_HANDLE.get() {
                        let windows: Vec<_> = app
                            .webview_windows()
                            .into_iter()
                            .filter(|(_, win)| {
                                // Keep the window showing settings
                                win.url()
                                    .map(|u| !u.path().contains("__penpot_desktop"))
                                    .unwrap_or(true)
                            })
                            .collect();
                        for (_, win) in windows {
                            let _ = win.close();
                        }
                    }

                    Ok::<_, warp::Rejection>(warp::reply::json(&serde_json::json!({"ok": true})))
                } else {
                    Ok(warp::reply::json(
                        &serde_json::json!({"error": "missing url"}),
                    ))
                }
            }
        });

    // ── POST /__penpot_desktop/set-mode → toggle online/offline mode
    // Persists the new mode and rebuilds the menu so the File submenu
    // surfaces the right items. The page is expected to reload itself
    // after the call so it picks up the new initial URL.
    let config_for_mode = config.clone();
    let set_mode = warp::path!("__penpot_desktop" / "set-mode")
        .and(warp::post())
        .and(warp::body::json())
        .and_then(move |body: serde_json::Value| {
            let cfg = config_for_mode.clone();
            async move {
                let mode_str = body.get("mode").and_then(|v| v.as_str()).unwrap_or("online");
                let new_mode = match mode_str {
                    "offline" => AppMode::Offline,
                    _ => AppMode::Online,
                };
                {
                    let mut c = cfg.write().await;
                    c.mode = new_mode;
                    save_config(&c);
                }
                // Rebuild menu to reflect mode-dependent File submenu items.
                if let Some(app) = APP_HANDLE.get() {
                    let mode = focused_window_mode(app);
                    if let Ok((menu, _)) = build_menu(app, &mode) {
                        let _ = app.set_menu(menu);
                        #[cfg(target_os = "macos")]
                        {
                            let _ = app.run_on_main_thread(|| {
                                register_help_menu();
                                register_window_menu();
                            });
                        }
                    }
                }
                Ok::<_, warp::Rejection>(warp::reply::json(&serde_json::json!({"ok": true})))
            }
        });

    // ── GET /__penpot_desktop/offline-files → list files in the offline store
    let backend_store_for_files = backend_store.clone();
    let offline_files = warp::path!("__penpot_desktop" / "offline-files")
        .and(warp::get())
        .and_then(move || {
            let store = backend_store_for_files.clone();
            async move {
                let files: Vec<serde_json::Value> = store
                    .list_project_files(crate::backend::model::LOCAL_PROJECT_ID)
                    .into_iter()
                    .map(|f| {
                        let pages = f
                            .data
                            .get("pages")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default();
                        let first_page = pages
                            .first()
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        serde_json::json!({
                            "id": f.id,
                            "projectId": f.project_id,
                            "teamId": crate::backend::model::LOCAL_TEAM_ID,
                            "name": f.name,
                            "revn": f.revn,
                            "modifiedAt": f.modified_at,
                            "firstPageId": first_page,
                        })
                    })
                    .collect();
                Ok::<_, warp::Rejection>(warp::reply::json(&serde_json::json!({"files": files})))
            }
        });

    // ── POST /__penpot_desktop/set-language → change language and rebuild menus
    let config_for_lang = config.clone();
    let set_language = warp::path!("__penpot_desktop" / "set-language")
        .and(warp::post())
        .and(warp::body::json())
        .and_then(move |body: serde_json::Value| {
            let cfg = config_for_lang.clone();
            async move {
                if let Some(lang) = body.get("language").and_then(|v| v.as_str()) {
                    let mut c = cfg.write().await;
                    c.language = lang.to_string();
                    save_config(&c);
                    drop(c);
                    // Update cached language for menu label updates
                    if let Some(lk) = CURRENT_LANG.get() {
                        if let Ok(mut l) = lk.write() {
                            *l = lang.to_string();
                        }
                    }
                    // Persist AppleLanguages so macOS system menu items use the
                    // correct language on next launch — even without immediate restart.
                    #[cfg(target_os = "macos")]
                    {
                        let apple_lang = desktop_to_apple_locale(lang).to_string();
                        let _ = APP_HANDLE.get().map(|app| {
                            let _ = app.run_on_main_thread(move || {
                                set_apple_languages(&apple_lang);
                            });
                        });
                    }
                    // Rebuild menus with new language
                    if let Some(app) = APP_HANDLE.get() {
                        let mode = focused_window_mode(app);
                        if let Ok((menu, _)) = build_menu(&app, &mode) {
                            let _ = app.set_menu(menu);
                            #[cfg(target_os = "macos")]
                            {
                                app.run_on_main_thread(|| {
                                    register_help_menu();
                                    register_window_menu();
                                })
                                .ok();
                            }
                        }
                        // Reload Penpot webviews so they pick up the new language
                        // via the updated navigator.language override in config.js
                        if desktop_to_penpot_locale(lang).is_some() {
                            for (_label, window) in app.webview_windows() {
                                let is_settings = window
                                    .url()
                                    .map(|u| u.path().contains("__penpot_desktop"))
                                    .unwrap_or(false);
                                if !is_settings {
                                    let _ = window.eval(
                                        "try { localStorage.removeItem('penpot-global:app.util.i18n/locale'); } catch(e) {} location.reload();"
                                    );
                                }
                            }
                        }
                    }
                }
                Ok::<_, warp::Rejection>(warp::reply::json(&serde_json::json!({"ok": true})))
            }
        });

    // ── POST /__penpot_desktop/restart-app → show native confirm dialog with
    // app icon, then save session + restart if confirmed.
    let config_for_restart = config.clone();
    let restart_app = warp::path!("__penpot_desktop" / "restart-app")
        .and(warp::post())
        .and_then(move || {
            let cfg = config_for_restart.clone();
            async move {
                if let Some(app) = APP_HANDLE.get() {
                    let lang = cfg.read().await.language.clone();
                    let app_for_dialog = app.clone();
                    let cfg_for_restart = cfg.clone();

                    // Show the dialog on the main thread (required for native UI)
                    let _ = app.run_on_main_thread(move || {
                        use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
                        let msg = crate::i18n::t(&lang, "settings.restart-prompt");
                        let cancel = crate::i18n::t(&lang, "settings.restart-later");

                        let confirmed = app_for_dialog
                            .dialog()
                            .message(&msg)
                            .title("Penpot Desktop")
                            .buttons(MessageDialogButtons::OkCancelCustom(
                                "OK".into(),
                                cancel,
                            ))
                            .kind(tauri_plugin_dialog::MessageDialogKind::Info)
                            .blocking_show();

                        if confirmed {
                            let app_clone = app_for_dialog.clone();
                            let cfg_clone = cfg_for_restart.clone();
                            std::thread::spawn(move || {
                                crate::save_session_state(&app_clone, &cfg_clone);
                                if let Ok(exe) = std::env::current_exe() {
                                    let _ = std::process::Command::new(exe).spawn();
                                }
                                std::process::exit(0);
                            });
                        }
                    });
                }
                Ok::<_, warp::Rejection>(warp::reply::json(&serde_json::json!({"ok": true})))
            }
        });

    // ── POST /__penpot_desktop/set-view → record per-window mode and,
    // if the posting window is currently focused, swap the menu.
    let set_view = warp::path!("__penpot_desktop" / "set-view")
        .and(warp::post())
        .and(warp::body::json())
        .and_then(move |body: serde_json::Value| async move {
            if let Some(mode) = body.get("mode").and_then(|v| v.as_str()) {
                let label = body
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !label.is_empty() {
                    set_window_mode(&label, mode);
                }
                if let Some(app) = APP_HANDLE.get() {
                    // Only rebuild the menu if the window that posted is the
                    // one currently focused (or if we can't tell — fall back to
                    // updating, so the very first window still gets a menu).
                    let focused_label = app
                        .webview_windows()
                        .into_iter()
                        .find(|(_, w)| w.is_focused().unwrap_or(false))
                        .map(|(l, _)| l);
                    let should_update = match (&focused_label, label.is_empty()) {
                        (Some(f), false) => f == &label,
                        (None, _) => true,
                        (_, true) => true,
                    };
                    if should_update {
                        if let Ok((menu, _help)) = build_menu(&app, mode) {
                            let _ = app.set_menu(menu);
                            #[cfg(target_os = "macos")]
                            {
                                app.run_on_main_thread(|| {
                                    register_help_menu();
                                    register_window_menu();
                                })
                                .ok();
                            }
                            // In workspace mode, disable selection-dependent items initially
                            if mode == "workspace" {
                                update_selection_items(app, 0, &[], &[]);
                            }
                        }
                    }
                }
            }
            Ok::<_, warp::Rejection>(warp::reply::json(&serde_json::json!({"ok": true})))
        });

    // ── POST /__penpot_desktop/window-focused → JS-driven focus notification.
    // macOS native tabs don't always surface NSWindow key changes through Tauri,
    // so each Penpot webview tells us when its document gains focus and we
    // rebuild the menu from the stored mode for that label.
    let window_focused = warp::path!("__penpot_desktop" / "window-focused")
        .and(warp::post())
        .and(warp::body::json())
        .and_then(move |body: serde_json::Value| async move {
            let label = body
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !label.is_empty() {
                if let Some(mode) = get_window_mode(&label) {
                    if let Some(app) = APP_HANDLE.get() {
                        let app_handle = app.clone();
                        let _ = app.run_on_main_thread(move || {
                            if let Ok((menu, _)) = build_menu(&app_handle, &mode) {
                                let _ = app_handle.set_menu(menu);
                                #[cfg(target_os = "macos")]
                                {
                                    register_help_menu();
                                    register_window_menu();
                                }
                                if mode == "workspace" {
                                    update_selection_items(&app_handle, 0, &[], &[]);
                                }
                            }
                        });
                    }
                }
            }
            Ok::<_, warp::Rejection>(warp::reply::json(&serde_json::json!({"ok": true})))
        });

    // ── GET /__penpot_desktop/clipboard → read system clipboard (for paste in input fields)
    let get_clipboard = warp::path!("__penpot_desktop" / "clipboard")
        .and(warp::get())
        .and_then(move || async move {
            let text = APP_HANDLE
                .get()
                .and_then(|app| {
                    use tauri_plugin_clipboard_manager::ClipboardExt;
                    app.clipboard().read_text().ok()
                })
                .unwrap_or_default();
            Ok::<_, warp::Rejection>(warp::reply::json(&serde_json::json!({"text": text})))
        });

    // ── POST /__penpot_desktop/set-selection → enable/disable selection-dependent menu items
    let set_selection = warp::path!("__penpot_desktop" / "set-selection")
        .and(warp::post())
        .and(warp::body::json())
        .and_then(move |body: serde_json::Value| async move {
            let count = body.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
            let types: Vec<String> = body
                .get("types")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let flags: Vec<String> = body
                .get("flags")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if let Some(app) = APP_HANDLE.get() {
                let _ = app.run_on_main_thread(move || {
                    if let Some(app) = APP_HANDLE.get() {
                        update_selection_items(app, count, &types, &flags);
                    }
                });
            }
            Ok::<_, warp::Rejection>(warp::reply::json(&serde_json::json!({"ok": true})))
        });

    // ── GET /__penpot_desktop/translations → return all translations for current language
    let config_for_i18n = config.clone();
    let get_translations = warp::path!("__penpot_desktop" / "translations")
        .and(warp::get())
        .and_then(move || {
            let cfg = config_for_i18n.clone();
            async move {
                let lang = cfg.read().await.language.clone();
                // Build JSON of all s.* keys for the settings page
                let keys = vec![
                    "settings.title",
                    "settings.subtitle",
                    "settings.backend-url",
                    "settings.connect",
                    "settings.how-title",
                    "settings.how-desc",
                    "settings.renderer",
                    "settings.wasm-gpu",
                    "settings.wasm-desc",
                    "settings.classic",
                    "settings.classic-desc",
                    "settings.recent",
                    "settings.language",
                    "settings.connecting",
                    "settings.connected",
                    "settings.error",
                    "settings.enter-url",
                    "settings.conn-failed",
                    "settings.cloud",
                    "settings.local",
                    "settings.dev",
                    "settings.mode",
                    "settings.mode-online",
                    "settings.mode-online-desc",
                    "settings.mode-offline",
                    "settings.mode-offline-desc",
                    "settings.offline-files",
                    "settings.offline-no-files",
                    "settings.offline-open-button",
                    "settings.offline-launch",
                    "settings.offline-import-error",
                    "settings.offline-launching",
                ];
                let mut map = serde_json::Map::new();
                map.insert("lang".into(), serde_json::Value::String(lang.clone()));
                for key in keys {
                    // Return keys with "s." prefix to match data-i18n attributes in HTML
                    let short_key = key.replacen("settings.", "s.", 1);
                    map.insert(short_key, serde_json::Value::String(i18n::t(&lang, key)));
                }
                Ok::<_, warp::Rejection>(warp::reply::json(&map))
            }
        });

    // ── POST /__penpot_desktop/open-tab → open URL in a new native tab,
    // or in the system browser if the URL points to a foreign origin
    // (e.g. plugin help links, GitHub, …).
    let open_tab_port = port;
    let open_tab = warp::path!("__penpot_desktop" / "open-tab")
        .and(warp::post())
        .and(warp::body::json())
        .and_then(move |body: serde_json::Value| {
            let port = open_tab_port;
            async move {
                if let Some(url) = body.get("url").and_then(|v| v.as_str()) {
                    let url = url.to_string();
                    // Decide: external URL → system browser, otherwise → in-app tab.
                    // External = absolute http(s) whose host is not 127.0.0.1/localhost.
                    let is_external = url::Url::parse(&url)
                        .ok()
                        .map(|u| {
                            (u.scheme() == "http" || u.scheme() == "https")
                                && !matches!(u.host_str(), Some("127.0.0.1") | Some("localhost"))
                        })
                        .unwrap_or(false);
                    if let Some(app) = APP_HANDLE.get() {
                        if is_external {
                            use tauri_plugin_opener::OpenerExt;
                            let _ = app.opener().open_url(&url, None::<&str>);
                        } else {
                            let app_for_run = app.clone();
                            let app_for_tab = app.clone();
                            let focused = app
                                .webview_windows()
                                .into_iter()
                                .find(|(_, w)| w.is_focused().unwrap_or(false))
                                .map(|(l, _)| l);
                            let _ = app_for_run.run_on_main_thread(move || {
                                let _ = create_tab_window(
                                    &app_for_tab,
                                    port,
                                    Some(&url),
                                    focused.as_deref(),
                                );
                            });
                        }
                    }
                }
                Ok::<_, warp::Rejection>(warp::reply::json(&serde_json::json!({"ok": true})))
            }
        });

    // ── POST /__penpot_desktop/update-plugins → receive installed plugin list from JS poller
    let update_plugins_ep = warp::path!("__penpot_desktop" / "update-plugins")
        .and(warp::post())
        .and(warp::body::json())
        .and_then(move |body: serde_json::Value| async move {
            if let Some(arr) = body.get("plugins").and_then(|v| v.as_array()) {
                let plugins: Vec<PluginInfo> = arr
                    .iter()
                    .filter_map(|p| {
                        let id = p.get("id")?.as_str()?.to_string();
                        let name = p.get("name")?.as_str()?.to_string();
                        Some(PluginInfo { id, name })
                    })
                    .collect();
                update_plugins(plugins);
            }
            Ok::<_, warp::Rejection>(warp::reply::json(&serde_json::json!({"ok": true})))
        });

    // ── POST /__penpot_desktop/update-tab-url → track tab URL for session restore
    // Uses warp::body::bytes() because sendBeacon sends as text/plain
    let update_tab_url = warp::path!("__penpot_desktop" / "update-tab-url")
        .and(warp::post())
        .and(warp::body::bytes())
        .and_then(move |bytes: bytes::Bytes| async move {
            if let Ok(body) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if let (Some(label), Some(url)) = (
                    body.get("label").and_then(|v| v.as_str()),
                    body.get("url").and_then(|v| v.as_str()),
                ) {
                    track_tab_url(label, url);
                    if let Some(title) = body.get("title").and_then(|v| v.as_str()) {
                        if !title.is_empty() {
                            track_tab_title(label, title);
                        }
                    }
                }
            }
            Ok::<_, warp::Rejection>(warp::reply::json(&serde_json::json!({"ok": true})))
        });

    // ── POST /__penpot_desktop/set-title → update window title
    let set_title = warp::path!("__penpot_desktop" / "set-title")
        .and(warp::post())
        .and(warp::body::json())
        .and_then(move |body: serde_json::Value| async move {
            if let (Some(label), Some(title)) = (
                body.get("label").and_then(|v| v.as_str()),
                body.get("title").and_then(|v| v.as_str()),
            ) {
                if let Some(app) = APP_HANDLE.get() {
                    if let Some(win) = app.get_webview_window(label) {
                        let _ = win.set_title(title);
                    }
                }
            }
            Ok::<_, warp::Rejection>(warp::reply::json(&serde_json::json!({"ok": true})))
        });

    // ── GET/* /__penpot_desktop/cors-proxy?url=... → relay arbitrary HTTPS targets
    // Bypasses browser CORS for cross-origin fetches (e.g. Penpot plugin manifests).
    // The fetch is performed by reqwest in Rust, so no preflight or Origin check happens.
    let cors_proxy = warp::path!("__penpot_desktop" / "cors-proxy")
        .and(warp::method())
        .and(warp::query::<HashMap<String, String>>())
        .and(warp::header::headers_cloned())
        .and(warp::body::bytes())
        .and_then(
            |method: warp::http::Method,
             query: HashMap<String, String>,
             headers: warp::http::HeaderMap,
             body: bytes::Bytes| async move {
                let Some(target) = query.get("url").cloned() else {
                    return Ok::<_, warp::Rejection>(
                        warp::http::Response::builder()
                            .status(400)
                            .body(bytes::Bytes::from("missing url"))
                            .unwrap(),
                    );
                };
                if !target.starts_with("http://") && !target.starts_with("https://") {
                    return Ok(warp::http::Response::builder()
                        .status(400)
                        .body(bytes::Bytes::from("invalid scheme"))
                        .unwrap());
                }
                match proxy_request_inner(&target, method, headers, body, false).await {
                    Ok(resp) => Ok(resp),
                    Err(e) => Ok(warp::http::Response::builder()
                        .status(502)
                        .body(bytes::Bytes::from(format!("cors-proxy error: {e}")))
                        .unwrap()),
                }
            },
        );

    // ── Offline backend: /api/rpc/command/upload-file-media-object
    // Multipart upload — must be matched BEFORE the generic JSON RPC
    // route below, because that route consumes the body as bytes and
    // tries to parse it as JSON.
    let backend_store_for_upload = backend_store.clone();
    let config_for_upload = config.clone();
    let upload_media = warp::path!(
        "api" / "rpc" / "command" / "upload-file-media-object"
    )
    .and(warp::post())
    .and(warp::multipart::form().max_length(64 * 1024 * 1024))
    .and_then(move |form: warp::multipart::FormData| {
        let store = backend_store_for_upload.clone();
        let cfg = config_for_upload.clone();
        async move { handle_upload_media(store, cfg, form).await }
    });

    // ── Offline backend: /api/rpc/command/<name> and /api/rpc/query/<name>
    // Dispatched in-process. Returns plain JSON; the frontend consumes that
    // because we set `enable-transit-readable-response` in penpotFlags.
    let backend_store_for_rpc = backend_store.clone();
    let config_for_rpc = config.clone();
    let rpc_command = warp::path!("api" / "rpc" / "command" / String)
        .and(warp::post())
        .and(warp::body::bytes())
        .and_then(move |name: String, body: bytes::Bytes| {
            let store = backend_store_for_rpc.clone();
            let cfg = config_for_rpc.clone();
            async move { handle_rpc(store, cfg, backend_rpc::RpcKind::Command, name, body).await }
        });

    let backend_store_for_q = backend_store.clone();
    let config_for_q = config.clone();
    let rpc_query = warp::path!("api" / "rpc" / "query" / String)
        .and(warp::post())
        .and(warp::body::bytes())
        .and_then(move |name: String, body: bytes::Bytes| {
            let store = backend_store_for_q.clone();
            let cfg = config_for_q.clone();
            async move { handle_rpc(store, cfg, backend_rpc::RpcKind::Query, name, body).await }
        });

    // ── Offline backend: /assets/by-id/<uuid> serves uploaded media.
    // Penpot's frontend rewrites image references to {public_uri}/assets/by-id/<uuid>
    // for both the SVG renderer and the WASM canvas renderer.
    let backend_store_for_assets = backend_store.clone();
    let config_for_assets_offline = config.clone();
    let assets_by_id = warp::path!("assets" / "by-id" / String)
        .and(warp::get())
        .and_then(move |id_str: String| {
            let store = backend_store_for_assets.clone();
            let cfg = config_for_assets_offline.clone();
            async move { handle_asset_by_id(store, cfg, id_str).await }
        });

    let backend_store_for_assets2 = backend_store.clone();
    let config_for_assets_offline2 = config.clone();
    let assets_by_file_media = warp::path!("assets" / "by-file-media-id" / String)
        .and(warp::get())
        .and_then(move |id_str: String| {
            let store = backend_store_for_assets2.clone();
            let cfg = config_for_assets_offline2.clone();
            async move { handle_asset_by_id(store, cfg, id_str).await }
        });

    // ── Offline backend: WebSocket noop endpoint.
    // The frontend opens `/ws/notifications` on boot; without a 101 Upgrade it
    // retries in a tight loop. We accept the upgrade and drop every frame.
    let config_for_ws_noop = config.clone();
    let ws_noop = warp::path!("ws" / ..)
        .and(warp::ws())
        .and_then(move |ws: warp::ws::Ws| {
            let cfg = config_for_ws_noop.clone();
            async move {
                let offline = matches!(cfg.read().await.mode, AppMode::Offline);
                if offline {
                    Ok::<_, warp::Rejection>(ws.on_upgrade(noop_ws_handler))
                } else {
                    Err(warp::reject::not_found())
                }
            }
        });

    // ── Proxy /api/* → backend
    let api_proxy = warp::path("api")
        .and(warp::path::tail())
        .and(warp::query::raw().or(warp::any().map(String::new)).unify())
        .and(warp::method())
        .and(warp::header::headers_cloned())
        .and(warp::body::bytes())
        .and_then(
            move |tail: warp::path::Tail,
                  query: String,
                  method: warp::http::Method,
                  headers: warp::http::HeaderMap,
                  body: bytes::Bytes| {
                let cfg = config_for_api.clone();
                let et = error_tracker_api.clone();
                async move {
                    let c = cfg.read().await;
                    if c.backend_url.is_empty() {
                        return Ok::<_, warp::Rejection>(
                            warp::http::Response::builder()
                                .status(503)
                                .body(bytes::Bytes::from("Backend URL not configured"))
                                .unwrap(),
                        );
                    }
                    let target = if query.is_empty() {
                        format!(
                            "{}/api/{}",
                            c.backend_url.trim_end_matches('/'),
                            tail.as_str()
                        )
                    } else {
                        format!(
                            "{}/api/{}?{}",
                            c.backend_url.trim_end_matches('/'),
                            tail.as_str(),
                            query
                        )
                    };
                    drop(c);

                    match proxy_request(&target, method.clone(), headers, body).await {
                        Ok(resp) => {
                            let status = resp.status();
                            if status.as_u16() >= 400 {
                                eprintln!("[proxy] {method} {target} → {status}");
                            }
                            Ok(resp)
                        }
                        Err(e) => {
                            let msg = format!("[proxy] error: {method} {target} → {e}");
                            et.lock().await.log("api", &msg);
                            Ok(warp::http::Response::builder()
                                .status(502)
                                .body(bytes::Bytes::from(format!("Proxy error: {e}")))
                                .unwrap())
                        }
                    }
                }
            },
        );

    // ── Proxy /ws/* → backend (WebSocket upgrade)
    let ws_proxy = warp::path("ws")
        .and(warp::path::tail())
        .and(warp::query::raw().or(warp::any().map(String::new)).unify())
        .and(warp::header::headers_cloned())
        .and(warp::ws())
        .and_then(
            move |tail: warp::path::Tail,
                  query: String,
                  headers: warp::http::HeaderMap,
                  ws: warp::ws::Ws| {
                let cfg = config_for_ws.clone();
                let tail_str = tail.as_str().to_string();
                async move {
                    let c = cfg.read().await;
                    let backend = c.backend_url.clone();
                    drop(c);

                    if backend.is_empty() {
                        return Err(warp::reject::not_found());
                    }

                    let ws_url = backend
                        .replace("https://", "wss://")
                        .replace("http://", "ws://");
                    let target = if query.is_empty() {
                        format!("{}/ws/{}", ws_url.trim_end_matches('/'), tail_str)
                    } else {
                        format!("{}/ws/{}?{}", ws_url.trim_end_matches('/'), tail_str, query)
                    };

                    // Extract cookie header for backend auth
                    let cookie = headers
                        .get("cookie")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();

                    Ok::<_, warp::Rejection>(ws.on_upgrade(move |websocket| {
                        ws_proxy_handler(websocket, target, cookie, backend)
                    }))
                }
            },
        );

    // ── Serve settings page at /__penpot_desktop/
    let settings_html_template = include_str!("../../src/settings.html");
    let settings_html = settings_html_template.replace("{{PENPOT_VERSION}}", &penpot_version);
    let settings_page = warp::path!("__penpot_desktop")
        .and(warp::get())
        .map(move || warp::reply::html(settings_html.clone()));

    // ── Serve settings page assets (icon, etc.)
    let settings_app_icon = warp::path!("__penpot_desktop" / "app-icon.png")
        .and(warp::get())
        .map(|| {
            warp::http::Response::builder()
                .header("Content-Type", "image/png")
                .body(include_bytes!("../../src/app-icon.png").as_ref())
                .unwrap()
        });

    // ── Serve runtime config JS files
    // In offline mode this overrides Penpot's built-in config.js with our
    // own penpotFlags + penpotPublicURI so the frontend boots against our
    // embedded backend. In online mode it stays an empty no-op.
    let config_for_runtime_cfg = config.clone();
    let config_js = warp::path!("js" / "config.js")
        .and(warp::get())
        .and_then(move || {
            let cfg = config_for_runtime_cfg.clone();
            async move {
                let snapshot = cfg.read().await.clone();
                let body = if matches!(snapshot.mode, AppMode::Offline) {
                    let origin = format!("http://127.0.0.1:{}", snapshot.proxy_port);
                    backend_flags::config_js(&origin, snapshot.renderer == "wasm")
                } else {
                    "// Penpot Desktop: no server-side config needed\n".to_string()
                };
                Ok::<_, warp::Rejection>(
                    warp::http::Response::builder()
                        .header("Content-Type", "application/javascript")
                        .header("Cache-Control", "no-cache")
                        .body(body)
                        .unwrap(),
                )
            }
        });

    let config_for_config_js = config.clone();
    let desktop_config_js = warp::path!("__penpot_desktop_config.js")
        .and(warp::get())
        .and_then(move || {
            let cfg = config_for_config_js.clone();
            async move {
                let lang = cfg.read().await.language.clone();
                let penpot_locale = desktop_to_penpot_locale(&lang).unwrap_or("en");
                // Convert underscore locale to hyphen for navigator.language (e.g. "ja_jp" → "ja-JP")
                let nav_lang = penpot_locale.replace('_', "-");

                // Dynamic locale override block, prepended to the static config JS
                let locale_js = format!(
                    r#"// Penpot Desktop: sync desktop language to Penpot
(function() {{
  try {{
    var _dl = '{}';
    Object.defineProperty(navigator, 'language', {{ get: function() {{ return _dl; }} }});
    Object.defineProperty(navigator, 'languages', {{ get: function() {{ return [_dl]; }} }});
  }} catch(e) {{}}
}})();
"#,
                    nav_lang
                );

                let backend_url = cfg.read().await.backend_url.clone();
                let backend_js = format!(
                    "window.__penpotBackendOrigin = '{}';\n",
                    backend_url.trim_end_matches('/')
                );

                let body = locale_js + &backend_js + DESKTOP_CONFIG_JS;

                Ok::<_, warp::Rejection>(
                    warp::http::Response::builder()
                        .header("Content-Type", "application/javascript")
                        .header("Cache-Control", "no-cache")
                        .body(body)
                        .unwrap(),
                )
            }
        });

    // ── Serve static Penpot frontend files
    let static_dir = penpot_dir.clone();
    let static_files =
        warp::any()
            .and(warp::path::full())
            .and_then(move |path: warp::path::FullPath| {
                let dir = static_dir.clone();
                async move {
                    let req_path = path.as_str().trim_start_matches('/');
                    let file_path = if req_path.is_empty() || req_path == "/" {
                        dir.join("index.html")
                    } else {
                        dir.join(req_path)
                    };

                    // Try exact path, then with .html, then index.html in dir
                    let resolved = if file_path.is_file() {
                        file_path
                    } else if file_path.with_extension("html").is_file() {
                        file_path.with_extension("html")
                    } else if file_path.join("index.html").is_file() {
                        file_path.join("index.html")
                    } else if std::path::Path::new(req_path).extension().is_some() {
                        // File with extension not found → 404 (don't serve index.html for missing assets)
                        return Err(warp::reject::not_found());
                    } else {
                        // SPA fallback: serve index.html for client-side routing
                        dir.join("index.html")
                    };

                    if resolved.is_file() {
                        let content = fs::read(&resolved).map_err(|_| warp::reject::not_found())?;
                        let mime = mime_guess::from_path(&resolved).first_or_octet_stream();
                        Ok::<_, warp::Rejection>(
                            warp::http::Response::builder()
                                .header("Content-Type", mime.as_ref())
                                .header("Cache-Control", "no-cache")
                                .body(bytes::Bytes::from(content))
                                .unwrap(),
                        )
                    } else {
                        Err(warp::reject::not_found())
                    }
                }
            });

    // ── Proxy /assets/* → backend
    let assets_proxy = warp::path("assets")
        .and(warp::path::tail())
        .and(warp::query::raw().or(warp::any().map(String::new)).unify())
        .and(warp::method())
        .and(warp::header::headers_cloned())
        .and(warp::body::bytes())
        .and_then(
            move |tail: warp::path::Tail,
                  query: String,
                  method: warp::http::Method,
                  headers: warp::http::HeaderMap,
                  body: bytes::Bytes| {
                let cfg = config_for_assets.clone();
                let et = error_tracker_assets.clone();
                async move {
                    let c = cfg.read().await;
                    if c.backend_url.is_empty() {
                        return Ok::<_, warp::Rejection>(
                            warp::http::Response::builder()
                                .status(503)
                                .body(bytes::Bytes::from("Backend URL not configured"))
                                .unwrap(),
                        );
                    }
                    let target = if query.is_empty() {
                        format!(
                            "{}/assets/{}",
                            c.backend_url.trim_end_matches('/'),
                            tail.as_str()
                        )
                    } else {
                        format!(
                            "{}/assets/{}?{}",
                            c.backend_url.trim_end_matches('/'),
                            tail.as_str(),
                            query
                        )
                    };
                    drop(c);

                    match proxy_request(&target, method, headers, body).await {
                        Ok(resp) => Ok(resp),
                        Err(e) => {
                            let msg = format!("[proxy] assets error: {e}");
                            et.lock().await.log("assets", &msg);
                            Ok(warp::http::Response::builder()
                                .status(502)
                                .body(bytes::Bytes::from(format!("Proxy error: {e}")))
                                .unwrap())
                        }
                    }
                }
            },
        );

    // ── Proxy /internal/* → backend (e.g. /internal/gfonts/css, /internal/gfonts/font/*)
    // Penpot's frontend rewrites Google Fonts URLs to {public_uri}/internal/gfonts/...
    // for both the SVG renderer (@font-face CSS) and the WASM canvas renderer (font binaries).
    // Without this proxy route, /internal/* falls through to static_files and 404s, so
    // text never picks up the selected font in the desktop app.
    let internal_proxy = warp::path("internal")
        .and(warp::path::tail())
        .and(warp::query::raw().or(warp::any().map(String::new)).unify())
        .and(warp::method())
        .and(warp::header::headers_cloned())
        .and(warp::body::bytes())
        .and_then(
            move |tail: warp::path::Tail,
                  query: String,
                  method: warp::http::Method,
                  headers: warp::http::HeaderMap,
                  body: bytes::Bytes| {
                let cfg = config_for_internal.clone();
                let et = error_tracker_internal.clone();
                async move {
                    let c = cfg.read().await;
                    if c.backend_url.is_empty() {
                        return Ok::<_, warp::Rejection>(
                            warp::http::Response::builder()
                                .status(503)
                                .body(bytes::Bytes::from("Backend URL not configured"))
                                .unwrap(),
                        );
                    }
                    let target = if query.is_empty() {
                        format!(
                            "{}/internal/{}",
                            c.backend_url.trim_end_matches('/'),
                            tail.as_str()
                        )
                    } else {
                        format!(
                            "{}/internal/{}?{}",
                            c.backend_url.trim_end_matches('/'),
                            tail.as_str(),
                            query
                        )
                    };
                    drop(c);

                    match proxy_request(&target, method, headers, body).await {
                        Ok(resp) => Ok(resp),
                        Err(e) => {
                            let msg = format!("[proxy] internal error: {e}");
                            et.lock().await.log("internal", &msg);
                            Ok(warp::http::Response::builder()
                                .status(502)
                                .body(bytes::Bytes::from(format!("Proxy error: {e}")))
                                .unwrap())
                        }
                    }
                }
            },
        );

    // ── Combine all routes (order matters!) ──────────────────
    // Offline backend RPC/WS routes come before the forwarding routes so they
    // intercept `/api/rpc/...` and `/ws/...` when running in offline mode.
    let routes = get_config
        .or(set_backend)
        .or(set_mode)
        .or(offline_files)
        .or(set_view)
        .or(window_focused)
        .or(set_selection)
        .or(get_clipboard)
        .or(set_language)
        .or(restart_app)
        .or(get_translations)
        .or(set_title)
        .or(open_tab)
        .or(update_plugins_ep)
        .or(update_tab_url)
        .or(cors_proxy)
        .or(settings_page)
        .or(settings_app_icon)
        .or(config_js)
        .or(desktop_config_js)
        .or(upload_media)
        .or(rpc_command)
        .or(rpc_query)
        .or(assets_by_id)
        .or(assets_by_file_media)
        .or(ws_noop)
        .or(api_proxy)
        .or(assets_proxy)
        .or(internal_proxy)
        .or(ws_proxy)
        .or(static_files);

    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    println!("🚀 Penpot Desktop Proxy auf http://{addr}");
    println!("   Settings: http://{addr}/__penpot_desktop");

    warp::serve(routes).run(addr).await;
}

// ── Offline backend handlers ─────────────────────────────────

/// Dispatch an `/api/rpc/*` request to the embedded backend if offline mode
/// is active. In online mode it falls through with `not_found` so the proxy
/// chain forwards to the configured backend.
async fn handle_rpc(
    store: backend_store::Store,
    config: SharedConfig,
    kind: backend_rpc::RpcKind,
    name: String,
    body: bytes::Bytes,
) -> Result<warp::http::Response<bytes::Bytes>, warp::Rejection> {
    let snapshot = config.read().await.clone();
    if !matches!(snapshot.mode, AppMode::Offline) {
        return Err(warp::reject::not_found());
    }
    let parsed: serde_json::Value = if body.is_empty() {
        serde_json::Value::Null
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                return Ok(warp::http::Response::builder()
                    .status(400)
                    .header("content-type", "application/json")
                    .body(bytes::Bytes::from(format!(
                        "{{\"error\":\"invalid JSON: {e}\"}}"
                    )))
                    .unwrap());
            }
        }
    };
    let backend = backend_rpc::Backend::new(store, snapshot.language.clone());
    let resp = backend.dispatch(kind, &name, &parsed);
    let (status, body_bytes) = match resp {
        backend_rpc::RpcResponse::Json(v) => (
            200u16,
            bytes::Bytes::from(serde_json::to_vec(&v).unwrap_or_default()),
        ),
        backend_rpc::RpcResponse::Error { status, message } => (
            status,
            bytes::Bytes::from(
                serde_json::to_vec(&serde_json::json!({
                    "type": "validation",
                    "code": "rpc-error",
                    "hint": message,
                }))
                .unwrap(),
            ),
        ),
    };
    Ok(warp::http::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("cache-control", "no-store")
        .body(body_bytes)
        .unwrap())
}

/// Accept a WebSocket upgrade and quietly drop everything. Penpot's
/// notification client opens this connection on every workspace boot; in
/// offline mode there are no real-time events to deliver, so we just keep
/// the channel open until the client closes.
async fn noop_ws_handler(ws: warp::ws::WebSocket) {
    use futures::StreamExt;
    let (_tx, mut rx) = ws.split();
    while let Some(msg) = rx.next().await {
        if msg.is_err() {
            break;
        }
    }
}

/// Multipart-upload handler for `/api/rpc/command/upload-file-media-object`.
/// Falls through to the online forwarder when offline mode is disabled.
///
/// Penpot's frontend posts:
///   - `file-id`     — UUID of the file we're attaching the asset to
///   - `name`        — display name (used as fallback alt text)
///   - `is-local`    — "true"/"false" (we ignore; everything is local now)
///   - `content`     — the actual file bytes, with `content-type`
///
/// Response is plain JSON with the storage-object descriptor.
async fn handle_upload_media(
    store: backend_store::Store,
    config: SharedConfig,
    mut form: warp::multipart::FormData,
) -> Result<warp::http::Response<bytes::Bytes>, warp::Rejection> {
    use futures::TryStreamExt;

    let snapshot = config.read().await.clone();
    if !matches!(snapshot.mode, AppMode::Offline) {
        return Err(warp::reject::not_found());
    }

    let mut file_id: Option<uuid::Uuid> = None;
    let mut name: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;
    let mut mime_type: Option<String> = None;

    while let Some(part) = form.try_next().await.map_err(|_| warp::reject::reject())? {
        let part_name = part.name().to_string();
        let part_mime = part.content_type().map(str::to_string);
        let buf = collect_multipart_part(part).await;
        match part_name.as_str() {
            "file-id" => {
                file_id = std::str::from_utf8(&buf)
                    .ok()
                    .and_then(|s| uuid::Uuid::parse_str(s.trim()).ok());
            }
            "name" => {
                name = String::from_utf8(buf).ok();
            }
            "content" => {
                mime_type = part_mime;
                bytes = Some(buf);
            }
            _ => {}
        }
    }

    let Some(bytes) = bytes else {
        return Ok(json_response(
            400,
            &serde_json::json!({"hint": "missing :content part"}),
        ));
    };
    let mime_type = mime_type.unwrap_or_else(|| "application/octet-stream".into());
    let name = name.unwrap_or_else(|| "asset".into());

    let stored = match store.store_media(&bytes, &mime_type, None) {
        Ok(s) => s,
        Err(e) => {
            return Ok(json_response(
                500,
                &serde_json::json!({"hint": format!("store_media failed: {e}")}),
            ));
        }
    };
    let asset_id = uuid::Uuid::new_v4();
    if let Some(file_id) = file_id {
        if let Err(e) = store.link_file_media(file_id, stored.id, asset_id, &name) {
            eprintln!("[upload-media] link_file_media failed: {e}");
        }
    }
    let payload = serde_json::json!({
        "id": stored.id,
        "fileId": file_id,
        "name": name,
        "width": stored.width.unwrap_or(0),
        "height": stored.height.unwrap_or(0),
        "mtype": stored.mime_type,
        "isLocal": true,
    });
    Ok(json_response(200, &payload))
}

async fn collect_multipart_part(mut part: warp::multipart::Part) -> Vec<u8> {
    use bytes::Buf;
    use futures::StreamExt;
    let mut out = Vec::new();
    while let Some(chunk) = part.data().await {
        if let Ok(buf) = chunk {
            out.extend_from_slice(buf.chunk());
        }
    }
    out
}

/// Serve a stored media blob by storage-object id.
async fn handle_asset_by_id(
    store: backend_store::Store,
    config: SharedConfig,
    id_str: String,
) -> Result<warp::http::Response<bytes::Bytes>, warp::Rejection> {
    let snapshot = config.read().await.clone();
    if !matches!(snapshot.mode, AppMode::Offline) {
        return Err(warp::reject::not_found());
    }
    let id = match uuid::Uuid::parse_str(&id_str) {
        Ok(u) => u,
        Err(_) => return Err(warp::reject::not_found()),
    };
    let bytes = match store.read_media(id) {
        Some(b) => b,
        None => return Err(warp::reject::not_found()),
    };
    let metadata = store.media_metadata(id);
    let mime = metadata
        .as_ref()
        .map(|m| m.mime_type.clone())
        .unwrap_or_else(|| "application/octet-stream".into());
    Ok(warp::http::Response::builder()
        .status(200)
        .header("content-type", mime)
        .header("cache-control", "public, max-age=31536000, immutable")
        .header("Cross-Origin-Resource-Policy", "same-origin")
        .body(bytes::Bytes::from(bytes))
        .unwrap())
}

fn json_response(status: u16, value: &serde_json::Value) -> warp::http::Response<bytes::Bytes> {
    warp::http::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("cache-control", "no-store")
        .body(bytes::Bytes::from(
            serde_json::to_vec(value).unwrap_or_default(),
        ))
        .unwrap()
}

// ── HTTP Proxy Logic ─────────────────────────────────────────

async fn proxy_request(
    target: &str,
    method: warp::http::Method,
    headers: warp::http::HeaderMap,
    body: bytes::Bytes,
) -> Result<warp::http::Response<bytes::Bytes>, String> {
    proxy_request_inner(target, method, headers, body, true).await
}

/// Remove every `<meta http-equiv="Content-Security-Policy" ...>` tag from an HTML
/// string. Used in cors-proxy mode so the body-level CSP doesn't block our
/// injected inline shim script. Case-insensitive, attribute-order-tolerant.
fn strip_csp_meta_tags(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let needle = "content-security-policy";
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    while let Some(meta_rel) = lower[cursor..].find("<meta") {
        let meta_start = cursor + meta_rel;
        let after_meta = meta_start + "<meta".len();
        let Some(close_rel) = lower[after_meta..].find('>') else {
            break;
        };
        let tag_end = after_meta + close_rel + 1;
        let tag_lower = &lower[meta_start..tag_end];
        // Only drop tags whose http-equiv targets CSP
        if tag_lower.contains("http-equiv") && tag_lower.contains(needle) {
            out.push_str(&html[cursor..meta_start]);
            cursor = tag_end;
        } else {
            out.push_str(&html[cursor..tag_end]);
            cursor = tag_end;
        }
    }
    out.push_str(&html[cursor..]);
    out
}

async fn proxy_request_inner(
    target: &str,
    method: warp::http::Method,
    headers: warp::http::HeaderMap,
    body: bytes::Bytes,
    rewrite_body: bool,
) -> Result<warp::http::Response<bytes::Bytes>, String> {
    // Extract backend origin from the target URL for header rewriting
    let backend_origin = url::Url::parse(target)
        .ok()
        .map(|u| format!("{}://{}", u.scheme(), u.host_str().unwrap_or("")))
        .unwrap_or_default();

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| e.to_string())?;

    let mut req = client.request(
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap(),
        target,
    );

    // Forward relevant headers (convert via strings to bridge http 0.2 → 1.x)
    for (key, value) in headers.iter() {
        let name = key.as_str().to_lowercase();
        if name == "host" || name == "connection" || name == "upgrade" || name == "accept-encoding"
        {
            continue;
        }
        // Rewrite Referer and Origin to match backend (avoids hotlink protection / CORS)
        if name == "referer" || name == "origin" {
            if !backend_origin.is_empty() {
                req = req.header(key.as_str(), &backend_origin);
                continue;
            }
        }
        req = req.header(key.as_str(), value.as_bytes());
    }

    if !body.is_empty() {
        req = req.body(body);
    }

    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let resp_headers = resp.headers().clone();
    let resp_body = resp.bytes().await.map_err(|e| e.to_string())?;

    let mut builder = warp::http::Response::builder().status(status);
    for (key, value) in resp_headers.iter() {
        let name = key.as_str().to_lowercase();
        if name == "transfer-encoding"
            || name == "connection"
            || name == "content-encoding"
            || name == "content-length"
            // Strip framing headers — Penpot Desktop never iframes Penpot itself,
            // and cors-proxy responses are loaded inside iframes that would
            // otherwise be blocked by upstream X-Frame-Options / frame-ancestors.
            || name == "x-frame-options"
        {
            continue;
        }
        if name == "content-security-policy" || name == "content-security-policy-report-only" {
            // In cors-proxy mode (rewrite_body=false), drop CSP entirely — we
            // need to inject inline scripts (the iframe shim) and the iframe
            // is already sandboxed by the parent's iframe element. Otherwise
            // (Penpot api/assets mode), only drop frame-* directives so the
            // response can still be iframed safely.
            if !rewrite_body {
                continue;
            }
            if let Ok(csp) = value.to_str() {
                let cleaned: String = csp
                    .split(';')
                    .map(|d| d.trim())
                    .filter(|d| {
                        let lower = d.to_lowercase();
                        !lower.starts_with("frame-ancestors") && !lower.starts_with("frame-src")
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                if !cleaned.is_empty() {
                    builder = builder.header(key.as_str(), cleaned);
                }
            }
            continue;
        }
        if name == "set-cookie" {
            // Rewrite Set-Cookie for localhost: strip Domain, Secure, and SameSite=None
            if let Ok(cookie_str) = value.to_str() {
                let rewritten = cookie_str
                    .split(';')
                    .map(|part| part.trim())
                    .filter(|part| {
                        let lower = part.to_lowercase();
                        !lower.starts_with("domain=")
                            && lower != "secure"
                            && !lower.starts_with("samesite=")
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                let with_samesite = format!("{}; SameSite=Lax", rewritten);
                builder = builder.header("set-cookie", with_samesite);
            }
            continue;
        }
        builder = builder.header(key.as_str(), value.as_bytes());
    }

    // Rewrite backend URLs in text responses so the browser uses our proxy
    let content_type = resp_headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let is_text = content_type.contains("json")
        || content_type.contains("transit")
        || content_type.contains("text");
    let is_html = content_type.contains("html");

    let final_body = if rewrite_body && is_text && !backend_origin.is_empty() {
        let body_str = String::from_utf8_lossy(&resp_body);
        if body_str.contains(&backend_origin) {
            let rewritten = body_str.replace(&backend_origin, "http://127.0.0.1:7080");
            bytes::Bytes::from(rewritten)
        } else {
            resp_body
        }
    } else if !rewrite_body && is_html {
        // cors-proxy mode + HTML: inject <base href> so relative URLs resolve to
        // the original origin, and inject the cross-origin fetch shim so plugin
        // code that uses fetch() also funnels through the proxy.
        let raw = String::from_utf8_lossy(&resp_body).into_owned();
        // Strip <meta http-equiv="Content-Security-Policy" ...> tags so the
        // body-level CSP doesn't block our injected inline shim script.
        let body_str = strip_csp_meta_tags(&raw);
        let base_href = url::Url::parse(target)
            .ok()
            .and_then(|mut u| {
                u.set_query(None);
                u.set_fragment(None);
                // Strip the file portion: keep everything up to the last "/"
                if let Ok(mut segs) = u.path_segments_mut() {
                    segs.pop();
                    segs.push("");
                }
                Some(u.to_string())
            })
            .unwrap_or_default();
        let injection = format!(
            "<base href=\"{}\"><script>{}</script>",
            base_href.replace('"', "&quot;"),
            IFRAME_SHIM_JS
        );
        let injected = if let Some(idx) = body_str.to_lowercase().find("<head>") {
            let insert_at = idx + "<head>".len();
            let mut s = String::with_capacity(body_str.len() + injection.len());
            s.push_str(&body_str[..insert_at]);
            s.push_str(&injection);
            s.push_str(&body_str[insert_at..]);
            s
        } else if let Some(idx) = body_str.to_lowercase().find("<head") {
            // <head> with attributes — find the closing >
            if let Some(close) = body_str[idx..].find('>') {
                let insert_at = idx + close + 1;
                let mut s = String::with_capacity(body_str.len() + injection.len());
                s.push_str(&body_str[..insert_at]);
                s.push_str(&injection);
                s.push_str(&body_str[insert_at..]);
                s
            } else {
                injection + &body_str
            }
        } else {
            // No <head> at all — prepend
            injection + &body_str
        };
        bytes::Bytes::from(injected)
    } else {
        resp_body
    };

    builder.body(final_body).map_err(|e| e.to_string())
}

// ── WebSocket Proxy Logic ────────────────────────────────────

async fn ws_proxy_handler(
    client_ws: warp::ws::WebSocket,
    target_url: String,
    cookie: String,
    backend_url: String,
) {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message as TMsg;

    let mut request = target_url.into_client_request().unwrap();
    if !cookie.is_empty() {
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
    }
    // Set Origin to match the backend (required by some servers)
    request
        .headers_mut()
        .insert("origin", backend_url.parse().unwrap());

    let ws_connect = tokio_tungstenite::connect_async(request).await;
    let (backend_ws, _) = match ws_connect {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("[ws-proxy] connect error: {e}");
            return;
        }
    };

    let (mut client_tx, mut client_rx) = client_ws.split();
    let (mut backend_tx, mut backend_rx) = backend_ws.split();

    // Client → Backend
    let c2b = tokio::spawn(async move {
        while let Some(Ok(msg)) = client_rx.next().await {
            let tmsg = if msg.is_text() {
                TMsg::Text(msg.to_str().unwrap_or_default().into())
            } else if msg.is_binary() {
                TMsg::Binary(msg.into_bytes().into())
            } else if msg.is_ping() {
                TMsg::Ping(msg.into_bytes().into())
            } else if msg.is_close() {
                break;
            } else {
                continue;
            };
            if backend_tx.send(tmsg).await.is_err() {
                break;
            }
        }
    });

    // Backend → Client
    let b2c = tokio::spawn(async move {
        while let Some(Ok(msg)) = backend_rx.next().await {
            let wmsg = match msg {
                TMsg::Text(t) => warp::ws::Message::text(t.to_string()),
                TMsg::Binary(b) => warp::ws::Message::binary(b.to_vec()),
                TMsg::Ping(p) => warp::ws::Message::ping(p.to_vec()),
                TMsg::Pong(p) => warp::ws::Message::pong(p.to_vec()),
                TMsg::Close(_) => break,
                _ => continue,
            };
            if client_tx.send(wmsg).await.is_err() {
                break;
            }
        }
    });

    tokio::select! {
        _ = c2b => {},
        _ = b2c => {},
    }
}
