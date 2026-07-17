# Widget 64 contributor context

Widget 64 is a Tauri 2 + React 19 desktop app derived from Terminal 64. Its visible product surface is intentionally limited to widgets and their infinite canvas.

## Product invariants

- Keep the workspace widget-only. Do not add terminal, AI chat, browser, or editor panel types to the UI.
- Preserve all three views: free canvas, fit-all overview, and horizontal gallery.
- Preserve widget aspect ratios in overview and gallery layouts.
- Keep existing Terminal 64 widgets compatible with the local server and postMessage bridge.
- A new widget is a Tauri 2 app by definition; Widget 64 embeds that app's frontend without creating a separate web-only widget.
- Write the complete widget-building context to `AGENTS.md` and `CLAUDE.md` before adding other scaffold files.
- Never overwrite a non-empty widget folder during scaffolding.

## Important modules

- `src/components/widget64/WidgetWorkspace.tsx` owns the three layout modes and frame interactions.
- `src/components/widget64/WidgetManager.tsx` owns creation, opening, zip installation, and deletion.
- `src/stores/canvasStore.ts` persists widget frames and the canvas transform.
- `src-tauri/src/widget_scaffold.rs` generates dual-runtime widget projects transactionally.
- `src-tauri/src/widget_instructions.rs` contains the context injected into widget folders.
- `src/components/widget/WidgetPanel.tsx` and `useWidgetBridgeHost.ts` are the compatibility-sensitive host runtime.

## Verification

Run `npm run typecheck`, `npm run build`, `cargo fmt --check`, `cargo test --lib`, and `cargo clippy --all-targets -- -D warnings` before release.
