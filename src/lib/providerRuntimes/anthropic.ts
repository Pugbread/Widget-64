import {
  createMcpConfigFile,
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
import type {
  ProviderHistoryDeleteResult,
  ProviderHistoryTruncateResult,
  ProviderHydrateResult,
  ProviderRuntime,
  ProviderTurnOptionsByProvider,
  ProviderTurnInput,
  ProviderTurnResult,
} from "../../contracts/providerRuntime";
import type { CreateClaudeRequest, SendClaudePromptRequest } from "../types";

function stringControlValue(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

declare module "../../contracts/providerIpc" {
  interface ProviderCreateRequestMap {
    anthropic: CreateClaudeRequest;
  }

  interface ProviderSendRequestMap {
    anthropic: SendClaudePromptRequest;
  }

  interface ProviderHistoryTruncateRequestMap {
    anthropic: {
      session_id: string;
      cwd: string;
      keep_messages: number;
    };
  }

  interface ProviderHistoryForkRequestMap {
    anthropic: {
      parent_session_id: string;
      new_session_id: string;
      cwd: string;
      keep_messages: number;
    };
  }

  interface ProviderHistoryHydrateRequestMap {
    anthropic: {
      session_id: string;
      cwd: string;
      resume_at_uuid?: string | null;
    };
  }

  interface ProviderHistoryDeleteRequestMap {
    anthropic: {
      session_id: string;
      cwd: string;
    };
  }
}

function buildClaudeRequest(input: ProviderTurnInput<"anthropic">): CreateClaudeRequest {
  const options = input.providerOptions?.anthropic;
  const selectedModel = stringControlValue(input.selectedControls?.model);
  const selectedEffort = stringControlValue(input.selectedControls?.effort);
  return {
    session_id: input.sessionId,
    cwd: input.cwd,
    prompt: input.prompt,
    permission_mode: input.permissionOverride || input.permissionMode || "default",
    ...(selectedModel ? { model: selectedModel } : {}),
    ...(selectedEffort ? { effort: selectedEffort } : {}),
    ...(options?.mcpConfig ? { mcp_config: options.mcpConfig } : {}),
    ...(options?.noSessionPersistence ? { no_session_persistence: true } : {}),
  };
}

function buildClaudeSendRequest(input: ProviderTurnInput<"anthropic">): SendClaudePromptRequest {
  const options = input.providerOptions?.anthropic;
  return {
    ...buildClaudeRequest(input),
    ...(options?.disallowedTools ? { disallowed_tools: options.disallowedTools } : {}),
    ...(input.resumeAtUuid ? { resume_session_at: input.resumeAtUuid } : {}),
  };
}

function delegationMcpEnvConfig(env: Record<string, string> | undefined) {
  if (!env) return null;
  const port = Number(env.T64_DELEGATION_PORT || "0");
  const delegationSecret = env.T64_DELEGATION_SECRET || "";
  const groupId = env.T64_GROUP_ID || "";
  if (!Number.isFinite(port) || port <= 0 || !delegationSecret || !groupId) {
    return null;
  }
  return {
    delegationPort: port,
    delegationSecret,
    groupId,
    agentLabel: env.T64_AGENT_LABEL || "Agent",
  };
}

async function prepareTurn(input: ProviderTurnInput<"anthropic">): Promise<ProviderTurnInput<"anthropic">> {
  const options = input.providerOptions?.anthropic;
  if (options?.mcpConfig) return input;

  const delegation = delegationMcpEnvConfig(options?.mcpEnv);
  if (!delegation) return input;

  const mcpConfig = await createMcpConfigFile(
    delegation.delegationPort,
    delegation.delegationSecret,
    delegation.groupId,
    delegation.agentLabel,
    input.cwd,
  );
  const providerOptions: ProviderTurnOptionsByProvider = {
    ...input.providerOptions,
    anthropic: {
      ...options,
      mcpConfig,
    },
  };
  return { ...input, providerOptions };
}

function legacyResumeResult(input: ProviderTurnInput<"anthropic">): ProviderTurnResult {
  return input.resumeAtUuid ? { clearResumeAtUuid: true } : {};
}

async function create(input: ProviderTurnInput<"anthropic">): Promise<ProviderTurnResult> {
  const req = buildClaudeRequest(input);
  try {
    await providerCreate({ provider: "anthropic", req }, input.skipOpenwolf);
  } catch {
    await providerSend({ provider: "anthropic", req }, input.skipOpenwolf);
  }
  return legacyResumeResult(input);
}

async function send(input: ProviderTurnInput<"anthropic">): Promise<ProviderTurnResult> {
  const req = buildClaudeSendRequest(input);
  if (input.forkParentSessionId) {
    const forkReq: SendClaudePromptRequest = { ...req, fork_session: input.forkParentSessionId };
    await providerSend({ provider: "anthropic", req: forkReq }, input.skipOpenwolf);
    return {
      clearForkParentSessionId: true,
      ...(input.resumeAtUuid ? { clearResumeAtUuid: true } : {}),
    };
  }
  try {
    await providerSend({ provider: "anthropic", req }, input.skipOpenwolf);
  } catch {
    await providerCreate({ provider: "anthropic", req }, input.skipOpenwolf);
  }
  return legacyResumeResult(input);
}

function operationStatus(status: "applied" | "skipped" | "unsupported" | undefined) {
  return status ?? "applied";
}

export const anthropicRuntime: ProviderRuntime<"anthropic"> = {
  provider: "anthropic",

  prepareTurn,

  create,

  send,

  cancel(sessionId) {
    return providerCancel("anthropic", sessionId);
  },

  close(sessionId) {
    return providerClose("anthropic", sessionId);
  },

  history: {
    source: "claude-jsonl",
    capabilities: {
      hydrate: true,
      fork: true,
      rewind: true,
      delete: true,
    },

    async rewind(input): Promise<ProviderHistoryTruncateResult> {
      const result = await providerHistoryTruncate({
        provider: "anthropic",
        req: {
          session_id: input.sessionId,
          cwd: input.cwd,
          keep_messages: input.keepMessages,
        },
      });
      const output: ProviderHistoryTruncateResult = {
        status: operationStatus(result.status),
        resumeAtUuid: result.resume_at_uuid ?? null,
      };
      if (result.reason) output.reason = result.reason;
      return output;
    },

    async fork(input) {
      if (input.keepMessages <= 0) {
        return { status: "skipped", reason: "no_messages_to_fork" };
      }
      const result = await providerHistoryFork({
        provider: "anthropic",
        req: {
          parent_session_id: input.parentSessionId,
          new_session_id: input.newSessionId,
          cwd: input.cwd,
          keep_messages: input.keepMessages,
        },
      });
      const output = { status: operationStatus(result.status) };
      if (result.reason) return { ...output, reason: result.reason };
      return output;
    },

    async hydrate(input): Promise<ProviderHydrateResult> {
      const result = await providerHistoryHydrate({
        provider: "anthropic",
        req: {
          session_id: input.sessionId,
          cwd: input.cwd,
          ...(input.resumeAtUuid ? { resume_at_uuid: input.resumeAtUuid } : {}),
        },
      });
      if (result.status === "skipped" || result.status === "unsupported") {
        return {
          status: result.status,
          ...(result.reason ? { reason: result.reason } : {}),
          clearCache: true,
        };
      }
      const stat = result.stat;
      if (!stat) {
        return { status: "empty", clearCache: true };
      }

      const cached = input.resumeAtUuid ? null : input.cacheEntry;
      if (cached && cached.mtimeMs === stat.mtime_ms && cached.size === stat.size) {
        return { status: "messages", messages: cached.messages };
      }

      const history = result.messages;
      if (history.length === 0) {
        return { status: "empty" };
      }

      const messages = mapHistoryMessages(history);
      if (input.resumeAtUuid) {
        return { status: "messages", messages, clearCache: true };
      }
      return {
        status: "messages",
        messages,
        cacheWrite: {
          mtimeMs: stat.mtime_ms,
          size: stat.size,
          messages,
        },
      };
    },

    async deleteHistory(input): Promise<ProviderHistoryDeleteResult> {
      const result = await providerHistoryDelete({
        provider: "anthropic",
        req: {
          session_id: input.sessionId,
          cwd: input.cwd,
        },
      });
      const output: ProviderHistoryDeleteResult = {
        status: operationStatus(result.status),
        method: result.method,
      };
      if (result.reason) output.reason = result.reason;
      return output;
    },
  },
};
