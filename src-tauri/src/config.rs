use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

// ── Config ───────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppConfig {
    pub backend_url: String,
    pub recent_urls: Vec<String>,
    pub proxy_port: u16,
    #[serde(default = "default_renderer")]
    pub renderer: String,
    #[serde(default = "default_language")]
    pub language: String,
    /// Legacy field — plain URL list from older configs. Migrated to
    /// `open_groups` on load.
    #[serde(default)]
    pub open_tabs: Vec<String>,
    /// Each inner Vec is one window-group (= one macOS tab bar).
    /// The first group is restored into the main window; additional
    /// groups each become a standalone top-level window with their
    /// own tabs.
    #[serde(default)]
    pub open_groups: Vec<Vec<String>>,
}

fn default_renderer() -> String {
    "classic".into()
}
fn default_language() -> String {
    "en".into()
}

/// Map desktop locale codes to Penpot frontend locale codes.
pub fn desktop_to_penpot_locale(desktop: &str) -> Option<&'static str> {
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
pub const IFRAME_SHIM_JS: &str = r#"(function(){
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

pub const DESKTOP_CONFIG_JS: &str = r#"// Penpot Desktop runtime config
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
      // Viewer / Inspect navigation should stay in the same tab, not open a
      // new one — Penpot calls window.open for these but the user expects
      // an in-place transition (like pressing G V / G I on the keyboard).
      var hash = (path.indexOf('#') !== -1) ? path.substring(path.indexOf('#')) : '';
      if (hash.match(new RegExp('/(view|inspect|workspace)([?/]|$)'))) {
        window.location.href = path;
        return null;
      }
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
  var _lastSelKey = '';
  window.__penpotDesktopOnSelection = function(count, types, flags) {
    var t = types || [];
    var f = flags || [];
    var key = count + '|' + t.slice().sort().join(',') + '|' + f.slice().sort().join(',');
    if (key !== _lastSelKey) {
      _lastSelKey = key;
      fetch('/__penpot_desktop/set-selection', {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        body: JSON.stringify({count: count, types: t, flags: f})
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

pub fn load_config() -> AppConfig {
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

pub fn save_config(config: &AppConfig) {
    if let Ok(json) = serde_json::to_string_pretty(config) {
        fs::write(config_path(), json).ok();
    }
}

pub type SharedConfig = Arc<RwLock<AppConfig>>;
