# AI Agent Guide — Penpot Desktop

This document provides core context and operating guidelines for AI agents
working in this repository.

## Before You Start

1. Read this file completely.
2. Identify which parts of the codebase are affected by the task.
3. Read the relevant source files before proposing changes.

## Role: Senior Software Engineer

You are a high-autonomy Senior Software Engineer. You have full permission to
navigate the codebase, modify files, and execute commands to fulfill your tasks.

### Operational Guidelines

1. Before writing code, describe your plan. If the task is complex, break it
   down into atomic steps.
2. Be concise and autonomous.
3. Commit only when explicitly asked.
4. When searching code, prefer `ripgrep` (`rg`) over `grep`.

## Architecture Overview

Penpot Desktop wraps the open-source Penpot design tool as a native desktop
application using **Tauri v2** (Rust). It does **not** bundle a backend — instead
it connects to a remote Penpot instance (self-hosted or SaaS) through an
embedded reverse proxy.

```
┌──────────────────────────────────────────────┐
│  Tauri v2 Shell (Rust)                       │
│                                              │
│  ┌────────────┐    ┌──────────────────────┐  │
│  │  WebView   │◄──►│  Warp Reverse Proxy  │──┼──► Remote Penpot Backend
│  │  (Penpot   │    │  (127.0.0.1:7080)    │  │
│  │  Frontend) │    └──────────────────────┘  │
│  └────────────┘                              │
└──────────────────────────────────────────────┘
```

**Key design decisions:**

- The Penpot frontend is built from source (ClojureScript → JS) via
  `scripts/build-frontend.sh` and served locally from `src/penpot/`.
- A Warp-based reverse proxy handles API requests, WebSocket connections,
  cookie rewriting, CORS proxy relay, and share-link rewriting.
- Config injection and frontend patches are applied at build time and runtime
  via JavaScript injection into `index.html`.

## Source Code Map

All Rust source lives in `src-tauri/src/`:

| File | Lines | Purpose |
|------|------:|---------|
| `main.rs` | ~900 | App lifecycle, setup, menu event delegation, session restore |
| `proxy.rs` | ~1300 | Warp reverse proxy — request routing, cookie/URL/referer rewriting, CORS proxy, WebSocket relay |
| `config.rs` | ~620 | `AppConfig` struct, save/load, locale mapping, JS injection (config loader, iframe shim) |
| `menu.rs` | ~940 | Context-aware native menu builder, 39 selection-dependent items, dynamic labels, keyboard shortcut forwarding |
| `windows.rs` | ~400 | Tab/window creation, plugin launcher, JS patch injection |
| `state.rs` | ~270 | Global state (`lazy_static` HashMaps/Vecs) — tab URLs/titles, closed tabs, plugins, window modes |
| `commands.rs` | ~40 | Tauri IPC commands (`save_download`, `get_proxy_url`, `check_for_updates`, `open_update_page`) |
| `i18n.rs` | ~85 | Translation function `t(lang, key)`, lazy-loaded JSON, 18 languages |
| `updater.rs` | ~210 | GitHub Releases-based update checker, version compare, in-memory cache |

### Other important files

| Path | Purpose |
|------|---------|
| `scripts/build-frontend.sh` | Clones Penpot repo, builds frontend from source, patches and injects config |
| `src-tauri/tauri.conf.json` | Tauri app configuration (window settings, bundle config, permissions) |
| `src-tauri/Cargo.toml` | Rust dependencies (Tauri 2, Warp 0.3, Tokio, Reqwest, etc.) |
| `package.json` | Bun workspace — setup, build-frontend, tauri dev/build scripts |
| `src-tauri/locales/*.json` | i18n translation files (18 languages) |
| `src-tauri/capabilities/` | Tauri v2 capability declarations |
| `.github/workflows/build.yml` | CI/CD — matrix build for macOS, Linux, Windows (x86_64 + ARM64) |

## Build & Development

### Prerequisites

- **Bun** (package manager)
- **Rust** (stable toolchain)
- **Node.js** ≥ 18, **pnpm** (for Penpot frontend build)
- **JDK** ≥ 21 (for ClojureScript compilation)
- **Clojure CLI** (for frontend build)

### Commands

```bash
bun run setup            # Install JS dependencies
bun run build-frontend   # Clone & build Penpot frontend from source
bun run tauri:dev        # Start dev mode (hot-reload Rust + WebView)
bun run tauri:build      # Production build (creates platform installer)
```

### Frontend Build Script (`scripts/build-frontend.sh`)

The build script performs these phases:

1. **Prerequisites check** — verifies git, node, pnpm, java, clojure
2. **Clone** — clones the Penpot repo at a specific release tag (default: latest,
   configurable via `PENPOT_BRANCH` env var)
3. **Compile** — ClojureScript → JavaScript via `pnpm build:app:main`
4. **Bundle** — JS libs with esbuild, CSS, templates, sprites, translations
5. **WASM** — downloads pre-built `render-wasm.wasm` / `.js`
6. **Patch** — removes unused assets, injects config loader and file-menu helper
   scripts into `index.html`

Output goes to `src/penpot/`.

## Key Conventions

### Rust Code Style

- **Section comments:** `// ── Description ──────────────────`
- **Function organization:** checks → implementation → helpers
- **Error handling:** descriptive error messages, `eprintln!` for non-fatal proxy errors
- **Platform-specific code:** `cfg!(target_os = "macos")` guards and `#[cfg(...)]` attributes

### Global State

State is managed via `lazy_static` in `state.rs`:

```rust
lazy_static! {
    pub static ref TAB_URLS: Mutex<HashMap<String, String>> = ...;
    pub static ref TAB_TITLES: Mutex<HashMap<String, String>> = ...;
    pub static ref CLOSED_TABS: Mutex<Vec<ClosedTab>> = ...;
    // etc.
}
```

Shared mutable config uses `Arc<RwLock<AppConfig>>`.

### Frontend Integration

- Config is injected via `<script src="/__penpot_desktop_config.js">` in `index.html`
- File-menu helper: `<script src="/__penpot_desktop_file_menu.js">`
- The proxy serves these virtual paths dynamically
- Synthetic `KeyboardEvent`s are dispatched for native menu actions

### Menu System

- Menus are context-aware: different items for dashboard vs. workspace mode
- 39 items have selection-dependent enable/disable states
- Dynamic labels (e.g., "Create Component" ↔ "Create Variant")
- macOS: Help & Window menus registered via AppKit (`objc2`)

## i18n

- 18 languages supported (en, de, es, fr, it, tr, ru, zh_CN, jpn_JP, ko, ar, pt_BR, ca, nl, pl, ro, ukr_UA, he)
- Translation files: `src-tauri/locales/<lang>.json`
- Loaded at compile time via `include_str!` macro
- Access via `t(lang, key)` function in `i18n.rs`
- Hierarchical JSON flattened to dot-notation keys (e.g., `app.settings.title`)

## CI/CD

The GitHub Actions workflow (`.github/workflows/build.yml`) builds for:

| Platform | Architectures | Artifacts |
|----------|--------------|-----------|
| macOS | ARM64, Universal | `.dmg` |
| Linux | x86_64, ARM64 | `.deb`, `.AppImage` |
| Windows | x86_64, ARM64 | `.msi`, `.exe` (NSIS) |

Releases are created automatically on `v*` tags with all platform artifacts.

## Dependency Graph

```
package.json (Bun)
  └── @tauri-apps/cli

src-tauri/Cargo.toml (Rust)
  ├── tauri 2 + plugins (shell, opener, dialog, clipboard)
  ├── warp 0.3 (reverse proxy)
  ├── tokio 1 (async runtime)
  ├── reqwest 0.12 (HTTP client, rustls-tls)
  ├── tokio-tungstenite 0.24 (WebSocket)
  ├── serde + serde_json (serialization)
  ├── hyper 1 + http 1 (HTTP primitives)
  └── objc2 0.6 (macOS only — AppKit integration)

scripts/build-frontend.sh
  └── Penpot repo (ClojureScript → JS build)
```
