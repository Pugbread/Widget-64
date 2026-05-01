//! `CodexAdapter` — OpenAI Codex CLI-backed implementation of `ProviderAdapter`.
//!
//! The primary path uses `codex app-server --listen stdio://`, speaks the
//! JSON-RPC app-server protocol, and translates rich app-server notifications
//! into Terminal 64's shared `ProviderRuntimeEvent` surface while preserving
//! the existing `codex-event` compatibility stream. The legacy
//! `codex exec --json` adapter is retained as a fallback via
//! `T64_CODEX_TRANSPORT=exec`.
//!
//! Supported flags (mapped from CreateCodexRequest / SendCodexPromptRequest):
//!   -m/--model <id>                                  → req.model
//!   -s/--sandbox {read-only|workspace-write|...}     → req.sandbox_mode
//!   --full-auto                                      → req.full_auto
//!   --dangerously-bypass-approvals-and-sandbox       → req.yolo
//!   --skip-git-repo-check                            → req.skip_git_repo_check
//!   -c approval_policy=<v>                           → req.approval_policy
//!   -c model_reasoning_effort=<v>                    → req.effort
//!   -C <cwd>                                         → req.cwd
//!
//! Terminal 64 keeps a local session id for UI routing and stores Codex's
//! thread id separately for resume/fork/rollback. The app-server transport
//! keeps that thread alive through JSON-RPC; the legacy exec transport
//! re-attaches with `codex exec resume <thread_id>`.

use serde::Deserialize;
use serde_json::{json, Value as JsonValue};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

use crate::providers::snapshots::{
    snapshot_from_descriptor, SnapshotControlDescriptor, SnapshotDescriptor,
    SnapshotDisplayDescriptor, SnapshotInstallDescriptor, SnapshotOptionDescriptor,
};
use crate::providers::traits::{
    ProviderAdapter, ProviderAdapterCapabilities, ProviderAdapterError, ProviderCommandLifecycle,
    ProviderCreateSessionRequest, ProviderHistoryCapabilities, ProviderHistoryRequest,
    ProviderHistoryResponse, ProviderKind, ProviderPreparedCommand, ProviderSendPromptRequest,
    ProviderSessionModelSwitchMode,
};
use crate::providers::util::{
    cap_event_size, expanded_tool_path, shim_command, terminate_child_process,
};
use crate::providers::{
    emit_provider_event, emit_provider_runtime_event, ProviderRuntimeEvent,
    ProviderRuntimeEventType,
};
use crate::types::{
    CodexDone, CodexEvent, CreateCodexRequest, DiskSession, HistoryMessage, HistoryToolCall,
    ProviderSnapshot, SendCodexPromptRequest,
};

const OPENAI_MODEL_OPTIONS: &[SnapshotOptionDescriptor] = &[
    SnapshotOptionDescriptor::basic("gpt-5.5", "GPT-5.5"),
    SnapshotOptionDescriptor::basic("gpt-5.4", "GPT-5.4"),
    SnapshotOptionDescriptor::basic("gpt-5.4-mini", "GPT-5.4 Mini"),
    SnapshotOptionDescriptor::basic("gpt-5.3-codex", "GPT-5.3 Codex"),
    SnapshotOptionDescriptor::basic("gpt-5.2", "GPT-5.2"),
];

const OPENAI_EFFORT_OPTIONS: &[SnapshotOptionDescriptor] = &[
    SnapshotOptionDescriptor::basic("minimal", "Minimal"),
    SnapshotOptionDescriptor::basic("low", "Low"),
    SnapshotOptionDescriptor::basic("medium", "Medium"),
    SnapshotOptionDescriptor::basic("high", "High"),
    SnapshotOptionDescriptor::basic("xhigh", "Extra High"),
];

const OPENAI_PERMISSION_OPTIONS: &[SnapshotOptionDescriptor] = &[
    SnapshotOptionDescriptor::described(
        "read-only",
        "Read",
        "No filesystem writes",
        "#89b4fa",
        None,
    ),
    SnapshotOptionDescriptor::described(
        "workspace",
        "Workspace",
        "Write inside cwd",
        "#94e2d5",
        None,
    ),
    SnapshotOptionDescriptor::described(
        "full-auto",
        "Auto",
        "Workspace + auto-approve all",
        "#a6e3a1",
        None,
    ),
    SnapshotOptionDescriptor::described(
        "yolo",
        "YOLO",
        "No sandbox, no approvals",
        "#f38ba8",
        None,
    ),
];

const OPENAI_CONTROLS: &[SnapshotControlDescriptor] = &[
    SnapshotControlDescriptor::select(
        "model",
        "Model",
        "gpt-5.5",
        "topbar",
        OPENAI_MODEL_OPTIONS,
        None,
        Some("model"),
    ),
    SnapshotControlDescriptor::select(
        "effort",
        "Effort",
        "medium",
        "topbar",
        OPENAI_EFFORT_OPTIONS,
        None,
        Some("effort"),
    ),
    SnapshotControlDescriptor::select(
        "sandbox",
        "Sandbox",
        "workspace",
        "composer",
        OPENAI_PERMISSION_OPTIONS,
        Some("sandbox"),
        Some("permission"),
    ),
];

const OPENAI_SNAPSHOT_DESCRIPTOR: SnapshotDescriptor = SnapshotDescriptor {
    id: "openai",
    display: SnapshotDisplayDescriptor {
        label: "OpenAI",
        short_label: "Codex",
        brand_title: "OpenAI Codex",
        empty_state_label: "Codex",
        default_session_name: "Codex",
    },
    auth_label: "Codex CLI",
    install: SnapshotInstallDescriptor {
        command: "codex",
        status_label: "Codex",
    },
    controls: OPENAI_CONTROLS,
};

// ── Binary discovery ───────────────────────────────────────

pub fn resolve_codex_path() -> String {
    let lookup = {
        let (cmd, arg) = if cfg!(windows) {
            ("where", "codex")
        } else {
            ("which", "codex")
        };
        let mut c = std::process::Command::new(cmd);
        c.arg(arg)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .stdin(Stdio::null());
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        c.output()
    };
    if let Ok(p) = lookup {
        if p.status.success() {
            let s = String::from_utf8_lossy(&p.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }

    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok();

    let mut candidates: Vec<String> = Vec::new();
    if cfg!(windows) {
        if let Some(ref h) = home {
            candidates.push(format!("{}\\.local\\bin\\codex.exe", h));
            candidates.push(format!("{}\\.local\\bin\\codex.cmd", h));
        }
        if let Ok(appdata) = std::env::var("APPDATA") {
            candidates.push(format!("{}\\npm\\codex.cmd", appdata));
            candidates.push(format!("{}\\npm\\codex.exe", appdata));
        }
    } else {
        if let Some(ref h) = home {
            candidates.push(format!("{}/.local/bin/codex", h));
            candidates.push(format!("{}/.npm-global/bin/codex", h));
        }
        candidates.push("/usr/local/bin/codex".to_string());
        candidates.push("/opt/homebrew/bin/codex".to_string());
    }
    for c in &candidates {
        if std::path::Path::new(c).exists() {
            return c.clone();
        }
    }
    #[cfg(target_os = "windows")]
    return "codex.cmd".to_string();
    #[cfg(not(target_os = "windows"))]
    return "codex".to_string();
}

// ── Session state + command builder ────────────────────────

struct CodexInstance {
    child: Child,
    generation: u64,
}

static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const APP_SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const APP_SERVER_TURN_START_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Copy)]
enum InvokeMode<'a> {
    /// Fresh session — `codex exec --json [prompt]`.
    Fresh,
    /// Resume an existing session — `codex exec resume <id> --json [prompt]`.
    Resume(&'a str),
}

#[allow(clippy::too_many_arguments)]
fn build_command(
    mode: InvokeMode<'_>,
    cwd: &str,
    prompt: &str,
    sandbox_mode: &Option<String>,
    approval_policy: &Option<String>,
    model: &Option<String>,
    effort: &Option<String>,
    full_auto: bool,
    yolo: bool,
    skip_git_repo_check: bool,
    mcp_env: &Option<HashMap<String, String>>,
) -> Command {
    let codex_bin = resolve_codex_path();
    let mut cmd = shim_command(&codex_bin);

    // `-C, --cd` is a TOP-LEVEL codex flag (not an `exec` flag) — it must be
    // emitted before any subcommand. It's also the only way to set cwd for
    // `codex exec resume`, which does not accept -C.
    if !cwd.is_empty() && cwd != "." {
        cmd.arg("-C").arg(cwd);
        cmd.current_dir(cwd); // belt + suspenders
    }

    cmd.arg("exec");
    if matches!(mode, InvokeMode::Resume(_)) {
        cmd.arg("resume");
    }

    cmd.arg("--json");
    if skip_git_repo_check {
        cmd.arg("--skip-git-repo-check");
    }

    // Sandbox flag and the convenience presets are mutually exclusive in the
    // CLI: `--full-auto` and `--yolo` already imply a sandbox choice.
    // Extra wrinkle: `codex exec resume` does NOT accept `-s`, so we translate
    // to `-c sandbox_mode=<value>` (a generic config override that DOES work
    // on resume).
    if yolo {
        cmd.arg("--dangerously-bypass-approvals-and-sandbox");
    } else if full_auto {
        cmd.arg("--full-auto");
    } else if let Some(s) = sandbox_mode {
        if !s.is_empty() {
            match mode {
                InvokeMode::Fresh => {
                    cmd.arg("-s").arg(s);
                }
                InvokeMode::Resume(_) => {
                    cmd.arg("-c").arg(format!("sandbox_mode={}", s));
                }
            }
        }
    }

    if !yolo && !full_auto {
        if let Some(p) = approval_policy {
            if !p.is_empty() {
                cmd.arg("-c").arg(format!("approval_policy={}", p));
            }
        }
    }

    if let Some(m) = model {
        if !m.is_empty() {
            cmd.arg("-m").arg(m);
        }
    }

    if let Some(e) = effort {
        if !e.is_empty() {
            cmd.arg("-c").arg(format!("model_reasoning_effort={}", e));
        }
    }

    // Positional args come last, in the order the CLI expects:
    //   Fresh:   codex exec [OPTIONS] [PROMPT]
    //   Resume:  codex exec resume [OPTIONS] [SESSION_ID] [PROMPT]
    // NB: on Windows when shim_command routes through cmd.exe, embedded
    // newlines may be truncated. Same caveat as the Claude adapter; accepted
    // for the legacy exec fallback because the primary app-server transport
    // avoids this path.
    if let InvokeMode::Resume(thread_id) = mode {
        cmd.arg(thread_id);
    }
    cmd.arg(prompt);

    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    cmd.env("PATH", expanded_tool_path());
    if let Some(env) = mcp_env {
        for (key, value) in env {
            cmd.env(key, value);
        }
    }
    cmd
}

fn codex_transport_is_exec() -> bool {
    std::env::var("T64_CODEX_TRANSPORT")
        .map(|v| v.eq_ignore_ascii_case("exec"))
        .unwrap_or(false)
}

fn build_app_server_command(cwd: &str, mcp_env: &Option<HashMap<String, String>>) -> Command {
    let codex_bin = resolve_codex_path();
    let mut cmd = shim_command(&codex_bin);
    if !cwd.is_empty() && cwd != "." {
        cmd.current_dir(cwd);
    }
    cmd.arg("app-server")
        .arg("--listen")
        .arg("stdio://")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::piped());
    cmd.env("PATH", expanded_tool_path());
    if let Some(env) = mcp_env {
        for (key, value) in env {
            cmd.env(key, value);
        }
    }
    cmd
}

fn app_server_sandbox(
    sandbox_mode: &Option<String>,
    full_auto: bool,
    yolo: bool,
) -> Option<String> {
    if yolo {
        Some("danger-full-access".to_string())
    } else if full_auto {
        Some("workspace-write".to_string())
    } else {
        sandbox_mode.clone().filter(|s| !s.is_empty())
    }
}

fn app_server_approval_policy(
    approval_policy: &Option<String>,
    full_auto: bool,
    yolo: bool,
) -> Option<String> {
    if yolo || full_auto {
        Some("never".to_string())
    } else {
        approval_policy.clone().filter(|s| !s.is_empty())
    }
}

#[allow(clippy::too_many_arguments)]
fn app_server_thread_params(
    cwd: &str,
    sandbox_mode: &Option<String>,
    approval_policy: &Option<String>,
    model: &Option<String>,
    effort: &Option<String>,
    full_auto: bool,
    yolo: bool,
) -> JsonValue {
    let mut params = serde_json::Map::new();
    if !cwd.is_empty() {
        params.insert("cwd".to_string(), json!(cwd));
    }
    if let Some(model) = model.as_ref().filter(|m| !m.is_empty()) {
        params.insert("model".to_string(), json!(model));
    }
    if let Some(effort) = effort.as_ref().filter(|e| !e.is_empty()) {
        params.insert(
            "config".to_string(),
            json!({ "model_reasoning_effort": effort }),
        );
    }
    if let Some(sandbox) = app_server_sandbox(sandbox_mode, full_auto, yolo) {
        params.insert("sandbox".to_string(), json!(sandbox));
    }
    if let Some(policy) = app_server_approval_policy(approval_policy, full_auto, yolo) {
        params.insert("approvalPolicy".to_string(), json!(policy));
    }
    params.insert("serviceName".to_string(), json!("terminal-64"));
    // Codex app-server 0.125+ generated contracts require these booleans on
    // thread/start and thread/resume. Omitting them can make the request fail
    // before the server returns a thread id, which leaves Terminal 64 waiting
    // forever to issue turn/start.
    params.insert("experimentalRawEvents".to_string(), json!(false));
    params.insert("persistExtendedHistory".to_string(), json!(true));
    JsonValue::Object(params)
}

#[allow(clippy::too_many_arguments)]
fn app_server_turn_params(
    thread_id: &str,
    cwd: &str,
    prompt: &str,
    sandbox_mode: &Option<String>,
    approval_policy: &Option<String>,
    model: &Option<String>,
    effort: &Option<String>,
    collaboration_mode: &Option<String>,
    full_auto: bool,
    yolo: bool,
) -> JsonValue {
    let mut params = match app_server_thread_params(
        cwd,
        sandbox_mode,
        approval_policy,
        model,
        effort,
        full_auto,
        yolo,
    ) {
        JsonValue::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    params.insert("threadId".to_string(), json!(thread_id));
    params.insert(
        "input".to_string(),
        json!([{ "type": "text", "text": prompt, "text_elements": [] }]),
    );
    if let Some(effort) = effort.as_ref().filter(|e| !e.is_empty()) {
        params.insert("effort".to_string(), json!(effort));
    }
    if let Some(mode) = collaboration_mode
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| matches!(*s, "plan" | "default"))
    {
        let mode_model = model
            .as_ref()
            .filter(|m| !m.is_empty())
            .cloned()
            .unwrap_or_else(|| "gpt-5.3-codex".to_string());
        params.insert(
            "collaborationMode".to_string(),
            json!({
                "mode": mode,
                "settings": {
                    "model": mode_model,
                    "reasoning_effort": effort.as_ref().filter(|e| !e.is_empty()),
                    "developer_instructions": null,
                },
            }),
        );
    }
    JsonValue::Object(params)
}

fn write_json_rpc(stdin: &mut std::process::ChildStdin, value: &JsonValue) -> Result<(), String> {
    let line = serde_json::to_string(value).map_err(|e| e.to_string())?;
    stdin
        .write_all(line.as_bytes())
        .and_then(|_| stdin.write_all(b"\n"))
        .and_then(|_| stdin.flush())
        .map_err(|e| format!("codex app-server write: {}", e))
}

fn app_server_stderr_excerpt(stderr_buf: &Arc<Mutex<String>>) -> String {
    stderr_buf.lock().map(|s| s.clone()).unwrap_or_default()
}

fn app_server_timeout_message(
    phase: &str,
    timeout: Duration,
    stderr_buf: &Arc<Mutex<String>>,
) -> String {
    let stderr_msg = app_server_stderr_excerpt(stderr_buf);
    let mut msg = format!(
        "Codex app-server timed out during {} after {}s.",
        phase,
        timeout.as_secs()
    );
    if !stderr_msg.is_empty() {
        msg.push_str(" Stderr: ");
        msg.push_str(&stderr_msg);
    } else {
        msg.push_str(
            " Install a recent OpenAI Codex CLI or set T64_CODEX_TRANSPORT=exec to use the legacy transport.",
        );
    }
    msg
}

fn app_server_remaining_timeout(
    phase: &str,
    deadline: Instant,
    total_timeout: Duration,
    stderr_buf: &Arc<Mutex<String>>,
) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| app_server_timeout_message(phase, total_timeout, stderr_buf))
}

fn read_app_server_message(
    rx: &std::sync::mpsc::Receiver<Result<Option<String>, String>>,
    phase: &str,
    timeout: Option<Duration>,
    stderr_buf: &Arc<Mutex<String>>,
) -> Result<Option<JsonValue>, String> {
    loop {
        let received = match timeout {
            Some(timeout) => match rx.recv_timeout(timeout) {
                Ok(v) => v,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    return Err(app_server_timeout_message(phase, timeout, stderr_buf));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Ok(None);
                }
            },
            None => match rx.recv() {
                Ok(v) => v,
                Err(_) => return Ok(None),
            },
        };
        let Some(line) = received.map_err(|e| format!("codex app-server read: {}", e))? else {
            return Ok(None);
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parsed: JsonValue = serde_json::from_str(trimmed).map_err(|e| {
            format!(
                "codex app-server JSON parse during {}: {} (line: {})",
                phase,
                e,
                &trimmed[..trimmed.len().min(240)]
            )
        })?;
        return Ok(Some(parsed));
    }
}

fn app_server_error_from_response(method: &str, parsed: &JsonValue) -> String {
    let message = get_json_str(parsed, &["error", "message"])
        .or_else(|| get_json_str(parsed, &["error", "data", "message"]))
        .unwrap_or("Codex app-server request failed");
    let code = parsed
        .get("error")
        .and_then(|e| e.get("code"))
        .map(|v| format!(" code={}", v))
        .unwrap_or_default();
    format!("Codex app-server {} failed{}: {}", method, code, message)
}

fn app_server_protocol_diagnostics(value: &JsonValue) -> JsonValue {
    let result = value.get("result").unwrap_or(value);
    let server_info = result
        .get("serverInfo")
        .or_else(|| result.get("server_info"));
    let server_name = server_info
        .and_then(|s| s.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let server_version = server_info
        .and_then(|s| s.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let protocol_version = result
        .get("protocolVersion")
        .or_else(|| result.get("protocol_version"))
        .or_else(|| result.get("appServerProtocolVersion"))
        .cloned()
        .unwrap_or(JsonValue::Null);
    json!({
        "serverName": server_name,
        "serverVersion": server_version,
        "protocolVersion": protocol_version,
    })
}

fn emit_app_server_diagnostic(
    app_handle: &AppHandle,
    session_id: &str,
    phase: &str,
    details: JsonValue,
) {
    safe_eprintln!(
        "[codex:app-server] diagnostics session={} phase={} details={}",
        session_id,
        phase,
        details
    );
    emit_codex_json(
        app_handle,
        session_id,
        json!({
            "type": "app_server.diagnostic",
            "phase": phase,
            "details": details,
        }),
    );
}

fn emit_codex_json(app_handle: &AppHandle, session_id: &str, value: JsonValue) {
    let runtime_events = codex_legacy_event_to_runtime_events(session_id, &value);
    let data = cap_event_size(value.to_string());
    if runtime_events.is_empty() {
        emit_provider_event(app_handle, "openai", session_id, &data);
    } else {
        for event in runtime_events {
            emit_provider_runtime_event(app_handle, event);
        }
    }
    if let Err(e) = app_handle.emit(
        "codex-event",
        CodexEvent {
            session_id: session_id.to_string(),
            data,
        },
    ) {
        safe_eprintln!(
            "[codex:app-server] Failed to emit codex-event for {}: {}",
            session_id,
            e
        );
    }
}

fn emit_codex_error(app_handle: &AppHandle, session_id: &str, message: impl Into<String>) {
    emit_codex_json(
        app_handle,
        session_id,
        json!({
            "type": "error",
            "message": message.into(),
        }),
    );
}

fn codex_runtime_event(
    event_type: ProviderRuntimeEventType,
    session_id: &str,
    phase: &str,
    native_type: &str,
) -> ProviderRuntimeEvent {
    ProviderRuntimeEvent::new(event_type, "openai", session_id)
        .with_payload("phase", json!(phase))
        .with_native_type(native_type)
}

fn codex_event_thread_id(value: &JsonValue) -> Option<&str> {
    get_json_str(value, &["thread_id"]).or_else(|| get_json_str(value, &["threadId"]))
}

fn codex_event_turn_id(value: &JsonValue) -> Option<&str> {
    get_json_str(value, &["turn_id"]).or_else(|| get_json_str(value, &["turnId"]))
}

fn codex_event_with_ids(
    mut event: ProviderRuntimeEvent,
    value: &JsonValue,
) -> ProviderRuntimeEvent {
    if let Some(thread_id) = codex_event_thread_id(value) {
        event = event.with_thread_id(thread_id);
    }
    if let Some(turn_id) = codex_event_turn_id(value) {
        event.turn_id = Some(turn_id.to_string());
    }
    event
}

fn codex_json_string(value: Option<&JsonValue>) -> String {
    match value {
        Some(JsonValue::String(text)) => text.clone(),
        Some(JsonValue::Null) | None => String::new(),
        Some(value) => serde_json::to_string(value).unwrap_or_else(|_| value.to_string()),
    }
}

fn codex_command_string(item: &JsonValue) -> String {
    if let Some(command) = item.get("command") {
        match command {
            JsonValue::String(text) => return text.clone(),
            JsonValue::Array(parts) => {
                let text = parts
                    .iter()
                    .filter_map(JsonValue::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
                if !text.is_empty() {
                    return text;
                }
            }
            _ => {}
        }
    }
    item.get("args")
        .and_then(JsonValue::as_array)
        .map(|args| {
            args.iter()
                .filter_map(JsonValue::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

fn codex_item_type(item: &JsonValue) -> &str {
    item.get("item_type")
        .or_else(|| item.get("type"))
        .and_then(JsonValue::as_str)
        .unwrap_or("")
}

fn codex_item_id(item: &JsonValue) -> Option<String> {
    get_json_str(item, &["id"])
        .or_else(|| get_json_str(item, &["call_id"]))
        .filter(|id| !id.trim().is_empty())
        .map(ToString::to_string)
}

fn codex_raw_tool_name(item: &JsonValue) -> &str {
    item.get("name")
        .or_else(|| item.get("tool_name"))
        .and_then(JsonValue::as_str)
        .unwrap_or("")
}

fn codex_is_shell_item(item: &JsonValue) -> bool {
    matches!(
        codex_raw_tool_name(item),
        "exec_command" | "write_stdin" | "local_shell" | "shell"
    )
}

fn codex_item_is_tool(item: &JsonValue) -> bool {
    matches!(
        codex_item_type(item),
        "command_execution"
            | "local_shell_call"
            | "file_change"
            | "mcp_tool_call"
            | "collab_tool_call"
            | "custom_tool_call"
            | "web_search"
            | "web_search_call"
            | "dynamic_tool_call"
    )
}

fn codex_single_shot_tool(item: &JsonValue) -> bool {
    matches!(
        codex_item_type(item),
        "mcp_tool_call" | "web_search" | "web_search_call" | "collab_tool_call"
    )
}

fn codex_tool_display_name(item: &JsonValue) -> String {
    let item_type = codex_item_type(item);
    match item_type {
        "command_execution" | "local_shell_call" => "Bash".to_string(),
        "file_change" => {
            let mut paths = Vec::new();
            for field in ["path", "file_path", "filePath"] {
                if let Some(path) = item.get(field).and_then(JsonValue::as_str) {
                    paths.push(path.to_string());
                }
            }
            if let Some(changes) = item.get("changes").and_then(JsonValue::as_array) {
                for change in changes {
                    for field in ["path", "file_path", "filePath"] {
                        if let Some(path) = change.get(field).and_then(JsonValue::as_str) {
                            paths.push(path.to_string());
                            break;
                        }
                    }
                }
            }
            paths.sort();
            paths.dedup();
            if paths.len() > 1 {
                "MultiEdit".to_string()
            } else {
                "Edit".to_string()
            }
        }
        "mcp_tool_call" => {
            let server = item.get("server").and_then(JsonValue::as_str).unwrap_or("");
            let tool = item
                .get("tool_name")
                .or_else(|| item.get("tool"))
                .or_else(|| item.get("name"))
                .and_then(JsonValue::as_str)
                .unwrap_or("");
            if !server.is_empty() && !tool.is_empty() {
                format!("{server}/{tool}")
            } else if !tool.is_empty() {
                tool.to_string()
            } else {
                "mcp_tool".to_string()
            }
        }
        "custom_tool_call"
            if item.get("name").and_then(JsonValue::as_str) == Some("apply_patch") =>
        {
            "Edit".to_string()
        }
        "custom_tool_call" | "dynamic_tool_call" if codex_is_shell_item(item) => "Bash".to_string(),
        "web_search" | "web_search_call" => "WebSearch".to_string(),
        _ => {
            let raw = codex_raw_tool_name(item);
            if raw.is_empty() {
                item_type.to_string()
            } else {
                raw.to_string()
            }
        }
    }
}

fn codex_insert_path_fields(out: &mut serde_json::Map<String, JsonValue>, path: &str) {
    out.insert("path".to_string(), json!(path));
    out.insert("file_path".to_string(), json!(path));
}

fn codex_tool_input(item: &JsonValue) -> JsonValue {
    let item_type = codex_item_type(item);
    let mut out = serde_json::Map::new();
    match item_type {
        "command_execution" | "local_shell_call" => {
            let command = codex_command_string(item);
            if !command.is_empty() {
                out.insert("command".to_string(), json!(command));
            }
        }
        "file_change" => {
            for field in ["path", "file_path", "filePath"] {
                if let Some(path) = item.get(field).and_then(JsonValue::as_str) {
                    codex_insert_path_fields(&mut out, path);
                    break;
                }
            }
            if let Some(changes) = item.get("changes").and_then(JsonValue::as_array) {
                out.insert("changes".to_string(), JsonValue::Array(changes.clone()));
                let paths = changes
                    .iter()
                    .filter_map(|change| {
                        change
                            .get("path")
                            .or_else(|| change.get("file_path"))
                            .or_else(|| change.get("filePath"))
                            .and_then(JsonValue::as_str)
                    })
                    .map(|path| json!(path))
                    .collect::<Vec<_>>();
                if !paths.is_empty() {
                    out.insert("paths".to_string(), JsonValue::Array(paths));
                }
            }
            for field in ["change", "diff", "unified_diff", "unifiedDiff"] {
                if let Some(value) = item.get(field) {
                    out.insert(field.to_string(), value.clone());
                }
            }
        }
        "mcp_tool_call" => {
            if let Some(server) = item.get("server").and_then(JsonValue::as_str) {
                out.insert("server".to_string(), json!(server));
            }
            if let Some(tool) = item
                .get("tool_name")
                .or_else(|| item.get("tool"))
                .and_then(JsonValue::as_str)
            {
                out.insert("tool_name".to_string(), json!(tool));
            }
            if let Some(arguments) = item.get("arguments") {
                out.insert("arguments".to_string(), arguments.clone());
            }
        }
        "web_search" | "web_search_call" => {
            if let Some(query) = item
                .get("action")
                .and_then(|action| action.get("query"))
                .or_else(|| item.get("query"))
                .and_then(JsonValue::as_str)
            {
                out.insert("query".to_string(), json!(query));
            }
            if let Some(queries) = item.get("action").and_then(|action| action.get("queries")) {
                out.insert("queries".to_string(), queries.clone());
            }
        }
        _ => {
            if let Some(arguments) = item.get("arguments") {
                if let Some(object) = arguments.as_object() {
                    out.extend(object.clone());
                } else {
                    out.insert("arguments".to_string(), arguments.clone());
                }
            }
            let raw = codex_raw_tool_name(item);
            if !raw.is_empty() {
                out.insert("tool_name".to_string(), json!(raw));
            }
            if codex_is_shell_item(item) && !out.contains_key("command") {
                let command = out
                    .get("cmd")
                    .or_else(|| out.get("command"))
                    .and_then(JsonValue::as_str)
                    .map(str::to_string)
                    .or_else(|| {
                        out.get("chars").and_then(JsonValue::as_str).map(|chars| {
                            format!("stdin: {}", chars.chars().take(80).collect::<String>())
                        })
                    })
                    .unwrap_or_else(|| raw.to_string());
                if !command.is_empty() {
                    out.insert("command".to_string(), json!(command));
                }
            }
        }
    }
    JsonValue::Object(out)
}

fn codex_item_result_text(item: &JsonValue) -> String {
    if item.get("output").is_some() {
        return codex_tool_output(item).0;
    }
    if let Some(result) = item.get("result") {
        return codex_json_string(Some(result));
    }
    codex_json_string(item.get("text"))
}

fn codex_item_is_error(item: &JsonValue) -> bool {
    matches!(
        item.get("status").and_then(JsonValue::as_str),
        Some("failed" | "error")
    ) || item
        .get("exit_code")
        .and_then(JsonValue::as_i64)
        .is_some_and(|code| code != 0)
        || codex_tool_output(item).1
}

fn codex_tool_runtime_event(
    session_id: &str,
    item: &JsonValue,
    phase: &str,
    native_type: &str,
) -> Option<ProviderRuntimeEvent> {
    let id = codex_item_id(item)?;
    let mut event = codex_runtime_event(
        ProviderRuntimeEventType::Tool,
        session_id,
        phase,
        native_type,
    )
    .with_item_id(id.clone())
    .with_payload("id", json!(id))
    .with_payload("name", json!(codex_tool_display_name(item)))
    .with_payload("input", codex_tool_input(item));

    if phase == "completed" {
        event = event
            .with_payload("result", json!(codex_item_result_text(item)))
            .with_payload("isError", json!(codex_item_is_error(item)));
    }

    Some(event)
}

fn codex_completed_item_runtime_events(
    session_id: &str,
    item: &JsonValue,
    native_type: &str,
) -> Vec<ProviderRuntimeEvent> {
    if matches!(codex_item_type(item), "agent_message" | "assistant_message") {
        let text = codex_json_string(item.get("text"));
        if text.trim().is_empty() {
            return Vec::new();
        }
        let mut event = codex_runtime_event(
            ProviderRuntimeEventType::Content,
            session_id,
            "message",
            native_type,
        )
        .with_payload("role", json!("assistant"))
        .with_payload("text", json!(text));
        if let Some(id) = codex_item_id(item) {
            event = event.with_item_id(id);
        }
        return vec![event];
    }

    if !codex_item_is_tool(item) {
        return Vec::new();
    }

    let Some(completed) = codex_tool_runtime_event(session_id, item, "completed", native_type)
    else {
        return Vec::new();
    };
    if codex_single_shot_tool(item) {
        if let Some(started) = codex_tool_runtime_event(session_id, item, "started", native_type) {
            return vec![started, completed];
        }
    }
    vec![completed]
}

fn codex_updated_item_runtime_event(
    session_id: &str,
    value: &JsonValue,
    native_type: &str,
) -> Option<ProviderRuntimeEvent> {
    let item = value.get("item")?;
    match codex_item_type(item) {
        "agent_message" | "assistant_message" => {
            let text = value
                .get("delta")
                .or_else(|| value.get("text"))
                .or_else(|| item.get("text"))
                .and_then(JsonValue::as_str)
                .unwrap_or("");
            if text.is_empty() {
                return None;
            }
            let mut event = codex_runtime_event(
                ProviderRuntimeEventType::Content,
                session_id,
                "delta",
                native_type,
            )
            .with_payload("role", json!("assistant"))
            .with_payload("text", json!(text));
            if let Some(id) = codex_item_id(item) {
                event = event.with_item_id(id);
            }
            Some(event)
        }
        "file_change" if item.get("changes").is_some() => {
            codex_tool_runtime_event(session_id, item, "updated", native_type)
        }
        _ => None,
    }
}

fn codex_mcp_servers_from_legacy(value: &JsonValue) -> Vec<JsonValue> {
    if let Some(servers) = value.get("servers").and_then(JsonValue::as_array) {
        return servers.clone();
    }
    if let Some(servers) = value.get("mcp_servers").and_then(JsonValue::as_array) {
        return servers.clone();
    }
    value
        .get("server")
        .cloned()
        .map(|server| vec![server])
        .unwrap_or_default()
}

fn codex_legacy_event_to_runtime_events(
    session_id: &str,
    value: &JsonValue,
) -> Vec<ProviderRuntimeEvent> {
    let native_type = value.get("type").and_then(JsonValue::as_str).unwrap_or("");
    match native_type {
        "thread.started" => {
            let Some(thread_id) = codex_event_thread_id(value) else {
                return Vec::new();
            };
            vec![codex_runtime_event(
                ProviderRuntimeEventType::Session,
                session_id,
                "started",
                native_type,
            )
            .with_thread_id(thread_id)]
        }
        "turn.started" => vec![codex_event_with_ids(
            codex_runtime_event(
                ProviderRuntimeEventType::Turn,
                session_id,
                "started",
                native_type,
            ),
            value,
        )],
        "turn.completed" => {
            let mut event = codex_event_with_ids(
                codex_runtime_event(
                    ProviderRuntimeEventType::Turn,
                    session_id,
                    "completed",
                    native_type,
                ),
                value,
            );
            if let Some(usage) = value.get("usage") {
                event = event.with_payload("usage", usage.clone());
            }
            let error_message = value
                .get("error")
                .and_then(|error| {
                    error
                        .get("message")
                        .and_then(JsonValue::as_str)
                        .or_else(|| error.as_str())
                })
                .unwrap_or("");
            if !error_message.is_empty() {
                event = event
                    .with_payload("isError", json!(true))
                    .with_payload("error", json!(error_message));
            }
            vec![event]
        }
        "mcp.status.updated" => vec![codex_runtime_event(
            ProviderRuntimeEventType::Mcp,
            session_id,
            "status",
            native_type,
        )
        .with_payload(
            "servers",
            JsonValue::Array(codex_mcp_servers_from_legacy(value)),
        )],
        "item.started" => {
            let Some(item) = value.get("item") else {
                return Vec::new();
            };
            if !codex_item_is_tool(item) {
                return Vec::new();
            }
            codex_tool_runtime_event(session_id, item, "started", native_type)
                .into_iter()
                .collect()
        }
        "item.updated" => codex_updated_item_runtime_event(session_id, value, native_type)
            .into_iter()
            .collect(),
        "item.completed" => {
            let Some(item) = value.get("item") else {
                return Vec::new();
            };
            codex_completed_item_runtime_events(session_id, item, native_type)
        }
        "error" => {
            let message = value
                .get("message")
                .and_then(JsonValue::as_str)
                .unwrap_or("Codex reported an error.");
            vec![codex_runtime_event(
                ProviderRuntimeEventType::Error,
                session_id,
                "error",
                native_type,
            )
            .with_payload("message", json!(message))
            .with_payload("terminal", json!(true))]
        }
        _ => Vec::new(),
    }
}

fn get_json_str<'a>(value: &'a JsonValue, path: &[&str]) -> Option<&'a str> {
    let mut cur = value;
    for key in path {
        cur = cur.get(*key)?;
    }
    cur.as_str()
}

fn normalize_codex_item(item: &JsonValue) -> JsonValue {
    let mut out = item.as_object().cloned().unwrap_or_default();
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let normalized_type = match item_type {
        "agentMessage" => "agent_message",
        "commandExecution" => "command_execution",
        "fileChange" => "file_change",
        "mcpToolCall" => "mcp_tool_call",
        "collabToolCall" => "collab_tool_call",
        "webSearch" => "web_search",
        "userMessage" => "user_message",
        "plan" => "agent_message",
        other => other,
    };
    out.insert("type".to_string(), json!(normalized_type));
    if let Some(v) = item.get("aggregatedOutput") {
        out.insert("output".to_string(), v.clone());
    }
    if let Some(v) = item.get("exitCode") {
        out.insert("exit_code".to_string(), v.clone());
    }
    if let Some(v) = item.get("tool") {
        out.insert("tool_name".to_string(), v.clone());
    }
    if normalized_type == "reasoning" {
        if let Some(summary) = item.get("summary").and_then(|v| v.as_array()) {
            let text = summary
                .iter()
                .filter_map(|part| part.get("text").and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                out.insert("text".to_string(), json!(text));
            }
        }
    }
    JsonValue::Object(out)
}

fn app_server_notification_to_exec_event(method: &str, params: &JsonValue) -> Option<JsonValue> {
    match method {
        "thread/started" => {
            let thread_id = get_json_str(params, &["thread", "id"])?;
            Some(json!({
                "type": "thread.started",
                "thread_id": thread_id,
                "threadId": thread_id,
            }))
        }
        "turn/started" => Some(json!({ "type": "turn.started" })),
        "turn/completed" => {
            let mut out = json!({ "type": "turn.completed" });
            if let Some(usage) = params
                .get("usage")
                .or_else(|| params.get("turn").and_then(|t| t.get("usage")))
            {
                out["usage"] = usage.clone();
            }
            if let Some(msg) = get_json_str(params, &["turn", "error", "message"]) {
                out["error"] = json!({ "message": msg });
            }
            Some(out)
        }
        "thread/tokenUsage/updated" => {
            let usage = params.get("tokenUsage")?;
            let last = usage.get("last")?;
            Some(json!({
                "type": "token_usage.updated",
                "thread_id": params.get("threadId").cloned().unwrap_or(JsonValue::Null),
                "turn_id": params.get("turnId").cloned().unwrap_or(JsonValue::Null),
                "context_window": usage.get("modelContextWindow").cloned().unwrap_or(JsonValue::Null),
                "usage": {
                    "input_tokens": last.get("inputTokens").cloned().unwrap_or_else(|| json!(0)),
                    "cached_input_tokens": last.get("cachedInputTokens").cloned().unwrap_or_else(|| json!(0)),
                    "output_tokens": last.get("outputTokens").cloned().unwrap_or_else(|| json!(0)),
                    "reasoning_output_tokens": last.get("reasoningOutputTokens").cloned().unwrap_or_else(|| json!(0)),
                    "total_tokens": last.get("totalTokens").cloned().unwrap_or_else(|| json!(0)),
                },
            }))
        }
        "mcpServer/startupStatus/updated" => {
            let name = get_json_str(params, &["name"])?;
            let status = match get_json_str(params, &["status"]).unwrap_or("unknown") {
                "ready" => "connected",
                other => other,
            };
            let mut server = json!({
                "name": name,
                "status": status,
            });
            if let Some(error) = params.get("error") {
                if !error.is_null() {
                    server["error"] = error.clone();
                }
            }
            Some(json!({
                "type": "mcp.status.updated",
                "server": server,
            }))
        }
        "error" => {
            let message = get_json_str(params, &["error", "message"])
                .or_else(|| get_json_str(params, &["message"]))
                .unwrap_or("Codex app-server error");
            Some(json!({ "type": "error", "message": message }))
        }
        "item/started" => {
            let item = params.get("item")?;
            Some(json!({
                "type": "item.started",
                "item": normalize_codex_item(item),
            }))
        }
        "item/completed" => {
            let item = params.get("item")?;
            Some(json!({
                "type": "item.completed",
                "item": normalize_codex_item(item),
            }))
        }
        "item/agentMessage/delta" => {
            let item_id = params.get("itemId").and_then(|v| v.as_str()).unwrap_or("");
            let delta = params.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            Some(json!({
                "type": "item.updated",
                "delta": delta,
                "item": {
                    "id": item_id,
                    "type": "agent_message",
                    "text": delta,
                },
            }))
        }
        "item/fileChange/patchUpdated" => {
            let item_id = params.get("itemId").and_then(|v| v.as_str()).unwrap_or("");
            Some(json!({
                "type": "item.updated",
                "item": {
                    "id": item_id,
                    "type": "file_change",
                    "changes": params.get("changes").cloned().unwrap_or_else(|| json!([])),
                    "status": "in_progress",
                },
            }))
        }
        "item/fileChange/outputDelta" => {
            let item_id = params.get("itemId").and_then(|v| v.as_str()).unwrap_or("");
            let delta = params.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            Some(json!({
                "type": "item.updated",
                "delta": delta,
                "item": {
                    "id": item_id,
                    "type": "file_change",
                    "output": delta,
                    "status": "in_progress",
                },
            }))
        }
        "item/commandExecution/outputDelta" => {
            let item_id = params.get("itemId").and_then(|v| v.as_str()).unwrap_or("");
            let delta = params.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            Some(json!({
                "type": "item.updated",
                "delta": delta,
                "item": {
                    "id": item_id,
                    "type": "command_execution",
                    "output": delta,
                    "status": "in_progress",
                },
            }))
        }
        "item/plan/delta" => {
            let item_id = params.get("itemId").and_then(|v| v.as_str()).unwrap_or("");
            let delta = params.get("delta").and_then(|v| v.as_str()).unwrap_or("");
            Some(json!({
                "type": "item.updated",
                "delta": delta,
                "item": {
                    "id": item_id,
                    "type": "agent_message",
                    "text": delta,
                },
            }))
        }
        _ => None,
    }
}

fn app_server_thread_id_from_response(value: &JsonValue) -> Option<String> {
    get_json_str(value, &["result", "thread", "id"])
        .or_else(|| get_json_str(value, &["result", "threadId"]))
        .or_else(|| get_json_str(value, &["result", "thread_id"]))
        .or_else(|| get_json_str(value, &["result", "id"]))
        .map(|s| s.to_string())
}

fn app_server_thread_id_from_result(value: &JsonValue) -> Option<String> {
    get_json_str(value, &["thread", "id"])
        .or_else(|| get_json_str(value, &["threadId"]))
        .or_else(|| get_json_str(value, &["thread_id"]))
        .or_else(|| get_json_str(value, &["id"]))
        .map(|s| s.to_string())
}

fn codex_content_text(payload: &JsonValue) -> String {
    let mut text = String::new();
    if let Some(arr) = payload.get("content").and_then(|v| v.as_array()) {
        for block in arr {
            if let Some(s) = block.get("text").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(s);
            }
        }
    }
    text
}

fn codex_function_input(payload: &JsonValue, ptype: &str) -> JsonValue {
    if let Some(args_str) = payload.get("arguments").and_then(|v| v.as_str()) {
        serde_json::from_str::<JsonValue>(args_str).unwrap_or_else(|_| json!({ "_raw": args_str }))
    } else if let Some(action) = payload.get("action") {
        action.clone()
    } else if ptype == "local_shell_call" {
        payload.get("command").cloned().unwrap_or_else(|| json!({}))
    } else {
        JsonValue::Null
    }
}

fn normalize_codex_history_tool(name: &str, input: JsonValue) -> (String, JsonValue) {
    match name {
        "exec_command" | "write_stdin" | "local_shell" | "shell" => {
            let mut normalized = input;
            if let Some(obj) = normalized.as_object_mut() {
                if !obj.contains_key("command") {
                    if let Some(cmd) = obj.get("cmd").cloned() {
                        obj.insert("command".to_string(), cmd);
                    } else if let Some(chars) = obj.get("chars").and_then(|v| v.as_str()) {
                        obj.insert(
                            "command".to_string(),
                            json!(format!(
                                "stdin: {}",
                                chars.chars().take(80).collect::<String>()
                            )),
                        );
                    } else {
                        obj.insert("command".to_string(), json!(name));
                    }
                }
            } else {
                normalized = json!({ "command": name });
            }
            ("Bash".to_string(), normalized)
        }
        other => (other.to_string(), input),
    }
}

fn codex_exec_envelope_to_frontend_events(envelope: &JsonValue) -> Vec<JsonValue> {
    let etype = envelope.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let payload = envelope.get("payload").unwrap_or(envelope);
    match etype {
        "session_meta" => {
            if let Some(thread_id) = get_json_str(payload, &["id"]) {
                return vec![json!({
                    "type": "thread.started",
                    "thread_id": thread_id,
                    "threadId": thread_id,
                })];
            }
        }
        "event_msg" => {
            let ptype = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
            return match ptype {
                "task_started" => vec![json!({ "type": "turn.started" })],
                "task_complete" => vec![json!({ "type": "turn.completed" })],
                "token_count" => {
                    let Some(info) = payload.get("info") else {
                        return Vec::new();
                    };
                    let Some(usage) = info.get("total_token_usage") else {
                        return Vec::new();
                    };
                    vec![json!({
                        "type": "token_usage.updated",
                        "context_window": info.get("model_context_window").cloned().unwrap_or(JsonValue::Null),
                        "usage": usage,
                    })]
                }
                "stream_error" => {
                    let message = get_json_str(payload, &["message"])
                        .or_else(|| get_json_str(payload, &["error", "message"]))
                        .unwrap_or("Codex reported an error");
                    vec![json!({ "type": "error", "message": message })]
                }
                // `event_msg.agent_message` duplicates the persisted
                // response_item message in exec JSON; the response_item path
                // gives us stable ids and avoids double-rendering commentary.
                _ => Vec::new(),
            };
        }
        "response_item" => {
            let ptype = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match ptype {
                "message" => {
                    if payload.get("role").and_then(|v| v.as_str()) != Some("assistant") {
                        return Vec::new();
                    }
                    let text = codex_content_text(payload);
                    if text.trim().is_empty() {
                        return Vec::new();
                    }
                    let id = get_json_str(payload, &["id"])
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                    return vec![json!({
                        "type": "item.completed",
                        "item": {
                            "id": id,
                            "type": "agent_message",
                            "text": text,
                        },
                    })];
                }
                "function_call" | "local_shell_call" => {
                    let call_id = payload
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if call_id.is_empty() {
                        return Vec::new();
                    }
                    let tool_type = if ptype == "local_shell_call" {
                        "local_shell_call"
                    } else {
                        "dynamic_tool_call"
                    };
                    let name = payload.get("name").cloned().unwrap_or_else(|| {
                        if ptype == "local_shell_call" {
                            json!("local_shell")
                        } else {
                            json!("function")
                        }
                    });
                    return vec![json!({
                        "type": "item.started",
                        "item": {
                            "id": call_id,
                            "type": tool_type,
                            "name": name,
                            "arguments": codex_function_input(payload, ptype),
                            "action": payload.get("action").cloned().unwrap_or(JsonValue::Null),
                        },
                    })];
                }
                "custom_tool_call" => {
                    let call_id = payload
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if call_id.is_empty() {
                        return Vec::new();
                    }
                    let raw_name = payload
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("custom_tool");
                    let raw_input = payload
                        .get("input")
                        .and_then(|v| v.as_str())
                        .or_else(|| payload.get("arguments").and_then(|v| v.as_str()))
                        .unwrap_or("");
                    if raw_name == "apply_patch" {
                        let mut item = codex_apply_patch_input(raw_input)
                            .unwrap_or_else(|| json!({ "input": raw_input }));
                        item["id"] = json!(call_id);
                        item["type"] = json!("file_change");
                        return vec![json!({ "type": "item.started", "item": item })];
                    }
                    let input = serde_json::from_str::<JsonValue>(raw_input)
                        .unwrap_or_else(|_| json!({ "input": raw_input }));
                    return vec![json!({
                        "type": "item.started",
                        "item": {
                            "id": call_id,
                            "type": "dynamic_tool_call",
                            "name": raw_name,
                            "arguments": input,
                        },
                    })];
                }
                "function_call_output" | "local_shell_call_output" | "custom_tool_call_output" => {
                    let call_id = payload
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if call_id.is_empty() {
                        return Vec::new();
                    }
                    let (output, is_error) = codex_tool_output(payload);
                    let tool_type = match ptype {
                        "local_shell_call_output" => "local_shell_call",
                        "custom_tool_call_output" => "dynamic_tool_call",
                        _ => "dynamic_tool_call",
                    };
                    return vec![json!({
                        "type": "item.completed",
                        "item": {
                            "id": call_id,
                            "type": tool_type,
                            "output": output,
                            "status": if is_error { "failed" } else { "completed" },
                        },
                    })];
                }
                "web_search_call" => {
                    let id = get_json_str(payload, &["id"])
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                    return vec![json!({
                        "type": "item.completed",
                        "item": {
                            "id": id,
                            "type": "web_search_call",
                            "action": payload.get("action").cloned().unwrap_or_else(|| json!({})),
                            "status": payload.get("status").cloned().unwrap_or_else(|| json!("completed")),
                        },
                    })];
                }
                "mcp_tool_call" => {
                    let mut item = normalize_codex_item(payload);
                    if item.get("id").is_none() {
                        let id = get_json_str(payload, &["call_id"])
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                        item["id"] = json!(id);
                    }
                    return vec![json!({ "type": "item.completed", "item": item })];
                }
                _ => {}
            }
        }
        _ => {}
    }
    Vec::new()
}

fn codex_exec_line_to_frontend_lines(line: &str) -> Vec<String> {
    let Ok(envelope) = serde_json::from_str::<JsonValue>(line) else {
        return vec![line.to_string()];
    };
    let events = codex_exec_envelope_to_frontend_events(&envelope);
    if events.is_empty() {
        Vec::new()
    } else {
        events.into_iter().map(|event| event.to_string()).collect()
    }
}

fn run_app_server_request(cwd: &str, method: &str, params: JsonValue) -> Result<JsonValue, String> {
    let mut cmd = build_app_server_command(cwd, &None);
    safe_eprintln!(
        "[codex:app-server] rpc {} cwd={}",
        method,
        if cwd.is_empty() { "<empty>" } else { cwd }
    );
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn codex app-server: {}", e))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or("Failed to capture app-server stdin")?;
    let stdout = child
        .stdout
        .take()
        .ok_or("Failed to capture app-server stdout")?;
    let mut reader = std::io::BufReader::new(stdout);

    write_json_rpc(
        &mut stdin,
        &json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "terminal_64",
                    "title": "Terminal 64",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": { "experimentalApi": true },
            },
        }),
    )?;
    write_json_rpc(
        &mut stdin,
        &json!({ "method": "initialized", "params": {} }),
    )?;
    write_json_rpc(
        &mut stdin,
        &json!({ "id": 2, "method": method, "params": params }),
    )?;

    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|e| format!("codex app-server read: {}", e))?;
        if read == 0 {
            terminate_child_process(&mut child);
            return Err("Codex app-server exited before responding".to_string());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parsed: JsonValue = serde_json::from_str(trimmed)
            .map_err(|e| format!("codex app-server JSON parse: {}", e))?;
        if parsed.get("id").and_then(|v| v.as_i64()) != Some(2) {
            continue;
        }
        terminate_child_process(&mut child);
        if let Some(msg) = get_json_str(&parsed, &["error", "message"]) {
            return Err(msg.to_string());
        }
        return Ok(parsed.get("result").cloned().unwrap_or_else(|| json!({})));
    }
}

fn spawn_and_stream(
    instances: &Arc<Mutex<HashMap<String, CodexInstance>>>,
    app_handle: &AppHandle,
    session_id: String,
    mut cmd: Command,
) -> Result<(), String> {
    {
        let mut inst = instances.lock().map_err(|e| e.to_string())?;
        if let Some(mut old) = inst.remove(&session_id) {
            terminate_child_process(&mut old.child);
            drop(inst);
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
    }

    // Diagnostics: dump the full argv so we can compare against a working
    // shell invocation. The child inherits the Tauri app's environment which
    // on macOS GUI launches can be surprisingly sparse.
    {
        let prog = cmd.get_program().to_string_lossy().to_string();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let cwd = cmd
            .get_current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "<inherited>".to_string());
        safe_eprintln!(
            "[codex] spawn argv for {}: {} {:?} (cwd={})",
            session_id,
            prog,
            args,
            cwd
        );
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn codex: {}", e))?;

    let stdout = child.stdout.take().ok_or("Failed to capture stdout")?;
    let stderr_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    if let Some(stderr) = child.stderr.take() {
        let sid_for_stderr = session_id.clone();
        let buf = stderr_buf.clone();
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                safe_eprintln!(
                    "[codex:stderr:{}] {}",
                    &sid_for_stderr[..8.min(sid_for_stderr.len())],
                    line
                );
                if let Ok(mut b) = buf.lock() {
                    if b.len() < 4000 {
                        if !b.is_empty() {
                            b.push('\n');
                        }
                        b.push_str(&line);
                    }
                }
            }
        });
    }

    let gen = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let sid = session_id.clone();
    let handle = app_handle.clone();
    let instances_clone = instances.clone();

    std::thread::spawn(move || {
        safe_eprintln!("[codex] Reader thread started for {} (gen {})", sid, gen);
        let reader = std::io::BufReader::new(stdout);
        let mut had_output = false;
        for line in reader.lines() {
            match line {
                Ok(line) if line.trim().is_empty() => continue,
                Ok(line) => {
                    had_output = true;
                    for event_line in codex_exec_line_to_frontend_lines(&line) {
                        if let Ok(event_value) = serde_json::from_str::<JsonValue>(&event_line) {
                            emit_codex_json(&handle, &sid, event_value);
                        } else {
                            let data = cap_event_size(event_line);
                            emit_provider_event(&handle, "openai", &sid, &data);
                            if let Err(e) = handle.emit(
                                "codex-event",
                                CodexEvent {
                                    session_id: sid.clone(),
                                    data,
                                },
                            ) {
                                safe_eprintln!(
                                    "[codex] Failed to emit codex-event for {}: {}",
                                    sid,
                                    e
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    safe_eprintln!("[codex] Reader error: {} for {}", e, sid);
                    break;
                }
            }
        }
        if !had_output {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let stderr_msg = stderr_buf.lock().map(|s| s.clone()).unwrap_or_default();
            let error_msg = if stderr_msg.is_empty() {
                "Codex process exited without output. The CLI may not be installed (try `which codex`) or the prompt was rejected.".to_string()
            } else {
                stderr_msg
            };
            safe_eprintln!(
                "[codex] No stdout for {} — emitting error: {}",
                sid,
                &error_msg[..error_msg.len().min(200)]
            );
            emit_codex_json(
                &handle,
                &sid,
                serde_json::json!({
                    "type": "error",
                    "message": error_msg,
                }),
            );
        }
        // Wait on the child if it's still ours so we can log the exit status
        // — otherwise silent exits look identical to normal completion in
        // the logs.
        let exit_info = {
            let mut inst_g = match instances_clone.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            match inst_g.get_mut(&sid) {
                Some(instance) if instance.generation == gen => match instance.child.try_wait() {
                    Ok(Some(status)) => format!("exit={:?}", status),
                    Ok(None) => {
                        drop(inst_g);
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        let mut again = match instances_clone.lock() {
                            Ok(g) => g,
                            Err(_) => return,
                        };
                        match again.get_mut(&sid) {
                            Some(i) => match i.child.wait() {
                                Ok(status) => format!("exit={:?}", status),
                                Err(e) => format!("wait-err={}", e),
                            },
                            None => "child-already-gone".to_string(),
                        }
                    }
                    Err(e) => format!("try_wait-err={}", e),
                },
                _ => "child-not-ours".to_string(),
            }
        };
        safe_eprintln!(
            "[codex] Reader thread ended for {} (gen {}) had_output={} {}",
            sid,
            gen,
            had_output,
            exit_info
        );
        let is_current = if let Ok(mut inst) = instances_clone.lock() {
            if let Some(instance) = inst.get(&sid) {
                if instance.generation == gen {
                    inst.remove(&sid);
                    true
                } else {
                    safe_eprintln!(
                        "[codex] Stale reader gen {} != current {} for {} — skipping codex-done",
                        gen,
                        instance.generation,
                        sid
                    );
                    false
                }
            } else {
                true
            }
        } else {
            true
        };
        if is_current {
            if let Err(e) = handle.emit(
                "codex-done",
                CodexDone {
                    session_id: sid.clone(),
                },
            ) {
                safe_eprintln!("[codex] Failed to emit codex-done for {}: {}", sid, e);
            }
        }
    });

    instances.lock().map_err(|e| e.to_string())?.insert(
        session_id,
        CodexInstance {
            child,
            generation: gen,
        },
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn spawn_app_server_turn(
    instances: &Arc<Mutex<HashMap<String, CodexInstance>>>,
    app_handle: &AppHandle,
    session_id: String,
    mode: InvokeMode<'_>,
    cwd: &str,
    prompt: &str,
    sandbox_mode: &Option<String>,
    approval_policy: &Option<String>,
    model: &Option<String>,
    effort: &Option<String>,
    collaboration_mode: &Option<String>,
    full_auto: bool,
    yolo: bool,
    mcp_env: &Option<HashMap<String, String>>,
) -> Result<(), String> {
    {
        let inst = instances.lock().map_err(|e| e.to_string())?;
        if inst.contains_key(&session_id) {
            return Err(format!(
                "Codex session {} already has an active turn. Wait for it to finish or cancel it before sending another prompt.",
                session_id
            ));
        }
    }

    let mut cmd = build_app_server_command(cwd, mcp_env);
    {
        let prog = cmd.get_program().to_string_lossy().to_string();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let cwd_display = cmd
            .get_current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "<inherited>".to_string());
        safe_eprintln!(
            "[codex:app-server] spawn argv for {}: {} {:?} (cwd={})",
            session_id,
            prog,
            args,
            cwd_display
        );
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn codex app-server: {}", e))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or("Failed to capture app-server stdin")?;
    let stdout = child
        .stdout
        .take()
        .ok_or("Failed to capture app-server stdout")?;

    let stderr_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    if let Some(stderr) = child.stderr.take() {
        let sid_for_stderr = session_id.clone();
        let buf = stderr_buf.clone();
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                safe_eprintln!(
                    "[codex:app-server:stderr:{}] {}",
                    &sid_for_stderr[..8.min(sid_for_stderr.len())],
                    line
                );
                if let Ok(mut b) = buf.lock() {
                    if b.len() < 4000 {
                        if !b.is_empty() {
                            b.push('\n');
                        }
                        b.push_str(&line);
                    }
                }
            }
        });
    }

    let gen = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    {
        let mut inst = instances.lock().map_err(|e| e.to_string())?;
        if inst.contains_key(&session_id) {
            terminate_child_process(&mut child);
            return Err(format!(
                "Codex session {} already has an active turn. Wait for it to finish or cancel it before sending another prompt.",
                session_id
            ));
        }
        inst.insert(
            session_id.clone(),
            CodexInstance {
                child,
                generation: gen,
            },
        );
    }

    let sid = session_id.clone();
    let handle = app_handle.clone();
    let instances_clone = instances.clone();
    let cwd_owned = cwd.to_string();
    let prompt_owned = prompt.to_string();
    let sandbox_mode = sandbox_mode.clone();
    let approval_policy = approval_policy.clone();
    let model = model.clone();
    let effort = effort.clone();
    let collaboration_mode = collaboration_mode.clone();
    let resume_thread_id = match mode {
        InvokeMode::Fresh => None,
        InvokeMode::Resume(thread_id) => Some(thread_id.to_string()),
    };

    std::thread::spawn(move || {
        safe_eprintln!(
            "[codex:app-server] Worker started for {} (gen {})",
            sid,
            gen
        );
        let (line_tx, line_rx) = std::sync::mpsc::channel::<Result<Option<String>, String>>();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        let _ = line_tx.send(Ok(None));
                        break;
                    }
                    Ok(_) => {
                        if line_tx.send(Ok(Some(line.clone()))).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = line_tx.send(Err(e.to_string()));
                        break;
                    }
                }
            }
        });

        let initialize = json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "terminal_64",
                    "title": "Terminal 64",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {
                    "experimentalApi": true,
                },
            },
        });
        let initialized = json!({ "method": "initialized", "params": {} });
        if let Err(e) = write_json_rpc(&mut stdin, &initialize)
            .and_then(|_| write_json_rpc(&mut stdin, &initialized))
        {
            emit_codex_error(&handle, &sid, e);
            finish_app_server_turn(&instances_clone, &handle, &sid, gen, true);
            return;
        }

        let thread_params = if let Some(thread_id) = resume_thread_id.as_ref() {
            let mut params = match app_server_thread_params(
                &cwd_owned,
                &sandbox_mode,
                &approval_policy,
                &model,
                &effort,
                full_auto,
                yolo,
            ) {
                JsonValue::Object(map) => map,
                _ => serde_json::Map::new(),
            };
            params.insert("threadId".to_string(), json!(thread_id));
            JsonValue::Object(params)
        } else {
            app_server_thread_params(
                &cwd_owned,
                &sandbox_mode,
                &approval_policy,
                &model,
                &effort,
                full_auto,
                yolo,
            )
        };
        let thread_method = if resume_thread_id.is_some() {
            "thread/resume"
        } else {
            "thread/start"
        };
        if let Err(e) = write_json_rpc(
            &mut stdin,
            &json!({ "id": 2, "method": thread_method, "params": thread_params }),
        ) {
            emit_codex_error(&handle, &sid, e);
            finish_app_server_turn(&instances_clone, &handle, &sid, gen, true);
            return;
        }

        let mut thread_id: Option<String> = None;
        let mut turn_requested = false;
        let mut turn_started = false;
        let mut saw_output = false;
        let startup_deadline = Instant::now() + APP_SERVER_STARTUP_TIMEOUT;
        let mut turn_start_deadline: Option<Instant> = None;
        loop {
            let (phase, timeout) = if !turn_requested {
                let phase = format!("{} response", thread_method);
                match app_server_remaining_timeout(
                    &phase,
                    startup_deadline,
                    APP_SERVER_STARTUP_TIMEOUT,
                    &stderr_buf,
                ) {
                    Ok(timeout) => (phase, Some(timeout)),
                    Err(e) => {
                        emit_codex_error(&handle, &sid, e);
                        break;
                    }
                }
            } else if !turn_started {
                let phase = "turn/start response".to_string();
                let deadline = turn_start_deadline
                    .unwrap_or_else(|| Instant::now() + APP_SERVER_TURN_START_TIMEOUT);
                match app_server_remaining_timeout(
                    &phase,
                    deadline,
                    APP_SERVER_TURN_START_TIMEOUT,
                    &stderr_buf,
                ) {
                    Ok(timeout) => (phase, Some(timeout)),
                    Err(e) => {
                        emit_codex_error(&handle, &sid, e);
                        break;
                    }
                }
            } else {
                ("turn stream".to_string(), None)
            };
            let parsed = match read_app_server_message(&line_rx, &phase, timeout, &stderr_buf) {
                Ok(Some(parsed)) => parsed,
                Ok(None) => {
                    if !saw_output {
                        let stderr_msg = app_server_stderr_excerpt(&stderr_buf);
                        let msg = if stderr_msg.is_empty() {
                            "Codex app-server exited without output. Install a recent OpenAI Codex CLI or set T64_CODEX_TRANSPORT=exec to use the legacy transport.".to_string()
                        } else {
                            stderr_msg
                        };
                        emit_codex_error(&handle, &sid, msg);
                    } else if !turn_started {
                        emit_codex_error(
                            &handle,
                            &sid,
                            format!("Codex app-server exited before {}", phase),
                        );
                    }
                    break;
                }
                Err(e) => {
                    emit_codex_error(&handle, &sid, e);
                    break;
                }
            };
            saw_output = true;

            if parsed.get("error").is_some() && parsed.get("id").is_some() {
                let method = match parsed.get("id").and_then(|v| v.as_i64()) {
                    Some(1) => "initialize",
                    Some(2) => thread_method,
                    Some(3) => "turn/start",
                    _ => "request",
                };
                emit_codex_error(
                    &handle,
                    &sid,
                    app_server_error_from_response(method, &parsed),
                );
                break;
            }

            if parsed.get("id").and_then(|v| v.as_i64()) == Some(1) {
                emit_app_server_diagnostic(
                    &handle,
                    &sid,
                    "initialize",
                    app_server_protocol_diagnostics(&parsed),
                );
                continue;
            }

            if parsed.get("id").and_then(|v| v.as_i64()) == Some(2) {
                thread_id = app_server_thread_id_from_response(&parsed);
                if let Some(tid) = thread_id.as_ref() {
                    emit_codex_json(
                        &handle,
                        &sid,
                        json!({
                            "type": "thread.started",
                            "thread_id": tid,
                            "threadId": tid,
                        }),
                    );
                    let turn_params = app_server_turn_params(
                        tid,
                        &cwd_owned,
                        &prompt_owned,
                        &sandbox_mode,
                        &approval_policy,
                        &model,
                        &effort,
                        &collaboration_mode,
                        full_auto,
                        yolo,
                    );
                    if let Err(e) = write_json_rpc(
                        &mut stdin,
                        &json!({ "id": 3, "method": "turn/start", "params": turn_params }),
                    ) {
                        emit_codex_error(&handle, &sid, e);
                        break;
                    }
                    turn_requested = true;
                    turn_start_deadline = Some(Instant::now() + APP_SERVER_TURN_START_TIMEOUT);
                } else {
                    emit_codex_error(
                        &handle,
                        &sid,
                        format!(
                            "Codex app-server {} did not return a thread id",
                            thread_method
                        ),
                    );
                    break;
                }
                continue;
            }

            if parsed.get("id").and_then(|v| v.as_i64()) == Some(3) {
                turn_started = true;
                continue;
            }

            if parsed.get("id").is_some() && parsed.get("method").is_some() {
                let request_id = parsed.get("id").cloned().unwrap_or(json!(null));
                let method = parsed.get("method").and_then(|v| v.as_str()).unwrap_or("");
                safe_eprintln!(
                    "[codex:app-server] Auto-declining unsupported server request {} for {}",
                    method,
                    sid
                );
                let _ = write_json_rpc(
                    &mut stdin,
                    &json!({
                        "id": request_id,
                        "result": { "decision": "decline" },
                    }),
                );
                continue;
            }

            if let Some(method) = parsed.get("method").and_then(|v| v.as_str()) {
                let params = parsed.get("params").cloned().unwrap_or_else(|| json!({}));
                if let Some(event) = app_server_notification_to_exec_event(method, &params) {
                    emit_codex_json(&handle, &sid, event);
                }
                if matches!(
                    method,
                    "turn/started"
                        | "item/started"
                        | "item/agentMessage/delta"
                        | "item/plan/delta"
                        | "item/commandExecution/outputDelta"
                        | "item/fileChange/outputDelta"
                        | "item/fileChange/patchUpdated"
                ) {
                    turn_started = true;
                }
                if method == "turn/completed" {
                    break;
                }
            }
        }

        if !turn_started {
            safe_eprintln!(
                "[codex:app-server] Turn never started for {} (thread_id={:?}, turn_requested={})",
                sid,
                thread_id,
                turn_requested
            );
        }
        finish_app_server_turn(&instances_clone, &handle, &sid, gen, false);
    });

    Ok(())
}

fn finish_app_server_turn(
    instances: &Arc<Mutex<HashMap<String, CodexInstance>>>,
    app_handle: &AppHandle,
    session_id: &str,
    generation: u64,
    suppress_done: bool,
) {
    if let Ok(mut inst) = instances.lock() {
        if let Some(instance) = inst.get(session_id) {
            if instance.generation != generation {
                return;
            }
        }
        if let Some(mut instance) = inst.remove(session_id) {
            terminate_child_process(&mut instance.child);
        }
    }
    if !suppress_done {
        if let Err(e) = app_handle.emit(
            "codex-done",
            CodexDone {
                session_id: session_id.to_string(),
            },
        ) {
            safe_eprintln!(
                "[codex:app-server] Failed to emit codex-done for {}: {}",
                session_id,
                e
            );
        }
    }
}

fn resolve_session_id(provided: &str) -> String {
    let trimmed = provided.trim();
    if trimmed.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        trimmed.to_string()
    }
}

fn payload_cwd(payload: &serde_json::Value) -> Option<&str> {
    payload
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .filter(|cwd| !cwd.trim().is_empty())
}

fn prepare_codex_command(
    lifecycle: &ProviderCommandLifecycle<'_>,
    req: ProviderCreateSessionRequest,
    command_label: &str,
) -> ProviderPreparedCommand {
    if let Some(cwd) = payload_cwd(&req.payload) {
        if let Err(e) = crate::ensure_codex_mcp_impl(lifecycle.app_handle, cwd) {
            safe_eprintln!("[codex:mcp] setup failed before {}: {}", command_label, e);
        }
    }
    ProviderPreparedCommand::new(req)
}

// ── CodexAdapter ──────────────────────────────────────────

pub struct CodexAdapter {
    instances: Arc<Mutex<HashMap<String, CodexInstance>>>,
    capabilities: ProviderAdapterCapabilities,
}

#[derive(Deserialize)]
struct CodexHistoryTruncateRequest {
    thread_id: String,
    cwd: String,
    num_turns: u32,
}

#[derive(Deserialize)]
struct CodexHistoryForkRequest {
    thread_id: String,
    cwd: String,
    drop_turns: u32,
}

#[derive(Deserialize)]
struct CodexHistoryHydrateRequest {
    thread_id: String,
}

#[derive(Deserialize)]
struct CodexHistoryDeleteRequest {
    thread_id: Option<String>,
}

impl CodexAdapter {
    pub fn new() -> Self {
        Self {
            instances: Arc::new(Mutex::new(HashMap::new())),
            capabilities: ProviderAdapterCapabilities {
                session_model_switch: ProviderSessionModelSwitchMode::InSession,
                history: ProviderHistoryCapabilities::FULL,
                mcp: true,
                plan: true,
                images: true,
                hook_log: false,
                native_slash_commands: false,
                compact: false,
            },
        }
    }

    /// Spawn a fresh `codex exec --json` process. Returns the local UUID we
    /// minted (or echoed back). The Codex CLI assigns its own thread id and
    /// emits it in the first `thread.started` event — the frontend should
    /// adopt that as the canonical id for follow-up `send_prompt` calls.
    pub fn create_session(
        &self,
        app_handle: &AppHandle,
        req: CreateCodexRequest,
    ) -> Result<String, String> {
        let resolved_id = resolve_session_id(&req.session_id);
        safe_eprintln!(
            "[codex] Creating session id={} cwd={} model={:?} sandbox={:?}",
            resolved_id,
            req.cwd,
            req.model,
            req.sandbox_mode
        );
        if codex_transport_is_exec() {
            let cmd = build_command(
                InvokeMode::Fresh,
                &req.cwd,
                &req.prompt,
                &req.sandbox_mode,
                &req.approval_policy,
                &req.model,
                &req.effort,
                req.full_auto.unwrap_or(false),
                req.yolo.unwrap_or(false),
                req.skip_git_repo_check.unwrap_or(true),
                &req.mcp_env,
            );
            spawn_and_stream(&self.instances, app_handle, resolved_id.clone(), cmd)?;
        } else {
            spawn_app_server_turn(
                &self.instances,
                app_handle,
                resolved_id.clone(),
                InvokeMode::Fresh,
                &req.cwd,
                &req.prompt,
                &req.sandbox_mode,
                &req.approval_policy,
                &req.model,
                &req.effort,
                &req.collaboration_mode,
                req.full_auto.unwrap_or(false),
                req.yolo.unwrap_or(false),
                &req.mcp_env,
            )?;
        }
        Ok(resolved_id)
    }

    /// Send a follow-up prompt to an existing Codex thread. `req.session_id`
    /// MUST be the Codex-assigned `thread_id` (captured from the
    /// `thread.started` event of the originating session) for the resume to
    /// succeed.
    pub fn send_prompt(
        &self,
        app_handle: &AppHandle,
        req: SendCodexPromptRequest,
    ) -> Result<(), String> {
        if req.session_id.trim().is_empty() {
            return Err("send_prompt: session_id is required".to_string());
        }
        // The thread_id (Codex-assigned) is what `codex exec resume` needs as
        // its positional argument. session_id (T64-local UUID) is what we
        // emit events under so the frontend can route them to the right
        // session row. Fall back to session_id for older callers that haven't
        // split the fields yet.
        let resume_id_owned = req
            .thread_id
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| req.session_id.clone());
        safe_eprintln!(
            "[codex] Resuming session_id={} thread_id={} cwd={}",
            req.session_id,
            resume_id_owned,
            req.cwd
        );
        let use_exec_transport = codex_transport_is_exec();
        if !use_exec_transport && find_codex_rollout(&resume_id_owned).is_none() {
            safe_eprintln!(
                "[codex] Stored thread_id={} for session_id={} has no rollout; starting a fresh app-server thread",
                resume_id_owned,
                req.session_id
            );
            return spawn_app_server_turn(
                &self.instances,
                app_handle,
                req.session_id,
                InvokeMode::Fresh,
                &req.cwd,
                &req.prompt,
                &req.sandbox_mode,
                &req.approval_policy,
                &req.model,
                &req.effort,
                &req.collaboration_mode,
                req.full_auto.unwrap_or(false),
                req.yolo.unwrap_or(false),
                &req.mcp_env,
            );
        }
        if use_exec_transport {
            let cmd = build_command(
                InvokeMode::Resume(&resume_id_owned),
                &req.cwd,
                &req.prompt,
                &req.sandbox_mode,
                &req.approval_policy,
                &req.model,
                &req.effort,
                req.full_auto.unwrap_or(false),
                req.yolo.unwrap_or(false),
                req.skip_git_repo_check.unwrap_or(true),
                &req.mcp_env,
            );
            spawn_and_stream(&self.instances, app_handle, req.session_id, cmd)
        } else {
            spawn_app_server_turn(
                &self.instances,
                app_handle,
                req.session_id,
                InvokeMode::Resume(&resume_id_owned),
                &req.cwd,
                &req.prompt,
                &req.sandbox_mode,
                &req.approval_policy,
                &req.model,
                &req.effort,
                &req.collaboration_mode,
                req.full_auto.unwrap_or(false),
                req.yolo.unwrap_or(false),
                &req.mcp_env,
            )
        }
    }

    pub fn cancel(&self, session_id: &str) -> Result<(), String> {
        let mut instances = self.instances.lock().map_err(|e| e.to_string())?;
        if let Some(instance) = instances.get_mut(session_id) {
            terminate_child_process(&mut instance.child);
            safe_eprintln!("[codex] Cancelled session {}", session_id);
        }
        Ok(())
    }

    pub fn close(&self, session_id: &str) -> Result<(), String> {
        let instance = self
            .instances
            .lock()
            .map_err(|e| e.to_string())?
            .remove(session_id);
        if let Some(mut instance) = instance {
            terminate_child_process(&mut instance.child);
            safe_eprintln!("[codex] Closed session {}", session_id);
        }
        Ok(())
    }

    pub fn rollback_thread(
        &self,
        thread_id: &str,
        cwd: &str,
        num_turns: u32,
    ) -> Result<(), String> {
        if thread_id.trim().is_empty() {
            return Err("rollback_thread: thread_id is required".to_string());
        }
        if num_turns == 0 {
            return Ok(());
        }
        if codex_transport_is_exec() {
            return Err("Codex native rollback requires app-server transport".to_string());
        }
        run_app_server_request(
            cwd,
            "thread/rollback",
            json!({
                "threadId": thread_id,
                "numTurns": num_turns,
            }),
        )?;
        Ok(())
    }

    fn fork_thread_via_app_server(
        thread_id: &str,
        cwd: &str,
        rollout_path: Option<&std::path::Path>,
    ) -> Result<String, String> {
        let mut params = serde_json::Map::new();
        params.insert("threadId".to_string(), json!(thread_id));
        if !cwd.is_empty() {
            params.insert("cwd".to_string(), json!(cwd));
        }
        if let Some(path) = rollout_path {
            params.insert(
                "path".to_string(),
                json!(path.to_string_lossy().to_string()),
            );
        }
        params.insert("excludeTurns".to_string(), json!(true));
        params.insert("persistExtendedHistory".to_string(), json!(true));

        let result = run_app_server_request(cwd, "thread/fork", JsonValue::Object(params))?;
        app_server_thread_id_from_result(&result)
            .ok_or("Codex app-server fork did not return a thread id".to_string())
    }

    pub fn fork_thread(
        &self,
        thread_id: &str,
        cwd: &str,
        drop_turns: u32,
    ) -> Result<String, String> {
        if thread_id.trim().is_empty() {
            return Err("fork_thread: thread_id is required".to_string());
        }
        if codex_transport_is_exec() {
            return Err("Codex native fork requires app-server transport".to_string());
        }
        let forked = match Self::fork_thread_via_app_server(thread_id, cwd, None) {
            Ok(forked) => forked,
            Err(thread_id_error) => {
                let Some(rollout_path) = find_codex_rollout(thread_id) else {
                    return Err(thread_id_error);
                };
                safe_eprintln!(
                    "[codex:app-server] thread/fork by id failed ({}); retrying with rollout path {}",
                    thread_id_error,
                    rollout_path.display()
                );
                Self::fork_thread_via_app_server(thread_id, cwd, Some(rollout_path.as_path()))
                    .map_err(|path_error| {
                        format!(
                            "Codex app-server fork failed by thread id ({}) and rollout path ({})",
                            thread_id_error, path_error
                        )
                    })?
            }
        };
        if drop_turns > 0 {
            self.rollback_thread(&forked, cwd, drop_turns)?;
        }
        Ok(forked)
    }
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderAdapter for CodexAdapter {
    fn prepare_create_session(
        &self,
        lifecycle: &ProviderCommandLifecycle<'_>,
        req: ProviderCreateSessionRequest,
    ) -> Result<ProviderPreparedCommand, ProviderAdapterError> {
        Ok(prepare_codex_command(lifecycle, req, "create"))
    }

    fn prepare_send_prompt(
        &self,
        lifecycle: &ProviderCommandLifecycle<'_>,
        req: ProviderSendPromptRequest,
    ) -> Result<ProviderPreparedCommand, ProviderAdapterError> {
        Ok(prepare_codex_command(lifecycle, req, "send"))
    }

    fn create_session(
        &self,
        app_handle: &AppHandle,
        req: ProviderCreateSessionRequest,
    ) -> Result<String, ProviderAdapterError> {
        let typed_req: CreateCodexRequest = serde_json::from_value(req.payload)
            .map_err(|e| format!("Invalid OpenAI create request: {}", e))?;
        CodexAdapter::create_session(self, app_handle, typed_req)
    }

    fn send_prompt(
        &self,
        app_handle: &AppHandle,
        req: ProviderSendPromptRequest,
    ) -> Result<(), ProviderAdapterError> {
        let typed_req: SendCodexPromptRequest = serde_json::from_value(req.payload)
            .map_err(|e| format!("Invalid OpenAI send request: {}", e))?;
        CodexAdapter::send_prompt(self, app_handle, typed_req)
    }

    fn cancel_session(&self, session_id: &str) -> Result<(), ProviderAdapterError> {
        self.cancel(session_id)
    }

    fn close_session(&self, session_id: &str) -> Result<(), ProviderAdapterError> {
        self.close(session_id)
    }

    fn provider(&self) -> ProviderKind {
        ProviderKind::Codex
    }

    fn capabilities(&self) -> &ProviderAdapterCapabilities {
        &self.capabilities
    }

    fn snapshot(&self) -> ProviderSnapshot {
        snapshot_from_descriptor(
            &OPENAI_SNAPSHOT_DESCRIPTOR,
            self.capabilities(),
            resolve_codex_path(),
        )
    }

    fn history_truncate(
        &self,
        req: ProviderHistoryRequest,
    ) -> Result<ProviderHistoryResponse, ProviderAdapterError> {
        let req: CodexHistoryTruncateRequest = serde_json::from_value(req)
            .map_err(|e| format!("Invalid OpenAI history truncate request: {}", e))?;
        if req.thread_id.trim().is_empty() {
            return Ok(json!({
                "status": "unsupported",
                "method": "unsupported",
                "turns": 0,
                "reason": "thread_id_required",
            }));
        }
        if req.num_turns == 0 {
            return Ok(json!({
                "status": "skipped",
                "method": "noop",
                "turns": 0,
                "reason": "no_turns_to_drop",
            }));
        }
        match self.rollback_thread(&req.thread_id, &req.cwd, req.num_turns) {
            Ok(()) => Ok(json!({
                "status": "applied",
                "method": "app_server",
                "turns": req.num_turns,
            })),
            Err(rollback_error) => {
                safe_eprintln!(
                    "[provider-history] Codex rollback failed ({}); falling back to rollout truncation",
                    rollback_error
                );
                let turns = truncate_codex_rollout_by_turns(&req.thread_id, req.num_turns)
                    .map_err(|truncate_error| {
                        format!(
                            "Codex rollback failed ({}); rollout truncation failed ({})",
                            rollback_error, truncate_error
                        )
                    })?;
                Ok(json!({
                    "status": "applied",
                    "method": "rollout",
                    "turns": turns,
                    "rollback_error": rollback_error,
                }))
            }
        }
    }

    fn history_fork(
        &self,
        req: ProviderHistoryRequest,
    ) -> Result<ProviderHistoryResponse, ProviderAdapterError> {
        let req: CodexHistoryForkRequest = serde_json::from_value(req)
            .map_err(|e| format!("Invalid OpenAI history fork request: {}", e))?;
        if req.thread_id.trim().is_empty() {
            return Ok(json!({
                "status": "unsupported",
                "reason": "thread_id_required",
            }));
        }
        let codex_thread_id = self.fork_thread(&req.thread_id, &req.cwd, req.drop_turns)?;
        Ok(json!({
            "status": "applied",
            "codex_thread_id": codex_thread_id,
        }))
    }

    fn history_hydrate(
        &self,
        req: ProviderHistoryRequest,
    ) -> Result<ProviderHistoryResponse, ProviderAdapterError> {
        let req: CodexHistoryHydrateRequest = serde_json::from_value(req)
            .map_err(|e| format!("Invalid OpenAI history hydrate request: {}", e))?;
        if req.thread_id.trim().is_empty() {
            return Ok(json!({
                "status": "skipped",
                "messages": [],
                "stat": null,
                "reason": "thread_id_required",
            }));
        }
        if find_codex_rollout(&req.thread_id).is_none() {
            return Ok(json!({
                "status": "skipped",
                "messages": [],
                "stat": null,
                "reason": "codex_rollout_missing",
            }));
        }
        let messages = load_codex_history_by_thread(&req.thread_id);
        let status = if messages.is_empty() {
            "empty"
        } else {
            "messages"
        };
        Ok(json!({
            "status": status,
            "messages": messages,
            "stat": null,
        }))
    }

    fn history_delete(
        &self,
        req: ProviderHistoryRequest,
    ) -> Result<ProviderHistoryResponse, ProviderAdapterError> {
        let req: CodexHistoryDeleteRequest = serde_json::from_value(req)
            .map_err(|e| format!("Invalid OpenAI history delete request: {}", e))?;
        let reason = if req
            .thread_id
            .as_deref()
            .is_some_and(|thread_id| !thread_id.trim().is_empty())
        {
            "codex_rollout_delete_is_not_safe"
        } else {
            "codex_thread_id_missing"
        };
        Ok(json!({
            "status": "skipped",
            "method": "skipped",
            "reason": reason,
        }))
    }
}

// ── Session JSONL history loader ──────────────────────────────────
//
// Codex persists each conversation to a "rollout" file at
//   ~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<thread_id>.jsonl
// Each line: { timestamp, type, payload }. Real chat content lives in
// `response_item` lines whose `payload.type == "message"` and whose role
// is "user" or "assistant". A few user messages are system-injected by the
// CLI itself (environment_context, permissions blurbs, model_switch
// notes); we filter those out so the rendered chat shows only what the
// human + the model actually said.
//
// Returns messages in chronological order, mapped to the same
// HistoryMessage shape Claude uses so the frontend can route through
// existing `mapHistoryMessages` / `loadFromDisk` plumbing.

fn codex_sessions_root() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    let p = std::path::Path::new(&home).join(".codex").join("sessions");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

/// Walk `~/.codex/sessions/**/rollout-*-<thread_id>.jsonl` and return the
/// path to the (single) rollout file matching the given Codex thread id, if
/// one exists. The directory layout is shallow enough (year/month/day) that
/// a manual three-level walk is cheaper than pulling in `walkdir`.
fn find_codex_rollout(thread_id: &str) -> Option<std::path::PathBuf> {
    let root = codex_sessions_root()?;
    let suffix = format!("-{}.jsonl", thread_id);
    for year in std::fs::read_dir(&root).ok()?.flatten() {
        if !year.file_type().ok()?.is_dir() {
            continue;
        }
        for month in std::fs::read_dir(year.path()).ok()?.flatten() {
            if !month.file_type().ok()?.is_dir() {
                continue;
            }
            for day in std::fs::read_dir(month.path()).ok()?.flatten() {
                if !day.file_type().ok()?.is_dir() {
                    continue;
                }
                for file in std::fs::read_dir(day.path()).ok()?.flatten() {
                    let p = file.path();
                    if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                        if name.ends_with(&suffix) {
                            return Some(p);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Detect user messages that Codex injects as part of its prompt assembly
/// (environment context, permission blurbs, model-switch nudges, developer
/// instructions, file blocks) so we can hide them from the rendered chat
/// history. Defensive fallback: any message that's wholly wrapped in a
/// matching `<tag>…</tag>` envelope is treated as injected.
fn is_codex_system_injected_user_text(text: &str) -> bool {
    let t = text.trim();
    const KNOWN: &[&str] = &[
        "<environment_context>",
        "<permissions instructions>",
        "<model_switch>",
        "<user_instructions>",
        "<developer_instructions>",
        "<files>",
    ];
    if KNOWN.iter().any(|p| t.starts_with(p)) {
        return true;
    }
    // Defensive: any wrapper that opens with <tag …> and ends with </tag>.
    if let Some(rest) = t.strip_prefix('<') {
        if let Some(close_pos) = rest.find('>') {
            let tag_inner = &rest[..close_pos];
            let tag_name = tag_inner
                .split(|c: char| c.is_whitespace())
                .next()
                .unwrap_or("");
            if !tag_name.is_empty() && !tag_name.starts_with('/') {
                let close_tag = format!("</{}>", tag_name);
                if t.ends_with(&close_tag) {
                    return true;
                }
            }
        }
    }
    false
}

/// Treat a Codex tool output blob as an error if it carries one of the
/// shell-style failure signals we render in the live event stream:
///   - `^Process exited with code N` for any non-zero N
///   - leading `Error:` (used by Codex's MCP / built-in tools on failure)
fn detect_codex_tool_error(output: &str) -> bool {
    if output.starts_with("Error:") {
        return true;
    }
    if let Some(rest) = output.strip_prefix("Process exited with code ") {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(code) = digits.parse::<i32>() {
            return code != 0;
        }
    }
    false
}

/// Append a `HistoryToolCall` to the most recent assistant `HistoryMessage`,
/// or synthesise an empty-content assistant message when none exists / the
/// trailing entry is a user turn. Records `(msg_idx, tc_idx)` in `pending`
/// so the matching `*_output` envelope can patch the result back in.
fn attach_codex_tool_call(
    out: &mut Vec<HistoryMessage>,
    pending: &mut HashMap<String, (usize, usize)>,
    tc: HistoryToolCall,
    pending_key: &str,
    ts_ms: f64,
) {
    let target_idx = match out.last() {
        Some(m) if m.role == "assistant" => out.len() - 1,
        _ => {
            out.push(HistoryMessage {
                id: format!("codex-tools-{}", pending_key),
                role: "assistant".to_string(),
                content: String::new(),
                timestamp: ts_ms,
                tool_calls: Some(Vec::new()),
            });
            out.len() - 1
        }
    };
    let msg = &mut out[target_idx];
    let tcs = msg.tool_calls.get_or_insert_with(Vec::new);
    let ti = tcs.len();
    tcs.push(tc);
    pending.insert(pending_key.to_string(), (target_idx, ti));
}

fn find_u32_field(value: &JsonValue, names: &[&str]) -> Option<u32> {
    match value {
        JsonValue::Object(map) => {
            for name in names {
                if let Some(raw) = map.get(*name) {
                    if let Some(n) = raw.as_u64().and_then(|n| u32::try_from(n).ok()) {
                        return Some(n);
                    }
                    if let Some(n) = raw
                        .as_str()
                        .and_then(|s| s.parse::<u64>().ok())
                        .and_then(|n| u32::try_from(n).ok())
                    {
                        return Some(n);
                    }
                }
            }
            map.values().find_map(|v| find_u32_field(v, names))
        }
        JsonValue::Array(items) => items.iter().find_map(|v| find_u32_field(v, names)),
        _ => None,
    }
}

fn codex_rollback_turns(envelope: &JsonValue) -> Option<u32> {
    let etype = envelope.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let payload = envelope.get("payload").unwrap_or(envelope);
    let ptype = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let is_rollback = etype.eq_ignore_ascii_case("thread_rolled_back")
        || ptype.eq_ignore_ascii_case("thread_rolled_back")
        || etype.eq_ignore_ascii_case("thread_rollback")
        || ptype.eq_ignore_ascii_case("thread_rollback");
    if !is_rollback {
        return None;
    }
    find_u32_field(
        payload,
        &[
            "num_turns",
            "numTurns",
            "dropped_turns",
            "droppedTurns",
            "num_dropped_turns",
            "numDroppedTurns",
        ],
    )
}

fn drop_last_codex_history_turns(out: &mut Vec<HistoryMessage>, num_turns: u32) {
    for _ in 0..num_turns {
        let Some(user_idx) = out.iter().rposition(|m| m.role == "user") else {
            out.clear();
            return;
        };
        out.truncate(user_idx);
    }
}

fn flush_codex_apply_patch_change(
    changes: &mut Vec<JsonValue>,
    path: &mut Option<String>,
    kind: &mut &'static str,
    move_path: &mut Option<String>,
    diff_lines: &mut Vec<String>,
) {
    let Some(p) = path.take() else {
        return;
    };
    let diff = diff_lines.join("\n");
    let mut change = json!({
        "path": p,
        "kind": *kind,
        "diff": diff,
    });
    if let Some(move_to) = move_path.take() {
        change["move_path"] = json!(move_to);
    }
    changes.push(change);
    *kind = "update";
    diff_lines.clear();
}

fn codex_apply_patch_input(raw: &str) -> Option<JsonValue> {
    let mut changes: Vec<JsonValue> = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_kind: &'static str = "update";
    let mut current_move_path: Option<String> = None;
    let mut current_diff: Vec<String> = Vec::new();

    for line in raw.lines() {
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            flush_codex_apply_patch_change(
                &mut changes,
                &mut current_path,
                &mut current_kind,
                &mut current_move_path,
                &mut current_diff,
            );
            current_path = Some(path.trim().to_string());
            current_kind = "update";
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            flush_codex_apply_patch_change(
                &mut changes,
                &mut current_path,
                &mut current_kind,
                &mut current_move_path,
                &mut current_diff,
            );
            current_path = Some(path.trim().to_string());
            current_kind = "create";
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            flush_codex_apply_patch_change(
                &mut changes,
                &mut current_path,
                &mut current_kind,
                &mut current_move_path,
                &mut current_diff,
            );
            current_path = Some(path.trim().to_string());
            current_kind = "delete";
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Move to: ") {
            current_move_path = Some(path.trim().to_string());
            continue;
        }
        if line == "*** Begin Patch" || line == "*** End Patch" {
            continue;
        }
        if current_path.is_some() {
            current_diff.push(line.to_string());
        }
    }
    flush_codex_apply_patch_change(
        &mut changes,
        &mut current_path,
        &mut current_kind,
        &mut current_move_path,
        &mut current_diff,
    );

    if changes.is_empty() {
        return None;
    }
    let paths = changes
        .iter()
        .filter_map(|change| change.get("path").and_then(|v| v.as_str()))
        .map(|path| path.to_string())
        .collect::<Vec<_>>();
    let primary = paths.first().cloned().unwrap_or_default();
    Some(json!({
        "file_path": primary,
        "path": primary,
        "paths": paths,
        "changes": changes,
    }))
}

fn codex_tool_output(payload: &JsonValue) -> (String, bool) {
    let raw = match payload.get("output") {
        Some(JsonValue::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    if let Ok(value) = serde_json::from_str::<JsonValue>(&raw) {
        let output = value
            .get("output")
            .map(|v| match v {
                JsonValue::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| raw.clone());
        let exit_error = value
            .get("metadata")
            .and_then(|m| m.get("exit_code"))
            .and_then(|v| v.as_i64())
            .map(|code| code != 0)
            .unwrap_or(false);
        let is_error = exit_error || detect_codex_tool_error(&output);
        return (output, is_error);
    }
    let is_error = detect_codex_tool_error(&raw);
    (raw, is_error)
}

/// Parse a Codex rollout JSONL into the same HistoryMessage shape Claude uses,
/// with tool calls (function/local-shell, web_search, mcp) attached to the
/// preceding assistant turn. Single pass over the file; `pending_tools` keys
/// each in-flight `function_call` / `local_shell_call` by `call_id` so the
/// matching `*_output` envelope can patch the result back in. Web-search and
/// MCP tool calls are single-shot — we materialise them with the embedded
/// status/result on the spot. App-server rollback markers are replayed too so
/// refreshed UI history matches the model-visible thread context.
pub fn load_codex_history_by_thread(thread_id: &str) -> Vec<HistoryMessage> {
    let Some(path) = find_codex_rollout(thread_id) else {
        return Vec::new();
    };
    let Ok(file) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    let reader = std::io::BufReader::new(file);
    let mut out: Vec<HistoryMessage> = Vec::new();
    let mut pending_tools: HashMap<String, (usize, usize)> = HashMap::new();

    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(envelope) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(num_turns) = codex_rollback_turns(&envelope) {
            drop_last_codex_history_turns(&mut out, num_turns);
            pending_tools.clear();
            continue;
        }
        if envelope.get("type").and_then(|v| v.as_str()) != Some("response_item") {
            continue;
        }
        let Some(payload) = envelope.get("payload") else {
            continue;
        };
        let ptype = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        // Codex timestamps are ISO 8601 strings; convert to ms-since-epoch
        // so the frontend's existing renderer doesn't need a separate path.
        let ts_ms = envelope
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp_millis() as f64)
            .unwrap_or(0.0);

        match ptype {
            "message" => {
                let role = match payload.get("role").and_then(|v| v.as_str()) {
                    Some(r @ ("user" | "assistant")) => r.to_string(),
                    _ => continue,
                };
                // Concatenate every text-bearing content block. Codex stores
                // assistant text under `output_text` and user text under
                // `input_text`; both have a `text` field directly.
                let mut text = String::new();
                if let Some(arr) = payload.get("content").and_then(|v| v.as_array()) {
                    for block in arr {
                        if let Some(s) = block.get("text").and_then(|v| v.as_str()) {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(s);
                        }
                    }
                }
                if text.trim().is_empty() {
                    continue;
                }
                if role == "user" && is_codex_system_injected_user_text(&text) {
                    continue;
                }
                let id = payload
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("codex-{}", out.len()));
                out.push(HistoryMessage {
                    id,
                    role,
                    content: text,
                    timestamp: ts_ms,
                    tool_calls: None,
                });
            }
            "function_call" | "local_shell_call" => {
                let call_id = payload
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if call_id.is_empty() {
                    continue;
                }
                let raw_name = payload
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| {
                        if ptype == "local_shell_call" {
                            "Bash".to_string()
                        } else {
                            "function".to_string()
                        }
                    });
                // `arguments` is a JSON string (per OpenAI tool-call schema);
                // parse it so the frontend renderer can pick fields out
                // (e.g. `command`, `path`). Fall back to a `{_raw: "..."}`
                // wrapper so malformed payloads still render. For
                // `local_shell_call`, prefer the structured `action` if
                // `arguments` is missing.
                let input =
                    if let Some(args_str) = payload.get("arguments").and_then(|v| v.as_str()) {
                        serde_json::from_str::<serde_json::Value>(args_str)
                            .unwrap_or_else(|_| serde_json::json!({ "_raw": args_str }))
                    } else if let Some(action) = payload.get("action") {
                        action.clone()
                    } else {
                        serde_json::Value::Null
                    };
                let (name, input) = normalize_codex_history_tool(&raw_name, input);
                let tc = HistoryToolCall {
                    id: call_id.clone(),
                    name,
                    input,
                    result: None,
                    is_error: false,
                };
                attach_codex_tool_call(&mut out, &mut pending_tools, tc, &call_id, ts_ms);
            }
            "custom_tool_call" => {
                let call_id = payload
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if call_id.is_empty() {
                    continue;
                }
                let raw_name = payload
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("custom_tool");
                let raw_input = payload
                    .get("input")
                    .and_then(|v| v.as_str())
                    .or_else(|| payload.get("arguments").and_then(|v| v.as_str()))
                    .unwrap_or("");
                let (name, input) = if raw_name == "apply_patch" {
                    let parsed = codex_apply_patch_input(raw_input)
                        .unwrap_or_else(|| json!({ "input": raw_input }));
                    let change_count = parsed
                        .get("changes")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                        .unwrap_or(1);
                    let display = if change_count > 1 {
                        "MultiEdit"
                    } else {
                        "Edit"
                    };
                    (display.to_string(), parsed)
                } else if let Ok(parsed) = serde_json::from_str::<JsonValue>(raw_input) {
                    normalize_codex_history_tool(raw_name, parsed)
                } else {
                    normalize_codex_history_tool(raw_name, json!({ "input": raw_input }))
                };
                let tc = HistoryToolCall {
                    id: call_id.clone(),
                    name,
                    input,
                    result: None,
                    is_error: false,
                };
                attach_codex_tool_call(&mut out, &mut pending_tools, tc, &call_id, ts_ms);
            }
            "function_call_output" | "local_shell_call_output" | "custom_tool_call_output" => {
                let call_id = payload
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if call_id.is_empty() {
                    continue;
                }
                let (output, is_error) = codex_tool_output(payload);
                if let Some(&(mi, ti)) = pending_tools.get(&call_id) {
                    if let Some(msg) = out.get_mut(mi) {
                        if let Some(tcs) = msg.tool_calls.as_mut() {
                            if let Some(tc) = tcs.get_mut(ti) {
                                tc.result = Some(output);
                                tc.is_error = is_error;
                            }
                        }
                    }
                    pending_tools.remove(&call_id);
                }
            }
            "web_search_call" => {
                // Single-shot: built-in search tool already includes the
                // status when persisted, so we synthesise the tool call and
                // its result in one go. No `*_output` envelope follows.
                let id = payload
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("ws-{}", out.len()));
                let action = payload
                    .get("action")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                let status = payload
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let is_error = !status.is_empty() && status != "completed";
                let result = if status.is_empty() {
                    None
                } else {
                    Some(status)
                };
                let tc = HistoryToolCall {
                    id: id.clone(),
                    name: "web_search".to_string(),
                    input: action,
                    result,
                    is_error,
                };
                attach_codex_tool_call(&mut out, &mut pending_tools, tc, &id, ts_ms);
            }
            "mcp_tool_call" => {
                // Single-shot: MCP tool calls embed the result in the same
                // envelope (per upstream `mcp_tool_call` schema). Pair fields
                // defensively — `server`, `tool`, `arguments`, `result`,
                // `is_error` — none are guaranteed by the spec we have.
                let id = payload
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| {
                        payload
                            .get("call_id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_else(|| format!("mcp-{}", out.len()));
                let server = payload.get("server").and_then(|v| v.as_str()).unwrap_or("");
                let tool = payload.get("tool").and_then(|v| v.as_str()).unwrap_or("");
                let name = if !server.is_empty() && !tool.is_empty() {
                    format!("{}/{}", server, tool)
                } else if !tool.is_empty() {
                    tool.to_string()
                } else {
                    "mcp_tool".to_string()
                };
                let input = payload
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                let result = payload.get("result").map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                });
                let is_error = payload
                    .get("is_error")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let tc = HistoryToolCall {
                    id: id.clone(),
                    name,
                    input,
                    result,
                    is_error,
                };
                attach_codex_tool_call(&mut out, &mut pending_tools, tc, &id, ts_ms);
            }
            // No UI for chain-of-thought yet; live handler also drops it.
            "reasoning" => continue,
            _ => continue,
        }
    }
    out
}

/// Walk `~/.codex/sessions/**/rollout-*.jsonl` and return one `DiskSession`
/// per rollout whose `session_meta.cwd` matches the requested directory.
/// Each rollout's id is the Codex thread id (the suffix after the timestamp
/// in the filename); summary is the first user-typed prompt or a fallback.
/// Used by the dialog's "Previous Sessions" list when provider == "openai".
pub fn list_codex_disk_sessions(cwd: &str) -> Vec<DiskSession> {
    let Some(root) = codex_sessions_root() else {
        return Vec::new();
    };
    let target = std::path::Path::new(cwd);
    let target_canon = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());

    let mut out: Vec<DiskSession> = Vec::new();
    let Ok(year_iter) = std::fs::read_dir(&root) else {
        return out;
    };
    for year in year_iter.flatten() {
        if !year.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(month_iter) = std::fs::read_dir(year.path()) else {
            continue;
        };
        for month in month_iter.flatten() {
            if !month.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Ok(day_iter) = std::fs::read_dir(month.path()) else {
                continue;
            };
            for day in day_iter.flatten() {
                if !day.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let Ok(file_iter) = std::fs::read_dir(day.path()) else {
                    continue;
                };
                for file in file_iter.flatten() {
                    let p = file.path();
                    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    if !name.starts_with("rollout-") || !name.ends_with(".jsonl") {
                        continue;
                    }
                    if let Some(meta) = peek_codex_rollout(&p) {
                        let rollout_cwd = std::path::Path::new(&meta.cwd);
                        let rollout_canon = std::fs::canonicalize(rollout_cwd)
                            .unwrap_or_else(|_| rollout_cwd.to_path_buf());
                        if rollout_canon != target_canon {
                            continue;
                        }
                        let modified = file
                            .metadata()
                            .and_then(|m| m.modified())
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
                        out.push(DiskSession {
                            id: meta.thread_id,
                            modified,
                            size,
                            summary: meta.summary,
                        });
                    }
                }
            }
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    out
}

struct CodexRolloutMeta {
    thread_id: String,
    cwd: String,
    summary: String,
}

/// Read just enough of a rollout JSONL to recover the thread id, cwd, and
/// the first real user prompt (skipping injected developer/permissions/env
/// blocks). Stops as soon as it finds a usable summary.
fn peek_codex_rollout(path: &std::path::Path) -> Option<CodexRolloutMeta> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut thread_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut summary: Option<String> = None;
    for line in reader.lines().map_while(Result::ok).take(200) {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(envelope) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let etype = envelope.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if etype == "session_meta" {
            if let Some(payload) = envelope.get("payload") {
                if let Some(id) = payload.get("id").and_then(|v| v.as_str()) {
                    thread_id = Some(id.to_string());
                }
                if let Some(c) = payload.get("cwd").and_then(|v| v.as_str()) {
                    cwd = Some(c.to_string());
                }
            }
        } else if etype == "response_item" && summary.is_none() {
            if let Some(payload) = envelope.get("payload") {
                let ptype = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let role = payload.get("role").and_then(|v| v.as_str()).unwrap_or("");
                if ptype == "message" && role == "user" {
                    if let Some(arr) = payload.get("content").and_then(|v| v.as_array()) {
                        for block in arr {
                            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                if !is_codex_system_injected_user_text(text) {
                                    let trimmed: String = text.chars().take(120).collect();
                                    summary = Some(trimmed);
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        if thread_id.is_some() && cwd.is_some() && summary.is_some() {
            break;
        }
    }
    Some(CodexRolloutMeta {
        thread_id: thread_id?,
        cwd: cwd.unwrap_or_default(),
        summary: summary.unwrap_or_default(),
    })
}

// ── Rollout truncation (rewind) ──────────────────────────────────
//
// `codex exec resume <thread_id>` re-reads the entire rollout JSONL as
// conversation memory. There is no `--resume-at` flag, so the only way to
// rewind is to physically truncate the rollout file on a turn boundary.
//
// A "turn" is the run between an `event_msg{type:"task_started"}` and the
// matching `event_msg{type:"task_complete"}`. Mid-turn truncation leaves an
// orphan `task_started`, an unpaired `function_call`, or stranded
// `agent_reasoning`, which Codex's own state machine refuses on resume —
// so we always cut immediately AFTER a `task_complete` line.
//
// Line 0 is `session_meta` (id, cwd, cli_version, base_instructions, git);
// it is preserved verbatim — Codex needs it to anchor the resume.

/// Truncate a Codex rollout to drop the last `num_turns` completed turns.
///
/// Returns the number of turns actually removed (capped at the total turn
/// count present in the rollout). Errors only on missing rollout / corrupt
/// `session_meta` / IO failure.
pub fn truncate_codex_rollout_by_turns(thread_id: &str, num_turns: u32) -> Result<u32, String> {
    if num_turns == 0 {
        return Ok(0);
    }
    let path = find_codex_rollout(thread_id)
        .ok_or_else(|| format!("rollout for thread {} not found", thread_id))?;
    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    if content.is_empty() {
        return Ok(0);
    }
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    if lines.is_empty() {
        return Ok(0);
    }
    // Validate line 0 is `session_meta` so we don't accidentally clobber a
    // file with a different schema.
    let first_trim = lines[0].trim();
    let first_envelope: serde_json::Value =
        serde_json::from_str(first_trim).map_err(|e| format!("parse session_meta line: {}", e))?;
    if first_envelope.get("type").and_then(|v| v.as_str()) != Some("session_meta") {
        return Err("first line is not session_meta — refusing to truncate".to_string());
    }

    // Walk the file once, recording the index of every `task_complete` event.
    let mut task_complete_indices: Vec<usize> = Vec::new();
    for (i, raw) in lines.iter().enumerate() {
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        let Ok(env) = serde_json::from_str::<serde_json::Value>(s) else {
            continue;
        };
        if env.get("type").and_then(|v| v.as_str()) == Some("event_msg") {
            if let Some(p) = env.get("payload") {
                if p.get("type").and_then(|v| v.as_str()) == Some("task_complete") {
                    task_complete_indices.push(i);
                }
            }
        }
    }
    let total_turns = task_complete_indices.len();
    if total_turns == 0 {
        // No completed turns — nothing safe to truncate.
        return Ok(0);
    }
    let drop = (num_turns as usize).min(total_turns);
    let keep_turns = total_turns - drop;
    // If we keep N turns, cut after the Nth `task_complete`. Keeping zero
    // turns means we drop everything past line 0 (`session_meta`).
    let truncate_after_idx = if keep_turns == 0 {
        0
    } else {
        task_complete_indices[keep_turns - 1]
    };
    let keep_count = truncate_after_idx + 1;
    let truncated: String = lines.iter().take(keep_count).copied().collect();

    // Atomic write: stage to sibling tmp, fsync, rename, fsync parent dir.
    let parent = path.parent().ok_or("rollout path has no parent")?;
    let suffix = format!(
        ".tmp.{}.{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    );
    let tmp = path.with_extension(format!(
        "{}{}",
        path.extension().and_then(|e| e.to_str()).unwrap_or("jsonl"),
        suffix
    ));
    {
        use std::io::Write;
        let mut f =
            std::fs::File::create(&tmp).map_err(|e| format!("create {}: {}", tmp.display(), e))?;
        f.write_all(truncated.as_bytes())
            .map_err(|e| format!("write {}: {}", tmp.display(), e))?;
        if let Err(e) = f.sync_all() {
            safe_eprintln!("[codex truncate] sync_all {}: {}", tmp.display(), e);
        }
    }
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename {} -> {}: {}", tmp.display(), path.display(), e)
    })?;
    #[cfg(unix)]
    {
        if let Err(e) = std::fs::File::open(parent).and_then(|d| d.sync_all()) {
            safe_eprintln!(
                "[codex truncate] parent sync_all {}: {}",
                parent.display(),
                e
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
    }

    Ok(drop as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_value(event: ProviderRuntimeEvent) -> JsonValue {
        event.into_value()
    }

    #[test]
    fn maps_codex_lifecycle_and_mcp_events_to_provider_runtime_events() {
        let session_events = codex_legacy_event_to_runtime_events(
            "session-1",
            &json!({
                "type": "thread.started",
                "thread_id": "thread-1",
            }),
        );
        assert_eq!(session_events.len(), 1);
        let session = runtime_value(session_events[0].clone());
        assert_eq!(session["type"], "provider.session");
        assert_eq!(session["provider"], "openai");
        assert_eq!(session["sessionId"], "session-1");
        assert_eq!(session["threadId"], "thread-1");
        assert_eq!(session["phase"], "started");

        let turn_events = codex_legacy_event_to_runtime_events(
            "session-1",
            &json!({
                "type": "turn.completed",
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 5,
                    "total_tokens": 15,
                },
            }),
        );
        assert_eq!(turn_events.len(), 1);
        let turn = runtime_value(turn_events[0].clone());
        assert_eq!(turn["type"], "provider.turn");
        assert_eq!(turn["phase"], "completed");
        assert_eq!(turn["usage"]["input_tokens"], 10);
        assert_eq!(turn["usage"]["total_tokens"], 15);

        let mcp_events = codex_legacy_event_to_runtime_events(
            "session-1",
            &json!({
                "type": "mcp.status.updated",
                "server": {
                    "name": "terminal-64",
                    "status": "connected",
                },
            }),
        );
        assert_eq!(mcp_events.len(), 1);
        let mcp = runtime_value(mcp_events[0].clone());
        assert_eq!(mcp["type"], "provider.mcp");
        assert_eq!(mcp["phase"], "status");
        assert_eq!(mcp["servers"][0]["name"], "terminal-64");
    }

    #[test]
    fn maps_codex_content_and_tool_events_to_provider_runtime_events() {
        let delta_events = codex_legacy_event_to_runtime_events(
            "session-2",
            &json!({
                "type": "item.updated",
                "delta": "hello",
                "item": {
                    "id": "msg-1",
                    "type": "agent_message",
                    "text": "hello",
                },
            }),
        );
        assert_eq!(delta_events.len(), 1);
        let delta = runtime_value(delta_events[0].clone());
        assert_eq!(delta["type"], "provider.content");
        assert_eq!(delta["phase"], "delta");
        assert_eq!(delta["itemId"], "msg-1");
        assert_eq!(delta["text"], "hello");

        let tool_started = codex_legacy_event_to_runtime_events(
            "session-2",
            &json!({
                "type": "item.started",
                "item": {
                    "id": "tool-1",
                    "type": "command_execution",
                    "command": "npm test",
                },
            }),
        );
        assert_eq!(tool_started.len(), 1);
        let started = runtime_value(tool_started[0].clone());
        assert_eq!(started["type"], "provider.tool");
        assert_eq!(started["phase"], "started");
        assert_eq!(started["id"], "tool-1");
        assert_eq!(started["name"], "Bash");
        assert_eq!(started["input"]["command"], "npm test");

        let mcp_completed = codex_legacy_event_to_runtime_events(
            "session-2",
            &json!({
                "type": "item.completed",
                "item": {
                    "id": "mcp-1",
                    "type": "mcp_tool_call",
                    "server": "terminal-64",
                    "tool_name": "read_team",
                    "arguments": { "last": 5 },
                    "output": "ok",
                    "status": "completed",
                },
            }),
        );
        assert_eq!(mcp_completed.len(), 2);
        let started = runtime_value(mcp_completed[0].clone());
        let completed = runtime_value(mcp_completed[1].clone());
        assert_eq!(started["phase"], "started");
        assert_eq!(started["name"], "terminal-64/read_team");
        assert_eq!(completed["phase"], "completed");
        assert_eq!(completed["result"], "ok");
        assert_eq!(completed["isError"], false);
    }

    #[test]
    fn maps_codex_error_events_to_provider_runtime_errors() {
        let error_events = codex_legacy_event_to_runtime_events(
            "session-3",
            &json!({
                "type": "error",
                "message": "Codex failed",
            }),
        );
        assert_eq!(error_events.len(), 1);
        let error = runtime_value(error_events[0].clone());
        assert_eq!(error["type"], "provider.error");
        assert_eq!(error["provider"], "openai");
        assert_eq!(error["phase"], "error");
        assert_eq!(error["message"], "Codex failed");
        assert_eq!(error["terminal"], true);
    }

    #[test]
    fn hydrate_missing_codex_rollout_reports_skipped_reason() {
        let adapter = CodexAdapter::new();
        let result = match adapter.history_hydrate(json!({
            "thread_id": "missing-thread-for-hydrate-test-00000000",
        })) {
            Ok(value) => value,
            Err(err) => panic!("history hydrate should not error for a missing rollout: {err}"),
        };

        assert_eq!(result["status"], "skipped");
        assert_eq!(result["reason"], "codex_rollout_missing");
        assert_eq!(result["messages"].as_array().map(Vec::len), Some(0));
    }
}
