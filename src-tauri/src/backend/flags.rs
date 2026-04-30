//! Penpot frontend feature flags injected when running in offline mode.
//!
//! These flags switch off online-only behavior (registration, telemetry,
//! Google Fonts proxy, dashboard templates) and enable JSON-readable RPC
//! responses so we can avoid Transit on the wire.

/// Space-separated `penpotFlags` value for offline mode.
pub const OFFLINE_FLAGS: &str = concat!(
    "disable-registration ",
    "disable-onboarding ",
    "disable-onboarding-newsletter ",
    "disable-onboarding-team ",
    "disable-secure-session-cookies ",
    "enable-login-with-password ",
    "disable-login-with-google ",
    "disable-login-with-github ",
    "disable-login-with-gitlab ",
    "disable-login-with-oidc ",
    "disable-google-fonts-provider ",
    "disable-dashboard-templates-section ",
    "disable-telemetry ",
    "enable-transit-readable-response ",
    "disable-feedback ",
    "enable-share-links",
);

/// Build the JS body for `/js/config.js` in offline mode.
/// `proxy_origin` should be the loopback origin (`http://127.0.0.1:7080`),
/// `wasm_renderer` controls whether `enable-render-wasm` is appended.
pub fn config_js(proxy_origin: &str, wasm_renderer: bool) -> String {
    let mut flags = OFFLINE_FLAGS.to_string();
    if wasm_renderer {
        flags.push_str(" enable-render-wasm");
    }
    format!(
        r#"// Penpot Desktop offline runtime config — auto-generated.
var penpotPublicURI = {proxy_uri};
var penpotFlags     = {flags_lit};
var penpotBuildDate = "{date}";
var penpotVersion   = "penpot-desktop-offline";
"#,
        proxy_uri = serde_json::to_string(proxy_origin).unwrap(),
        flags_lit = serde_json::to_string(&flags).unwrap(),
        date = chrono::Utc::now().format("%Y-%m-%d"),
    )
}
