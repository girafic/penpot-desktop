// ── Window creation helpers ─────────────────────────────────

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use tauri::Manager;

static TAB_COUNTER: AtomicU32 = AtomicU32::new(1);

/// JS helper injected into every Penpot webview on page load. Exposes
/// `window.__penpotDesktopFileAction(idOrIds)` which opens Penpot's
/// workspace burger menu → File submenu, clicks the target `#file-menu-*`
/// item, then closes the menu. Used by the native File menu handlers for
/// actions that have no Mousetrap shortcut (pin version, download .penpot,
/// export frames as PDF, toggle shared library).
/// Overrides `window.open()` so that Penpot's "Open in new tab/window"
/// actions (viewer, inspect, etc.) create a proper Tauri tab instead of
/// navigating in-place or being blocked by WKWebView.
pub const WINDOW_OPEN_OVERRIDE: &str = "\
if(!window.__penpotOpenPatched){window.__penpotOpenPatched=true;\
  var _origOpen=window.open.bind(window);\
  window.open=function(url,name,features){\
    if(url&&String(url).indexOf('127.0.0.1')!==-1){\
      fetch('/__penpot_desktop/open-tab',{method:'POST',\
        headers:{'Content-Type':'application/json'},\
        body:JSON.stringify({url:String(url)})});return null}\
    return _origOpen(url,name,features)};}";

/// Polls the Penpot profile API for installed plugins and reports them
/// to the desktop wrapper. Runs once after 3 s, then every 30 s.
pub const PLUGIN_POLLER: &str = "\
if(!window.__penpotPluginPoller){window.__penpotPluginPoller=true;\
  function __pptParse(v){\
    if(typeof v==='string'){try{v=JSON.parse(v)}catch(e){return v}}\
    if(Array.isArray(v)&&v[0]==='^ '){\
      var o={};for(var i=1;i<v.length;i+=2)o[v[i]]=__pptParse(v[i+1]);return o}\
    if(Array.isArray(v))return v.map(__pptParse);\
    return v}\
  function __pptPollPlugins(){\
    fetch('/api/main/methods/get-profile',{\
      headers:{'accept':'application/transit+json'},\
      credentials:'include'})\
    .then(function(r){return r.text()})\
    .then(function(t){try{\
      var j=__pptParse(t);\
      var pl=__pptParse(j['~:props']);\
      pl=__pptParse(pl['~:plugins']);\
      var d=__pptParse(pl['~:data']);\
      var dk=Object.keys(d||{});\
      var cacheMap={};\
      if(dk.length>1){var fk=Object.keys(d[dk[0]]||{}),sk=Object.keys(d[dk[1]]||{});\
        for(var ci=0;ci<sk.length&&ci<fk.length;ci++)\
          if(sk[ci][0]==='^')cacheMap[sk[ci]]=fk[ci]}\
      function resolveKeys(obj){if(!obj||typeof obj!=='object')return obj;\
        var r={};Object.keys(obj).forEach(function(k){\
          r[cacheMap[k]||k]=obj[k]});return r}\
      var list=dk.map(function(k){\
        var e=resolveKeys(__pptParse(d[k]))||{};\
        return{id:e['~:plugin-id']||k,name:e['~:name']||''}})\
        .filter(function(p){return p.name});\
      fetch('/__penpot_desktop/update-plugins',{method:'POST',\
        headers:{'Content-Type':'application/json'},\
        body:JSON.stringify({plugins:list})})\
    }catch(e){console.warn('[penpot-desktop] plugin poll error:',e)}})\
    .catch(function(e){console.warn('[penpot-desktop] plugin fetch error:',e)})}\
  setTimeout(__pptPollPlugins,3000);\
  setInterval(__pptPollPlugins,30000);}";

/// Opens burger → Plugins submenu → clicks the Nth plugin item.
pub const PLUGIN_LAUNCHER: &str = "\
if(!window.__penpotDesktopPluginAction){(function(){\
  function poll(fn,ms){return new Promise(function(ok,no){\
    var t=Date.now();(function f(){var r=fn();if(r)return ok(r);\
    if(Date.now()-t>ms)return no();requestAnimationFrame(f)})()})}\
  window.__penpotDesktopPluginAction=function(name){\
    var btn=document.querySelector('[class*=\"menu-section\"] button');\
    if(!btn)return;\
    btn.click();\
    poll(function(){return document.querySelector('[data-testid=\"plugins\"]')},800)\
    .then(function(p){p.click();\
      return poll(function(){\
        var items=document.querySelectorAll('[class*=\"plugins\"] [class*=\"submenu-item\"]');\
        for(var i=0;i<items.length;i++){\
          if(items[i].textContent.trim()===name)return items[i]}\
        return null},800)})\
    .then(function(t){t.click()})\
    .catch(function(){})};\
  window.__penpotDesktopOpenPluginManager=function(){\
    var btn=document.querySelector('[class*=\"menu-section\"] button');\
    if(!btn)return;\
    btn.click();\
    poll(function(){return document.querySelector('[data-testid=\"plugins\"]')},800)\
    .then(function(p){p.click();\
      return poll(function(){return document.getElementById('file-menu-open-plugins')},800)})\
    .then(function(t){t.click()})\
    .catch(function(){})};\
})();}";

pub const FILE_MENU_HELPER: &str = "\
if(!window.__penpotDesktopFileAction){(function(){\
  function poll(fn,ms){return new Promise(function(ok,no){\
    var t=Date.now();(function f(){var r=fn();if(r)return ok(r);\
    if(Date.now()-t>ms)return no();requestAnimationFrame(f)})()})}\
  function any(ids){for(var i=0;i<ids.length;i++){\
    var e=document.getElementById(ids[i]);if(e)return e}return null}\
  window.__penpotDesktopFileAction=function(raw){\
    var ids=Array.isArray(raw)?raw:[raw];\
    var btn=document.querySelector('[class*=\"menu-section\"] button');\
    if(!btn)return;\
    btn.click();\
    poll(function(){return document.getElementById('file-menu-file')},800)\
    .then(function(f){f.click();\
      return poll(function(){return any(ids)},800)})\
    .then(function(t){t.click()})\
    .catch(function(){})};\
})();}";


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
