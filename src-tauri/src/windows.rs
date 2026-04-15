// ── Window creation helpers ─────────────────────────────────

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use tauri::Manager;

static TAB_COUNTER: AtomicU32 = AtomicU32::new(1);

pub fn create_tab_window(
    app: &tauri::AppHandle,
    port: u16,
    url: Option<&str>,
    anchor_label: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    use tauri::webview::{DownloadEvent, WebviewWindowBuilder};

    let n = TAB_COUNTER.fetch_add(1, Ordering::Relaxed);
    let label = format!("tab-{n}");

    // Default: open dashboard root (will show dashboard if logged in, login if not)
    let nav_url = url.unwrap_or("/").to_string();

    // For URLs with a hash fragment, load base URL and use location.replace to
    // restore — necessary because navigate() drops the fragment. For URLs without
    // a hash (e.g. /__penpot_desktop settings page), load directly.
    let has_hash = nav_url.contains('#');
    let initial_url = if has_hash {
        format!("http://127.0.0.1:{port}/")
    } else if nav_url.starts_with("http") {
        nav_url.clone()
    } else {
        format!("http://127.0.0.1:{port}{nav_url}")
    };
    let restore_url = if has_hash {
        let full = if nav_url.starts_with("http") {
            nav_url.clone()
        } else {
            format!("http://127.0.0.1:{port}{nav_url}")
        };
        Some(full)
    } else {
        None
    };

    let label_clone = label.clone();
    let restore_url_clone = restore_url.clone();
    let mut builder = WebviewWindowBuilder::new(
        app,
        &label,
        tauri::WebviewUrl::External(initial_url.parse().unwrap()),
    )
    .title("Penpot Desktop")
    .inner_size(1440.0, 900.0)
    .min_inner_size(900.0, 600.0)
    .tabbing_identifier("penpot")
    .disable_drag_drop_handler()
    .on_navigation(|url| {
        url.scheme() == "blob" || url.host_str() == Some("127.0.0.1")
    })
    .on_page_load(move |webview, payload| {
        if let tauri::webview::PageLoadEvent::Finished = payload.event() {
            let lbl = &label_clone;
            // Restore URL via location.replace — triggers a full SPA re-route
            let restore_js = if let Some(ref u) = restore_url_clone {
                let escaped = u.replace('\\', "\\\\").replace('\'', "\\'");
                format!("if(!window.__penpotRestored){{window.__penpotRestored=true;window.location.replace('{escaped}');}}")
            } else {
                String::new()
            };
            let _ = webview.eval(&format!(
                "window.__penpotWindowLabel='{lbl}';\
                 {restore_js}\
                 if(!window.__penpotUrlTracker){{window.__penpotUrlTracker=true;\
                   var __pptLastUrl='';\
                   setInterval(()=>{{\
                     if(location.href!==__pptLastUrl){{\
                       __pptLastUrl=location.href;\
                       navigator.sendBeacon('/__penpot_desktop/update-tab-url',\
                         JSON.stringify({{label:window.__penpotWindowLabel,url:location.href}}));\
                     }}\
                   }},2000);\
                   window.addEventListener('beforeunload',()=>\
                     navigator.sendBeacon('/__penpot_desktop/update-tab-url',\
                       JSON.stringify({{label:window.__penpotWindowLabel,url:location.href}})));\
                 }}"
            ));
        }
    })
    .on_download(|_webview, event| match event {
        DownloadEvent::Requested { url, destination } => {
            let filename = url
                .query_pairs()
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
            let downloads = dirs::download_dir().unwrap_or_else(|| PathBuf::from("."));
            *destination = downloads.join(&filename);
            true
        }
        DownloadEvent::Finished { success, .. } => {
            if !success {
                eprintln!("[download] failed");
            }
            true
        }
        _ => true,
    });
    if let Some(ua) = safari_user_agent() {
        builder = builder.user_agent(&ua);
    }
    let _new_win = builder.build()?;

    // macOS: add new window as the last tab in the existing window. Use any existing
    // webview window as the tab anchor — not the literal "main" label, which can be gone
    // after a backend/renderer switch (set-backend closes non-settings tabs).
    #[cfg(target_os = "macos")]
    {
        let anchor_win = anchor_label
            .and_then(|al| app.get_webview_window(al))
            .or_else(|| {
                app.webview_windows()
                    .into_values()
                    .find(|w| w.is_focused().unwrap_or(false))
            })
            .or_else(|| {
                app.webview_windows()
                    .into_iter()
                    .find(|(l, _)| l != &label)
                    .map(|(_, w)| w)
            });
        if let Some(main_win) = anchor_win {
            if let Some(new_win) = app.get_webview_window(&label) {
                let main_ns: *mut objc2::runtime::AnyObject = main_win.ns_window().unwrap().cast();
                let new_ns: *mut objc2::runtime::AnyObject = new_win.ns_window().unwrap().cast();
                unsafe {
                    // Get the last tab in the group (tabbedWindows can be nil if not yet tabbed)
                    let tabbed_windows: *mut objc2::runtime::AnyObject =
                        objc2::msg_send![main_ns, tabbedWindows];
                    let last_tab: *mut objc2::runtime::AnyObject = if !tabbed_windows.is_null() {
                        let count: usize = objc2::msg_send![tabbed_windows, count];
                        if count > 0 {
                            objc2::msg_send![tabbed_windows, objectAtIndex: count - 1]
                        } else {
                            main_ns
                        }
                    } else {
                        main_ns
                    };
                    // Add after the last tab (ordered: .above = 1)
                    let _: () = objc2::msg_send![last_tab, addTabbedWindow: new_ns, ordered: 1i64];
                    // Make new tab active
                    let _: () = objc2::msg_send![new_ns, makeKeyAndOrderFront: std::ptr::null::<objc2::runtime::AnyObject>()];
                }
            }
        }
    }

    Ok(())
}

/// Create a standalone Penpot window — no `tabbing_identifier`, so it appears
/// as a separate top-level window rather than a tab in the existing group.
pub fn create_standalone_window(
    app: &tauri::AppHandle,
    port: u16,
    url: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    use tauri::webview::WebviewWindowBuilder;

    let n = TAB_COUNTER.fetch_add(1, Ordering::Relaxed);
    let label = format!("win-{n}");

    let nav_url = url.unwrap_or("/").to_string();
    let has_hash = nav_url.contains('#');
    let initial_url = if has_hash {
        format!("http://127.0.0.1:{port}/")
    } else if nav_url.starts_with("http") {
        nav_url.clone()
    } else {
        format!("http://127.0.0.1:{port}{nav_url}")
    };
    let restore_url = if has_hash {
        let full = if nav_url.starts_with("http") {
            nav_url.clone()
        } else {
            format!("http://127.0.0.1:{port}{nav_url}")
        };
        Some(full)
    } else {
        None
    };

    let label_for_load = label.clone();
    let restore_for_load = restore_url.clone();
    let mut b = WebviewWindowBuilder::new(
        app,
        &label,
        tauri::WebviewUrl::External(initial_url.parse().unwrap()),
    )
    .title("Penpot Desktop")
    .inner_size(1440.0, 900.0)
    .min_inner_size(900.0, 600.0)
    .disable_drag_drop_handler()
    .on_navigation(|url| url.scheme() == "blob" || url.host_str() == Some("127.0.0.1"))
    .on_page_load(move |webview, payload| {
        if let tauri::webview::PageLoadEvent::Finished = payload.event() {
            let lbl = &label_for_load;
            let restore_js = if let Some(ref u) = restore_for_load {
                let escaped = u.replace('\\', "\\\\").replace('\'', "\\'");
                format!("if(!window.__penpotRestored){{window.__penpotRestored=true;window.location.replace('{escaped}');}}")
            } else {
                String::new()
            };
            let _ = webview.eval(&format!(
                "window.__penpotWindowLabel='{lbl}';\
                 {restore_js}\
                 if(!window.__penpotUrlTracker){{window.__penpotUrlTracker=true;\
                   var __pptLastUrl='';\
                   setInterval(()=>{{\
                     if(location.href!==__pptLastUrl){{\
                       __pptLastUrl=location.href;\
                       navigator.sendBeacon('/__penpot_desktop/update-tab-url',\
                         JSON.stringify({{label:window.__penpotWindowLabel,url:location.href}}));\
                     }}\
                   }},2000);\
                   window.addEventListener('beforeunload',()=>\
                     navigator.sendBeacon('/__penpot_desktop/update-tab-url',\
                       JSON.stringify({{label:window.__penpotWindowLabel,url:location.href}})));\
                 }}"
            ));
        }
    });
    if let Some(ua) = safari_user_agent() {
        b = b.user_agent(&ua);
    }
    b.build()?;
    Ok(label)
}

// ── Safari User-Agent (macOS only) ──────────────────────────

#[cfg(target_os = "macos")]
pub fn safari_user_agent() -> Option<String> {
    let version = std::process::Command::new("defaults")
        .args([
            "read",
            "/Applications/Safari.app/Contents/Info",
            "CFBundleShortVersionString",
        ])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())?;

    let major = version.split('.').next().unwrap_or("17");

    Some(format!(
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
         AppleWebKit/605.1.15 (KHTML, like Gecko) \
         Version/{major} Safari/605.1.15 PenpotDesktop/1.0"
    ))
}

#[cfg(not(target_os = "macos"))]
pub fn safari_user_agent() -> Option<String> {
    None
}
