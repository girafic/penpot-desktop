# Penpot Desktop

Native desktop app that runs the **real Penpot frontend locally**. The backend is freely configurable — Penpot Cloud, self-hosted, or local instance.

## Architecture

```
┌─────────────────────────────────────────────────┐
│  Tauri App (native window)                      │
│  ┌───────────────────────────────────────────┐  │
│  │  Penpot ClojureScript Frontend            │  │
│  │  (locally built, static files)            │  │
│  └──────────────┬────────────────────────────┘  │
│                 │ /api/*, /ws/*, /assets/*      │
│  ┌──────────────▼────────────────────────────┐  │
│  │  Embedded Reverse Proxy (Rust/Warp)       │  │
│  │  127.0.0.1:7080                           │  │
│  └──────────────┬────────────────────────────┘  │
└─────────────────┼───────────────────────────────┘
                  │ HTTP / WebSocket
                  ▼
       ┌────────────────────┐
       │  Penpot Backend    │
       │  (configurable URL)│
       └────────────────────┘
```

**How it works:**

1. The Penpot frontend (ClojureScript → JS) is built locally and bundled as static files
2. An embedded reverse proxy (Warp) runs on `127.0.0.1:7080`
3. Static files are served locally
4. API calls (`/api/*`), assets (`/assets/*`), and WebSockets (`/ws/*`) are forwarded to the configured backend
5. Cookie rewriting ensures authentication works across the proxy
6. No CORS — everything is same-origin from the frontend's perspective

## Features

### Native Desktop Experience

- **macOS native tabs** (Cmd+T) and separate windows (Cmd+N) — links targeting `_blank` automatically open as new tabs
- **Tab session restore** — open tabs are saved on exit (in visual tab order) and restored on next launch
- **Dynamic window titles** — each tab syncs its document title to the native tab/window title
- **Full clipboard support** — Cmd+C/X/V/A work everywhere (input fields + canvas) via a native clipboard bridge
- **Native file export** — Save dialog for asset downloads with proper filename handling (extracted from query params, URL fragments, or path segments)

### Menus & Shortcuts

- **Context-aware menus** that switch automatically between dashboard and workspace mode:
  - _Dashboard:_ File (New Project), Edit, View, Go (Drafts, Libraries, Search)
  - _Workspace:_ File (Export), Edit (Duplicate, Group, Components), View (Zoom, Rulers, Panels), Shape (Tools, Boolean ops, Alignment, Ordering), Go (Viewer, Inspect, Dashboard)
- **Selection-dependent menu items** — items like Duplicate, Delete, Group, Boolean operations, and Alignment auto-enable/disable based on current selection
- **Native accelerator forwarding** — modifier-based menu accelerators (Cmd/Ctrl/Alt/Shift combos) are handled natively and forwarded to Penpot's internal shortcut handler
- **Keyboard shortcut bridge** — menu actions are translated to synthetic keyboard events for Penpot/Mousetrap, including platform-aware Cmd→Ctrl normalization on Windows/Linux
- **Help menu** — links to User Guide, Tutorials, Courses, Plugins, Libraries, Community, GitHub, Feedback, Website, and Release Notes (open in default browser)

### Internationalization

- **18 languages** — English, Deutsch, Español, Français, Português (BR), Italiano, Türkçe, Русский, 中文, 日本語, 한국어, العربية, Català, Nederlands, Polski, Română, Українська, עברית
- Language selector in settings — menus, settings UI, and Penpot frontend locale all update automatically

### Rendering

- **WASM (GPU)** — Skia-based renderer, faster, requires WebGL2
- **Classic (SVG)** — broader compatibility
- Configurable in settings (Cmd+,)

### Reverse Proxy

- Embedded Warp reverse proxy on `127.0.0.1:7080` — everything is same-origin from the frontend's perspective
- API (`/api/*`), assets (`/assets/*`), and WebSocket (`/ws/*`) forwarding with cookie authentication
- **Cookie rewriting** — `Set-Cookie` headers are rewritten for localhost (strips Domain, Secure, SameSite=None → SameSite=Lax)
- **Backend URL rewriting** — text responses (JSON/transit) are rewritten so the SPA uses the proxy origin
- **Share link rewriting** — share/view links are automatically rewritten from proxy URL to real backend URL (in UI inputs and clipboard)
- **Referer/Origin header rewriting** — avoids CORS and hotlink protection issues
- **Error deduplication** — repeated proxy errors are suppressed (5s cooldown) to keep logs clean
- **Safari user-agent** (macOS) — spoofs Safari UA for maximum WebKit compatibility

## Prerequisites

### For the Tauri build

- **Rust** → [rustup.rs](https://rustup.rs)
- **Bun** → [bun.sh](https://bun.sh)
- **Node.js ≥ 18** → [nodejs.org](https://nodejs.org) (needed for Penpot frontend build)
- **Tauri system deps:**
  - macOS: `xcode-select --install`
  - Ubuntu: `sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev`
  - Windows: VS C++ Build Tools + WebView2

### For the Penpot frontend build

- **JDK ≥ 21** → [adoptium.net](https://adoptium.net) or `brew install openjdk@21`
- **Clojure CLI** → [clojure.org/guides/install_clojure](https://clojure.org/guides/install_clojure)
- **pnpm** → `npm install -g pnpm`
- **Git**

## Quick Start

```bash
# 1. Setup
bun install

# 2. Clone & build Penpot frontend (takes ~5-10 min on first run)
bun run build-frontend

# 3. Start dev mode
bun run tauri:dev

# 4. Or: production build
bun run tauri:build
```

### Matching frontend to backend

The frontend build must match the backend version. Use the right branch/tag:

| Backend                        | Branch/Tag         | Command                                        |
| ------------------------------ | ------------------ | ---------------------------------------------- |
| `design.penpot.app`            | Latest release tag | `PENPOT_BRANCH=2.14.1 bun run build-frontend`  |
| Local devenv (`:3449`/`:3450`) | `develop`          | `PENPOT_BRANCH=develop bun run build-frontend` |
| Docker self-hosted (`:9001`)   | Latest release tag | `PENPOT_BRANCH=2.14.0 bun run build-frontend`  |

Check the latest release tag at [github.com/penpot/penpot/releases](https://github.com/penpot/penpot/releases).

When switching branches, delete the repo first to avoid conflicts:

```bash
rm -rf penpot-repo
PENPOT_BRANCH=staging bun run build-frontend
```

## Usage

On first launch, the **Settings page** opens:

1. Enter a backend URL (e.g., `https://design.penpot.app`)
2. Click **Connect**
3. The Penpot frontend loads — all API calls go through the local proxy to the backend

The URL is saved. On the next launch, the app connects automatically.

### Keyboard shortcuts

`Cmd` in the tables maps to `Ctrl` on Windows/Linux when triggered through native menu accelerators.

**App-wide:**

| Shortcut   | Action           |
| ---------- | ---------------- |
| Cmd+,      | Open Settings    |
| Cmd+T      | New Tab          |
| Cmd+N      | New Window       |
| Cmd+W      | Close Tab/Window |
| Cmd+R      | Reload Tab       |
| Ctrl+Cmd+F | Fullscreen       |
| Cmd+Alt+I  | DevTools         |
| Alt+M      | Toggle Theme     |

**Workspace — Edit:**

| Shortcut    | Action           |
| ----------- | ---------------- |
| Cmd+Z       | Undo             |
| Cmd+Shift+Z | Redo             |
| Cmd+D       | Duplicate        |
| Backspace   | Delete           |
| Cmd+G       | Group            |
| Shift+G     | Ungroup          |
| Cmd+K       | Create Component |
| Cmd+Shift+K | Detach Component |

**Workspace — View:**

| Shortcut              | Action                                 |
| --------------------- | -------------------------------------- |
| + / -                 | Zoom In / Out                          |
| Shift+0               | Zoom Reset                             |
| Shift+1               | Zoom to Fit                            |
| Shift+2               | Zoom to Selected                       |
| Cmd+Shift+R           | Toggle Rulers                          |
| Cmd+'                 | Toggle Guides                          |
| Shift+,               | Toggle Pixel Grid                      |
| Alt+L / Alt+I / Alt+P | Toggle Layers / Assets / Color Palette |
| Cmd+Alt+H             | Toggle History                         |
| \                     | Hide UI                                |

**Workspace — Shape tools:**

| Shortcut | Action       |
| -------- | ------------ |
| B        | Frame        |
| R        | Rectangle    |
| E        | Ellipse      |
| T        | Text         |
| P        | Path         |
| Shift+C  | Curve        |
| Shift+K  | Insert Image |

**Workspace — Shape operations:**

| Shortcut          | Action                                              |
| ----------------- | --------------------------------------------------- |
| Shift+H / Shift+V | Flip Horizontal / Vertical                          |
| Shift+A           | Add Flex Layout                                     |
| Cmd+Shift+A       | Add Grid Layout                                     |
| Cmd+Alt+U/D/I/E   | Boolean Union / Difference / Intersection / Exclude |

**Workspace — Navigation:**

| Shortcut    | Action            |
| ----------- | ----------------- |
| G then V    | Open Viewer       |
| G then I    | Open Inspect      |
| G then D    | Back to Dashboard |
| Cmd+Shift+E | Export            |

**Dashboard:**

| Shortcut | Action          |
| -------- | --------------- |
| +        | New Project     |
| G then D | Go to Drafts    |
| G then L | Go to Libraries |
| Cmd+F    | Search          |

All Penpot shortcuts work as normal in the workspace.

### Presets

| Name         | URL                         | Description              |
| ------------ | --------------------------- | ------------------------ |
| Penpot Cloud | `https://design.penpot.app` | Official cloud           |
| Local :9001  | `http://localhost:9001`     | Standard Docker setup    |
| Dev :3449    | `http://localhost:3449`     | ClojureScript dev server |

## Self-hosted Penpot

```bash
curl -o docker-compose.yaml \
  https://raw.githubusercontent.com/penpot/penpot/main/docker/images/docker-compose.yaml

docker compose -p penpot -f docker-compose.yaml up -d
```

Then enter `http://localhost:9001` as the backend in the app.

## Project Structure

```
penpot-desktop/
├── package.json               # Bun workspace (Tauri CLI)
├── bun.lockb
├── scripts/
│   └── build-frontend.sh      # Clones & builds Penpot frontend
├── src/
│   ├── index.html              # Placeholder (Tauri requirement)
│   ├── app-icon.png
│   ├── settings.html           # Settings/launcher UI
│   └── penpot/                 # ← Built Penpot frontend (after build)
├── penpot-repo/                # ← Cloned Penpot repo (after build)
└── src-tauri/
    ├── Cargo.toml
    ├── tauri.conf.json
    ├── capabilities/
    │   └── default.json
    ├── locales/                # i18n translations (JSON)
    └── src/
        ├── main.rs             # Reverse proxy + Tauri app
        └── i18n.rs             # Internationalization module
```

## Configuration

The app stores its config at:

- macOS: `~/Library/Application Support/penpot-desktop/config.json`
- Linux: `~/.config/penpot-desktop/config.json`
- Windows: `%APPDATA%\penpot-desktop\config.json`

```json
{
  "backend_url": "https://design.penpot.app",
  "recent_urls": ["https://design.penpot.app", "http://localhost:9001"],
  "proxy_port": 7080,
  "renderer": "wasm",
  "language": "en",
  "open_tabs": ["/#/workspace/...", "/#/view/..."]
}
```

| Key           | Default  | Description                                       |
| ------------- | -------- | ------------------------------------------------- |
| `backend_url` | `""`     | Penpot backend URL                                |
| `recent_urls` | `[]`     | Previously used backend URLs                      |
| `proxy_port`  | `7080`   | Local reverse proxy port                          |
| `renderer`    | `"wasm"` | Renderer engine (`wasm` or `classic`)             |
| `language`    | `"en"`   | UI language (e.g. `de`, `es`, `fr`, `ru`)         |
| `open_tabs`   | `[]`     | Saved tab URLs for session restore (auto-managed) |

Changes to `proxy_port`, `renderer`, and `language` require a restart.

## Troubleshooting

**Frontend build fails:**

- Check JDK version: `java -version` (≥21 required)
- Check Clojure: `clojure --version`
- Check pnpm (used by Penpot frontend): `pnpm --version`
- Alternatively: build Penpot manually and copy files to `src/penpot/`

**Images not loading (403):**

- The proxy rewrites `Referer`/`Origin` headers automatically
- If connecting to a new backend, try clearing cookies (Settings → reconnect)

**WebSocket connection drops:**

- Backend must support WebSocket
- The proxy forwards cookies and Origin headers for authentication

**Port already in use:**

- Kill leftover processes: `pkill -9 -f penpot-desktop`
- Or change `proxy_port` in config.json

## License

MIT
