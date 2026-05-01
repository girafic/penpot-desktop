use std::collections::{HashMap, VecDeque};
use std::sync::OnceLock;
use tauri::Manager;

pub static APP_HANDLE: OnceLock<tauri::AppHandle> = OnceLock::new();

// Current UI language — updated when the user changes language
pub static CURRENT_LANG: OnceLock<std::sync::RwLock<String>> = OnceLock::new();

// Ordered list of (label, url) — preserves tab order for session restore
pub static TAB_URLS: OnceLock<std::sync::RwLock<Vec<(String, String)>>> = OnceLock::new();

// Per-tab document title — updated by the same polling script that feeds TAB_URLS.
// Used for human-readable labels in the "Recently Closed Tabs" submenu.
pub static TAB_TITLES: OnceLock<std::sync::RwLock<HashMap<String, String>>> = OnceLock::new();

// Recently closed tabs (LIFO), capped at CLOSED_TABS_CAP. Populated on
// WindowEvent::Destroyed before the label is untracked, drained by the File
// menu's "Reopen Closed Tab" and "Recently Closed Tabs" handlers.
#[derive(Clone, Debug)]
pub struct ClosedTab {
    pub url: String,
    pub title: String,
}

pub static CLOSED_TABS: OnceLock<std::sync::RwLock<VecDeque<ClosedTab>>> = OnceLock::new();
const CLOSED_TABS_CAP: usize = 10;

pub fn track_tab_title(label: &str, title: &str) {
    if let Some(map) = TAB_TITLES.get() {
        if let Ok(mut m) = map.write() {
            m.insert(label.to_string(), title.to_string());
        }
    }
}

pub fn archive_closed_tab(label: &str, url: &str) {
    if url.contains("__penpot_desktop") {
        return;
    }
    let title = TAB_TITLES
        .get()
        .and_then(|m| m.read().ok())
        .and_then(|m| m.get(label).cloned())
        .unwrap_or_default();
    if let Some(stack) = CLOSED_TABS.get() {
        if let Ok(mut q) = stack.write() {
            q.push_back(ClosedTab {
                url: url.to_string(),
                title,
            });
            while q.len() > CLOSED_TABS_CAP {
                q.pop_front();
            }
        }
    }
}

pub fn pop_closed_tab() -> Option<ClosedTab> {
    CLOSED_TABS
        .get()
        .and_then(|s| s.write().ok())
        .and_then(|mut q| q.pop_back())
}

pub fn take_closed_tab_at(index: usize) -> Option<ClosedTab> {
    let stack = CLOSED_TABS.get()?;
    let mut q = stack.write().ok()?;
    // Index 0 refers to the most-recently-closed — translate to VecDeque's
    // chronological order by reversing.
    let len = q.len();
    if index >= len {
        return None;
    }
    let actual = len - 1 - index;
    q.remove(actual)
}

pub fn get_closed_tabs() -> Vec<ClosedTab> {
    CLOSED_TABS
        .get()
        .and_then(|s| s.read().ok())
        .map(|q| q.iter().rev().cloned().collect())
        .unwrap_or_default()
}

// Installed Penpot plugins — polled from the profile API and used to
// build the dynamic Plugins submenu.
#[derive(Clone, Debug)]
pub struct PluginInfo {
    /// Penpot's plugin id — kept on the struct for parity with the
    /// frontend's plugin-poller payload. Not yet consumed by the
    /// menu code (which only needs `name`).
    #[allow(dead_code)]
    pub id: String,
    pub name: String,
}

pub static PLUGINS: OnceLock<std::sync::RwLock<Vec<PluginInfo>>> = OnceLock::new();

pub fn update_plugins(plugins: Vec<PluginInfo>) {
    if let Some(list) = PLUGINS.get() {
        if let Ok(mut v) = list.write() {
            *v = plugins;
        }
    }
}

pub fn get_plugins() -> Vec<PluginInfo> {
    PLUGINS
        .get()
        .and_then(|l| l.read().ok())
        .map(|v| v.clone())
        .unwrap_or_default()
}

// Per-window menu mode ("dashboard" / "workspace") — needed so the menu
// reflects whichever window currently has focus, not whichever window last
// posted a /set-view event.
pub static WINDOW_MODES: OnceLock<std::sync::RwLock<HashMap<String, String>>> = OnceLock::new();

pub fn set_window_mode(label: &str, mode: &str) {
    if let Some(map) = WINDOW_MODES.get() {
        if let Ok(mut m) = map.write() {
            m.insert(label.to_string(), mode.to_string());
        }
    }
}

pub fn get_window_mode(label: &str) -> Option<String> {
    WINDOW_MODES
        .get()
        .and_then(|m| m.read().ok())
        .and_then(|m| m.get(label).cloned())
}

pub fn forget_window_mode(label: &str) {
    if let Some(map) = WINDOW_MODES.get() {
        if let Ok(mut m) = map.write() {
            m.remove(label);
        }
    }
}

/// Returns the menu mode of the currently focused Penpot window, falling
/// back to any tracked mode, then "dashboard".
pub fn focused_window_mode(app: &tauri::AppHandle) -> String {
    if let Some(focused_label) = app
        .webview_windows()
        .into_iter()
        .find(|(_, w)| w.is_focused().unwrap_or(false))
        .map(|(label, _)| label)
    {
        if let Some(mode) = get_window_mode(&focused_label) {
            return mode;
        }
    }
    if let Some(map) = WINDOW_MODES.get() {
        if let Ok(m) = map.read() {
            if let Some((_, mode)) = m.iter().next() {
                return mode.clone();
            }
        }
    }
    "dashboard".to_string()
}

pub fn normalize_tab_url(url: &str) -> String {
    // Store only the path+hash portion, stripping the origin so restore works
    // even if the proxy port changes between sessions.
    if let Some(pos) = url.find("/#") {
        url[pos..].to_string()
    } else if let Some(pos) = url.find("://") {
        if let Some(slash) = url[pos + 3..].find('/') {
            url[pos + 3 + slash..].to_string()
        } else {
            "/".to_string()
        }
    } else {
        url.to_string()
    }
}

pub fn track_tab_url(label: &str, url: &str) {
    let store_url = normalize_tab_url(url);
    if let Some(list) = TAB_URLS.get() {
        if let Ok(mut v) = list.write() {
            if let Some(entry) = v.iter_mut().find(|(l, _)| l == label) {
                // Update existing — keeps original position
                entry.1 = store_url;
            } else {
                v.push((label.to_string(), store_url));
            }
        }
    }
}

/// Discover all tab groups across every top-level window.
/// Returns a `Vec<Vec<String>>` where each inner vec is one group of
/// Tauri labels in visual (left→right) tab-bar order. Standalone
/// windows without siblings become a single-element group.
#[cfg(target_os = "macos")]
pub fn get_all_tab_groups(app: &tauri::AppHandle) -> Vec<Vec<String>> {
    use std::collections::HashSet;

    // NSWindow pointer → Tauri label
    let mut ns_to_label: HashMap<usize, String> = HashMap::new();
    for (label, window) in app.webview_windows() {
        if let Ok(ptr) = window.ns_window() {
            ns_to_label.insert(ptr as usize, label.to_string());
        }
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut groups: Vec<Vec<String>> = Vec::new();

    // Process "main" first so that the main window's group is always groups[0].
    let mut labels_to_visit: Vec<String> = Vec::new();
    if app.get_webview_window("main").is_some() {
        labels_to_visit.push("main".to_string());
    }
    for (label, _) in app.webview_windows() {
        if label != "main" {
            labels_to_visit.push(label);
        }
    }

    for label in &labels_to_visit {
        if seen.contains(label) {
            continue;
        }
        let Some(win) = app.get_webview_window(label) else {
            continue;
        };
        let Ok(ns_ptr) = win.ns_window() else {
            continue;
        };
        let ns: *mut objc2::runtime::AnyObject = ns_ptr.cast();

        let mut group: Vec<String> = Vec::new();
        unsafe {
            let tabbed: *mut objc2::runtime::AnyObject = objc2::msg_send![ns, tabbedWindows];
            if !tabbed.is_null() {
                let count: usize = objc2::msg_send![tabbed, count];
                for i in 0..count {
                    let tab_ns: *mut objc2::runtime::AnyObject =
                        objc2::msg_send![tabbed, objectAtIndex: i];
                    if let Some(lbl) = ns_to_label.get(&(tab_ns as usize)) {
                        if !seen.contains(lbl) {
                            group.push(lbl.clone());
                            seen.insert(lbl.clone());
                        }
                    }
                }
            }
        }
        if group.is_empty() {
            // Standalone window (no tab siblings)
            group.push(label.clone());
            seen.insert(label.clone());
        }
        groups.push(group);
    }
    groups
}

pub fn untrack_tab(label: &str) {
    if let Some(list) = TAB_URLS.get() {
        if let Ok(mut v) = list.write() {
            v.retain(|(l, _)| l != label);
        }
    }
    if let Some(map) = TAB_TITLES.get() {
        if let Ok(mut m) = map.write() {
            m.remove(label);
        }
    }
}
