#![cfg_attr(windows, windows_subsystem = "windows")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

mod backend;
mod config;
mod i18n;
mod proxy;

use config::{load_config, save_config, SharedConfig};
use tauri::Manager;
use tokio::sync::RwLock;

use proxy::start_proxy_with;

use backend::store::Store as BackendStore;

mod state;
#[cfg(target_os = "macos")]
use state::get_all_tab_groups;
use state::{
    archive_closed_tab, forget_window_mode, get_window_mode, pop_closed_tab, take_closed_tab_at,
    untrack_tab, APP_HANDLE, CLOSED_TABS, CURRENT_LANG, PLUGINS, TAB_TITLES, TAB_URLS,
    WINDOW_MODES,
};

mod windows;
use windows::{
    create_standalone_window, create_tab_window, safari_user_agent, FILE_MENU_HELPER,
    PLUGIN_LAUNCHER, PLUGIN_POLLER, WINDOW_OPEN_OVERRIDE,
};

mod menu;
use menu::{build_menu, update_selection_items};
#[cfg(target_os = "macos")]
use menu::{register_help_menu, register_window_menu};

mod commands;
use commands::{
    delete_offline_file, export_penpot_file, get_proxy_url, import_penpot_file, list_offline_files,
    open_penpot_file, save_download, save_penpot_file, switch_mode,
};

fn normalize_shortcut_for_platform(shortcut: &str, is_macos: bool) -> String {
    if is_macos {
        return shortcut.to_string();
    }
    shortcut
        .split('+')
        .map(|part| if part == "meta" { "ctrl" } else { part })
        .collect::<Vec<_>>()
        .join("+")
}

fn platform_shortcut(shortcut: &str) -> String {
    normalize_shortcut_for_platform(shortcut, cfg!(target_os = "macos"))
}

// ── Main ─────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let config = load_config();
    let shared_config: SharedConfig = Arc::new(RwLock::new(config.clone()));

    // Shared backend store for offline mode. Lives for the entire process
    // and is handed to both the proxy (for /api/rpc/*) and the Tauri
    // command handlers (for file open/save). Persisted to
    // `<app_data>/workspace.sqlite` so edits survive restarts; falls back
    // to RAM-only if the DB can't be opened (sandboxed CI, broken
    // permissions, etc.) — that path keeps the desktop usable, just
    // ephemerally.
    let backend_store = match dirs::data_dir()
        .map(|d| d.join("penpot-desktop").join("workspace.sqlite"))
    {
        Some(db_path) => match BackendStore::open_sqlite(&db_path) {
            Ok(store) => {
                println!("💾 Offline store: {}", db_path.display());
                store
            }
            Err(e) => {
                eprintln!(
                    "[backend] failed to open {}: {e} — falling back to in-memory store",
                    db_path.display()
                );
                BackendStore::in_memory()
            }
        },
        None => {
            eprintln!("[backend] no data dir — falling back to in-memory store");
            BackendStore::in_memory()
        }
    };

    let proxy_config = shared_config.clone();
    let port = config.proxy_port;

    // Determine Penpot frontend dir
    // Priority: bundled resources (release) → dev mode fallback
    let penpot_dir = {
        let exe = std::env::current_exe().ok();

        // Tauri bundles `"resources": ["../src/penpot/**/*"]` preserving the
        // relative path structure. `../` becomes `_up_/` in the bundle:
        //   macOS:   .app/Contents/Resources/_up_/src/penpot/
        //   Linux:   usr/lib/<name>/_up_/src/penpot/ (deb) or alongside exe (AppImage)
        //   Windows: exe-dir/_up_/src/penpot/
        let candidates: Vec<PathBuf> = exe.iter().flat_map(|e| {
            let parent = e.parent().unwrap();
            vec![
                // macOS .app bundle
                parent.join("../Resources/_up_/src/penpot"),
                // Linux deb
                parent.join("../lib/penpot-desktop/_up_/src/penpot"),
                // Linux AppImage / Windows
                parent.join("_up_/src/penpot"),
            ]
        }).collect();

        candidates.into_iter()
            .find(|p| p.is_dir())
            .unwrap_or_else(|| {
                // Dev mode: relative to Cargo.toml
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .map(|p| p.join("src/penpot"))
                    .unwrap_or_else(|| PathBuf::from("src/penpot"))
            })
    };

    println!("📁 Penpot frontend directory: {}", penpot_dir.display());

    let config_for_exit = shared_config.clone();
    let initial_lang = config.language.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(shared_config.clone())
        .manage(backend_store.clone())
        .on_window_event(|window, event| {
            match event {
                tauri::WindowEvent::Destroyed => {
                    // Archive the closed tab's URL + title before untrack_tab
                    // removes it — feeds "Reopen Closed Tab" / "Recently Closed".
                    if let Some(list) = TAB_URLS.get() {
                        if let Ok(v) = list.read() {
                            if let Some((_, url)) =
                                v.iter().find(|(l, _)| l == window.label())
                            {
                                archive_closed_tab(window.label(), url);
                            }
                        }
                    }
                    untrack_tab(window.label());
                    forget_window_mode(window.label());
                }
                tauri::WindowEvent::Focused(true) => {
                    // Settings webviews don't have a tracked Penpot mode; leave the
                    // current menu in place when focusing them.
                    let label = window.label().to_string();
                    if let Some(mode) = get_window_mode(&label) {
                        let app = window.app_handle().clone();
                        let app_for_closure = app.clone();
                        let _ = app.run_on_main_thread(move || {
                            if let Ok((menu, _)) = build_menu(&app_for_closure, &mode) {
                                let _ = app_for_closure.set_menu(menu);
                                #[cfg(target_os = "macos")]
                                {
                                    register_help_menu();
                                    register_window_menu();
                                }
                                if mode == "workspace" {
                                    update_selection_items(&app_for_closure, 0, &[], &[]);
                                }
                            }
                        });
                    }
                }
                _ => {}
            }
        })
        .setup(move |app| {
            // Store app handle for proxy → menu communication
            APP_HANDLE.get_or_init(|| app.handle().clone());
            TAB_URLS.get_or_init(|| std::sync::RwLock::new(Vec::new()));
            TAB_TITLES.get_or_init(|| std::sync::RwLock::new(HashMap::new()));
            CLOSED_TABS.get_or_init(|| std::sync::RwLock::new(std::collections::VecDeque::new()));
            PLUGINS.get_or_init(|| std::sync::RwLock::new(Vec::new()));
            WINDOW_MODES.get_or_init(|| std::sync::RwLock::new(HashMap::new()));
            CURRENT_LANG.get_or_init(|| std::sync::RwLock::new(initial_lang.clone()));

            // Set initial menu (dashboard mode)
            let (initial_menu, _) = build_menu(&app.handle(), "dashboard")
                .expect("Failed to build menu");
            app.set_menu(initial_menu)?;
            #[cfg(target_os = "macos")]
            {
                register_help_menu();
                register_window_menu();
            }

            // Poll which window is currently focused and rebuild the menu
            // when it changes. macOS native tabs share one NSWindow, so
            // neither Tauri's WindowEvent::Focused nor JS focus events fire
            // reliably on tab-bar clicks — webview.is_focused() does report
            // the truth via NSWindow.isKeyWindow.
            let app_for_poll = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                use std::sync::Mutex as StdMutex;
                let last_key: StdMutex<Option<(String, String)>> = StdMutex::new(None);
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    let focused_label = app_for_poll
                        .webview_windows()
                        .into_iter()
                        .find(|(_, w)| w.is_focused().unwrap_or(false))
                        .map(|(l, _)| l);
                    let Some(focused_label) = focused_label else { continue };

                    let Some(mode) = get_window_mode(&focused_label) else {
                        continue;
                    };
                    let key = (focused_label.clone(), mode.clone());
                    let changed = {
                        let mut last = last_key.lock().unwrap();
                        if last.as_ref() == Some(&key) {
                            false
                        } else {
                            *last = Some(key.clone());
                            true
                        }
                    };
                    if !changed {
                        continue;
                    }
                    let app_clone = app_for_poll.clone();
                    let mode_clone = mode.clone();
                    let _ = app_for_poll.run_on_main_thread(move || {
                        if let Ok((menu, _)) = build_menu(&app_clone, &mode_clone) {
                            let _ = app_clone.set_menu(menu);
                            #[cfg(target_os = "macos")]
                            {
                                register_help_menu();
                                register_window_menu();
                            }
                            if mode_clone == "workspace" {
                                update_selection_items(&app_clone, 0, &[], &[]);
                            }
                        }
                    });
                }
            });

            // Handle menu events — simulate keyboard shortcuts for Penpot
            let port_for_menu = port;
            app.on_menu_event(move |app, event| {
                let id = event.id().as_ref();

                // Window-independent actions: handle before looking up a target window,
                // so the menu keeps working even if every Penpot window has been closed
                // (e.g. after switching backend/renderer, which closes non-settings tabs).
                match id {
                    "settings" => {
                        let _ = create_tab_window(app, port_for_menu, Some("/__penpot_desktop"), None);
                        return;
                    }
                    "new-tab" => {
                        let focused = app.webview_windows().into_iter()
                            .find(|(_, w)| w.is_focused().unwrap_or(false))
                            .map(|(l, _)| l);
                        let _ = create_tab_window(app, port_for_menu, None, focused.as_deref());
                        return;
                    }
                    "new-window" => {
                        let _ = create_standalone_window(app, port_for_menu, None);
                        return;
                    }
                    "reopen-closed-tab" => {
                        if let Some(tab) = pop_closed_tab() {
                            let focused = app.webview_windows().into_iter()
                                .find(|(_, w)| w.is_focused().unwrap_or(false))
                                .map(|(l, _)| l);
                            let _ = create_tab_window(app, port_for_menu, Some(&tab.url), focused.as_deref());
                        }
                        return;
                    }
                    "open-url-from-clipboard" => {
                        use tauri_plugin_clipboard_manager::ClipboardExt;
                        use tauri_plugin_dialog::DialogExt;
                        let lang = menu::current_lang();
                        let show_invalid = |app: &tauri::AppHandle| {
                            app.dialog()
                                .message(i18n::t(&lang, "file.invalid-url-body"))
                                .title(i18n::t(&lang, "file.invalid-url-title"))
                                .kind(tauri_plugin_dialog::MessageDialogKind::Warning)
                                .blocking_show();
                        };
                        let text = match app.clipboard().read_text() {
                            Ok(t) => t,
                            Err(_) => {
                                show_invalid(app);
                                return;
                            }
                        };
                        let trimmed = text.trim();
                        let backend = app
                            .try_state::<SharedConfig>()
                            .and_then(|c| c.try_read().ok().map(|c| c.backend_url.clone()))
                            .unwrap_or_default();
                        let normalized = if trimmed.starts_with("http") {
                            if backend.is_empty() {
                                show_invalid(app);
                                return;
                            }
                            let base = backend.trim_end_matches('/');
                            if !trimmed.starts_with(base) {
                                show_invalid(app);
                                return;
                            }
                            state::normalize_tab_url(trimmed)
                        } else if trimmed.starts_with("/#/") {
                            trimmed.to_string()
                        } else {
                            show_invalid(app);
                            return;
                        };
                        let focused = app.webview_windows().into_iter()
                            .find(|(_, w)| w.is_focused().unwrap_or(false))
                            .map(|(l, _)| l);
                        let _ = create_tab_window(app, port_for_menu, Some(&normalized), focused.as_deref());
                        return;
                    }
                    id if id.starts_with("reopen-closed-") => {
                        // Matches "reopen-closed-<index>" — "reopen-closed-tab"
                        // was already handled by the arm above.
                        if let Ok(idx) = id.trim_start_matches("reopen-closed-").parse::<usize>() {
                            if let Some(tab) = take_closed_tab_at(idx) {
                                let focused = app.webview_windows().into_iter()
                                    .find(|(_, w)| w.is_focused().unwrap_or(false))
                                    .map(|(l, _)| l);
                                let _ = create_tab_window(app, port_for_menu, Some(&tab.url), focused.as_deref());
                            }
                        }
                        return;
                    }
                    "help-guide" | "help-tutorials" | "help-courses" |
                    "help-plugins" | "help-libraries" |
                    "help-community" | "help-github" | "help-feedback" |
                    "help-website" | "help-release-notes" => {
                        let url = match id {
                            "help-guide" => "https://help.penpot.app",
                            "help-tutorials" => "https://www.youtube.com/@Penpot",
                            "help-community" => "https://community.penpot.app",
                            "help-github" => "https://github.com/penpot/penpot",
                            "help-feedback" => "https://github.com/penpot/penpot/issues",
                            "help-website" => "https://penpot.app",
                            "help-courses" => "https://penpot.app/courses/",
                            "help-plugins" => "https://penpot.app/penpothub/plugins",
                            "help-libraries" => "https://penpot.app/penpothub/libraries-templates",
                            "help-release-notes" => "https://penpot.app/dev-diaries",
                            _ => return,
                        };
                        use tauri_plugin_opener::OpenerExt;
                        let _ = app.opener().open_url(url, None::<&str>);
                        return;
                    }
                    _ => {}
                }

                // Window-dependent actions: prefer the focused webview, fall back to any
                // non-settings webview, then to anything that's still around. Don't hard-code
                // the literal "main" label — it gets closed on backend switches.
                let window = app
                    .webview_windows()
                    .into_values()
                    .find(|w| w.is_focused().unwrap_or(false))
                    .or_else(|| {
                        app.webview_windows().into_values().find(|w| {
                            w.url()
                                .map(|u| !u.path().contains("__penpot_desktop"))
                                .unwrap_or(false)
                        })
                    })
                    .or_else(|| app.webview_windows().into_values().next());
                let Some(window) = window else { return };

                // Map menu IDs to Mousetrap key sequences
                let shortcut = match id {
                    // Native actions
                    "devtools" => {
                        if window.is_devtools_open() { window.close_devtools(); }
                        else { window.open_devtools(); }
                        return;
                    }
                    "fullscreen" => {
                        let _ = window.set_fullscreen(!window.is_fullscreen().unwrap_or(false));
                        return;
                    }
                    "reload-tab" => {
                        let _ = window.eval("location.reload()");
                        return;
                    }
                    "close-tab" => {
                        let _ = window.close();
                        return;
                    }

                    // File
                    "export" => "meta+shift+e",
                    "show-version-history" => "meta+alt+h",

                    // File — Penpot actions without a Mousetrap shortcut: open
                    // Penpot's own File submenu and click the matching item.
                    "pin-version" => {
                        let _ = window.eval(
                            "window.__penpotDesktopFileAction('file-menu-create-version')",
                        );
                        return;
                    }
                    "toggle-shared" => {
                        let _ = window.eval(
                            "window.__penpotDesktopFileAction(['file-menu-add-shared','file-menu-remove-shared'])",
                        );
                        return;
                    }
                    "download-binary" => {
                        let _ = window.eval(
                            "window.__penpotDesktopFileAction('file-menu-binary-file')",
                        );
                        return;
                    }
                    "export-frames-pdf" => {
                        let _ = window.eval(
                            "window.__penpotDesktopFileAction('file-menu-export-frames')",
                        );
                        return;
                    }

                    // Plugins
                    "plugins-manager" => {
                        let _ = window.eval(
                            "window.__penpotDesktopOpenPluginManager()",
                        );
                        return;
                    }
                    id if id.starts_with("plugin-") => {
                        if let Ok(idx) = id.trim_start_matches("plugin-").parse::<usize>() {
                            let plugins = state::get_plugins();
                            if let Some(plugin) = plugins.get(idx) {
                                let name = plugin.name.replace('\\', "\\\\").replace('\'', "\\'");
                                let js = format!(
                                    "window.__penpotDesktopPluginAction('{name}')"
                                );
                                let _ = window.eval(&js);
                            }
                        }
                        return;
                    }

                    "copy-file-url" => {
                        // Combine the focused tab's stored path/hash with the
                        // configured backend URL to produce a shareable link.
                        let label = window.label().to_string();
                        let path_hash = TAB_URLS
                            .get()
                            .and_then(|l| l.read().ok())
                            .and_then(|v| {
                                v.iter().find(|(l, _)| l == &label).map(|(_, u)| u.clone())
                            });
                        let Some(path_hash) = path_hash else { return };
                        let backend = app
                            .try_state::<SharedConfig>()
                            .and_then(|c| c.try_read().ok().map(|c| c.backend_url.clone()));
                        let Some(backend) = backend else { return };
                        if backend.is_empty() {
                            return;
                        }
                        let full = format!("{}{}", backend.trim_end_matches('/'), path_hash);
                        use tauri_plugin_clipboard_manager::ClipboardExt;
                        let _ = app.clipboard().write_text(full);
                        return;
                    }

                    // Edit — standard actions
                    "undo" => "meta+z",
                    "redo" => "meta+shift+z",
                    "cut" => "meta+x",
                    "copy" => "meta+c",
                    "paste" => {
                        // Paste needs real clipboard data — synthetic keydown won't
                        // trigger a trusted paste event. Read clipboard from Rust
                        // and dispatch a ClipboardEvent with the content.
                        use tauri_plugin_clipboard_manager::ClipboardExt;
                        if let Ok(text) = app.clipboard().read_text() {
                            let escaped = text.replace('\\', "\\\\")
                                .replace('\'', "\\'")
                                .replace('\n', "\\n")
                                .replace('\r', "\\r");
                            let js = format!(
                                "(() => {{ \
                                    var dt = new DataTransfer(); \
                                    dt.setData('text/plain', '{}'); \
                                    var ev = new ClipboardEvent('paste', {{ clipboardData: dt, bubbles: true, cancelable: true }}); \
                                    (document.activeElement || document.body).dispatchEvent(ev); \
                                }})()",
                                escaped
                            );
                            let _ = window.eval(&js);
                        }
                        return;
                    }
                    "select-all" => "meta+a",
                    // Edit — Penpot-specific
                    "duplicate" => "meta+d",
                    "delete" => "backspace",
                    "group" => "meta+g",
                    "ungroup" => "shift+g",
                    "create-component" => "meta+k",
                    "detach-component" => "meta+shift+k",
                    "rename" => "alt+n",
                    "selection-to-board" => "meta+alt+g",
                    "focus-on" => "f",
                    "toggle-visibility" => "meta+shift+h",
                    "toggle-lock" => "meta+shift+l",
                    "set-thumbnail" => "shift+t",

                    // View — Penpot canvas zoom (plain keys, no modifiers)
                    "zoom-in" => "+",
                    "zoom-out" => "-",
                    "zoom-reset" => "shift+0",
                    "zoom-fit" => "shift+1",
                    "zoom-selected" => "shift+2",
                    "toggle-rulers" => "meta+shift+r",
                    "toggle-guides" => "meta+'",
                    "toggle-grid" => "shift+,",
                    "toggle-layers" => "alt+l",
                    "toggle-assets" => "alt+i",
                    "toggle-palette" => "alt+p",
                    "toggle-history" => "meta+alt+h",
                    "hide-ui" => "\\",
                    "toggle-theme" => "alt+m",

                    // Shape tools
                    "tool-board" => "b",
                    "tool-rect" => "r",
                    "tool-ellipse" => "e",
                    "tool-text" => "t",
                    "tool-path" => "p",
                    "tool-curve" => "shift+c",
                    "insert-image" => "shift+k",
                    "flip-h" => "shift+h",
                    "flip-v" => "shift+v",
                    "bring-forward" => "meta+up",
                    "bring-front" => "meta+shift+up",
                    "send-backward" => "meta+down",
                    "send-back" => "meta+shift+down",
                    "bool-union" => "meta+alt+u",
                    "bool-difference" => "meta+alt+d",
                    "bool-intersection" => "meta+alt+i",
                    "bool-exclude" => "meta+alt+e",
                    "toggle-layout-flex" => "shift+a",
                    "toggle-layout-grid" => "meta+shift+a",

                    // Align
                    "align-left" => "alt+a",
                    "align-hcenter" => "alt+h",
                    "align-right" => "alt+d",
                    "align-top" => "alt+w",
                    "align-vcenter" => "alt+v",
                    "align-bottom" => "alt+s",
                    "dist-h" => "meta+shift+alt+h",
                    "dist-v" => "meta+shift+alt+v",

                    // Go — Mousetrap key sequences
                    "go-drafts" => {
                        let _ = window.eval("window.__penpotKey('g'); setTimeout(()=>window.__penpotKey('d'),100)");
                        return;
                    }
                    "go-libs" => {
                        let _ = window.eval("window.__penpotKey('g'); setTimeout(()=>window.__penpotKey('l'),100)");
                        return;
                    }
                    "go-search" => "meta+f",
                    "go-viewer" => {
                        let _ = window.eval("window.__penpotKey('g'); setTimeout(()=>window.__penpotKey('v'),100)");
                        return;
                    }
                    "go-inspect" => {
                        let _ = window.eval("window.__penpotKey('g'); setTimeout(()=>window.__penpotKey('i'),100)");
                        return;
                    }
                    "go-dashboard" => {
                        let _ = window.eval("window.__penpotKey('g'); setTimeout(()=>window.__penpotKey('d'),100)");
                        return;
                    }

                    // File — dashboard (click UI button directly)
                    "new-project" => {
                        let _ = window.eval("document.querySelector('[data-testid=\"new-project-button\"]')?.click()");
                        return;
                    }

                    // Help
                    "help-shortcuts" => "?",

                    _ => return,
                };

                // Normalize `meta` to `ctrl` on non-macOS so Penpot receives
                // the expected modifier in its platform-specific shortcuts.
                let shortcut = platform_shortcut(shortcut);
                // Simulate keyboard event with proper keyCode for Mousetrap.
                // Escape backslash and single-quote so shortcuts containing them
                // (e.g. "meta+'" for guides, "\\" for hide-ui) don't break the JS literal.
                let escaped_shortcut = shortcut.replace('\\', "\\\\").replace('\'', "\\'");
                let js = format!("window.__penpotKey('{escaped_shortcut}')");
                let _ = window.eval(&js);
            });

            // Start reverse proxy in background. Hand it the shared backend
            // store so the offline RPC handler talks to the same data the
            // Tauri commands open/save against.
            let penpot_dir_clone = penpot_dir.clone();
            let backend_store_for_proxy = backend_store.clone();
            tauri::async_runtime::spawn(async move {
                start_proxy_with(proxy_config, penpot_dir_clone, backend_store_for_proxy).await;
            });

            // Create main window with download handler
            use tauri::webview::{DownloadEvent, WebviewWindowBuilder};

            // Read saved window groups early so we can inject hash into main window
            let no_backend = shared_config.try_read().map(|c| c.backend_url.is_empty()).unwrap_or(true);
            let saved_groups: Vec<Vec<String>> = if !no_backend {
                shared_config.try_read()
                    .map(|c| c.open_groups.clone())
                    .unwrap_or_default()
            } else {
                vec![]
            };

            #[allow(unused_mut)]
            let mut main_builder = WebviewWindowBuilder::new(app, "main", Default::default())
                .title("Penpot Desktop")
                .maximized(true)
                .inner_size(1440.0, 900.0)
                .min_inner_size(900.0, 600.0)
                .disable_drag_drop_handler();
            #[cfg(target_os = "macos")]
            { main_builder = main_builder.tabbing_identifier("penpot"); }
            #[allow(unused_mut)]
            let mut main_builder = main_builder
                .on_navigation(|url| {
                    url.scheme() == "blob" || url.host_str() == Some("127.0.0.1")
                })
                .on_page_load(|webview, payload| {
                    if let tauri::webview::PageLoadEvent::Finished = payload.event() {
                        let label = webview.label().to_string();
                        let _ = webview.eval(&format!(
                            "window.__penpotWindowLabel='{label}';\
                             if(!window.__penpotUrlTracker){{window.__penpotUrlTracker=true;\
                               var __pptLastUrl='',__pptLastTitle='';\
                               setInterval(()=>{{\
                                 if(location.href!==__pptLastUrl||document.title!==__pptLastTitle){{\
                                   __pptLastUrl=location.href;__pptLastTitle=document.title;\
                                   navigator.sendBeacon('/__penpot_desktop/update-tab-url',\
                                     JSON.stringify({{label:window.__penpotWindowLabel,url:location.href,title:document.title}}));\
                                 }}\
                               }},2000);\
                               window.addEventListener('beforeunload',()=>\
                                 navigator.sendBeacon('/__penpot_desktop/update-tab-url',\
                                   JSON.stringify({{label:window.__penpotWindowLabel,url:location.href,title:document.title}})));\
                             }}\
                             {WINDOW_OPEN_OVERRIDE}\
                             {FILE_MENU_HELPER}\
                             {PLUGIN_POLLER}\
                             {PLUGIN_LAUNCHER}"
                        ));
                    }
                })
                .on_download(|_webview, event| {
                    match event {
                        DownloadEvent::Requested { url, destination } => {
                            // Extract filename from query param, URL fragment, or path
                            let filename = url.query_pairs()
                                .find(|(k, _)| k == "filename" || k == "name")
                                .map(|(_, v)| v.to_string())
                                .or_else(|| {
                                    url.fragment()
                                        .map(|f| percent_encoding::percent_decode_str(f).decode_utf8_lossy().into_owned())
                                })
                                .unwrap_or_else(|| {
                                    url.path_segments()
                                        .and_then(|s| s.last())
                                        .unwrap_or("download")
                                        .to_string()
                                });

                            let downloads = dirs::download_dir()
                                .unwrap_or_else(|| PathBuf::from("."));
                            *destination = downloads.join(&filename);
                            println!("[download] → {}", destination.display());
                            true
                        }
                        DownloadEvent::Finished { success, .. } => {
                            if !success {
                                eprintln!("[download] failed");
                            }
                            true
                        }
                        _ => true,
                    }
                });
            if let Some(ua) = safari_user_agent() {
                main_builder = main_builder.user_agent(&ua);
            }
            let window = main_builder.build()?;

            // Navigate to base URL first, then set hash via JS
            // (navigate() drops the URL fragment/hash)
            let base_url = if no_backend {
                format!("http://127.0.0.1:{port}/__penpot_desktop")
            } else {
                format!("http://127.0.0.1:{port}/")
            };

            let main_tab_url = if !no_backend {
                saved_groups.first().and_then(|g| g.first()).cloned()
            } else {
                None
            };
            let default_hash = if !no_backend {
                let wasm = shared_config.try_read()
                    .map(|c| c.renderer == "wasm")
                    .unwrap_or(false);
                Some(format!("#/?wasm={wasm}"))
            } else {
                None
            };

            // Small delay so proxy can start
            let window_clone = window.clone();
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let _ = window_clone.navigate(base_url.parse().unwrap());

                // Wait for page to load, then set the correct hash via JS
                tokio::time::sleep(std::time::Duration::from_millis(2000)).await;

                let target_url = main_tab_url
                    .or(default_hash.map(|h| format!("/{h}")));
                if let Some(ref tab_url) = target_url {
                    let full = if tab_url.starts_with("http") {
                        tab_url.clone()
                    } else {
                        format!("http://127.0.0.1:{port}{tab_url}")
                    };
                    let escaped = full.replace('\\', "\\\\").replace('\'', "\\'");
                    let _ = window_clone.eval(&format!(
                        "window.location.replace('{escaped}');"
                    ));
                }

                // Restore window groups from previous session.
                // Group 0: extra tabs go into the main window's tab bar.
                // Groups 1+: first URL becomes a standalone window, the rest
                // are tabs anchored to it.
                for (gi, group) in saved_groups.iter().enumerate() {
                    let skip = if gi == 0 { 1 } else { 0 }; // group 0's first URL is already in main
                    let urls: Vec<String> = group.iter().skip(skip).cloned().collect();
                    if urls.is_empty() {
                        continue;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                    if gi == 0 {
                        // Additional tabs for the main window group
                        for url in &urls {
                            let _ = app_handle.run_on_main_thread({
                                let app = app_handle.clone();
                                let url = url.clone();
                                move || {
                                    let _ = create_tab_window(&app, port, Some(&url), Some("main"));
                                }
                            });
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                    } else {
                        // New standalone window group: first URL → standalone, rest → tabs
                        use std::sync::{Arc, Mutex as StdMutex};
                        let anchor_label: Arc<StdMutex<Option<String>>> = Arc::new(StdMutex::new(None));
                        let first_url = urls[0].clone();
                        let anchor_for_first = anchor_label.clone();
                        let _ = app_handle.run_on_main_thread({
                            let app = app_handle.clone();
                            move || {
                                if let Ok(label) = create_standalone_window(&app, port, Some(&first_url)) {
                                    *anchor_for_first.lock().unwrap() = Some(label);
                                }
                            }
                        });
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                        let anchor = anchor_label.lock().unwrap().clone();
                        for url in &urls[1..] {
                            let _ = app_handle.run_on_main_thread({
                                let app = app_handle.clone();
                                let url = url.clone();
                                let anchor = anchor.clone();
                                move || {
                                    let _ = create_tab_window(&app, port, Some(&url), anchor.as_deref());
                                }
                            });
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                    }
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_proxy_url,
            save_download,
            open_penpot_file,
            save_penpot_file,
            import_penpot_file,
            export_penpot_file,
            list_offline_files,
            delete_offline_file,
            switch_mode,
        ])
        .build(tauri::generate_context!())
        .expect("Failed to build Penpot Desktop")
        .run(move |#[cfg_attr(not(target_os = "macos"), allow(unused_variables))] app, event| {
            if let tauri::RunEvent::Exit = event {
                if let Some(list) = TAB_URLS.get() {
                    if let Ok(tab_map) = list.read() {
                        let url_map: HashMap<&str, &str> = tab_map.iter()
                            .map(|(l, u)| (l.as_str(), u.as_str()))
                            .collect();

                        #[cfg(target_os = "macos")]
                        let label_groups = get_all_tab_groups(app);
                        #[cfg(not(target_os = "macos"))]
                        let label_groups: Vec<Vec<String>> = {
                            // No native tab groups — treat every tracked window
                            // as its own group.
                            tab_map.iter().map(|(l, _)| vec![l.clone()]).collect()
                        };

                        let groups: Vec<Vec<String>> = label_groups
                            .iter()
                            .map(|group| {
                                group
                                    .iter()
                                    .filter_map(|label| url_map.get(label.as_str()).copied())
                                    .filter(|u| !u.is_empty() && !u.contains("__penpot_desktop"))
                                    .map(|u| u.to_string())
                                    .collect::<Vec<String>>()
                            })
                            .filter(|g| !g.is_empty())
                            .collect();

                        let mut cfg = config_for_exit.blocking_write();
                        cfg.open_groups = groups;
                        cfg.open_tabs.clear();
                        save_config(&cfg);
                    }
                }
            }
        });
}

/// Save current tab groups to config so the session can be restored after
/// a restart (e.g. after a language change that requires re-launching the
/// app to pick up the new AppleLanguages setting).
pub fn save_session_state(app: &tauri::AppHandle, config: &SharedConfig) {
    if let Some(list) = TAB_URLS.get() {
        if let Ok(tab_map) = list.read() {
            let url_map: HashMap<&str, &str> = tab_map
                .iter()
                .map(|(l, u)| (l.as_str(), u.as_str()))
                .collect();

            #[cfg(target_os = "macos")]
            let label_groups = get_all_tab_groups(app);
            #[cfg(not(target_os = "macos"))]
            let label_groups: Vec<Vec<String>> = {
                let _ = app;
                tab_map.iter().map(|(l, _)| vec![l.clone()]).collect()
            };

            let groups: Vec<Vec<String>> = label_groups
                .iter()
                .map(|group| {
                    group
                        .iter()
                        .filter_map(|label| url_map.get(label.as_str()).copied())
                        .filter(|u| !u.is_empty() && !u.contains("__penpot_desktop"))
                        .map(|u| u.to_string())
                        .collect::<Vec<String>>()
                })
                .filter(|g| !g.is_empty())
                .collect();

            if let Ok(mut cfg) = config.try_write() {
                cfg.open_groups = groups;
                cfg.open_tabs.clear();
                save_config(&cfg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_shortcut_for_platform;

    #[test]
    fn keeps_meta_on_macos() {
        assert_eq!(
            normalize_shortcut_for_platform("meta+shift+z", true),
            "meta+shift+z"
        );
    }

    #[test]
    fn rewrites_meta_to_ctrl_on_non_macos() {
        assert_eq!(
            normalize_shortcut_for_platform("meta+shift+z", false),
            "ctrl+shift+z"
        );
    }

    #[test]
    fn keeps_plus_shortcut_stable() {
        assert_eq!(normalize_shortcut_for_platform("+", false), "+");
    }
}

fn main() {
    run();
}
