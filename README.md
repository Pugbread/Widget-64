<div align="center">

# Widget 64

**A focused spatial desktop for widgets that also run as standalone Tauri apps.**

[![Build](https://img.shields.io/github/actions/workflow/status/Pugbread/Widget-64/ci.yml?branch=main&label=build)](https://github.com/Pugbread/Widget-64/actions)
[![Release](https://img.shields.io/github/v/release/Pugbread/Widget-64?include_prereleases&label=release)](https://github.com/Pugbread/Widget-64/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](#license)
[![Tauri v2](https://img.shields.io/badge/Tauri-v2-24C8DB?logo=tauri)](https://tauri.app/)
[![React 19](https://img.shields.io/badge/React-19-61DAFB?logo=react)](https://react.dev/)

</div>

Widget 64 takes the widget system and infinite canvas from [Terminal 64](https://github.com/Pugbread/Terminal-64) and turns them into the whole product. There are no terminal, chat, or browser panels in the workspace UI—only widgets, their canvas, and three ways to move through them.

## What it does

- **Infinite canvas** — pan, pinch-to-zoom, drag, resize, layer, and fit widgets on a persistent spatial surface.
- **Overview** — packs every open widget into the current screen using its real dimensions, so portrait, square, and wide widgets stay proportional.
- **Gallery** — lines widgets up horizontally at a shared visual height, preserves their aspect ratios, and maps vertical trackpad motion to horizontal browsing.
- **Tauri-native widget format** — every created widget is a Tauri 2 application. Widget 64 embeds the same frontend that runs in its standalone native window.
- **LLM-ready folders** — Widget 64 writes `AGENTS.md` and `CLAUDE.md` before it scaffolds the rest of a new widget. Any coding agent entering that folder immediately sees the runtime contract, bridge API, responsive rules, and run commands.
- **Terminal 64 widget compatibility** — keeps the existing local widget server, hot reload, persistent state, permissions, postMessage bridge, native-webview transport, zip installation, and host API.

## The three views

| View | Purpose | Layout behavior |
| --- | --- | --- |
| Canvas | Arrange and work | Free x/y positioning with pan, zoom, drag, resize, and z-order |
| Overview | See everything | Searches candidate row/column layouts and chooses the best minimum scale, then centers each widget in its cell |
| Gallery | Browse in sequence | Fits widgets by available height, keeps every aspect ratio, and places them on a horizontal scroll track |

Use the segmented control in the titlebar or press `1`, `2`, and `3` to switch views. Press `N` to open the widget library.

## A widget is a real Tauri app

Creating `Build monitor` produces this structure under `~/.terminal64/widgets/build-monitor/`:

```text
build-monitor/
├── AGENTS.md                 # Complete agent context and Widget 64 API
├── CLAUDE.md                 # Same context for Claude Code
├── index.html                # Embedded and Vite entry point
├── package.json
├── vite.config.js
├── src/
│   ├── bridge.js             # Host bridge + standalone fallbacks
│   ├── main.js
│   └── style.css
└── src-tauri/
    ├── capabilities/default.json
    ├── src/{lib.rs,main.rs}
    ├── Cargo.toml
    └── tauri.conf.json
```

Run either target from the widget folder:

```bash
npm install

# Browser development
npm run dev

# Standalone native app
npm run tauri dev

# Native release bundle
npm run tauri build
```

Embedded mode is detected with `window.parent !== window`. The generated `src/bridge.js` talks to Widget 64 when embedded and supplies local fallbacks for state, fetch, notifications, and external URLs when standalone. Host-only features return a clear unavailable response instead of failing silently.

## Install and run Widget 64

### Prerequisites

- Node.js 20+
- Rust stable 1.77.2+
- The [Tauri 2 platform prerequisites](https://v2.tauri.app/start/prerequisites/) for your OS

```bash
git clone https://github.com/Pugbread/Widget-64.git
cd Widget-64
npm install
npm run tauri dev
```

For a production bundle:

```bash
npm run tauri build
```

Artifacts are written under `src-tauri/target/release/bundle/`. GitHub Actions is configured to build macOS, Linux, and Windows releases from `v*` tags.

## Web preview

`npm run dev` opens a graceful browser preview without requiring the Tauri host. Add `?demo=1` during development to populate three local sample widgets with different aspect ratios for layout testing.

## Architecture

Widget 64 keeps Terminal 64's Tauri 2 widget runtime for compatibility and replaces its workstation shell with a widget-only React 19 interface.

- `src/components/widget64/` — workspace, overview/grid math, gallery layout, frame interactions, and widget manager
- `src/components/widget/` — compatible embedded widget renderer and host bridge
- `src/stores/canvasStore.ts` — persistent widget frames, canvas transform, and legacy widget migration
- `src-tauri/src/widget_scaffold.rs` — transactional dual-runtime project generator
- `src-tauri/src/widget_instructions.rs` — context injected into every fresh widget folder
- `src-tauri/src/widget_server.rs` — local file server and hot reload

The backend retains the wider bridge implementation used by existing Terminal 64 widgets, even though those capabilities are not exposed as workspace panel types.

## Validation

```bash
npm run typecheck
npm run build
cd src-tauri
cargo test --lib
cargo clippy --all-targets -- -D warnings
```

## Lineage

Widget 64 is derived from [Pugbread/Terminal-64](https://github.com/Pugbread/Terminal-64) and preserves its Git history and MIT license.

## License

[MIT](LICENSE) © Pugbread
