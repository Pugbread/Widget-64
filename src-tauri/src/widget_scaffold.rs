use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use crate::widget_instructions;

pub(crate) fn scaffold_widget_project(
    widget_dir: &Path,
    widget_id: &str,
    display_name: &str,
) -> Result<(), String> {
    if widget_dir.exists() {
        let mut entries = fs::read_dir(widget_dir).map_err(|e| format!("read widget dir: {e}"))?;
        if entries
            .next()
            .transpose()
            .map_err(|e| format!("read widget dir: {e}"))?
            .is_some()
        {
            return Err(format!("A widget named '{widget_id}' already exists"));
        }
    } else {
        fs::create_dir_all(widget_dir).map_err(|e| format!("create widget dir: {e}"))?;
    }

    let title = if display_name.trim().is_empty() {
        title_from_id(widget_id)
    } else {
        display_name.trim().to_string()
    };
    let crate_stem = format!("widget_{}", widget_id.replace('-', "_"));
    let identifier_stem = widget_id.replace('_', "-");

    if let Err(error) = scaffold_files(widget_dir, widget_id, &title, &crate_stem, &identifier_stem)
    {
        let _ = fs::remove_dir_all(widget_dir);
        return Err(error);
    }
    Ok(())
}

fn scaffold_files(
    widget_dir: &Path,
    widget_id: &str,
    title: &str,
    crate_stem: &str,
    identifier_stem: &str,
) -> Result<(), String> {
    widget_instructions::write_widget_instruction_files(widget_dir)?;

    let files = [
        (".gitignore", GITIGNORE.to_string()),
        ("README.md", readme(title)),
        ("package.json", package_json(widget_id)),
        ("vite.config.js", VITE_CONFIG.to_string()),
        ("index.html", index_html(title)),
        ("src/main.js", main_js(title)),
        ("src/bridge.js", BRIDGE_JS.to_string()),
        ("src/style.css", STYLE_CSS.to_string()),
        ("src-tauri/Cargo.toml", cargo_toml(widget_id, crate_stem)),
        (
            "src-tauri/build.rs",
            "fn main() { tauri_build::build() }\n".to_string(),
        ),
        ("src-tauri/src/lib.rs", tauri_lib(crate_stem)),
        ("src-tauri/src/main.rs", tauri_main(crate_stem)),
        (
            "src-tauri/tauri.conf.json",
            tauri_config(title, identifier_stem),
        ),
        (
            "src-tauri/capabilities/default.json",
            CAPABILITIES.to_string(),
        ),
    ];

    for (relative_path, content) in files {
        write_new_file(&widget_dir.join(relative_path), content.as_bytes())?;
    }
    Ok(())
}

fn write_new_file(path: &Path, content: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    file.write_all(content)
        .map_err(|e| format!("write {}: {e}", path.display()))
}

fn title_from_id(widget_id: &str) -> String {
    widget_id
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"Widget\"".to_string())
}

fn package_json(widget_id: &str) -> String {
    format!(
        r#"{{
  "name": "{widget_id}",
  "private": true,
  "version": "0.1.0",
  "type": "module",
  "scripts": {{
    "dev": "vite",
    "build": "vite build",
    "preview": "vite preview",
    "tauri": "tauri"
  }},
  "devDependencies": {{
    "@tauri-apps/cli": "^2.10.1",
    "vite": "^8.0.0"
  }}
}}
"#
    )
}

fn index_html(title: &str) -> String {
    let title = html_escape(title);
    format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <meta name="color-scheme" content="dark" />
    <title>{title}</title>
    <link rel="stylesheet" href="./src/style.css" />
  </head>
  <body>
    <main id="app"></main>
    <script type="module" src="./src/main.js"></script>
  </body>
</html>
"#
    )
}

fn main_js(title: &str) -> String {
    let title = json_string(title);
    format!(
        r##"import {{ widget64 }} from "./bridge.js";

const title = {title};
const root = document.querySelector("#app");

root.innerHTML = `
  <section class="starter">
    <div class="starter__topline">
      <span class="starter__status"><i></i>${{widget64.isEmbedded ? "Running in Widget 64" : "Running standalone"}}</span>
      <span class="starter__runtime">TAURI / WEB</span>
    </div>
    <div class="starter__content">
      <span class="starter__mark" aria-hidden="true"><i></i><i></i><i></i><i></i></span>
      <p class="starter__eyebrow">Your widget is ready</p>
      <h1>${{title}}</h1>
      <p class="starter__copy">Edit <code>src/main.js</code> to begin. This exact frontend runs here and as a standalone Tauri app.</p>
    </div>
    <footer>
      <span>Responsive by default</span>
      <span>${{new Date().toLocaleDateString(undefined, {{ month: "short", day: "numeric" }})}}</span>
    </footer>
  </section>
`;

widget64.on("t64:init", (payload) => {{
  const colors = payload?.theme?.ui;
  if (!colors) return;
  document.documentElement.style.setProperty("--host-bg", colors.bg);
  document.documentElement.style.setProperty("--host-fg", colors.fg);
  document.documentElement.style.setProperty("--host-accent", colors.accent);
}});
"##
    )
}

fn cargo_toml(widget_id: &str, crate_stem: &str) -> String {
    format!(
        r#"[package]
name = "{widget_id}-widget"
version = "0.1.0"
description = "A standalone Widget 64 widget"
edition = "2021"
rust-version = "1.77.2"

[lib]
name = "{crate_stem}_widget_lib"
crate-type = ["staticlib", "cdylib", "rlib"]

[build-dependencies]
tauri-build = {{ version = "2", features = [] }}

[dependencies]
tauri = {{ version = "2", features = [] }}
serde = {{ version = "1", features = ["derive"] }}
serde_json = "1"
"#
    )
}

fn tauri_lib(crate_stem: &str) -> String {
    format!(
        r#"#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {{
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running {crate_stem} widget");
}}
"#
    )
}

fn tauri_main(crate_stem: &str) -> String {
    format!(
        r#"#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {{
    {crate_stem}_widget_lib::run();
}}
"#
    )
}

fn tauri_config(title: &str, identifier_stem: &str) -> String {
    let title = json_string(title);
    format!(
        r#"{{
  "$schema": "../node_modules/@tauri-apps/cli/config.schema.json",
  "productName": {title},
  "version": "0.1.0",
  "identifier": "com.widget64.{identifier_stem}",
  "build": {{
    "beforeDevCommand": "npm run dev",
    "beforeBuildCommand": "npm run build",
    "devUrl": "http://localhost:1421",
    "frontendDist": "../dist"
  }},
  "app": {{
    "windows": [{{
      "title": {title},
      "width": 720,
      "height": 520,
      "minWidth": 320,
      "minHeight": 240,
      "resizable": true,
      "center": true
    }}],
    "security": {{ "csp": null }}
  }},
  "bundle": {{
    "active": true,
    "targets": "all"
  }}
}}
"#
    )
}

fn readme(title: &str) -> String {
    format!(
        r#"# {title}

This widget was generated by Widget 64. The frontend runs both inside Widget 64 and as a standalone Tauri 2 desktop app.

## Run it

```bash
npm install
npm run tauri dev
```

For browser-only development, run `npm run dev`. Build a native release with `npm run tauri build`.

Read `AGENTS.md` before making changes; it contains the embedded bridge contract, responsive requirements, and standalone fallback rules.
"#
    )
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

const GITIGNORE: &str = "node_modules/\ndist/\nsrc-tauri/target/\n.DS_Store\n";

const VITE_CONFIG: &str = r#"import { defineConfig } from "vite";

export default defineConfig({
  clearScreen: false,
  server: {
    port: 1421,
    strictPort: true,
  },
});
"#;

const CAPABILITIES: &str = r#"{
  "$schema": "../gen/schemas/desktop-schema.json",
  "identifier": "default",
  "description": "Default capability for the standalone widget window",
  "windows": ["main"],
  "permissions": ["core:default"]
}
"#;

const BRIDGE_JS: &str = r#"const listeners = new Map();
const pending = new Map();
const standaloneState = new Map();

export const isEmbedded = window.parent !== window;

window.addEventListener("message", (event) => {
  const message = event.data;
  if (!message || typeof message.type !== "string") return;
  const id = message.payload?.id;
  if (id && pending.has(id)) {
    pending.get(id)(message.payload);
    pending.delete(id);
  }
  for (const listener of listeners.get(message.type) ?? []) listener(message.payload);
});

async function standalone(type, payload) {
  if (type === "t64:get-state") {
    return { id: payload.id, key: payload.key, value: payload.key ? standaloneState.get(payload.key) : Object.fromEntries(standaloneState) };
  }
  if (type === "t64:set-state") {
    standaloneState.set(payload.key, payload.value);
    localStorage.setItem(`widget64:${payload.key}`, JSON.stringify(payload.value));
    return { id: payload.id };
  }
  if (type === "t64:clear-state") {
    standaloneState.clear();
    return { id: payload.id };
  }
  if (type === "t64:fetch") {
    const response = await fetch(payload.url, { method: payload.method, headers: payload.headers, body: payload.body });
    return { id: payload.id, status: response.status, ok: response.ok, headers: Object.fromEntries(response.headers), body: await response.text(), is_base64: false };
  }
  if (type === "t64:open-url") {
    window.open(payload.url, "_blank", "noopener,noreferrer");
    return { id: payload.id };
  }
  if (type === "t64:notify") {
    if ("Notification" in window && Notification.permission === "granted") new Notification(payload.title, { body: payload.body });
    return { id: payload.id };
  }
  return { id: payload.id, error: `${type} is only available inside Widget 64` };
}

export const widget64 = {
  isEmbedded,
  on(type, listener) {
    const group = listeners.get(type) ?? new Set();
    group.add(listener);
    listeners.set(type, group);
    return () => group.delete(listener);
  },
  emit(type, payload = {}) {
    if (isEmbedded) window.parent.postMessage({ type, payload }, "*");
  },
  request(type, payload = {}) {
    const id = crypto.randomUUID?.() ?? Math.random().toString(36).slice(2);
    const message = { ...payload, id };
    if (!isEmbedded) return standalone(type, message);
    return new Promise((resolve, reject) => {
      const timeout = window.setTimeout(() => {
        pending.delete(id);
        reject(new Error(`Widget 64 request timed out: ${type}`));
      }, 15000);
      pending.set(id, (result) => {
        window.clearTimeout(timeout);
        resolve(result);
      });
      window.parent.postMessage({ type, payload: message }, "*");
    });
  },
};
"#;

const STYLE_CSS: &str = r#":root {
  color-scheme: dark;
  --host-bg: #0d1015;
  --host-fg: #edf1f7;
  --host-accent: #7fa9f8;
  font-family: Inter, "SF Pro Display", "Segoe UI", sans-serif;
  font-synthesis: none;
  -webkit-font-smoothing: antialiased;
}

* { box-sizing: border-box; }

html, body, #app { width: 100%; height: 100%; margin: 0; }

body {
  overflow: hidden;
  color: var(--host-fg);
  background: var(--host-bg);
}

.starter {
  width: 100%;
  height: 100%;
  min-height: 240px;
  display: grid;
  grid-template-rows: auto 1fr auto;
  padding: clamp(16px, 4vw, 30px);
  background:
    radial-gradient(circle at 75% 15%, color-mix(in srgb, var(--host-accent) 14%, transparent), transparent 35%),
    var(--host-bg);
}

.starter__topline,
.starter footer {
  display: flex;
  align-items: center;
  justify-content: space-between;
  color: color-mix(in srgb, var(--host-fg) 43%, transparent);
  font-family: "SFMono-Regular", Consolas, monospace;
  font-size: clamp(8px, 1.6vw, 10px);
  letter-spacing: .055em;
  text-transform: uppercase;
}

.starter__status { display: flex; align-items: center; gap: 7px; }
.starter__status i { width: 6px; height: 6px; border-radius: 50%; background: #7bd99a; box-shadow: 0 0 8px rgba(123, 217, 154, .6); }

.starter__content { align-self: center; max-width: 560px; }

.starter__mark {
  width: 32px;
  height: 32px;
  display: grid;
  grid-template-columns: repeat(2, 1fr);
  gap: 2px;
  padding: 4px;
  border-radius: 10px;
  background: color-mix(in srgb, var(--host-accent) 13%, transparent);
  box-shadow: 0 0 0 1px color-mix(in srgb, var(--host-accent) 22%, transparent);
}

.starter__mark i { border-radius: 2px; background: var(--host-accent); opacity: .82; }
.starter__eyebrow { margin: 16px 0 8px; color: var(--host-accent); font-size: clamp(9px, 1.8vw, 11px); font-weight: 650; letter-spacing: .07em; text-transform: uppercase; }
.starter h1 { margin: 0; font-size: clamp(30px, 9vw, 64px); font-weight: 540; letter-spacing: -.055em; line-height: .98; text-wrap: balance; }
.starter__copy { max-width: 500px; margin: 16px 0 0; color: color-mix(in srgb, var(--host-fg) 52%, transparent); font-size: clamp(11px, 2vw, 14px); line-height: 1.55; text-wrap: pretty; }
.starter code { color: color-mix(in srgb, var(--host-accent) 82%, white); font-family: "SFMono-Regular", Consolas, monospace; font-size: .88em; }
.starter footer { padding-top: 14px; border-top: 1px solid color-mix(in srgb, var(--host-fg) 9%, transparent); }

@media (max-height: 340px) {
  .starter__mark { display: none; }
  .starter__eyebrow { margin-top: 0; }
}
"#;

#[cfg(test)]
mod tests {
    use super::scaffold_widget_project;
    use std::error::Error;
    use std::fs;

    #[test]
    fn creates_tauri_app_that_also_embeds() -> Result<(), Box<dyn Error>> {
        let dir = std::env::temp_dir().join(format!("widget64-scaffold-{}", uuid::Uuid::new_v4()));
        scaffold_widget_project(&dir, "build-monitor", "Build Monitor")?;

        assert!(dir.join("AGENTS.md").exists());
        assert!(dir.join("index.html").exists());
        assert!(dir.join("src/bridge.js").exists());
        assert!(dir.join("src-tauri/Cargo.toml").exists());
        assert!(dir.join("src-tauri/src/main.rs").exists());
        assert!(dir.join("src-tauri/tauri.conf.json").exists());
        assert!(dir.join("src-tauri/capabilities/default.json").exists());
        let agents = fs::read_to_string(dir.join("AGENTS.md"))?;
        assert!(agents.contains("Tauri 2 desktop app by definition"));
        assert!(agents.contains("npm run tauri dev"));

        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn refuses_to_overwrite_existing_widget() -> Result<(), Box<dyn Error>> {
        let dir = std::env::temp_dir().join(format!("widget64-scaffold-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("keep.txt"), "user data")?;

        let result = scaffold_widget_project(&dir, "existing", "Existing");
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(dir.join("keep.txt"))?, "user data");

        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
