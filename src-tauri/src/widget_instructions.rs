use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const WIDGET_INSTRUCTION_FILENAMES: [&str; 2] = ["CLAUDE.md", "AGENTS.md"];

const WIDGET_BUILDING_INSTRUCTIONS: &str = r#"You are building a widget for Widget 64, a spatial desktop runtime for standalone Tauri widgets.

A widget has two equal targets: it runs inside Widget 64 from `index.html`, and it runs as its own Tauri 2 desktop app from the `src-tauri/` project in this folder. Build one responsive frontend for both targets. Widget 64 hot-loads the source files through its local HTTP server; Vite builds those same files for Tauri.

**Rules:**
- The entry point is always `index.html` - Widget 64 loads it automatically
- You can use MULTIPLE files: separate CSS, JS, images, JSON, sub-pages - anything served over HTTP works. Use relative paths (e.g. `<script src="app.js">`, `<link href="style.css">`)
- The iframe is sandboxed with `allow-scripts allow-same-origin allow-popups allow-forms allow-modals` and has camera/microphone/geolocation/clipboard permissions
- You CAN use external CDN imports, embed external iframes, and fetch from APIs
- Widget 64 auto-reloads the iframe whenever ANY file in the widget folder changes
- Make it visually polished - use good typography, spacing, and color
- The widget should be responsive and look good at any size (the user can resize the panel)
- For simple widgets, a single `index.html` with inline CSS/JS is fine. For complex widgets, split into multiple files
- Preserve the generated Tauri project and keep `npm run tauri dev` and `npm run tauri build` working
- Detect embedded mode with `window.parent !== window`. Host-only bridge calls need a sensible standalone fallback or a clear unavailable state
- Do not depend on a fixed aspect ratio. Test portrait, square, and wide layouts

## Run targets

```bash
# Browser/Vite development
npm install
npm run dev

# Standalone native window
npm run tauri dev

# Production desktop build
npm run tauri build
```

## Widget 64 host API (postMessage bridge)

Widgets communicate with Widget 64 via `window.parent.postMessage(msg, "*")` and listen for responses via `window.addEventListener("message", handler)`. All async operations return results via response events. Include an `id` field in your payload to correlate requests with responses. The generated `src/bridge.js` already wraps this contract and includes standalone fallbacks; extend it rather than duplicating request plumbing.

```js
// Reusable helper - use this for all async bridge calls
function t64(type, payload = {}) {
  return new Promise((resolve) => {
    const id = Math.random().toString(36).slice(2);
    const handler = (e) => {
      if (e.data?.payload?.id === id) {
        window.removeEventListener("message", handler);
        resolve(e.data.payload);
      }
    };
    window.addEventListener("message", handler);
    window.parent.postMessage({ type, payload: { ...payload, id } }, "*");
  });
}
```

---

### 1. SHELL / SYSTEM - Run any command

| Request | Payload | Response event | Response payload |
|---|---|---|---|
| `t64:exec` | `{ command, cwd?, id? }` | `t64:exec-result` | `{ id, stdout, stderr, code }` |

```js
const result = await t64("t64:exec", { command: "git log --oneline -20" });
const ls = await t64("t64:exec", { command: "ls -la", cwd: "/Users/me/projects" });
```

---

### 2. FILE SYSTEM - Read, write, list, search, delete

| Request | Payload | Response event | Response payload |
|---|---|---|---|
| `t64:read-file` | `{ path, id? }` | `t64:file-content` | `{ id, path, content, error }` |
| `t64:write-file` | `{ path, content, id? }` | `t64:file-written` | `{ id, path, error }` |
| `t64:list-dir` | `{ path, id? }` | `t64:dir-listing` | `{ id, path, entries[], error }` |
| `t64:search-files` | `{ cwd, query, id? }` | `t64:search-results` | `{ id, results[], error }` |
| `t64:delete-files` | `{ paths[], id? }` | `t64:files-deleted` | `{ id, error }` |

```js
const file = await t64("t64:read-file", { path: "/Users/me/project/src/main.ts" });
const dir = await t64("t64:list-dir", { path: "/Users/me/project/src" });
// dir.entries = [{ name, is_dir, size, modified }, ...]
```

---

### 3. TERMINAL - Create and control interactive terminals

| Request | Payload | Response event | Response payload |
|---|---|---|---|
| `t64:create-terminal` | `{ cwd?, id? }` | `t64:terminal-created` | `{ id, terminalId }` |
| `t64:write-terminal` | `{ terminalId, data }` | none (fire & forget) | - |

```js
const term = await t64("t64:create-terminal", { cwd: "/Users/me/project" });
window.parent.postMessage({ type: "t64:write-terminal", payload: { terminalId: term.terminalId, data: "npm run dev\r" } }, "*");
```

---

### 4. AI SESSIONS - Create sessions and send prompts

| Request | Payload | Response event | Response payload |
|---|---|---|---|
| `t64:create-session` | `{ cwd?, name?, prompt?, provider?, id? }` | `t64:session-spawned` | `{ id, sessionId }` |
| `t64:send-prompt` | `{ sessionId, prompt, id? }` | `t64:prompt-sent` | `{ id, error }` |
| `t64:request-state` | none | `t64:state` | `{ sessions, activeTerminals, theme }` |
| `t64:request-messages` | `{ sessionId }` | `t64:messages` | `{ sessionId, messages[] }` |
| `t64:subscribe-session-events` | `{ events?: "all" | string[] }` | `t64:session-events-subscribed` | `{ events[] }` |
| `t64:unsubscribe-session-events` | `{ events?: "all" | string[] }` | `t64:session-events-unsubscribed` | `{ events[] }` |

---

### 5. REAL-TIME EVENTS - Listen to AI session activity

Session activity events are opt-in. Send `t64:subscribe-session-events` first with `"all"` or any of `"session"`, `"message"`, `"tool-result"`, `"streaming"`, `"streaming-text"`.

| `event.data.type` | Payload | When |
|---|---|---|
| `t64:init` | `{ sessions, activeTerminals, theme }` | On iframe load - full app state snapshot |
| `t64:state` | Same as init | Response to `t64:request-state` |
| `t64:message` | `{ sessionId, messageId, role, content, toolCalls[] }` | New message in any AI session |
| `t64:tool-result` | `{ sessionId, toolCallId, toolName, input, result, isError }` | Tool call completed |
| `t64:streaming` | `{ sessionId, isStreaming }` | A specific session starts/stops streaming |
| `t64:any-streaming` | `{ isStreaming }` | True if ANY session is currently streaming |
| `t64:streaming-text` | `{ sessionId, text }` | Live streaming text update |
| `t64:messages` | `{ sessionId, messages[] }` | Response to `t64:request-messages` |
| `t64:session-created` | `{ sessionId, name, cwd }` | New session created |

---

### 6. EMBEDDED BROWSER - Load any webpage inside the widget

| Request | Payload | Response |
|---|---|---|
| `t64:embed-browser` | `{ url }` | `t64:browser-ready { browserId }` |
| `t64:navigate-browser` | `{ url }` | - |
| `t64:show-browser` / `t64:hide-browser` | none | - |
| `t64:eval-browser` | `{ js }` | - |
| `t64:close-browser` | none | - |
| `t64:open-url` | `{ url, title? }` | Opens a separate browser panel on canvas |

---

### 7. FETCH PROXY - Bypass CORS restrictions

| Request | Payload | Response event | Response payload |
|---|---|---|---|
| `t64:fetch` | `{ url, method?, headers?, body?, id? }` | `t64:fetch-result` | `{ id, status, ok, headers, body, is_base64, error }` |

Fetches any URL through the Rust backend, bypassing CORS. Binary responses are returned as base64 (`is_base64: true`). Max 50MB.

```js
const res = await t64("t64:fetch", { url: "https://api.github.com/repos/owner/repo", headers: { "Accept": "application/json" } });
const data = JSON.parse(res.body);
```

---

### 8. PERSISTENT STATE - Save data across reloads

| Request | Payload | Response event | Response payload |
|---|---|---|---|
| `t64:get-state` | `{ key?, id? }` | `t64:state-value` | `{ id, key, value, error }` |
| `t64:set-state` | `{ key, value, id? }` | `t64:state-saved` | `{ id, error }` |
| `t64:clear-state` | `{ id? }` | `t64:state-cleared` | `{ id, error }` |

State is stored per-widget in `~/.terminal64/widgets/{id}/state.json`. Omit `key` in get-state to retrieve all keys.

```js
await t64("t64:set-state", { key: "lastQuery", value: "SELECT * FROM users" });
const saved = await t64("t64:get-state", { key: "lastQuery" });
// saved.value === "SELECT * FROM users"
```

---

### 9. FILE OPEN - Open files in the Monaco editor overlay

| Request | Payload | Response |
|---|---|---|
| `t64:open-file` | `{ path }` | - (opens in first available AI session's editor) |

```js
window.parent.postMessage({ type: "t64:open-file", payload: { path: "/Users/me/project/src/main.ts" } }, "*");
```

---

### 10. SYSTEM NOTIFICATIONS - macOS native alerts

| Request | Payload | Response event | Response payload |
|---|---|---|---|
| `t64:notify` | `{ title, body?, id? }` | `t64:notify-result` | `{ id, error }` |

```js
await t64("t64:notify", { title: "Build Complete", body: "No errors found" });
```

---

### 11. INTER-WIDGET COMMUNICATION - Pub/sub between widgets

| Request | Payload | Response |
|---|---|---|
| `t64:subscribe` | `{ topic }` | Receives `t64:broadcast` events with `{ topic, data }` |
| `t64:unsubscribe` | `{ topic }` | - |
| `t64:broadcast` | `{ topic, data }` | Sent to all OTHER widgets subscribed to that topic |

```js
// Widget A: subscribe to "data-updates"
window.parent.postMessage({ type: "t64:subscribe", payload: { topic: "data-updates" } }, "*");
window.addEventListener("message", (e) => {
  if (e.data?.type === "t64:broadcast" && e.data.payload?.topic === "data-updates") {
    console.log("Got update:", e.data.payload.data);
  }
});

// Widget B: broadcast to all subscribers
window.parent.postMessage({ type: "t64:broadcast", payload: { topic: "data-updates", data: { count: 42 } } }, "*");
```

---

### Theme colors (from `t64:init` payload.theme.ui):
`bg`, `bgSecondary`, `bgTertiary`, `fg`, `fgSecondary`, `fgMuted`, `border`, `accent`, `accentHover`

---

### Example: Git commit visualizer
```js
const result = await t64("t64:exec", { command: "git log --oneline --graph --all -50" });
document.getElementById("graph").textContent = result.stdout;

setInterval(async () => {
  const status = await t64("t64:exec", { command: "git status --porcelain" });
  document.getElementById("status").textContent = status.stdout || "Clean";
}, 3000);
```

Do not infer requirements from the folder name alone. If the user has not described what the widget should do, how it should look, or which specific features it needs, ask for those details before writing code."#;

const WIDGET_THEME_GUIDANCE: &str = r#"**Theme is reactive.** Do NOT hardcode colors. On load, listen for the `t64:init` event and read `payload.theme.ui` to get the current theme colors (bg, fg, accent, border, bgSecondary, fgMuted, etc.), then apply them as CSS variables or inline styles. The theme can change at any time - use the `t64:init` event each time the iframe reloads to stay in sync."#;

fn instruction_content() -> String {
    format!("{WIDGET_BUILDING_INSTRUCTIONS}\n\n{WIDGET_THEME_GUIDANCE}\n")
}

pub(crate) fn write_widget_instruction_files(widget_dir: &Path) -> Result<Vec<PathBuf>, String> {
    if !widget_dir.exists() {
        return Err(format!(
            "Widget folder does not exist: {}",
            widget_dir.display()
        ));
    }
    if !widget_dir.is_dir() {
        return Err(format!(
            "Widget path is not a directory: {}",
            widget_dir.display()
        ));
    }

    let mut entries = fs::read_dir(widget_dir).map_err(|e| format!("read widget dir: {e}"))?;
    if entries
        .next()
        .transpose()
        .map_err(|e| format!("read widget dir: {e}"))?
        .is_some()
    {
        return Err(
            "Widget instruction files are only written to fresh empty widget folders".into(),
        );
    }

    let content = instruction_content();
    let mut written = Vec::new();
    for filename in WIDGET_INSTRUCTION_FILENAMES {
        let path = widget_dir.join(filename);
        if let Err(err) = write_new_file(&path, content.as_bytes()) {
            for written_path in written {
                let _ = fs::remove_file(written_path);
            }
            return Err(err);
        }
        written.push(path);
    }

    Ok(written)
}

fn write_new_file(path: &Path, content: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    file.write_all(content)
        .map_err(|e| format!("write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::write_widget_instruction_files;
    use std::error::Error;
    use std::fs;

    #[test]
    fn writes_claude_and_agents_instructions_to_empty_widget_dir() -> Result<(), Box<dyn Error>> {
        let dir = std::env::temp_dir().join(format!(
            "terminal64-widget-instructions-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir)?;

        let paths = write_widget_instruction_files(&dir)?;
        let claude = fs::read_to_string(dir.join("CLAUDE.md"))?;
        let agents = fs::read_to_string(dir.join("AGENTS.md"))?;

        assert_eq!(paths.len(), 2);
        assert!(claude.contains("Widget 64 host API"));
        assert!(claude.contains("two equal targets"));
        assert!(claude.contains("Theme is reactive"));
        assert_eq!(claude, agents);

        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn refuses_to_write_into_existing_widget_dir() -> Result<(), Box<dyn Error>> {
        let dir = std::env::temp_dir().join(format!(
            "terminal64-widget-instructions-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("index.html"), "<!doctype html>")?;

        let result = write_widget_instruction_files(&dir);

        assert!(result.is_err());
        assert!(!dir.join("CLAUDE.md").exists());
        assert!(!dir.join("AGENTS.md").exists());

        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
