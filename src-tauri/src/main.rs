use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

mod i18n;

use serde::{Deserialize, Serialize};
use tauri::Manager;
use tokio::sync::{Mutex, RwLock};
use warp::Filter;

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

// ── Config ───────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
struct AppConfig {
    backend_url: String,
    recent_urls: Vec<String>,
    proxy_port: u16,
    #[serde(default = "default_renderer")]
    renderer: String,
    #[serde(default = "default_language")]
    language: String,
    /// Legacy field — plain URL list from older configs. Migrated to
    /// `open_groups` on load.
    #[serde(default)]
    open_tabs: Vec<String>,
    /// Each inner Vec is one window-group (= one macOS tab bar).
    /// The first group is restored into the main window; additional
    /// groups each become a standalone top-level window with their
    /// own tabs.
    #[serde(default)]
    open_groups: Vec<Vec<String>>,
}

fn default_renderer() -> String {
    "classic".into()
}
fn default_language() -> String {
    "en".into()
}

/// Map desktop locale codes to Penpot frontend locale codes.
fn desktop_to_penpot_locale(desktop: &str) -> Option<&'static str> {
    match desktop {
        "en" => Some("en"),
        "de" => Some("de"),
        "es" => Some("es"),
        "fr" => Some("fr"),
        "it" => Some("it"),
        "tr" => Some("tr"),
        "ru" => Some("ru"),
        "ko" => Some("ko"),
        "ar" => Some("ar"),
        "ca" => Some("ca"),
        "nl" => Some("nl"),
        "pl" => Some("pl"),
        "ro" => Some("ro"),
        "he" => Some("he"),
        "zh_CN" => Some("zh_cn"),
        "jpn_JP" => Some("ja_jp"),
        "pt_BR" => Some("pt_br"),
        "ukr_UA" => Some("uk"),
        _ => None,
    }
}

/// Static portion of the `/__penpot_desktop_config.js` script.
/// The dynamic locale override is prepended at request time.
/// Minimal shim injected into HTML responses served by /__penpot_desktop/cors-proxy.
/// Reroutes cross-origin fetch() and iframe.src through the parent's cors-proxy
/// so plugin UIs can transitively load their own assets without hitting CORS.
const IFRAME_SHIM_JS: &str = r#"(function(){
  var _f = window.fetch.bind(window);
  function proxify(u){
    try {
      var abs = new URL(u, document.baseURI);
      if (abs.origin !== location.origin && /^https?:$/.test(abs.protocol)) {
        return location.origin + '/__penpot_desktop/cors-proxy?url=' + encodeURIComponent(abs.href);
      }
    } catch(e) {}
    return u;
  }
  // Resolve a URL the way <a href> would, for the open-tab handoff.
  function resolveAbs(u){
    try { return new URL(u, document.baseURI).href; } catch(e) { return u; }
  }
  function openExternal(u){
    var abs = resolveAbs(u);
    if (!/^https?:/i.test(abs)) return false;
    try {
      _f(location.origin + '/__penpot_desktop/open-tab', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({url: abs})
      }).catch(function(){});
    } catch(e) {}
    return true;
  }
  window.fetch = function(input, init){
    try {
      var url = typeof input === 'string' ? input : (input && input.url);
      if (url) {
        var p = proxify(url);
        if (p !== url) {
          input = typeof input === 'string' ? p : new Request(p, input);
        }
      }
    } catch(e) {}
    return _f(input, init);
  };
  // Patch HTMLIFrameElement.src setter so nested iframes also route through proxy
  try {
    var d = Object.getOwnPropertyDescriptor(HTMLIFrameElement.prototype, 'src');
    if (d && d.set) {
      Object.defineProperty(HTMLIFrameElement.prototype, 'src', {
        configurable: true,
        enumerable: d.enumerable,
        get: d.get,
        set: function(v){ d.set.call(this, proxify(v)); }
      });
    }
  } catch(e) {}
  // Plugin-side window.open: forward to the parent's open-tab handler so that
  // external URLs land in the system browser instead of a blocked WebView nav.
  window.open = function(url){
    if (url) openExternal(url);
    return null;
  };
  // <a target="_blank"> clicks bypass window.open — intercept them too.
  document.addEventListener('click', function(e){
    var t = e.target;
    while (t && t.nodeType === 1 && t.tagName !== 'A') t = t.parentNode;
    if (!t || t.tagName !== 'A') return;
    var href = t.getAttribute('href');
    var target = t.getAttribute('target');
    if (!href) return;
    var abs = resolveAbs(href);
    var isExternal = false;
    try {
      var u = new URL(abs);
      isExternal = /^https?:$/.test(u.protocol) && u.origin !== location.origin;
    } catch(e) {}
    // Always intercept _blank/_new/_top, regardless of origin — those mean
    // "open elsewhere" and would otherwise spawn an empty popup window.
    if (target === '_blank' || target === '_new' || target === '_top') {
      e.preventDefault();
      e.stopPropagation();
      openExternal(abs);
    }
  }, true);
})();"#;

const DESKTOP_CONFIG_JS: &str = r#"// Penpot Desktop runtime config
(function() {
  // Cross-origin bypass — route 3rd-party fetches AND iframe srcs through warp
  // so plugins (and any other code that talks to non-127.0.0.1 hosts) work
  // despite browser CORS / X-Frame-Options. Same-origin and relative URLs pass
  // through untouched.
  function __penpotProxify(u) {
    try {
      var abs = new URL(u, location.href);
      if (abs.origin !== location.origin && /^https?:$/.test(abs.protocol)) {
        return '/__penpot_desktop/cors-proxy?url=' + encodeURIComponent(abs.href);
      }
    } catch(e) {}
    return u;
  }
  var _origFetch = window.fetch.bind(window);
  window.fetch = function(input, init) {
    try {
      var url = typeof input === 'string' ? input : (input && input.url);
      if (url) {
        var p = __penpotProxify(url);
        if (p !== url) {
          input = typeof input === 'string' ? p : new Request(p, input);
        }
      }
    } catch(e) {}
    return _origFetch(input, init);
  };
  // Inject our click/window.open handlers directly into a same-origin iframe
  // document. Called on every iframe 'load' event. Because the iframe is loaded
  // through cors-proxy on the same origin as the parent, contentDocument is
  // accessible — no HTML rewriting required.
  function __penpotInjectIntoIframe(iframe) {
    try {
      var doc = iframe.contentDocument;
      var win = iframe.contentWindow;
      if (!doc || !win) return;
      if (doc.__penpotInjected) return;
      doc.__penpotInjected = true;
      // Patch the iframe's window.open → forward to open-tab
      win.open = function(url) {
        if (url) {
          var abs;
          try { abs = new win.URL(url, doc.baseURI).href; } catch(e) { abs = url; }
          fetch('/__penpot_desktop/open-tab', {
            method: 'POST',
            headers: {'Content-Type': 'application/json'},
            body: JSON.stringify({url: abs})
          }).catch(function(){});
        }
        return null;
      };
      // Capture-phase click handler for <a target="_blank"> (dynamic-friendly)
      doc.addEventListener('click', function(e) {
        var t = e.target;
        while (t && t.nodeType === 1 && t.tagName !== 'A') t = t.parentNode;
        if (!t || t.tagName !== 'A') return;
        var href = t.getAttribute('href');
        var target = t.getAttribute('target');
        if (!href) return;
        if (target === '_blank' || target === '_new' || target === '_top') {
          e.preventDefault();
          e.stopPropagation();
          var abs;
          try { abs = new win.URL(href, doc.baseURI).href; } catch(e2) { abs = href; }
          fetch('/__penpot_desktop/open-tab', {
            method: 'POST',
            headers: {'Content-Type': 'application/json'},
            body: JSON.stringify({url: abs})
          }).catch(function(){});
        }
      }, true);
    } catch(e) {}
  }

  // Patch HTMLIFrameElement.src setter — Penpot's plugin runtime sets
  // iframe.src directly (in shadow DOM), bypassing setAttribute hooks. The
  // property setter is the only reliable interception point. We also attach
  // a load listener so we can inject handlers into the iframe document once
  // it's ready (more reliable than HTML injection for SPA contents).
  try {
    var __ifd = Object.getOwnPropertyDescriptor(HTMLIFrameElement.prototype, 'src');
    if (__ifd && __ifd.set) {
      Object.defineProperty(HTMLIFrameElement.prototype, 'src', {
        configurable: true,
        enumerable: __ifd.enumerable,
        get: __ifd.get,
        set: function(v) {
          var p = __penpotProxify(v);
          var self = this;
          // Inject handlers once the iframe document is ready. Re-run a few
          // times because SPAs may rewrite the document after initial load.
          self.addEventListener('load', function() {
            __penpotInjectIntoIframe(self);
            var tries = 0;
            var iv = setInterval(function() {
              if (++tries > 10) { clearInterval(iv); return; }
              __penpotInjectIntoIframe(self);
            }, 500);
          });
          __ifd.set.call(this, p);
        }
      });
    }
  } catch(e) {}

  // Fix Cmd+C/X/V/A — WKWebView without PredefinedMenuItems doesn't handle
  // clipboard/selectAll natively. This capture-phase listener handles both
  // input fields (direct DOM manipulation) and canvas (synthetic ClipboardEvent).
  function __readClipboard() {
    try {
      var xhr = new XMLHttpRequest();
      xhr.open('GET', '/__penpot_desktop/clipboard', false);
      xhr.send();
      if (xhr.status === 200) return JSON.parse(xhr.responseText).text || '';
    } catch(ex) {}
    return '';
  }
  document.addEventListener('keydown', function(e) {
    if (!e.metaKey || e.altKey || e.shiftKey) return;
    if (e.key !== 'c' && e.key !== 'x' && e.key !== 'v' && e.key !== 'a') return;
    var el = document.activeElement;
    var tag = el ? el.tagName : '';
    var isInput = tag === 'INPUT' || tag === 'TEXTAREA' || (el && el.isContentEditable);

    if (e.key === 'v') {
      // Paste — WKWebView never fires a native paste ClipboardEvent without
      // PredefinedMenuItems. Handle it ourselves everywhere.
      var text = __readClipboard();
      if (isInput) {
        // Input field: write directly into the DOM
        if (el.isContentEditable) {
          var sel = window.getSelection();
          if (sel.rangeCount) {
            var range = sel.getRangeAt(0);
            range.deleteContents();
            range.insertNode(document.createTextNode(text));
            range.collapse(false);
          }
        } else {
          var start = el.selectionStart || 0;
          var end = el.selectionEnd || 0;
          var val = el.value || '';
          el.value = val.substring(0, start) + text + val.substring(end);
          el.selectionStart = el.selectionEnd = start + text.length;
          el.dispatchEvent(new Event('input', {bubbles: true}));
        }
      } else {
        // Canvas: dispatch a synthetic ClipboardEvent so Penpot gets the data
        var dt = new DataTransfer();
        dt.setData('text/plain', text);
        var ev = new ClipboardEvent('paste', {clipboardData: dt, bubbles: true, cancelable: true});
        (el || document.body).dispatchEvent(ev);
      }
      e.preventDefault();
      e.stopPropagation();
      return;
    }

    // Copy/Cut/SelectAll — only need special handling in input fields
    if (!isInput) return;
    if (e.key === 'a') document.execCommand('selectAll');
    else if (e.key === 'c') document.execCommand('copy');
    else if (e.key === 'x') document.execCommand('cut');
    e.stopPropagation();
  }, true);

  // Patch download links: fetch via proxy, then save via Tauri command (no navigation)
  var origCreateElement = document.createElement.bind(document);
  document.createElement = function(tag) {
    var el = origCreateElement(tag);
    if (tag.toLowerCase() === 'a') {
      var origClick = el.click.bind(el);
      el.click = function() {
        if (el.download && el.href) {
          try {
            // Rewrite to go through proxy if pointing to backend directly
            var url = new URL(el.href, location.origin);
            if (url.origin !== location.origin) {
              url = new URL(url.pathname + url.search, location.origin);
            }
            var filename = el.download;
            // Show save dialog, fetch through proxy, save via Tauri IPC
            var ext = filename.split('.').pop() || '*';
            window.__TAURI__.dialog.save({
              defaultPath: filename,
              filters: [{ name: ext.toUpperCase(), extensions: [ext] }]
            }).then(function(savePath) {
              if (!savePath) return; // User cancelled
              return fetch(url.toString())
                .then(function(r) { return r.arrayBuffer(); })
                .then(function(buf) {
                  return window.__TAURI__.core.invoke('save_download', {
                    data: Array.from(new Uint8Array(buf)),
                    path: savePath
                  });
                })
                .then(function(path) {
                  console.log('[penpot-desktop] saved to', path);
                });
            }).catch(function(e) {
              console.error('[penpot-desktop] download failed', e);
            });
            return;
          } catch(e) {}
        }
        return origClick();
      };
    }
    return el;
  };

  // Rewrite share links: replace proxy URL with real backend URL
  if (window.__penpotBackendOrigin) {
    var _proxyOrigin = location.origin;
    var _backendOrigin = window.__penpotBackendOrigin;
    // Watch for input elements containing the proxy URL and rewrite them
    var _rewriteInputs = function() {
      document.querySelectorAll('input[readonly]').forEach(function(input) {
        if (input.value && input.value.indexOf(_proxyOrigin) === 0) {
          input.value = input.value.replace(_proxyOrigin, _backendOrigin);
        }
      });
    };
    // Also patch clipboard to rewrite copied share links
    var _origWriteText = navigator.clipboard && navigator.clipboard.writeText;
    if (_origWriteText) {
      navigator.clipboard.writeText = function(text) {
        if (text && text.indexOf(_proxyOrigin) === 0) {
          text = text.replace(_proxyOrigin, _backendOrigin);
        }
        return _origWriteText.call(navigator.clipboard, text);
      };
    }
    new MutationObserver(function() { _rewriteInputs(); }).observe(document.body || document.documentElement, {childList: true, subtree: true});
  }

  // Override window.open to open new native tabs for _blank targets
  var _origOpen = window.open;
  window.open = function(url, target, features) {
    if (url && (!target || target === '_blank' || (target && target[0] !== '_'))) {
      // Convert relative URLs to path-only
      var path = url;
      try {
        var parsed = new URL(url, location.origin);
        if (parsed.origin === location.origin) {
          path = parsed.pathname + parsed.search + parsed.hash;
        }
      } catch(e) {}
      fetch('/__penpot_desktop/open-tab', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({url: path})
      }).catch(function(e) { console.error('[penpot-desktop] open-tab failed', e); });
      return null;
    }
    // Non-blank targets: use original (e.g. named windows that navigate in-place)
    return _origOpen ? _origOpen.call(window, url, target, features) : null;
  };

  // Keyboard shortcut bridge for native menus → Mousetrap
  var KEY_CODES = {
    backspace:8, tab:9, enter:13, shift:16, ctrl:17, alt:18, esc:27, space:32,
    pageup:33, pagedown:34, end:35, home:36, left:37, up:38, right:39, down:40,
    del:46, meta:91,
    '0':48,'1':49,'2':50,'3':51,'4':52,'5':53,'6':54,'7':55,'8':56,'9':57,
    a:65,b:66,c:67,d:68,e:69,f:70,g:71,h:72,i:73,j:74,k:75,l:76,m:77,
    n:78,o:79,p:80,q:81,r:82,s:83,t:84,u:85,v:86,w:87,x:88,y:89,z:90,
    '=':187,'+':187,'-':189,',':188,'.':190,"'":222,'\\':220,'/':191,'[':219,']':221,
    ';':186,'_':189
  };

  window.__penpotKey = function(shortcut) {
    var parts = shortcut.split('+');
    // Handle bare "+" key: split('+') gives ["",""], so last part is ""
    var key = parts[parts.length - 1] || '+';
    var code = KEY_CODES[key] || key.charCodeAt(0);
    var opts = {
      bubbles: true,
      cancelable: true,
      metaKey: parts.includes('meta'),
      ctrlKey: parts.includes('ctrl'),
      shiftKey: parts.includes('shift'),
      altKey: parts.includes('alt'),
      keyCode: code,
      which: code,
      key: key
    };
    // For keypress events, Mousetrap uses String.fromCharCode(e.which)
    // to identify the character, so we need the ASCII char code, not the
    // physical key code used for keydown.
    var charCode = key.length === 1 ? key.charCodeAt(0) : code;
    // Override read-only keyCode/which
    var makeEvent = function(type) {
      var c = type === 'keypress' ? charCode : code;
      var e = new KeyboardEvent(type, opts);
      Object.defineProperty(e, 'keyCode', {get: function(){return c;}});
      Object.defineProperty(e, 'which', {get: function(){return c;}});
      return e;
    };
    // Dispatch on viewport or body — must be an Element for .closest() to work
    // Events bubble up to document where Mousetrap catches them
    var el = document.getElementById('app') || document.body;
    el.dispatchEvent(makeEvent('keydown'));
    // Only dispatch keypress when no modifier is held. Mousetrap uses keypress
    // for plain single-character bindings (like '+', '-', '2'), so dispatching
    // a keypress with which=charCode while shift is held would also trigger the
    // plain-character binding (e.g. shift+2 → opacity 20% on top of zoom-fit).
    // A real shift+2 keypress would carry '@' or '"' depending on layout, never '2'.
    var hasModifier = opts.shiftKey || opts.ctrlKey || opts.altKey || opts.metaKey;
    if (!hasModifier) {
      el.dispatchEvent(makeEvent('keypress'));
    }
    el.dispatchEvent(makeEvent('keyup'));
  };
  // Desktop selection bridge — Penpot calls this when selection changes
  var _lastSelCount = -1;
  window.__penpotDesktopOnSelection = function(count) {
    if (count !== _lastSelCount) {
      _lastSelCount = count;
      fetch('/__penpot_desktop/set-selection', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({count: count})
      }).catch(function(){});
    }
  };

  // Watch URL changes to switch menu between dashboard/workspace
  // and update the window/tab title
  var _lastMode = '';
  function updateView() {
    var hash = location.hash || location.href;
    var mode = hash.includes('/workspace') || hash.includes('/view') ? 'workspace' : 'dashboard';
    if (mode !== _lastMode) {
      _lastMode = mode;
      console.log('[penpot-desktop] view mode:', mode, 'hash:', hash);
      fetch('/__penpot_desktop/set-view', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({mode: mode, label: window.__penpotWindowLabel || ''})
      }).catch(function(){});
    }
  }
  window.addEventListener('hashchange', updateView);
  window.addEventListener('popstate', updateView);

  // Notify backend whenever this window/tab gains focus, so the native menu
  // can be swapped to match. macOS native tabs don't reliably surface
  // window-focused events through Tauri, so we drive it from JS instead.
  function notifyFocused() {
    var label = window.__penpotWindowLabel;
    if (!label) return;
    fetch('/__penpot_desktop/window-focused', {
      method: 'POST',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({label: label})
    }).catch(function(){});
  }
  window.addEventListener('focus', notifyFocused);
  document.addEventListener('visibilitychange', function() {
    if (document.visibilityState === 'visible') notifyFocused();
  });
  // Cover the case where focus is already on this tab when the page first loads.
  if (document.hasFocus && document.hasFocus()) {
    setTimeout(notifyFocused, 100);
  }
  // Poll for hash changes and title updates
  var _lastTitle = '';
  setInterval(function() {
    updateView();
    // Sync document.title to native window title
    var t = document.title || '';
    var label = window.__penpotWindowLabel;
    if (t && label && t !== _lastTitle) {
      _lastTitle = t;
      fetch('/__penpot_desktop/set-title', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({label: label, title: t})
      }).catch(function(){});
    }
  }, 1000);

})();
"#;

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            backend_url: String::new(),
            recent_urls: vec!["https://design.penpot.app".into()],
            proxy_port: 7080,
            renderer: default_renderer(),
            language: default_language(),
            open_tabs: vec![],
            open_groups: vec![],
        }
    }
}

fn config_path() -> PathBuf {
    let dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("penpot-desktop");
    fs::create_dir_all(&dir).ok();
    dir.join("config.json")
}

fn load_config() -> AppConfig {
    let mut cfg: AppConfig = fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    // Migrate legacy `open_tabs` → single group in `open_groups`.
    if cfg.open_groups.is_empty() && !cfg.open_tabs.is_empty() {
        cfg.open_groups = vec![cfg.open_tabs.clone()];
    }
    cfg.open_tabs.clear();
    cfg
}

fn save_config(config: &AppConfig) {
    if let Ok(json) = serde_json::to_string_pretty(config) {
        fs::write(config_path(), json).ok();
    }
}

type SharedConfig = Arc<RwLock<AppConfig>>;

use std::sync::OnceLock;
static APP_HANDLE: OnceLock<tauri::AppHandle> = OnceLock::new();

// Ordered list of (label, url) — preserves tab order for session restore
static TAB_URLS: OnceLock<std::sync::RwLock<Vec<(String, String)>>> = OnceLock::new();

// Per-window menu mode ("dashboard" / "workspace") — needed so the menu
// reflects whichever window currently has focus, not whichever window last
// posted a /set-view event.
static WINDOW_MODES: OnceLock<std::sync::RwLock<HashMap<String, String>>> = OnceLock::new();

fn set_window_mode(label: &str, mode: &str) {
    if let Some(map) = WINDOW_MODES.get() {
        if let Ok(mut m) = map.write() {
            m.insert(label.to_string(), mode.to_string());
        }
    }
}

fn get_window_mode(label: &str) -> Option<String> {
    WINDOW_MODES
        .get()
        .and_then(|m| m.read().ok())
        .and_then(|m| m.get(label).cloned())
}

fn forget_window_mode(label: &str) {
    if let Some(map) = WINDOW_MODES.get() {
        if let Ok(mut m) = map.write() {
            m.remove(label);
        }
    }
}

/// Returns the menu mode of the currently focused Penpot window, falling
/// back to any tracked mode, then "dashboard".
fn focused_window_mode(app: &tauri::AppHandle) -> String {
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

fn normalize_tab_url(url: &str) -> String {
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

fn track_tab_url(label: &str, url: &str) {
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
fn get_all_tab_groups(app: &tauri::AppHandle) -> Vec<Vec<String>> {
    use tauri::Manager;
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
        let Some(win) = app.get_webview_window(label) else { continue };
        let Ok(ns_ptr) = win.ns_window() else { continue };
        let ns: *mut objc2::runtime::AnyObject = ns_ptr.cast();

        let mut group: Vec<String> = Vec::new();
        unsafe {
            let tabbed: *mut objc2::runtime::AnyObject =
                objc2::msg_send![ns, tabbedWindows];
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

fn untrack_tab(label: &str) {
    if let Some(list) = TAB_URLS.get() {
        if let Ok(mut v) = list.write() {
            v.retain(|(l, _)| l != label);
        }
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

async fn start_proxy(config: SharedConfig, penpot_dir: PathBuf) {
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
                    // Rebuild menus with new language
                    if let Some(app) = APP_HANDLE.get() {
                        let mode = focused_window_mode(app);
                        if let Ok((menu, _)) = build_menu(&app, &mode) {
                            let _ = app.set_menu(menu);
                            #[cfg(target_os = "macos")]
                            {
                                app.run_on_main_thread(|| {
                                    register_help_menu();
                                })
                                .ok();
                            }
                        }
                        // Reload Penpot webviews so they pick up the new language
                        // via the updated navigator.language override in config.js
                        if desktop_to_penpot_locale(lang).is_some() {
                            for (_label, window) in app.webview_windows() {
                                // Skip the settings page — it handles its own translations
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
                                })
                                .ok();
                            }
                            // In workspace mode, disable selection-dependent items initially
                            if mode == "workspace" {
                                update_selection_items(app, false);
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
                                register_help_menu();
                                if mode == "workspace" {
                                    update_selection_items(&app_handle, false);
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
            let text = APP_HANDLE.get().and_then(|app| {
                use tauri_plugin_clipboard_manager::ClipboardExt;
                app.clipboard().read_text().ok()
            }).unwrap_or_default();
            Ok::<_, warp::Rejection>(warp::reply::json(&serde_json::json!({"text": text})))
        });

    // ── POST /__penpot_desktop/set-selection → enable/disable selection-dependent menu items
    let set_selection = warp::path!("__penpot_desktop" / "set-selection")
        .and(warp::post())
        .and(warp::body::json())
        .and_then(move |body: serde_json::Value| async move {
            let count = body.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
            if let Some(app) = APP_HANDLE.get() {
                let enabled = count > 0;
                let _ = app.run_on_main_thread(move || {
                    if let Some(app) = APP_HANDLE.get() {
                        update_selection_items(app, enabled);
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
                            let _ = app_for_run.run_on_main_thread(move || {
                                let _ = create_tab_window(&app_for_tab, port, Some(&url), None);
                            });
                        }
                    }
                }
                Ok::<_, warp::Rejection>(warp::reply::json(&serde_json::json!({"ok": true})))
            }
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
    let config_js = warp::path!("js" / "config.js").and(warp::get()).map(|| {
        warp::http::Response::builder()
            .header("Content-Type", "application/javascript")
            .body("// Penpot Desktop: no server-side config needed\n")
            .unwrap()
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
    let routes = get_config
        .or(set_backend)
        .or(set_view)
        .or(window_focused)
        .or(set_selection)
        .or(get_clipboard)
        .or(set_language)
        .or(get_translations)
        .or(set_title)
        .or(open_tab)
        .or(update_tab_url)
        .or(cors_proxy)
        .or(settings_page)
        .or(settings_app_icon)
        .or(config_js)
        .or(desktop_config_js)
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
                        !lower.starts_with("frame-ancestors")
                            && !lower.starts_with("frame-src")
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

// ── Tab Window Creator ──────────────────────────────────────

use std::sync::atomic::{AtomicU32, Ordering};

static TAB_COUNTER: AtomicU32 = AtomicU32::new(1);

fn create_tab_window(
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
fn create_standalone_window(
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

#[cfg(target_os = "macos")]
fn register_help_menu() {
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

// ── Selection-dependent menu items ─────────────────────────

/// IDs of menu items that require a selection in the workspace.
const SELECTION_ITEMS: &[&str] = &[
    "duplicate", "delete",
    "group", "ungroup",
    "create-component", "detach-component",
    "bool-union", "bool-difference", "bool-intersection", "bool-exclude",
    "flip-h", "flip-v",
    "bring-forward", "bring-front", "send-backward", "send-back",
    "align-left", "align-hcenter", "align-right",
    "align-top", "align-vcenter", "align-bottom",
    "dist-h", "dist-v",
    "add-flex", "add-grid",
];

fn update_selection_items(app: &tauri::AppHandle, enabled: bool) {
    use tauri::menu::MenuItemKind;
    if let Some(menu) = app.menu() {
        // Menu.get() doesn't search submenus, so iterate through them
        for kind in menu.items().unwrap_or_default() {
            if let MenuItemKind::Submenu(sub) = kind {
                for item in sub.items().unwrap_or_default() {
                    if let MenuItemKind::MenuItem(mi) = &item {
                        let id = &mi.id().0;
                        if SELECTION_ITEMS.contains(&id.as_str()) {
                            let _ = mi.set_enabled(enabled);
                        }
                    }
                    // Also check nested submenus (e.g. Zoom, Ordering)
                    if let MenuItemKind::Submenu(nested) = &item {
                        for nested_item in nested.items().unwrap_or_default() {
                            if let MenuItemKind::MenuItem(mi) = &nested_item {
                                let id = &mi.id().0;
                                if SELECTION_ITEMS.contains(&id.as_str()) {
                                    let _ = mi.set_enabled(enabled);
                                }
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

// ── Menu Builder ────────────────────────────────────────────

fn build_menu(
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
        .and_then(|a| a.try_state::<SharedConfig>())
        .and_then(|c| c.try_read().ok().map(|c| c.language.clone()))
        .unwrap_or_else(|| "en".to_string());
    let t = |key: &str| i18n::t(&lang, key);
    // Translated label with shortcut hint: d("key", "Cmd+X") → "Translated\t\tCmd+X"
    // Double-tab ensures the hint stays right-aligned even for short labels.
    let d = |key: &str, shortcut: &str| format!("{}\t\t{}", t(key), prettify_shortcut(shortcut));
    use tauri::menu::{
        AboutMetadata, MenuBuilder, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder,
    };

    let about_metadata = AboutMetadata {
        name: Some("Penpot Desktop".into()),
        version: Some(app.package_info().version.to_string()),
        copyright: Some("© 2026 Penpot Desktop".into()),
        website: Some("https://penpot.app".into()),
        website_label: Some("penpot.app".into()),
        icon: Some(tauri::image::Image::from_bytes(include_bytes!("../icons/128x128@2x.png"))?),
        ..Default::default()
    };

    macro_rules! mi {
        ($app:expr, $id:expr, $label:expr) => {
            MenuItemBuilder::with_id($id, &$label.replace('&', "&&")).build($app)?
        };
        ($app:expr, $id:expr, $label:expr, $accel:expr) => {
            MenuItemBuilder::with_id($id, &$label.replace('&', "&&"))
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
    // All display-only so the WebView receives keyboard events for Penpot's handlers.
    // A JS capture-phase listener in DESKTOP_CONFIG_JS handles Cmd+C/X/V/A in input fields.
    let mut edit = SubmenuBuilder::new(app, &t("edit.edit"))
        .item(&mi!(app, "undo", d("edit.undo", "Cmd+Z")))
        .item(&mi!(app, "redo", d("edit.redo", "Cmd+Shift+Z")))
        .separator()
        .item(&mi!(app, "cut", d("edit.cut", "Cmd+X")))
        .item(&mi!(app, "copy", d("edit.copy", "Cmd+C")))
        .item(&mi!(app, "paste", d("edit.paste", "Cmd+V")))
        .separator()
        .item(&mi!(app, "select-all", d("edit.select-all", "Cmd+A")));

    if mode == "workspace" {
        edit = edit
            .separator()
            .item(&mi!(app, "duplicate", d("edit.duplicate", "Cmd+D")))
            .item(&mi!(app, "delete", d("edit.delete", "Backspace")))
            .separator()
            .item(&mi!(app, "group", d("edit.group", "Cmd+G")))
            .item(&mi!(app, "ungroup", d("edit.ungroup", "Shift+G")))
            .separator()
            .item(&mi!(
                app,
                "create-component",
                d("edit.create-component", "Cmd+K")
            ))
            .item(&mi!(
                app,
                "detach-component",
                d("edit.detach-component", "Cmd+Shift+K")
            ));
    }
    let edit_submenu = edit.build()?;

    // ── View ──
    let mut view = SubmenuBuilder::new(app, &t("view.view"));

    if mode == "workspace" {
        let zoom_submenu = SubmenuBuilder::new(app, &t("view.zoom"))
            .item(&mi!(app, "zoom-in", d("view.zoom-in", "+")))
            .item(&mi!(app, "zoom-out", d("view.zoom-out", "\u{2212}")))
            .item(&mi!(app, "zoom-reset", d("view.zoom-reset", "Shift+0")))
            .item(&mi!(app, "zoom-fit", d("view.zoom-fit", "Shift+1")))
            .item(&mi!(
                app,
                "zoom-selected",
                d("view.zoom-selected", "Shift+2")
            ))
            .build()?;

        let panels_submenu = SubmenuBuilder::new(app, &t("view.panels"))
            .item(&mi!(app, "toggle-layers", d("view.layers", "Alt+L")))
            .item(&mi!(app, "toggle-assets", d("view.assets", "Alt+I")))
            .item(&mi!(
                app,
                "toggle-palette",
                d("view.color-palette", "Alt+P")
            ))
            .item(&mi!(app, "toggle-history", d("view.history", "Cmd+Alt+H")))
            .build()?;

        view = view
            .item(&zoom_submenu)
            .separator()
            .item(&mi!(app, "toggle-rulers", d("view.rulers", "Cmd+Shift+R")))
            .item(&mi!(app, "toggle-guides", d("view.guides", "Cmd+'")))
            .item(&mi!(app, "toggle-grid", d("view.pixel-grid", "Shift+,")))
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
    let window_submenu = SubmenuBuilder::new(app, &t("window.window"))
        .item(&PredefinedMenuItem::minimize(
            app,
            Some(&t("app.minimize")),
        )?)
        .item(&mi!(app, "new-tab", t("window.new-tab"), "CmdOrCtrl+T"))
        .item(&mi!(
            app,
            "new-window",
            t("window.new-window"),
            "CmdOrCtrl+N"
        ))
        .item(&mi!(
            app,
            "reload-tab",
            t("window.reload-tab"),
            "CmdOrCtrl+R"
        ))
        .separator()
        .item(&mi!(app, "close-tab", t("app.close-window"), "CmdOrCtrl+W"))
        .build()?;

    let mut menu_builder = MenuBuilder::new(app).item(&app_submenu);

    if mode == "dashboard" {
        // ── File (dashboard) ──
        let file_submenu = SubmenuBuilder::new(app, &t("file.file"))
            .item(&mi!(app, "new-project", d("file.new-project", "+")))
            .separator()
            .item(&mi!(app, "close-tab", t("app.close-window"), "CmdOrCtrl+W"))
            .build()?;

        // ── Go (dashboard) ──
        let go_submenu = SubmenuBuilder::new(app, &t("go.go"))
            .item(&mi!(app, "go-drafts", d("go.drafts", "G D")))
            .item(&mi!(app, "go-libs", d("go.libraries", "G L")))
            .item(&mi!(app, "go-search", d("go.search", "Cmd+F")))
            .build()?;

        menu_builder = menu_builder
            .item(&file_submenu)
            .item(&edit_submenu)
            .item(&view_submenu)
            .item(&go_submenu);
    } else {
        // ── File (workspace) ──
        let file_submenu = SubmenuBuilder::new(app, &t("file.file"))
            .item(&mi!(app, "export", t("file.export"), "CmdOrCtrl+Shift+E"))
            .separator()
            .item(&mi!(app, "close-tab", t("app.close-window"), "CmdOrCtrl+W"))
            .build()?;

        // ── Shape ──
        let tools_submenu = SubmenuBuilder::new(app, &t("shape.tools"))
            .item(&mi!(app, "tool-frame", d("shape.frame", "B")))
            .item(&mi!(app, "tool-rect", d("shape.rectangle", "R")))
            .item(&mi!(app, "tool-ellipse", d("shape.ellipse", "E")))
            .item(&mi!(app, "tool-text", d("shape.text", "T")))
            .item(&mi!(app, "tool-path", d("shape.path", "P")))
            .item(&mi!(app, "tool-curve", d("shape.curve", "Shift+C")))
            .item(&mi!(
                app,
                "insert-image",
                d("shape.insert-image", "Shift+K")
            ))
            .build()?;

        let order_submenu = SubmenuBuilder::new(app, &t("shape.order"))
            .item(&mi!(
                app,
                "bring-forward",
                d("shape.bring-forward", "Cmd+\u{2191}")
            ))
            .item(&mi!(
                app,
                "bring-front",
                d("shape.bring-front", "Cmd+Shift+\u{2191}")
            ))
            .item(&mi!(
                app,
                "send-backward",
                d("shape.send-backward", "Cmd+\u{2193}")
            ))
            .item(&mi!(
                app,
                "send-back",
                d("shape.send-back", "Cmd+Shift+\u{2193}")
            ))
            .build()?;

        let bool_submenu = SubmenuBuilder::new(app, &t("shape.boolean"))
            .item(&mi!(app, "bool-union", d("shape.union", "Cmd+Alt+U")))
            .item(&mi!(
                app,
                "bool-difference",
                d("shape.difference", "Cmd+Alt+D")
            ))
            .item(&mi!(
                app,
                "bool-intersection",
                d("shape.intersection", "Cmd+Alt+I")
            ))
            .item(&mi!(app, "bool-exclude", d("shape.exclude", "Cmd+Alt+E")))
            .build()?;

        let shape_submenu = SubmenuBuilder::new(app, &t("shape.shape"))
            .item(&tools_submenu)
            .separator()
            .item(&mi!(app, "flip-h", d("shape.flip-h", "Shift+H")))
            .item(&mi!(app, "flip-v", d("shape.flip-v", "Shift+V")))
            .separator()
            .item(&order_submenu)
            .item(&bool_submenu)
            .separator()
            .item(&mi!(
                app,
                "toggle-layout-flex",
                d("shape.flex-layout", "Shift+A")
            ))
            .item(&mi!(
                app,
                "toggle-layout-grid",
                d("shape.grid-layout", "Cmd+Shift+A")
            ))
            .separator()
            .item(&mi!(app, "align-left", d("shape.align-left", "Alt+A")))
            .item(&mi!(
                app,
                "align-hcenter",
                d("shape.align-hcenter", "Alt+H")
            ))
            .item(&mi!(app, "align-right", d("shape.align-right", "Alt+D")))
            .separator()
            .item(&mi!(app, "align-top", d("shape.align-top", "Alt+W")))
            .item(&mi!(
                app,
                "align-vcenter",
                d("shape.align-vcenter", "Alt+V")
            ))
            .item(&mi!(app, "align-bottom", d("shape.align-bottom", "Alt+S")))
            .separator()
            .item(&mi!(app, "dist-h", t("shape.dist-h")))
            .item(&mi!(app, "dist-v", t("shape.dist-v")))
            .build()?;

        // ── Go (workspace) ──
        let go_submenu = SubmenuBuilder::new(app, &t("go.go"))
            .item(&mi!(app, "go-viewer", d("go.open-viewer", "G V")))
            .item(&mi!(app, "go-inspect", d("go.open-inspect", "G I")))
            .separator()
            .item(&mi!(app, "go-dashboard", d("go.back-dashboard", "G D")))
            .build()?;

        menu_builder = menu_builder
            .item(&file_submenu)
            .item(&edit_submenu)
            .item(&view_submenu)
            .item(&shape_submenu)
            .item(&go_submenu);
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

// ── Safari User-Agent (macOS only) ──────────────────────────

#[cfg(target_os = "macos")]
fn safari_user_agent() -> Option<String> {
    let version = std::process::Command::new("defaults")
        .args(["read", "/Applications/Safari.app/Contents/Info", "CFBundleShortVersionString"])
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
fn safari_user_agent() -> Option<String> {
    None
}

// ── Tauri Commands ───────────────────────────────────────────

#[tauri::command]
fn save_download(data: Vec<u8>, path: String) -> Result<String, String> {
    std::fs::write(&path, &data).map_err(|e| e.to_string())?;
    Ok(path)
}

#[tauri::command]
fn get_proxy_url(state: tauri::State<SharedConfig>) -> String {
    let port = state
        .inner()
        .try_read()
        .map(|c| c.proxy_port)
        .unwrap_or(7080);
    format!("http://127.0.0.1:{port}")
}

// ── Main ─────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let config = load_config();
    let shared_config: SharedConfig = Arc::new(RwLock::new(config.clone()));

    let proxy_config = shared_config.clone();
    let port = config.proxy_port;

    // Determine Penpot frontend dir
    let penpot_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|pp| pp.join("penpot-frontend")))
        .unwrap_or_else(|| PathBuf::from("src/penpot"));

    // Fallback: check relative to project root (dev mode)
    let penpot_dir = if penpot_dir.is_dir() {
        penpot_dir
    } else {
        // Dev mode: look relative to Cargo.toml
        let dev_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .map(|p| p.join("src/penpot"))
            .unwrap_or_else(|| PathBuf::from("src/penpot"));
        dev_dir
    };

    println!("📁 Penpot frontend directory: {}", penpot_dir.display());

    let config_for_exit = shared_config.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(shared_config.clone())
        .on_window_event(|window, event| {
            match event {
                tauri::WindowEvent::Destroyed => {
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
                                register_help_menu();
                                if mode == "workspace" {
                                    update_selection_items(&app_for_closure, false);
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
            WINDOW_MODES.get_or_init(|| std::sync::RwLock::new(HashMap::new()));

            // Set initial menu (dashboard mode)
            let (initial_menu, _) = build_menu(&app.handle(), "dashboard")
                .expect("Failed to build menu");
            app.set_menu(initial_menu)?;
            #[cfg(target_os = "macos")]
            register_help_menu();

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
                            register_help_menu();
                            if mode_clone == "workspace" {
                                update_selection_items(&app_clone, false);
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
                    "tool-frame" => "b",
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

                // Simulate keyboard event with proper keyCode for Mousetrap.
                // Escape backslash and single-quote so shortcuts containing them
                // (e.g. "meta+'" for guides, "\\" for hide-ui) don't break the JS literal.
                let escaped_shortcut = shortcut.replace('\\', "\\\\").replace('\'', "\\'");
                let js = format!("window.__penpotKey('{escaped_shortcut}')");
                let _ = window.eval(&js);
            });

            // Start reverse proxy in background
            let penpot_dir_clone = penpot_dir.clone();
            tauri::async_runtime::spawn(async move {
                start_proxy(proxy_config, penpot_dir_clone).await;
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

            let mut main_builder = WebviewWindowBuilder::new(app, "main", Default::default())
                .title("Penpot Desktop")
                .maximized(true)
                .inner_size(1440.0, 900.0)
                .min_inner_size(900.0, 600.0)
                .tabbing_identifier("penpot")
                .disable_drag_drop_handler()
                .on_navigation(|url| {
                    url.scheme() == "blob" || url.host_str() == Some("127.0.0.1")
                })
                .on_page_load(|webview, payload| {
                    if let tauri::webview::PageLoadEvent::Finished = payload.event() {
                        let label = webview.label().to_string();
                        let _ = webview.eval(&format!(
                            "window.__penpotWindowLabel='{label}';\
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
        .invoke_handler(tauri::generate_handler![get_proxy_url, save_download])
        .build(tauri::generate_context!())
        .expect("Failed to build Penpot Desktop")
        .run(move |app, event| {
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

fn main() {
    run();
}
