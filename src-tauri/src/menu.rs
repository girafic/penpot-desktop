use crate::i18n;
use crate::state::{get_closed_tabs, get_plugins, APP_HANDLE, CURRENT_LANG};
use tauri::menu::{MenuBuilder, MenuItemBuilder, MenuItemKind, PredefinedMenuItem, SubmenuBuilder};
use tauri::Manager;

/// Human-readable label for a closed tab entry. Prefers the stored
/// document title (e.g. "My Design — Penpot"); falls back to truncated URL.
fn closed_tab_label(tab: &crate::state::ClosedTab) -> String {
    if !tab.title.is_empty() {
        const MAX: usize = 55;
        if tab.title.chars().count() <= MAX {
            return tab.title.clone();
        }
        let truncated: String = tab.title.chars().take(MAX - 1).collect();
        return format!("{truncated}…");
    }
    const MAX: usize = 55;
    let url = &tab.url;
    if url.chars().count() <= MAX {
        return url.to_string();
    }
    let head: String = url.chars().take(30).collect();
    let tail: String = url
        .chars()
        .rev()
        .take(20)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{head}…{tail}")
}

// ── macOS Help-menu registration ─────────────────────────

#[cfg(target_os = "macos")]
pub fn register_help_menu() {
    unsafe {
        use objc2::runtime::{AnyClass, AnyObject};
        let ns_app: *mut AnyObject =
            objc2::msg_send![AnyClass::get(c"NSApplication").unwrap(), sharedApplication];
        let main_menu: *mut AnyObject = objc2::msg_send![ns_app, mainMenu];
        let count: isize = objc2::msg_send![main_menu, numberOfItems];
        if count > 0 {
            let last_item: *mut AnyObject = objc2::msg_send![main_menu, itemAtIndex: count - 1];
            let help_ns: *mut AnyObject = objc2::msg_send![last_item, submenu];
            let _: () = objc2::msg_send![ns_app, setHelpMenu: help_ns];
        }
    }
}

// ── macOS Window-menu registration ───────────────────────
//
// Marks the Window submenu as NSApp's `windowsMenu`. Once registered, AppKit
// automatically appends localized standard items (Minimize, Zoom, Move & Resize,
// Tile / Fill / Center, Bring All to Front, Select Previous/Next Tab, the open
// window list, etc.) and keeps them in sync with the user's system language.
//
// Layout of the main menu (workspace/dashboard): App, File, Edit, View, [Shape],
// Go, Window, Help — so the Window submenu is always at index `count - 2`.

#[cfg(target_os = "macos")]
pub fn register_window_menu() {
    unsafe {
        use objc2::runtime::{AnyClass, AnyObject, Sel};
        use objc2::sel;
        let ns_app: *mut AnyObject =
            objc2::msg_send![AnyClass::get(c"NSApplication").unwrap(), sharedApplication];
        let main_menu: *mut AnyObject = objc2::msg_send![ns_app, mainMenu];
        let count: isize = objc2::msg_send![main_menu, numberOfItems];
        if count < 2 {
            return;
        }
        let win_item: *mut AnyObject = objc2::msg_send![main_menu, itemAtIndex: count - 2];
        let win_ns: *mut AnyObject = objc2::msg_send![win_item, submenu];
        let _: () = objc2::msg_send![ns_app, setWindowsMenu: win_ns];

        // macOS Sequoia injects window-tile items (Fill, Center, Move & Resize,
        // …) at the top of the windowsMenu, which pushes our Minimize item down.
        // Walk the items, find the one that fires `performMiniaturize:`, and move
        // it to index 0 so it appears first — matching Safari/Finder layout.
        let target_sel: Sel = sel!(performMiniaturize:);
        let n: isize = objc2::msg_send![win_ns, numberOfItems];
        let mut minimize_idx: isize = -1;
        for i in 0..n {
            let item: *mut AnyObject = objc2::msg_send![win_ns, itemAtIndex: i];
            let action: Sel = objc2::msg_send![item, action];
            if action == target_sel {
                minimize_idx = i;
                break;
            }
        }
        if minimize_idx > 0 {
            let item: *mut AnyObject = objc2::msg_send![win_ns, itemAtIndex: minimize_idx];
            let _: *mut AnyObject = objc2::msg_send![item, retain];
            let _: () = objc2::msg_send![win_ns, removeItemAtIndex: minimize_idx];
            let _: () = objc2::msg_send![win_ns, insertItem: item, atIndex: 0_isize];
            let _: () = objc2::msg_send![item, release];
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn register_help_menu() {}

#[cfg(not(target_os = "macos"))]
pub fn register_window_menu() {}

// ── Selection-dependent menu items ─────────────────────────

/// IDs of menu items that require a selection in the workspace.
pub(crate) const SELECTION_ITEMS: &[&str] = &[
    "duplicate",
    "delete",
    "group",
    "ungroup",
    "create-component",
    "detach-component",
    "bool-union",
    "bool-difference",
    "bool-intersection",
    "bool-exclude",
    "flip-h",
    "flip-v",
    "bring-forward",
    "bring-front",
    "send-backward",
    "send-back",
    "align-left",
    "align-hcenter",
    "align-right",
    "align-top",
    "align-vcenter",
    "align-bottom",
    "dist-h",
    "dist-v",
    "add-flex",
    "add-grid",
    "rename",
    "selection-to-board",
    "focus-on",
    "toggle-visibility",
    "toggle-lock",
    "set-thumbnail",
];

/// Shape types that support boolean operations.
pub(crate) const BOOL_ELIGIBLE_TYPES: &[&str] =
    &["rect", "circle", "path", "bool", "image", "svg-raw"];

/// Shape types that can be ungrouped.
pub(crate) const UNGROUP_ELIGIBLE_TYPES: &[&str] = &["group", "bool", "board"];

/// Determine whether a specific menu item should be enabled based on
/// the selection count, types of selected shapes, and component/instance flags.
fn is_item_enabled(id: &str, count: u64, types: &[String], flags: &[String]) -> bool {
    if count == 0 {
        return false;
    }
    // If types are unavailable (e.g. frontend not rebuilt yet), fall back to
    // the old behaviour: enabled whenever count > 0.
    if types.is_empty() {
        return true;
    }
    let is_component = flags.iter().any(|f| f == "component");
    let is_instance = flags.iter().any(|f| f == "instance");
    let is_focused = flags.iter().any(|f| f == "focused");
    match id {
        // Boolean operations: need 2+ shapes, all must be geometric
        "bool-union" | "bool-difference" | "bool-intersection" | "bool-exclude" => {
            count >= 2
                && types
                    .iter()
                    .all(|t| BOOL_ELIGIBLE_TYPES.contains(&t.as_str()))
        }
        // Group: enabled whenever anything is selected (Penpot allows grouping a single object)
        "group" => true,
        // Ungroup: only for plain groups or bools, not components/instances
        "ungroup" => {
            !is_component
                && !is_instance
                && types
                    .iter()
                    .any(|t| UNGROUP_ELIGIBLE_TYPES.contains(&t.as_str()))
        }
        // Detach: only for instances (copies), not main components
        "detach-component" => is_instance && !is_component,
        // Focus: always enabled when in focus mode (to exit), needs selection to enter
        "focus-on" => is_focused || count > 0,
        // Everything else: enabled when anything is selected
        _ => true,
    }
}

/// Get the current UI language.
pub fn current_lang() -> String {
    CURRENT_LANG
        .get()
        .and_then(|lk| lk.read().ok().map(|l| l.clone()))
        .unwrap_or_else(|| "en".to_string())
}

/// Update a menu item's label and enabled state based on selection.
pub fn update_menu_item<R: tauri::Runtime>(
    mi: &tauri::menu::MenuItem<R>,
    count: u64,
    types: &[String],
    flags: &[String],
    lang: &str,
) {
    let id = &mi.id().0;
    if !SELECTION_ITEMS.contains(&id.as_str()) {
        return;
    }
    let _ = mi.set_enabled(is_item_enabled(id, count, types, flags));

    // Dynamic label changes based on component/instance/focus flags
    let is_component = flags.iter().any(|f| f == "component");
    let is_instance = flags.iter().any(|f| f == "instance");
    let is_focused = flags.iter().any(|f| f == "focused");
    match id.as_str() {
        "create-component" => {
            let key = if is_component {
                "edit.create-variant"
            } else {
                "edit.create-component"
            };
            let label = i18n::t(lang, key);
            let _ = mi.set_text(label);
        }
        "detach-component" => {
            let key = if is_instance {
                "edit.detach-instance"
            } else {
                "edit.detach-component"
            };
            let label = i18n::t(lang, key);
            let _ = mi.set_text(label);
        }
        "focus-on" => {
            let key = if is_focused {
                "edit.focus-off"
            } else {
                "edit.focus-on"
            };
            let label = format!("{}\t\t{}", i18n::t(lang, key), prettify_shortcut("F"));
            let _ = mi.set_text(label);
        }
        _ => {}
    }
}

pub fn update_selection_items(
    app: &tauri::AppHandle,
    count: u64,
    types: &[String],
    flags: &[String],
) {
    let lang = current_lang();
    if let Some(menu) = app.menu() {
        // Menu.get() doesn't search submenus, so iterate through them
        for kind in menu.items().unwrap_or_default() {
            if let MenuItemKind::Submenu(sub) = kind {
                for item in sub.items().unwrap_or_default() {
                    if let MenuItemKind::MenuItem(mi) = &item {
                        update_menu_item(&mi, count, types, flags, &lang);
                    }
                    // Also check nested submenus (e.g. Zoom, Ordering)
                    if let MenuItemKind::Submenu(nested) = &item {
                        for nested_item in nested.items().unwrap_or_default() {
                            if let MenuItemKind::MenuItem(mi) = &nested_item {
                                update_menu_item(&mi, count, types, flags, &lang);
                            }
                        }
                    }
                }
            }
        }
    }
}

// ── Shortcut prettifier ────────────────────────────────────

#[cfg(target_os = "macos")]
fn prettify_shortcut(raw: &str) -> String {
    if !raw.contains('+') || raw == "+" {
        return raw.to_string();
    }
    let parts: Vec<&str> = raw.split('+').collect();
    let mut result = String::new();
    for (i, part) in parts.iter().enumerate() {
        let is_last = i == parts.len() - 1;
        match part.trim() {
            "Ctrl" => result.push('⌃'),
            "Alt" => result.push('⌥'),
            "Shift" => result.push('⇧'),
            "Cmd" => result.push('⌘'),
            key if is_last => match key {
                "Backspace" => result.push('⌫'),
                other => result.push_str(&other.to_uppercase()),
            },
            key => result.push_str(key),
        }
    }
    result
}

#[cfg(not(target_os = "macos"))]
fn prettify_shortcut(raw: &str) -> String {
    raw.to_string()
}

fn strip_shortcut_hint(label: &str) -> &str {
    label.split("\t\t").next().unwrap_or(label)
}

// ── Menu Builder ────────────────────────────────────────────

pub fn build_menu(
    app: &tauri::AppHandle,
    mode: &str,
) -> Result<
    (
        tauri::menu::Menu<tauri::Wry>,
        tauri::menu::Submenu<tauri::Wry>,
    ),
    Box<dyn std::error::Error>,
> {
    // Get language from config
    let lang = APP_HANDLE
        .get()
        .and_then(|a| a.try_state::<crate::SharedConfig>())
        .and_then(|c| c.try_read().ok().map(|c| c.language.clone()))
        .unwrap_or_else(|| "en".to_string());
    let t = |key: &str| i18n::t(&lang, key);
    // Translated label with optional shortcut hint:
    // d("key", "Cmd+X") → "Translated\t\tCmd+X"
    // For menu items with real native accelerators, the macro strips the hint
    // so the shortcut is only shown once by the platform menu renderer.
    let d = |key: &str, shortcut: &str| format!("{}\t\t{}", t(key), prettify_shortcut(shortcut));
    use tauri::menu::AboutMetadata;

    let about_metadata = AboutMetadata {
        name: Some("Penpot Desktop".into()),
        version: Some(app.package_info().version.to_string()),
        copyright: Some("© 2026 Penpot Desktop".into()),
        website: Some("https://penpot.app".into()),
        website_label: Some("penpot.app".into()),
        icon: Some(tauri::image::Image::from_bytes(include_bytes!(
            "../icons/128x128@2x.png"
        ))?),
        ..Default::default()
    };

    macro_rules! mi {
        ($app:expr, $id:expr, $label:expr) => {
            MenuItemBuilder::with_id($id, &$label.replace('&', "&&")).build($app)?
        };
        ($app:expr, $id:expr, $label:expr, $accel:expr) => {
            MenuItemBuilder::with_id($id, &strip_shortcut_hint(&$label).replace('&', "&&"))
                .accelerator($accel)
                .build($app)?
        };
    }

    // ── App (always) ──
    let app_submenu = SubmenuBuilder::new(app, "Penpot Desktop")
        .item(&PredefinedMenuItem::about(
            app,
            Some(&t("app.about")),
            Some(about_metadata),
        )?)
        .separator()
        .item(&mi!(app, "settings", t("app.settings"), "CmdOrCtrl+,"))
        .separator()
        .services()
        .separator()
        .item(&PredefinedMenuItem::hide(app, Some(&t("app.hide-app")))?)
        .item(&PredefinedMenuItem::hide_others(
            app,
            Some(&t("app.hide-others")),
        )?)
        .item(&PredefinedMenuItem::show_all(
            app,
            Some(&t("app.show-all")),
        )?)
        .separator()
        .item(&PredefinedMenuItem::quit(app, Some(&t("app.quit")))?)
        .build()?;

    // ── Edit (always) ──
    // Native accelerators trigger menu events; main.rs forwards those events
    // back into Penpot's shortcut handler via window.__penpotKey(...).
    let mut edit = SubmenuBuilder::new(app, &t("edit.edit"))
        .item(&mi!(app, "undo", d("edit.undo", "Cmd+Z"), "CmdOrCtrl+Z"))
        .item(&mi!(
            app,
            "redo",
            d("edit.redo", "Cmd+Shift+Z"),
            "CmdOrCtrl+Shift+Z"
        ))
        .separator()
        .item(&mi!(app, "cut", d("edit.cut", "Cmd+X"), "CmdOrCtrl+X"))
        .item(&mi!(app, "copy", d("edit.copy", "Cmd+C"), "CmdOrCtrl+C"))
        .item(&mi!(app, "paste", d("edit.paste", "Cmd+V"), "CmdOrCtrl+V"))
        .separator()
        .item(&mi!(
            app,
            "select-all",
            d("edit.select-all", "Cmd+A"),
            "CmdOrCtrl+A"
        ));

    if mode == "workspace" {
        edit = edit
            .separator()
            .item(&mi!(
                app,
                "duplicate",
                d("edit.duplicate", "Cmd+D"),
                "CmdOrCtrl+D"
            ))
            .item(&mi!(app, "delete", d("edit.delete", "Backspace")))
            .separator()
            .item(&mi!(app, "group", d("edit.group", "Cmd+G"), "CmdOrCtrl+G"))
            .item(&mi!(app, "ungroup", d("edit.ungroup", "Shift+G")))
            .separator()
            .item(&mi!(
                app,
                "create-component",
                d("edit.create-component", "Cmd+K"),
                "CmdOrCtrl+K"
            ))
            .item(&mi!(
                app,
                "detach-component",
                d("edit.detach-component", "Cmd+Shift+K"),
                "CmdOrCtrl+Shift+K"
            ))
            .separator()
            .item(&mi!(app, "rename", d("edit.rename", "Alt+N")))
            .item(&mi!(
                app,
                "selection-to-board",
                d("edit.selection-to-board", "Cmd+Alt+G"),
                "CmdOrCtrl+Alt+G"
            ))
            .separator()
            .item(&mi!(app, "focus-on", d("edit.focus-on", "F")))
            .item(&mi!(
                app,
                "toggle-visibility",
                d("edit.toggle-visibility", "Cmd+Shift+H"),
                "CmdOrCtrl+Shift+H"
            ))
            .item(&mi!(
                app,
                "toggle-lock",
                d("edit.toggle-lock", "Cmd+Shift+L"),
                "CmdOrCtrl+Shift+L"
            ))
            .separator()
            .item(&mi!(
                app,
                "set-thumbnail",
                d("edit.set-thumbnail", "Shift+T")
            ));
    }
    let edit_submenu = edit.build()?;

    // ── View ──
    let mut view = SubmenuBuilder::new(app, &t("view.view"));

    if mode == "workspace" {
        let zoom_submenu = SubmenuBuilder::new(app, &t("view.zoom"))
            .item(&mi!(app, "zoom-in", d("view.zoom-in", "+")))
            .item(&mi!(app, "zoom-out", d("view.zoom-out", "\u{2212}")))
            .item(&mi!(
                app,
                "zoom-reset",
                d("view.zoom-reset", "Shift+0"),
                "Shift+0"
            ))
            .item(&mi!(
                app,
                "zoom-fit",
                d("view.zoom-fit", "Shift+1"),
                "Shift+1"
            ))
            .item(&mi!(
                app,
                "zoom-selected",
                d("view.zoom-selected", "Shift+2"),
                "Shift+2"
            ))
            .build()?;

        let panels_submenu = SubmenuBuilder::new(app, &t("view.panels"))
            .item(&mi!(
                app,
                "toggle-layers",
                d("view.layers", "Alt+L"),
                "Alt+L"
            ))
            .item(&mi!(
                app,
                "toggle-assets",
                d("view.assets", "Alt+I"),
                "Alt+I"
            ))
            .item(&mi!(
                app,
                "toggle-palette",
                d("view.color-palette", "Alt+P"),
                "Alt+P"
            ))
            .item(&mi!(
                app,
                "toggle-history",
                d("view.history", "Cmd+Alt+H"),
                "CmdOrCtrl+Alt+H"
            ))
            .build()?;

        view = view
            .item(&zoom_submenu)
            .separator()
            .item(&mi!(
                app,
                "toggle-rulers",
                d("view.rulers", "Cmd+Shift+R"),
                "CmdOrCtrl+Shift+R"
            ))
            .item(&mi!(
                app,
                "toggle-guides",
                d("view.guides", "Cmd+'"),
                "CmdOrCtrl+'"
            ))
            .item(&mi!(
                app,
                "toggle-grid",
                d("view.pixel-grid", "Shift+,"),
                "Shift+Comma"
            ))
            .separator()
            .item(&panels_submenu)
            .item(&mi!(app, "hide-ui", d("view.hide-ui", "\\")))
            .separator();
    }

    view = view
        .item(&mi!(app, "toggle-theme", t("view.toggle-theme"), "Alt+M"))
        .item(&mi!(
            app,
            "fullscreen",
            t("view.fullscreen"),
            "Ctrl+CmdOrCtrl+F"
        ))
        .separator()
        .item(&mi!(app, "devtools", t("view.devtools"), "CmdOrCtrl+Alt+I"));
    let view_submenu = view.build()?;

    // ── Window (always) ──
    // Tab/window creation lives in the File menu. AppKit automatically appends
    // the localized standard items (Zoom, Move & Resize, Tile, tab navigation,
    // open window list, etc.) once `register_window_menu()` runs.
    let window_submenu = SubmenuBuilder::new(app, &t("window.window"))
        .item(&PredefinedMenuItem::minimize(
            app,
            Some(&t("app.minimize")),
        )?)
        .build()?;

    let mut menu_builder = MenuBuilder::new(app).item(&app_submenu);

    if mode == "dashboard" {
        // ── File (dashboard) ──
        // Recently closed tabs submenu — same shape as the workspace version.
        let closed_tabs = get_closed_tabs();
        let has_closed = !closed_tabs.is_empty();
        let recently_closed_title = if has_closed {
            format!("{} ({})", t("file.recently-closed"), closed_tabs.len())
        } else {
            t("file.recently-closed")
        };
        let mut recently_closed_builder =
            SubmenuBuilder::new(app, &recently_closed_title);
        if has_closed {
            for (i, tab) in closed_tabs.iter().enumerate().take(10) {
                let label = closed_tab_label(tab);
                let id = format!("reopen-closed-{i}");
                recently_closed_builder = recently_closed_builder.item(&mi!(app, id, label));
            }
        } else {
            let placeholder = MenuItemBuilder::with_id(
                "recently-closed-empty",
                &t("file.recently-closed-empty").replace('&', "&&"),
            )
            .enabled(false)
            .build(app)?;
            recently_closed_builder = recently_closed_builder.item(&placeholder);
        }
        let recently_closed_submenu = recently_closed_builder.build()?;

        let reopen_item = MenuItemBuilder::with_id(
            "reopen-closed-tab",
            &t("file.reopen-tab").replace('&', "&&"),
        )
        .accelerator("CmdOrCtrl+Shift+T")
        .enabled(has_closed)
        .build(app)?;

        let file_submenu = SubmenuBuilder::new(app, &t("file.file"))
            .item(&mi!(app, "new-tab", t("window.new-tab"), "CmdOrCtrl+T"))
            .item(&mi!(
                app,
                "new-window",
                t("window.new-window"),
                "CmdOrCtrl+N"
            ))
            .item(&mi!(app, "open-url-from-clipboard", t("file.open-url")))
            .separator()
            .item(&mi!(app, "new-project", d("file.new-project", "+")))
            .separator()
            .item(&mi!(
                app,
                "reload-tab",
                t("window.reload-tab"),
                "CmdOrCtrl+R"
            ))
            .separator()
            .item(&mi!(app, "close-tab", t("app.close-window"), "CmdOrCtrl+W"))
            .item(&reopen_item)
            .item(&recently_closed_submenu)
            .build()?;

        // ── Go (dashboard) ──
        let go_submenu = SubmenuBuilder::new(app, &t("go.go"))
            .item(&mi!(app, "go-drafts", d("go.drafts", "G D")))
            .item(&mi!(app, "go-libs", d("go.libraries", "G L")))
            .item(&mi!(
                app,
                "go-search",
                d("go.search", "Cmd+F"),
                "CmdOrCtrl+F"
            ))
            .build()?;

        menu_builder = menu_builder
            .item(&file_submenu)
            .item(&edit_submenu)
            .item(&view_submenu)
            .item(&go_submenu);
    } else {
        // ── File (workspace) ──
        // Recently closed tabs submenu (dynamic). Built from CLOSED_TABS; each
        // entry gets id `reopen-closed-<i>` where 0 = most recent.
        let closed_tabs = get_closed_tabs();
        let has_closed = !closed_tabs.is_empty();
        let recently_closed_title = if has_closed {
            format!("{} ({})", t("file.recently-closed"), closed_tabs.len())
        } else {
            t("file.recently-closed")
        };
        let mut recently_closed_builder =
            SubmenuBuilder::new(app, &recently_closed_title);
        if has_closed {
            for (i, tab) in closed_tabs.iter().enumerate().take(10) {
                let label = closed_tab_label(tab);
                let id = format!("reopen-closed-{i}");
                recently_closed_builder = recently_closed_builder.item(&mi!(app, id, label));
            }
        } else {
            let placeholder = MenuItemBuilder::with_id(
                "recently-closed-empty",
                &t("file.recently-closed-empty").replace('&', "&&"),
            )
            .enabled(false)
            .build(app)?;
            recently_closed_builder = recently_closed_builder.item(&placeholder);
        }
        let recently_closed_submenu = recently_closed_builder.build()?;

        let reopen_item = MenuItemBuilder::with_id(
            "reopen-closed-tab",
            &t("file.reopen-tab").replace('&', "&&"),
        )
        .accelerator("CmdOrCtrl+Shift+T")
        .enabled(has_closed)
        .build(app)?;

        let file_submenu = SubmenuBuilder::new(app, &t("file.file"))
            .item(&mi!(app, "new-tab", t("window.new-tab"), "CmdOrCtrl+T"))
            .item(&mi!(
                app,
                "new-window",
                t("window.new-window"),
                "CmdOrCtrl+N"
            ))
            .item(&mi!(app, "copy-file-url", t("file.copy-url")))
            .item(&mi!(app, "open-url-from-clipboard", t("file.open-url")))
            .separator()
            .item(&mi!(
                app,
                "reload-tab",
                t("window.reload-tab"),
                "CmdOrCtrl+R"
            ))
            .separator()
            .item(&mi!(app, "toggle-shared", t("file.add-shared")))
            .separator()
            .item(&mi!(app, "pin-version", t("file.pin-version")))
            .item(&mi!(
                app,
                "show-version-history",
                d("file.version-history", "Cmd+Alt+H")
            ))
            .separator()
            .item(&mi!(app, "export", t("file.export"), "CmdOrCtrl+Shift+E"))
            .item(&mi!(app, "download-binary", t("file.download-binary")))
            .item(&mi!(app, "export-frames-pdf", t("file.export-frames-pdf")))
            .separator()
            .item(&reopen_item)
            .item(&recently_closed_submenu)
            .item(&mi!(app, "close-tab", t("app.close-window"), "CmdOrCtrl+W"))
            .build()?;

        // ── Shape ──
        let tools_submenu = SubmenuBuilder::new(app, &t("shape.tools"))
            .item(&mi!(app, "tool-board", d("shape.board", "B"), "B"))
            .item(&mi!(app, "tool-rect", d("shape.rectangle", "R"), "R"))
            .item(&mi!(app, "tool-ellipse", d("shape.ellipse", "E"), "E"))
            .item(&mi!(app, "tool-text", d("shape.text", "T"), "T"))
            .item(&mi!(app, "tool-path", d("shape.path", "P"), "P"))
            .item(&mi!(app, "tool-curve", d("shape.curve", "Shift+C"), "Shift+C"))
            .item(&mi!(
                app,
                "insert-image",
                d("shape.insert-image", "Shift+K"),
                "Shift+K"
            ))
            .build()?;

        let order_submenu = SubmenuBuilder::new(app, &t("shape.order"))
            .item(&mi!(
                app,
                "bring-forward",
                d("shape.bring-forward", "Cmd+\u{2191}"),
                "CmdOrCtrl+Up"
            ))
            .item(&mi!(
                app,
                "bring-front",
                d("shape.bring-front", "Cmd+Shift+\u{2191}"),
                "CmdOrCtrl+Shift+Up"
            ))
            .item(&mi!(
                app,
                "send-backward",
                d("shape.send-backward", "Cmd+\u{2193}"),
                "CmdOrCtrl+Down"
            ))
            .item(&mi!(
                app,
                "send-back",
                d("shape.send-back", "Cmd+Shift+\u{2193}"),
                "CmdOrCtrl+Shift+Down"
            ))
            .build()?;

        let bool_submenu = SubmenuBuilder::new(app, &t("shape.boolean"))
            .item(&mi!(
                app,
                "bool-union",
                d("shape.union", "Cmd+Alt+U"),
                "CmdOrCtrl+Alt+U"
            ))
            .item(&mi!(
                app,
                "bool-difference",
                d("shape.difference", "Cmd+Alt+D"),
                "CmdOrCtrl+Alt+D"
            ))
            .item(&mi!(
                app,
                "bool-intersection",
                d("shape.intersection", "Cmd+Alt+I"),
                "CmdOrCtrl+Alt+I"
            ))
            .item(&mi!(
                app,
                "bool-exclude",
                d("shape.exclude", "Cmd+Alt+E"),
                "CmdOrCtrl+Alt+E"
            ))
            .build()?;

        let shape_submenu = SubmenuBuilder::new(app, &t("shape.shape"))
            .item(&tools_submenu)
            .separator()
            .item(&mi!(app, "flip-h", d("shape.flip-h", "Shift+H"), "Shift+H"))
            .item(&mi!(app, "flip-v", d("shape.flip-v", "Shift+V"), "Shift+V"))
            .separator()
            .item(&order_submenu)
            .item(&bool_submenu)
            .separator()
            .item(&mi!(
                app,
                "toggle-layout-flex",
                d("shape.flex-layout", "Shift+A"),
                "Shift+A"
            ))
            .item(&mi!(
                app,
                "toggle-layout-grid",
                d("shape.grid-layout", "Cmd+Shift+A"),
                "CmdOrCtrl+Shift+A"
            ))
            .separator()
            .item(&mi!(
                app,
                "align-left",
                d("shape.align-left", "Alt+A"),
                "Alt+A"
            ))
            .item(&mi!(
                app,
                "align-hcenter",
                d("shape.align-hcenter", "Alt+H"),
                "Alt+H"
            ))
            .item(&mi!(
                app,
                "align-right",
                d("shape.align-right", "Alt+D"),
                "Alt+D"
            ))
            .separator()
            .item(&mi!(
                app,
                "align-top",
                d("shape.align-top", "Alt+W"),
                "Alt+W"
            ))
            .item(&mi!(
                app,
                "align-vcenter",
                d("shape.align-vcenter", "Alt+V"),
                "Alt+V"
            ))
            .item(&mi!(
                app,
                "align-bottom",
                d("shape.align-bottom", "Alt+S"),
                "Alt+S"
            ))
            .separator()
            .item(&mi!(
                app,
                "dist-h",
                d("shape.dist-h", "Cmd+Shift+Alt+H"),
                "CmdOrCtrl+Shift+Alt+H"
            ))
            .item(&mi!(
                app,
                "dist-v",
                d("shape.dist-v", "Cmd+Shift+Alt+V"),
                "CmdOrCtrl+Shift+Alt+V"
            ))
            .build()?;

        // ── Go (workspace) ──
        let go_submenu = SubmenuBuilder::new(app, &t("go.go"))
            .item(&mi!(app, "go-viewer", d("go.open-viewer", "G V")))
            .item(&mi!(app, "go-inspect", d("go.open-inspect", "G I")))
            .separator()
            .item(&mi!(app, "go-dashboard", d("go.back-dashboard", "G D")))
            .build()?;

        // ── Plugins (workspace) — dynamic list from profile API ──
        let plugins = get_plugins();
        let mut plugins_builder =
            SubmenuBuilder::new(app, &t("plugins.plugins"))
                .item(&mi!(
                    app,
                    "plugins-manager",
                    d("plugins.manager", "Cmd+Alt+P"),
                    "CmdOrCtrl+Alt+P"
                ))
                .separator();
        if plugins.is_empty() {
            let placeholder = MenuItemBuilder::with_id(
                "plugins-empty",
                &t("plugins.no-plugins").replace('&', "&&"),
            )
            .enabled(false)
            .build(app)?;
            plugins_builder = plugins_builder.item(&placeholder);
        } else {
            for (i, plugin) in plugins.iter().enumerate() {
                let id = format!("plugin-{i}");
                plugins_builder =
                    plugins_builder.item(&mi!(app, id, plugin.name.clone()));
            }
        }
        let plugins_submenu = plugins_builder.build()?;

        menu_builder = menu_builder
            .item(&file_submenu)
            .item(&edit_submenu)
            .item(&view_submenu)
            .item(&shape_submenu)
            .item(&go_submenu)
            .item(&plugins_submenu);
    }

    // ── Help (always) ──
    let mut help = SubmenuBuilder::new(app, &t("help.help")).item(&mi!(
        app,
        "help-guide",
        t("help.user-guide")
    ));
    if mode == "workspace" {
        help = help.item(&mi!(app, "help-shortcuts", d("help.shortcuts", "?")));
    }
    let help_submenu = help
        .item(&mi!(app, "help-tutorials", t("help.tutorials")))
        .item(&mi!(app, "help-courses", t("help.courses")))
        .separator()
        .item(&mi!(app, "help-plugins", t("help.plugins")))
        .item(&mi!(app, "help-libraries", t("help.libs-templates")))
        .separator()
        .item(&mi!(app, "help-community", t("help.community")))
        .item(&mi!(app, "help-github", t("help.github")))
        .item(&mi!(app, "help-feedback", t("help.feedback")))
        .separator()
        .item(&mi!(app, "help-website", t("help.website")))
        .item(&mi!(app, "help-release-notes", t("help.release-notes")))
        .build()?;
    menu_builder = menu_builder.item(&window_submenu).item(&help_submenu);
    let menu = menu_builder.build()?;
    Ok((menu, help_submenu))
}
