import {
  ensureCodexSkills,
  mapHistoryMessages,
  providerCancel,
  providerClose,
  providerCreate,
  providerHistoryDelete,
  providerHistoryFork,
  providerHistoryHydrate,
  providerHistoryTruncate,
  providerSend,
} from "../tauriApi";
import type { PermissionMode, ChatMessage } from "../types";
import type { CreateCodexRequest, SendCodexPromptRequest } from "../../contracts/providerIpc";
import type {
  ProviderHistoryTruncateResult,
  ProviderHistoryDeleteResult,
  ProviderHydrateResult,
  ProviderRuntime,
  ProviderTurnInput,
  ProviderTurnResult,
} from "../../contracts/providerRuntime";
import { getProviderTurnResumeId } from "../../contracts/providerRuntime";
import { decodeCodexPermission } from "../providers";
import { getOpenAiThreadIdForSession, setOpenAiThreadIdForSession } from "./openaiSessionMetadata";

function stringControlValue(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

declare module "../../contracts/providerIpc" {
  interface ProviderCreateRequestMap {
    openai: CreateCodexRequest;
  }

  interface ProviderSendRequestMap {
    openai: SendCodexPromptRequest;
  }

  interface ProviderHistoryTruncateRequestMap {
    openai: {
      thread_id: string;
      cwd: string;
      num_turns: number;
    };
  }

  interface ProviderHistoryForkRequestMap {
    openai: {
      thread_id: string;
      cwd: string;
      drop_turns: number;
    };
  }

  interface ProviderHistoryHydrateRequestMap {
    openai: {
      thread_id: string;
    };
  }

  interface ProviderHistoryDeleteRequestMap {
    openai: {
      thread_id?: string | null;
    };
  }
}

export const CODEX_MAX_TURN_PROMPT_CHARS = 900_000;
const CODEX_MIN_SEED_TRANSCRIPT_CHARS = 24_000;
const CODEX_MAX_SEED_MESSAGE_CHARS = 24_000;
const CODEX_MAX_SEED_TOOL_INPUT_CHARS = 4_000;
const CODEX_MAX_SEED_TOOL_RESULT_CHARS = 4_000;

export function codexPermissionForOverride(current: string, override?: PermissionMode) {
  if (override === "bypass_all") return decodeCodexPermission("yolo");
  if (override === "accept_edits" || override === "auto") return decodeCodexPermission("full-auto");
  if (override === "plan") return decodeCodexPermission("read-only");
  return decodeCodexPermission(current);
}

function clipMiddle(text: string, maxChars: number, label: string): string {
  if (text.length <= maxChars) return text;
  if (maxChars <= 0) return "";
  const omitted = text.length - maxChars;
  const marker = `\n[${label} truncated by Terminal 64 to stay under Codex app-server input limits; omitted ${omitted} chars]\n`;
  if (marker.length >= maxChars) return text.slice(0, maxChars);
  const remaining = maxChars - marker.length;
  const head = Math.ceil(remaining * 0.65);
  const tail = remaining - head;
  return `${text.slice(0, head)}${marker}${tail > 0 ? text.slice(-tail) : ""}`;
}

function stringifySeedValue(value: unknown, maxChars: number, label: string): string {
  let text: string;
  if (typeof value === "string") {
    text = value;
  } else {
    try {
      text = JSON.stringify(value);
    } catch {
      text = String(value);
    }
  }
  return clipMiddle(text, maxChars, label);
}

function renderSeedMessage(message: ChatMessage): string {
  const who = message.role === "user" ? "User" : "Assistant";
  const text = clipMiddle((message.content || "").trim(), CODEX_MAX_SEED_MESSAGE_CHARS, "message");
  const tools = message.toolCalls?.map((toolCall) => {
    const lines = [`Tool: ${toolCall.name}`];
    if (toolCall.input && Object.keys(toolCall.input).length > 0) {
      lines.push(`Input: ${stringifySeedValue(toolCall.input, CODEX_MAX_SEED_TOOL_INPUT_CHARS, "tool input")}`);
    }
    if (toolCall.result !== undefined) {
      lines.push(`Result: ${stringifySeedValue(toolCall.result, CODEX_MAX_SEED_TOOL_RESULT_CHARS, "tool result")}`);
    }
    return lines.join("\n");
  }).join("\n");
  return `${who}: ${text}${tools ? `\n${tools}` : ""}`.trim();
}

function renderSeedTranscript(messages: ChatMessage[], maxChars: number): string {
  const parts: string[] = [];
  let used = 0;
  let omitted = 0;

  for (let i = messages.length - 1; i >= 0; i--) {
    const message = messages[i];
    if (!message) continue;
    const rendered = renderSeedMessage(message);
    if (!rendered) continue;
    const separator = parts.length > 0 ? 2 : 0;
    const needed = rendered.length + separator;
    if (needed > maxChars - used) {
      if (parts.length === 0) {
        parts.unshift(clipMiddle(rendered, maxChars, "latest fork transcript message"));
      } else {
        omitted = i + 1;
      }
      break;
    }
    parts.unshift(rendered);
    used += needed;
  }

  if (omitted > 0) {
    const marker = `[${omitted} older fork transcript message${omitted === 1 ? "" : "s"} omitted to stay under Codex app-server input limits]`;
    if (parts.length === 0) {
      return marker;
    }
    const currentLength = parts.join("\n\n").length;
    if (currentLength + marker.length + 2 <= maxChars) {
      parts.unshift(marker);
    }
  }

  return parts.join("\n\n");
}

export function promptWithCodexSeed(prompt: string, seedTranscript: ChatMessage[] | null | undefined): string {
  const clippedPrompt = clipMiddle(prompt, CODEX_MAX_TURN_PROMPT_CHARS, "user prompt");
  if (!seedTranscript?.length) return clippedPrompt;

  const prefix = "You are continuing from a forked Terminal 64 conversation. Prior transcript:\n\n";
  const suffix = "\n\nContinue from there and answer this new user message:\n\n";
  const transcriptBudget = Math.max(
    CODEX_MIN_SEED_TRANSCRIPT_CHARS,
    CODEX_MAX_TURN_PROMPT_CHARS - prefix.length - suffix.length - clippedPrompt.length,
  );
  const boundedTranscript = renderSeedTranscript(seedTranscript, transcriptBudget);
  const fullPrompt = `${prefix}${boundedTranscript}${suffix}${clippedPrompt}`;
  return clipMiddle(fullPrompt, CODEX_MAX_TURN_PROMPT_CHARS, "fork prompt");
}

async function ensureCodexRuntime(input: ProviderTurnInput<"openai">) {
  await ensureCodexSkills().catch(() => {});
  return input;
}

export function buildCodexCreateRequest(input: ProviderTurnInput<"openai">): CreateCodexRequest {
  const options = input.providerOptions?.openai;
  const selectedModel = stringControlValue(input.selectedControls?.model);
  const selectedEffort = stringControlValue(input.selectedControls?.effort);
  const prompt = promptWithCodexSeed(input.prompt, input.seedTranscript);
  const codexPerm = codexPermissionForOverride(
    input.providerPermissionId ?? "workspace",
    input.permissionOverride,
  );
  return {
    session_id: input.sessionId,
    cwd: input.cwd,
    prompt,
    ...(selectedModel ? { model: selectedModel } : {}),
    ...(selectedEffort ? { effort: selectedEffort } : {}),
    ...(options?.collaborationMode ? { collaboration_mode: options.collaborationMode } : {}),
    ...(options?.skipGitRepoCheck ? { skip_git_repo_check: true } : {}),
    ...(options?.mcpEnv ? { mcp_env: options.mcpEnv } : {}),
    ...codexPerm,
  };
}

export function buildCodexSendRequest(
  input: ProviderTurnInput<"openai">,
  createReq: CreateCodexRequest,
): SendCodexPromptRequest {
  const threadId = getProviderTurnResumeId(input);
  return {
    ...createReq,
    ...(threadId ? { thread_id: threadId } : {}),
  };
}

export function codexDropTurnsForKeepMessages(preMessages: ChatMessage[], keepMessages: number): number {
  const totalTurns = preMessages.filter((m) => m.role === "user").length;
  const keepTurns = preMessages.slice(0, keepMessages).filter((m) => m.role === "user").length;
  return Math.max(0, totalTurns - keepTurns);
}

function seedResult(input: ProviderTurnInput<"openai">): ProviderTurnResult {
  return { clearSeedTranscript: !!input.seedTranscript?.length };
}

function operationStatus(status: "applied" | "skipped" | "unsupported" | undefined) {
  return status ?? "applied";
}

async function create(input: ProviderTurnInput<"openai">): Promise<ProviderTurnResult> {
  const createReq = buildCodexCreateRequest(input);
  const sendReq = buildCodexSendRequest(input, createReq);
  try {
    await providerCreate({ provider: "openai", req: createReq }, input.skipOpenwolf);
  } catch {
    if (!input.started) {
      throw new Error("Codex session failed to start before a thread id was created.");
    }
    // Legacy metadata from early Codex builds did not persist the external
    // thread id. Only already-started sessions get the old local-id resume
    // fallback; a true first turn would otherwise resume the wrong id.
    await providerSend({ provider: "openai", req: sendReq }, input.skipOpenwolf);
  }
  return seedResult(input);
}

async function send(input: ProviderTurnInput<"openai">): Promise<ProviderTurnResult> {
  const createReq = buildCodexCreateRequest(input);
  const sendReq = buildCodexSendRequest(input, createReq);
  if (!getProviderTurnResumeId(input)) {
    try {
      await providerCreate({ provider: "openai", req: createReq }, input.skipOpenwolf);
    } catch {
      if (!input.started) {
        throw new Error("Codex session failed to start before a thread id was created.");
      }
      await providerSend({ provider: "openai", req: sendReq }, input.skipOpenwolf);
    }
    return seedResult(input);
  }
  try {
    await providerSend({ provider: "openai", req: sendReq }, input.skipOpenwolf);
  } catch {
    await providerCreate({ provider: "openai", req: createReq }, input.skipOpenwolf);
  }
  return seedResult(input);
}

export const openaiRuntime: ProviderRuntime<"openai"> = {
  provider: "openai",

  prepareTurn: ensureCodexRuntime,

  create,

  send,

  cancel(sessionId) {
    return providerCancel("openai", sessionId);
  },

  close(sessionId) {
    return providerClose("openai", sessionId);
  },

  history: {
    source: "codex-rollout",
    capabilities: {
      hydrate: true,
      fork: true,
      rewind: true,
      delete: true,
    },

    async rewind(input): Promise<ProviderHistoryTruncateResult> {
      const threadId = getOpenAiThreadIdForSession(input.sessionId);
      if (!threadId) {
        return {
          status: "unsupported",
          reason: "codex_thread_id_missing",
        };
      }
      const dropTurns = codexDropTurnsForKeepMessages(input.preMessages, input.keepMessages);
      const result = await providerHistoryTruncate({
        provider: "openai",
        req: {
          thread_id: threadId,
          cwd: input.cwd,
          num_turns: dropTurns,
        },
      });
      if (result.method === "rollout" && result.rollback_error) {
        console.warn("[providerRuntime] Codex app-server rollback failed, fell back to rollout truncation:", result.rollback_error);
      }
      const output: ProviderHistoryTruncateResult = {
        status: operationStatus(result.status),
      };
      if (result.reason) output.reason = result.reason;
      return output;
    },

    async fork(input) {
      if (input.keepMessages <= 0) {
        return { status: "skipped", reason: "no_messages_to_fork" };
      }
      const threadId = getOpenAiThreadIdForSession(input.parentSessionId);
      if (!threadId) {
        return {
          status: "skipped",
          reason: "codex_thread_id_missing",
          seedTranscript: true,
        };
      }

      const dropTurns = codexDropTurnsForKeepMessages(input.preMessages, input.keepMessages);
      try {
        const result = await providerHistoryFork({
          provider: "openai",
          req: {
            thread_id: threadId,
            cwd: input.cwd,
            drop_turns: dropTurns,
          },
        });
        if (result.status === "unsupported") {
          return {
            status: "unsupported",
            ...(result.reason ? { reason: result.reason } : {}),
          };
        }
        if (!result.codex_thread_id) {
          throw new Error("OpenAI history fork did not return a thread id");
        }
        setOpenAiThreadIdForSession(input.newSessionId, result.codex_thread_id);
        return {
          status: operationStatus(result.status),
        };
      } catch (err) {
        console.warn("[fork] Codex app-server fork failed; falling back to seeded transcript:", err);
        return {
          status: "skipped",
          reason: "codex_native_fork_failed",
          seedTranscript: true,
        };
      }
    },

    async hydrate(input): Promise<ProviderHydrateResult> {
      const threadId = getOpenAiThreadIdForSession(input.sessionId);
      if (!threadId) {
        return { status: "skipped", reason: "codex_thread_id_missing" };
      }
      const result = await providerHistoryHydrate({
        provider: "openai",
        req: { thread_id: threadId },
      });
      if (result.status === "skipped" || result.status === "unsupported") {
        if (result.reason === "codex_rollout_missing") {
          setOpenAiThreadIdForSession(input.sessionId, null);
        }
        return {
          status: result.status,
          ...(result.reason ? { reason: result.reason } : {}),
          ...(result.reason === "codex_rollout_missing" ? { clearCache: true } : {}),
        };
      }
      const history = result.messages;
      if (history.length === 0) {
        return { status: "empty" };
      }
      return { status: "messages", messages: mapHistoryMessages(history) };
    },

    async deleteHistory(input): Promise<ProviderHistoryDeleteResult> {
      const threadId = getOpenAiThreadIdForSession(input.sessionId);
      const result = await providerHistoryDelete({
        provider: "openai",
        req: threadId ? { thread_id: threadId } : {},
      });
      return {
        status: operationStatus(result.status),
        method: result.method,
        ...(result.reason ? { reason: result.reason } : {}),
      };
    },
  },
};
