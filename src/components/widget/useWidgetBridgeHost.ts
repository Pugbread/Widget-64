import { useCallback, useEffect, useRef } from "react";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import {
  browserEval,
  closeBrowser,
  closeTerminal,
  deleteFiles,
  listDirectory,
  navigateBrowser,
  createBrowser,
  onTerminalOutput,
  openwolfDaemonInfo,
  openwolfDaemonStopAll,
  openwolfDaemonSwitch,
  proxyFetch,
  readFile,
  searchFiles,
  setBrowserVisible,
  shellExec,
  widgetClearState,
  widgetGetState,
  widgetSetState,
  writeFile,
  writeTerminal,
} from "../../lib/tauriApi";
import { runProviderTurn } from "../../lib/providerRuntime";
import { pushToast } from "../../lib/notifications";
import { isProviderId, type ProviderId } from "../../lib/providers";
import { widgetBus } from "../../lib/widgetBus";
import { invokePlugin, onPluginEvent, type PluginStreamHandle } from "../../lib/pluginApi";
import { startVoice as startVoiceCapture, stopVoice as stopVoiceCapture, onVoiceIntent, onVoicePartial, onVoiceFinal } from "../../lib/voiceApi";
import {
  getProviderPermissionId,
  getProviderSessionRuntimeMetadata,
  resolveSessionProviderState,
  useProviderSessionStore,
  type ClaudeSession,
} from "../../stores/providerSessionStore";
import { useCanvasStore } from "../../stores/canvasStore";
import { useThemeStore } from "../../stores/themeStore";
import { useVoiceStore } from "../../stores/voiceStore";
import { useWidgetMetricsStore } from "../../stores/widgetMetricsStore";
import type { ProviderTurnInput, ProviderTurnResult } from "../../contracts/providerRuntime";

type WidgetSessionEventName = "all" | "session" | "message" | "tool-result" | "streaming" | "streaming-text";

const SESSION_EVENT_NAMES = new Set<WidgetSessionEventName>([
  "all",
  "session",
  "message",
  "tool-result",
  "streaming",
  "streaming-text",
]);

export interface WidgetBridgeTraffic {
  inbound: number;
  outbound: number;
  errors: number;
}

type RefCell<T> = { current: T };

type WidgetBridgeRequest = {
  type: string;
  payload?: Record<string, any>;
};

interface UseWidgetBridgeHostOptions {
  widgetId: string;
  enabled: boolean;
  lifecycleKey: string | null;
  post: (message: unknown) => void;
  panelRef: RefCell<HTMLDivElement | null>;
  embeddedBrowserIdRef: RefCell<string | null>;
  disposedRef: RefCell<boolean>;
  lastBoundsRef: RefCell<string>;
  metricsInstanceIdRef: RefCell<string>;
  bridgeTrafficRef: RefCell<WidgetBridgeTraffic>;
  setBrowserActive: (active: boolean) => void;
  syncBrowserBounds: () => void;
}

export interface WidgetBridgeHost {
  handleRequest: (message: unknown) => void;
  sendInit: () => void;
}

function countVoiceSubscriptions(unlisteners: { intent?: () => void; partial?: () => void; final?: () => void }) {
  return Number(Boolean(unlisteners.intent)) + Number(Boolean(unlisteners.partial)) + Number(Boolean(unlisteners.final));
}

function normalizeSessionEventNames(raw: unknown): WidgetSessionEventName[] {
  if (raw == null) return ["all"];
  const values = Array.isArray(raw) ? raw : [raw];
  const events = values.filter((value): value is WidgetSessionEventName => (
    typeof value === "string" && SESSION_EVENT_NAMES.has(value as WidgetSessionEventName)
  ));
  return events.length > 0 ? events : ["all"];
}

function asBridgeRequest(message: unknown): WidgetBridgeRequest | null {
  if (!message || typeof message !== "object") return null;
  const candidate = message as { type?: unknown; payload?: unknown };
  if (typeof candidate.type !== "string") return null;
  return candidate as WidgetBridgeRequest;
}

function stringifyError(err: unknown) {
  return String(err instanceof Error ? err.message : err);
}

/**
 * Build a snapshot of Terminal 64 state that widget transports send on init
 * and in response to t64:request-state.
 */
export function buildWidgetBridgeStateSnapshot() {
  const claude = useProviderSessionStore.getState();
  const theme = useThemeStore.getState();
  const canvas = useCanvasStore.getState();

  const sessions: Record<string, {
    sessionId: string;
    name: string;
    cwd: string;
    provider: ProviderId;
    model: string;
    isStreaming: boolean;
    promptCount: number;
    messageCount: number;
    totalTokens: number;
    totalCost: number;
    mcpServers: { name: string; status: string }[];
  }> = {};

  for (const [sid, s] of Object.entries(claude.sessions)) {
    const providerState = resolveSessionProviderState(s);
    sessions[sid] = {
      sessionId: sid,
      name: s.name,
      cwd: s.cwd,
      provider: providerState.provider,
      model: s.model,
      isStreaming: s.isStreaming,
      promptCount: s.promptCount,
      messageCount: s.messages.length,
      totalTokens: s.totalTokens,
      totalCost: s.totalCost,
      mcpServers: s.mcpServers,
    };
  }

  return {
    sessions,
    activeTerminals: canvas.terminals.map((t) => ({
      id: t.id,
      panelType: t.panelType,
      title: t.title,
      widgetId: t.widgetId,
      terminalId: t.terminalId,
    })),
    theme: {
      name: theme.currentThemeName,
      ui: theme.currentTheme.ui,
      terminal: theme.currentTheme.terminal,
    },
  };
}

function providerTurnForSession(
  sessionId: string,
  session: ClaudeSession,
  prompt: string,
  opts?: { started?: boolean; defaultCodexPermission?: string },
): ProviderTurnInput {
  const providerState = resolveSessionProviderState(session);
  return {
    provider: providerState.provider,
    sessionId,
    cwd: session.cwd || ".",
    prompt,
    started: opts?.started ?? session.hasBeenStarted,
    runtimeMetadata: getProviderSessionRuntimeMetadata(providerState, providerState.provider),
    selectedControls: providerState.selectedControls[providerState.provider] ?? {},
    providerPermissionId: providerState.providerPermissions[providerState.provider]
      ?? opts?.defaultCodexPermission
      ?? getProviderPermissionId(providerState, providerState.provider),
    permissionMode: "auto",
    skipOpenwolf: session.skipOpenwolf,
    seedTranscript: providerState.seedTranscript,
    resumeAtUuid: session.resumeAtUuid ?? null,
    forkParentSessionId: session.forkParentSessionId ?? null,
  };
}

function applyProviderTurnResult(sessionId: string, result: ProviderTurnResult) {
  const store = useProviderSessionStore.getState();
  if (result.clearSeedTranscript) store.clearSeedTranscript(sessionId);
  if (result.clearResumeAtUuid) store.setResumeAtUuid(sessionId, null);
  if (result.clearForkParentSessionId) store.setForkParentSessionId(sessionId, null);
}

function resolveActiveProviderForWidgetBridge(): ProviderId | null {
  const activeId = useCanvasStore.getState().activeTerminalId;
  if (!activeId) return null;
  const activeSession = useProviderSessionStore.getState().sessions[activeId];
  return activeSession ? resolveSessionProviderState(activeSession).provider : null;
}

function resolveCreateSessionProvider(rawProvider: unknown, hasPrompt: boolean): {
  provider: ProviderId;
  providerLocked: boolean;
} {
  if (isProviderId(rawProvider)) {
    return { provider: rawProvider, providerLocked: true };
  }
  if (hasPrompt) {
    return { provider: resolveActiveProviderForWidgetBridge() ?? "anthropic", providerLocked: true };
  }
  return { provider: "anthropic", providerLocked: false };
}

export function useWidgetBridgeHost({
  widgetId,
  enabled,
  lifecycleKey,
  post,
  panelRef,
  embeddedBrowserIdRef,
  disposedRef,
  lastBoundsRef,
  metricsInstanceIdRef,
  bridgeTrafficRef,
  setBrowserActive,
  syncBrowserBounds,
}: UseWidgetBridgeHostOptions): WidgetBridgeHost {
  const voiceUnlistenersRef = useRef<{ intent?: () => void; partial?: () => void; final?: () => void }>({});
  const pluginSubscriptionsRef = useRef<Map<string, PluginStreamHandle>>(new Map());
  const widgetBusSubscriptionsRef = useRef<Set<string>>(new Set());
  const sessionEventSubscriptionsRef = useRef<Set<WidgetSessionEventName>>(new Set());
  const terminalReadyCleanupsRef = useRef<Set<() => void>>(new Set());
  const enabledRef = useRef(enabled);
  const activeBridgeTokenRef = useRef(0);

  useEffect(() => {
    enabledRef.current = enabled;
  }, [enabled]);

  const updateWidgetResourceMetrics = useCallback(() => {
    useWidgetMetricsStore.getState().setResourceCounts(metricsInstanceIdRef.current, {
      pluginSubscriptions: pluginSubscriptionsRef.current.size,
      busSubscriptions: widgetBusSubscriptionsRef.current.size,
      sessionEventSubscriptions: sessionEventSubscriptionsRef.current.size,
      terminalWaiters: terminalReadyCleanupsRef.current.size,
      voiceSubscriptions: countVoiceSubscriptions(voiceUnlistenersRef.current),
    });
  }, [metricsInstanceIdRef]);

  const markEmbeddedBrowserInactive = useCallback(() => {
    embeddedBrowserIdRef.current = null;
    lastBoundsRef.current = "";
    setBrowserActive(false);
    useWidgetMetricsStore.getState().setBrowserState(metricsInstanceIdRef.current, false, null);
  }, [embeddedBrowserIdRef, lastBoundsRef, metricsInstanceIdRef, setBrowserActive]);

  const cleanupBridgeResources = useCallback(() => {
    widgetBus.unsubscribeAll(widgetId);
    widgetBusSubscriptionsRef.current.clear();
    sessionEventSubscriptionsRef.current.clear();
    for (const cleanup of terminalReadyCleanupsRef.current) {
      cleanup();
    }
    terminalReadyCleanupsRef.current.clear();

    const voiceUnlisteners = voiceUnlistenersRef.current;
    try { voiceUnlisteners.intent?.(); } catch { /* ignore */ }
    try { voiceUnlisteners.partial?.(); } catch { /* ignore */ }
    try { voiceUnlisteners.final?.(); } catch { /* ignore */ }
    voiceUnlistenersRef.current = {};

    for (const handle of pluginSubscriptionsRef.current.values()) {
      try { handle.close(); } catch { /* ignore */ }
    }
    pluginSubscriptionsRef.current.clear();
    updateWidgetResourceMetrics();
  }, [updateWidgetResourceMetrics, widgetId]);

  const sendInit = useCallback(() => {
    if (!enabledRef.current) return;
    post({ type: "t64:init", payload: buildWidgetBridgeStateSnapshot() });
  }, [post]);

  useEffect(() => {
    if (!enabled) return;
    const bridgeToken = activeBridgeTokenRef.current + 1;
    activeBridgeTokenRef.current = bridgeToken;
    const pendingStreamingText = new Map<string, string>();
    let streamingTextFlushTimer: ReturnType<typeof setTimeout> | null = null;
    const wantsSessionEvent = (eventName: WidgetSessionEventName) => {
      const subscriptions = sessionEventSubscriptionsRef.current;
      return subscriptions.has("all") || subscriptions.has(eventName);
    };
    const flushStreamingText = () => {
      streamingTextFlushTimer = null;
      for (const [sid, text] of pendingStreamingText.entries()) {
        post({ type: "t64:streaming-text", payload: { sessionId: sid, text } });
      }
      pendingStreamingText.clear();
    };
    const queueStreamingText = (sid: string, text: string) => {
      pendingStreamingText.set(sid, text);
      if (streamingTextFlushTimer) return;
      streamingTextFlushTimer = setTimeout(flushStreamingText, 250);
    };

    const unsub = useProviderSessionStore.subscribe((state, prev) => {
      if (sessionEventSubscriptionsRef.current.size === 0) return;
      for (const [sid, session] of Object.entries(state.sessions)) {
        const prevSession = prev.sessions[sid] as ClaudeSession | undefined;
        if (!prevSession) {
          if (wantsSessionEvent("session")) {
            post({ type: "t64:session-created", payload: { sessionId: sid, name: session.name, cwd: session.cwd } });
          }
          continue;
        }

        if (wantsSessionEvent("session") && session.cwd && session.cwd !== prevSession.cwd) {
          post({ type: "t64:session-cwd-changed", payload: { sessionId: sid, cwd: session.cwd } });
        }

        if (wantsSessionEvent("message") && session.messages.length > prevSession.messages.length) {
          const newMsgs = session.messages.slice(prevSession.messages.length);
          for (const msg of newMsgs) {
            post({
              type: "t64:message",
              payload: {
                sessionId: sid,
                messageId: msg.id,
                role: msg.role,
                content: msg.content,
                toolCalls: msg.toolCalls?.map((tc) => ({
                  id: tc.id,
                  name: tc.name,
                  input: tc.input,
                  result: tc.result,
                  isError: tc.isError,
                })),
              },
            });
          }
        }

        const lastMsg = session.messages[session.messages.length - 1];
        const prevLastMsg = prevSession.messages[prevSession.messages.length - 1];
        if (wantsSessionEvent("tool-result") && lastMsg?.toolCalls && prevLastMsg?.toolCalls && lastMsg.id === prevLastMsg.id) {
          for (let i = 0; i < lastMsg.toolCalls.length; i++) {
            const tc = lastMsg.toolCalls[i];
            const ptc = prevLastMsg.toolCalls[i];
            if (tc && ptc && tc.result !== ptc.result && tc.result !== undefined) {
              post({
                type: "t64:tool-result",
                payload: {
                  sessionId: sid,
                  toolCallId: tc.id,
                  toolName: tc.name,
                  input: tc.input,
                  result: tc.result,
                  isError: tc.isError,
                },
              });
            }
          }
        }

        if (wantsSessionEvent("streaming") && session.isStreaming !== prevSession.isStreaming) {
          post({ type: "t64:streaming", payload: { sessionId: sid, isStreaming: session.isStreaming } });
          const anyStreaming = Object.values(state.sessions).some((s) => s.isStreaming);
          post({ type: "t64:any-streaming", payload: { isStreaming: anyStreaming } });
        }

        if (wantsSessionEvent("streaming-text") && session.streamingText && session.streamingText !== prevSession.streamingText) {
          queueStreamingText(sid, session.streamingText);
        }
      }
    });

    return () => {
      if (activeBridgeTokenRef.current === bridgeToken) {
        activeBridgeTokenRef.current += 1;
      }
      if (streamingTextFlushTimer) {
        clearTimeout(streamingTextFlushTimer);
        streamingTextFlushTimer = null;
      }
      pendingStreamingText.clear();
      unsub();
      cleanupBridgeResources();
    };
  }, [cleanupBridgeResources, enabled, lifecycleKey, post]);

  const handleRequest = useCallback((rawMessage: unknown) => {
    if (!enabledRef.current) return;
    const msg = asBridgeRequest(rawMessage);
    if (!msg) return;
    bridgeTrafficRef.current.inbound += 1;

    switch (msg.type) {
      case "t64:subscribe-session-events": {
        const events = normalizeSessionEventNames(msg.payload?.events ?? msg.payload?.event);
        for (const eventName of events) {
          sessionEventSubscriptionsRef.current.add(eventName);
        }
        updateWidgetResourceMetrics();
        post({ type: "t64:session-events-subscribed", payload: { id: msg.payload?.id, events: Array.from(sessionEventSubscriptionsRef.current) } });
        return;
      }

      case "t64:unsubscribe-session-events": {
        const rawEvents = msg.payload?.events ?? msg.payload?.event;
        if (rawEvents == null || rawEvents === "all") {
          sessionEventSubscriptionsRef.current.clear();
        } else {
          for (const eventName of normalizeSessionEventNames(rawEvents)) {
            if (eventName === "all") sessionEventSubscriptionsRef.current.clear();
            else sessionEventSubscriptionsRef.current.delete(eventName);
          }
        }
        updateWidgetResourceMetrics();
        post({ type: "t64:session-events-unsubscribed", payload: { id: msg.payload?.id, events: Array.from(sessionEventSubscriptionsRef.current) } });
        return;
      }

      case "t64:debug-metrics":
      case "t64:widget-metrics":
        useWidgetMetricsStore.getState().recordWidgetReport(metricsInstanceIdRef.current, msg.payload);
        return;

      case "t64:request-debug-metrics": {
        const currentMetrics = useWidgetMetricsStore.getState().widgets[metricsInstanceIdRef.current] ?? null;
        post({ type: "t64:debug-metrics", payload: { id: msg.payload?.id, metrics: currentMetrics } });
        return;
      }

      case "t64:request-state":
        post({ type: "t64:state", payload: { ...buildWidgetBridgeStateSnapshot(), id: msg.payload?.id } });
        return;

      case "t64:plugin": {
        const action = msg.payload?.action;
        const reqId = msg.payload?.id;
        const targetPlugin = typeof msg.payload?.pluginId === "string"
          ? msg.payload.pluginId
          : widgetId;
        if (action === "invoke") {
          const method = msg.payload?.method;
          if (typeof method !== "string") {
            post({ type: "t64:plugin-result", payload: { id: reqId, ok: false, error: "method required" } });
            return;
          }
          invokePlugin(targetPlugin, method, msg.payload?.args)
            .then((result) => post({ type: "t64:plugin-result", payload: { id: reqId, ok: true, result } }))
            .catch((err) => post({ type: "t64:plugin-result", payload: { id: reqId, ok: false, error: stringifyError(err) } }));
          return;
        }
        if (action === "subscribe") {
          const topic = typeof msg.payload?.topic === "string" ? msg.payload.topic : "*";
          const subId = typeof reqId === "string" ? reqId : Math.random().toString(36).slice(2);
          const existing = pluginSubscriptionsRef.current.get(subId);
          if (existing) {
            existing.close();
          }
          const handle = onPluginEvent(targetPlugin, topic, (frame) => {
            post({ type: "t64:plugin-event", payload: { id: subId, pluginId: targetPlugin, frame } });
          });
          pluginSubscriptionsRef.current.set(subId, handle);
          updateWidgetResourceMetrics();
          post({ type: "t64:plugin-subscribed", payload: { id: subId } });
          return;
        }
        if (action === "unsubscribe") {
          const subId = typeof reqId === "string" ? reqId : "";
          const handle = pluginSubscriptionsRef.current.get(subId);
          if (handle) {
            handle.close();
            pluginSubscriptionsRef.current.delete(subId);
            updateWidgetResourceMetrics();
          }
          return;
        }
        post({ type: "t64:plugin-result", payload: { id: reqId, ok: false, error: `unknown plugin action: ${String(action)}` } });
        return;
      }

      case "t64:pick-directory": {
        const reqId = msg.payload?.id;
        openDialog({ directory: true, title: "Select project directory" }).then((dir) => {
          post({ type: "t64:directory-picked", payload: { id: reqId, path: dir || null } });
        }).catch(() => {
          post({ type: "t64:directory-picked", payload: { id: reqId, path: null } });
        });
        return;
      }

      case "t64:pick-file": {
        const reqId = msg.payload?.id;
        const title = typeof msg.payload?.title === "string" ? msg.payload.title : "Select file";
        const filters = Array.isArray(msg.payload?.filters) ? msg.payload.filters : undefined;
        const options = filters === undefined
          ? { directory: false, multiple: false, title }
          : { directory: false, multiple: false, title, filters };
        openDialog(options).then((path) => {
          post({ type: "t64:file-picked", payload: { id: reqId, path: path || null } });
        }).catch(() => {
          post({ type: "t64:file-picked", payload: { id: reqId, path: null } });
        });
        return;
      }

      case "t64:open-url": {
        const url = msg.payload?.url;
        if (url && typeof url === "string") {
          useCanvasStore.getState().addBrowserPanel(url, msg.payload?.title);
        }
        return;
      }

      case "t64:embed-browser": {
        const url = msg.payload?.url;
        if (!url || typeof url !== "string") return;
        const el = panelRef.current;
        if (!el) return;
        const rect = el.getBoundingClientRect();
        const inset = 6;
        const bid = `wdg-browser-${widgetId}`;
        if (embeddedBrowserIdRef.current) {
          navigateBrowser(bid, url).catch(() => {});
        } else {
          embeddedBrowserIdRef.current = bid;
          createBrowser(bid, url, rect.x + inset, rect.y, rect.width - inset * 2, rect.height - inset)
            .then(() => {
              if (disposedRef.current || embeddedBrowserIdRef.current !== bid) {
                closeBrowser(bid).catch(() => {});
                return;
              }
              setBrowserActive(true);
              syncBrowserBounds();
            })
            .catch((err) => {
              if (disposedRef.current) return;
              console.warn("[widget] Failed to create embedded browser:", err);
              markEmbeddedBrowserInactive();
            });
        }
        post({ type: "t64:browser-ready", payload: { browserId: bid } });
        return;
      }

      case "t64:navigate-browser": {
        const url = msg.payload?.url;
        if (embeddedBrowserIdRef.current && url && typeof url === "string") {
          navigateBrowser(embeddedBrowserIdRef.current, url).catch(() => {});
        }
        return;
      }

      case "t64:close-browser":
        if (embeddedBrowserIdRef.current) {
          closeBrowser(embeddedBrowserIdRef.current).catch(() => {});
          markEmbeddedBrowserInactive();
        }
        return;

      case "t64:show-browser":
        if (embeddedBrowserIdRef.current) setBrowserVisible(embeddedBrowserIdRef.current, true).catch(() => {});
        return;

      case "t64:hide-browser":
        if (embeddedBrowserIdRef.current) setBrowserVisible(embeddedBrowserIdRef.current, false).catch(() => {});
        return;

      case "t64:eval-browser": {
        const js = msg.payload?.js;
        if (embeddedBrowserIdRef.current && js && typeof js === "string") {
          browserEval(embeddedBrowserIdRef.current, js).catch(() => {});
        }
        return;
      }

      case "t64:get-bounds": {
        const { id: gbId } = msg.payload || {};
        const panel = useCanvasStore.getState().terminals.find(
          (t) => t.panelType === "widget" && t.widgetId === widgetId
        );
        if (panel) {
          post({ type: "t64:bounds", payload: { id: gbId, x: panel.x, y: panel.y, width: panel.width, height: panel.height } });
        }
        return;
      }

      case "t64:open-file": {
        const { path: filePath } = msg.payload || {};
        if (filePath && typeof filePath === "string") {
          readFile(filePath)
            .then(() => {
              window.dispatchEvent(new CustomEvent("t64-open-file", { detail: { path: filePath } }));
            })
            .catch(() => {
              post({ type: "t64:open-file-error", payload: { path: filePath, error: "File not found" } });
            });
        }
        return;
      }

      case "t64:exec": {
        const { command, cwd: execCwd, id: execId } = msg.payload || {};
        if (!command || typeof command !== "string") return;
        shellExec(command, execCwd || undefined)
          .then((result) => post({ type: "t64:exec-result", payload: { id: execId, ...result } }))
          .catch((err) => post({ type: "t64:exec-result", payload: { id: execId, stdout: "", stderr: String(err), code: -1 } }));
        return;
      }

      case "t64:read-file": {
        const { path, id: rfId } = msg.payload || {};
        if (!path || typeof path !== "string") return;
        readFile(path)
          .then((content) => post({ type: "t64:file-content", payload: { id: rfId, path, content, error: null } }))
          .catch((err) => post({ type: "t64:file-content", payload: { id: rfId, path, content: null, error: String(err) } }));
        return;
      }

      case "t64:write-file": {
        const { path, content, id: wfId } = msg.payload || {};
        if (!path || typeof path !== "string" || typeof content !== "string") return;
        writeFile(path, content)
          .then(() => post({ type: "t64:file-written", payload: { id: wfId, path, error: null } }))
          .catch((err) => post({ type: "t64:file-written", payload: { id: wfId, path, error: String(err) } }));
        return;
      }

      case "t64:list-dir": {
        const { path, id: ldId } = msg.payload || {};
        if (!path || typeof path !== "string") return;
        listDirectory(path)
          .then((entries) => post({ type: "t64:dir-listing", payload: { id: ldId, path, entries, error: null } }))
          .catch((err) => post({ type: "t64:dir-listing", payload: { id: ldId, path, entries: null, error: String(err) } }));
        return;
      }

      case "t64:search-files": {
        const { cwd: sfCwd, query, id: sfId } = msg.payload || {};
        if (!sfCwd || !query) return;
        searchFiles(sfCwd, query)
          .then((results) => post({ type: "t64:search-results", payload: { id: sfId, results, error: null } }))
          .catch((err) => post({ type: "t64:search-results", payload: { id: sfId, results: null, error: String(err) } }));
        return;
      }

      case "t64:delete-files": {
        const { paths, id: dfId } = msg.payload || {};
        if (!Array.isArray(paths)) return;
        deleteFiles(paths)
          .then(() => post({ type: "t64:files-deleted", payload: { id: dfId, error: null } }))
          .catch((err) => post({ type: "t64:files-deleted", payload: { id: dfId, error: String(err) } }));
        return;
      }

      case "t64:create-terminal": {
        const { cwd: termCwd, id: ctId, x: termX, y: termY, width: termW, height: termH, title: termTitle } = msg.payload || {};
        const newTerm = useCanvasStore.getState().addTerminal(
          termX ?? undefined, termY ?? undefined, termCwd || undefined,
          termW ?? undefined, termH ?? undefined, termTitle ?? undefined,
        );
        const tid = newTerm.terminalId;
        let responded = false;
        const respond = () => {
          if (responded) return;
          responded = true;
          post({ type: "t64:terminal-created", payload: { id: ctId, terminalId: tid } });
        };
        let finished = false;
        let unlisten: (() => void) | null = null;
        let timeout: ReturnType<typeof setTimeout> | null = null;
        const finish = (shouldRespond: boolean) => {
          if (finished) return;
          finished = true;
          if (timeout) clearTimeout(timeout);
          if (unlisten) unlisten();
          terminalReadyCleanupsRef.current.delete(cancel);
          updateWidgetResourceMetrics();
          if (shouldRespond) respond();
        };
        const cancel = () => finish(false);
        terminalReadyCleanupsRef.current.add(cancel);
        updateWidgetResourceMetrics();
        timeout = setTimeout(() => finish(true), 3000);
        onTerminalOutput((out) => {
          if (out.id === tid) finish(true);
        }).then((stopListening) => {
          if (finished) {
            stopListening();
            return;
          }
          unlisten = stopListening;
        }).catch(() => finish(true));
        return;
      }

      case "t64:write-terminal": {
        const { terminalId, data } = msg.payload || {};
        if (!terminalId || typeof data !== "string") return;
        writeTerminal(terminalId, data).catch(() => {});
        return;
      }

      case "t64:close-terminal": {
        const { terminalId: closeTid } = msg.payload || {};
        if (!closeTid || typeof closeTid !== "string") return;
        closeTerminal(closeTid).catch(() => {});
        const panel = useCanvasStore.getState().terminals.find((t) => t.terminalId === closeTid);
        if (panel) useCanvasStore.getState().removeTerminal(panel.id);
        return;
      }

      case "t64:send-prompt": {
        const { sessionId, prompt, id: spId } = msg.payload || {};
        if (!sessionId || !prompt) return;
        const sess = useProviderSessionStore.getState().sessions[sessionId];
        if (!sess) { post({ type: "t64:prompt-sent", payload: { id: spId, error: "Session not found" } }); return; }
        runProviderTurn(providerTurnForSession(sessionId, sess, prompt, { defaultCodexPermission: "full-auto" }))
          .then((result) => {
            applyProviderTurnResult(sessionId, result);
            post({ type: "t64:prompt-sent", payload: { id: spId, error: null } });
          })
          .catch((err) => post({ type: "t64:prompt-sent", payload: { id: spId, error: String(err) } }));
        return;
      }

      case "t64:create-session": {
        const {
          cwd: sessCwd,
          name: sessName,
          prompt: sessPrompt,
          id: csId,
          provider: rawProvider,
          x: sessX,
          y: sessY,
          width: sessW,
          height: sessH,
        } = msg.payload || {};
        const hasPrompt = typeof sessPrompt === "string" && sessPrompt.length > 0;
        const { provider, providerLocked } = resolveCreateSessionProvider(rawProvider, hasPrompt);
        const x = typeof sessX === "number" && Number.isFinite(sessX) ? sessX : undefined;
        const y = typeof sessY === "number" && Number.isFinite(sessY) ? sessY : undefined;
        const width = typeof sessW === "number" && Number.isFinite(sessW) ? sessW : undefined;
        const height = typeof sessH === "number" && Number.isFinite(sessH) ? sessH : undefined;
        const panel = useCanvasStore.getState().addClaudeTerminalAt(
          sessCwd || ".", false, sessName || "Widget Session", undefined, x, y, width, height
        );
        const sid = panel.terminalId;
        useProviderSessionStore.getState().createSession(
          sid,
          sessName || "Widget Session",
          false,
          true,
          sessCwd || ".",
          provider,
          providerLocked,
        );
        if (hasPrompt) {
          setTimeout(() => {
            const store = useProviderSessionStore.getState();
            const createdSession = store.sessions[sid];
            if (!createdSession) return;
            store.addUserMessage(sid, sessPrompt);
            store.incrementPromptCount(sid);
            runProviderTurn(providerTurnForSession(sid, createdSession, sessPrompt, {
              started: false,
              defaultCodexPermission: "full-auto",
            })).then((result) => {
              applyProviderTurnResult(sid, result);
            }).catch((err) => {
              useProviderSessionStore.getState().setError(sid, `Failed to start session: ${err}`);
            });
          }, 300);
        }
        post({ type: "t64:session-spawned", payload: { id: csId, sessionId: sid } });
        return;
      }

      case "t64:fetch": {
        const { url, method, headers: hdrs, body: fetchBody, id: fetchId } = msg.payload || {};
        if (!url || typeof url !== "string") return;
        proxyFetch(url, method, hdrs, fetchBody)
          .then((result) => post({ type: "t64:fetch-result", payload: { id: fetchId, ...result, error: null } }))
          .catch((err) => post({ type: "t64:fetch-result", payload: { id: fetchId, status: 0, ok: false, headers: {}, body: "", is_base64: false, error: String(err) } }));
        return;
      }

      case "t64:get-state": {
        const { key, id: gsId } = msg.payload || {};
        widgetGetState(widgetId, key || undefined)
          .then((value) => post({ type: "t64:state-value", payload: { id: gsId, key, value, error: null } }))
          .catch((err) => post({ type: "t64:state-value", payload: { id: gsId, key, value: null, error: String(err) } }));
        return;
      }

      case "t64:set-state": {
        const { key, value, id: ssId } = msg.payload || {};
        if (!key || typeof key !== "string") return;
        widgetSetState(widgetId, key, value)
          .then(() => post({ type: "t64:state-saved", payload: { id: ssId, error: null } }))
          .catch((err) => post({ type: "t64:state-saved", payload: { id: ssId, error: String(err) } }));
        return;
      }

      case "t64:clear-state": {
        const { id: csId2 } = msg.payload || {};
        widgetClearState(widgetId)
          .then(() => post({ type: "t64:state-cleared", payload: { id: csId2, error: null } }))
          .catch((err) => post({ type: "t64:state-cleared", payload: { id: csId2, error: String(err) } }));
        return;
      }

      case "t64:notify": {
        const { title, body: notifBody, id: nId } = msg.payload || {};
        if (!title || typeof title !== "string") return;
        pushToast(title.slice(0, 256), notifBody ? String(notifBody).slice(0, 1024) : undefined);
        post({ type: "t64:notify-result", payload: { id: nId, error: null } });
        return;
      }

      case "t64:openwolf:switch": {
        const { cwd: owCwd, id: owSwId } = msg.payload || {};
        if (!owCwd || typeof owCwd !== "string") {
          post({ type: "t64:openwolf:switched", payload: { id: owSwId, error: "cwd required" } });
          return;
        }
        openwolfDaemonSwitch(owCwd)
          .then(() => post({ type: "t64:openwolf:switched", payload: { id: owSwId, cwd: owCwd, error: null } }))
          .catch((err) => post({ type: "t64:openwolf:switched", payload: { id: owSwId, cwd: owCwd, error: String(err) } }));
        return;
      }

      case "t64:openwolf:info": {
        const { id: owInfoId } = msg.payload || {};
        openwolfDaemonInfo()
          .then((info) => post({ type: "t64:openwolf:info-result", payload: { id: owInfoId, info, error: null } }))
          .catch((err) => post({ type: "t64:openwolf:info-result", payload: { id: owInfoId, info: null, error: String(err) } }));
        return;
      }

      case "t64:openwolf:stop": {
        const { id: owStopId } = msg.payload || {};
        openwolfDaemonStopAll()
          .then(() => post({ type: "t64:openwolf:stopped", payload: { id: owStopId, error: null } }))
          .catch((err) => post({ type: "t64:openwolf:stopped", payload: { id: owStopId, error: String(err) } }));
        return;
      }

      case "t64:subscribe": {
        const { topic } = msg.payload || {};
        if (!topic || typeof topic !== "string") return;
        widgetBus.subscribe(topic, widgetId, (data) => {
          post({ type: "t64:broadcast", payload: { topic, data } });
        });
        widgetBusSubscriptionsRef.current.add(topic);
        updateWidgetResourceMetrics();
        return;
      }

      case "t64:unsubscribe": {
        const { topic } = msg.payload || {};
        if (!topic || typeof topic !== "string") return;
        widgetBus.unsubscribe(topic, widgetId);
        widgetBusSubscriptionsRef.current.delete(topic);
        updateWidgetResourceMetrics();
        return;
      }

      case "t64:broadcast": {
        const { topic, data } = msg.payload || {};
        if (!topic || typeof topic !== "string") return;
        widgetBus.broadcast(topic, data, widgetId);
        return;
      }

      case "t64:voice:start": {
        const { id: vsId } = msg.payload || {};
        useVoiceStore.getState().setEnabled(true);
        startVoiceCapture()
          .then(() => post({ type: "t64:voice:started", payload: { id: vsId, error: null } }))
          .catch((err) => post({ type: "t64:voice:started", payload: { id: vsId, error: String(err) } }));
        return;
      }

      case "t64:voice:stop": {
        const { id: vstopId } = msg.payload || {};
        useVoiceStore.getState().setEnabled(false);
        stopVoiceCapture()
          .then(() => post({ type: "t64:voice:stopped", payload: { id: vstopId, error: null } }))
          .catch((err) => post({ type: "t64:voice:stopped", payload: { id: vstopId, error: String(err) } }));
        return;
      }

      case "t64:voice:status": {
        const { id: vstId } = msg.payload || {};
        const vs = useVoiceStore.getState();
        post({
          type: "t64:voice:status-result",
          payload: {
            id: vstId,
            enabled: vs.enabled,
            state: vs.state,
            lastIntent: vs.lastIntent,
            partial: vs.partial,
            error: vs.error,
            modelsDownloaded: vs.modelsDownloaded,
            activeSessionId: vs.activeSessionId,
          },
        });
        return;
      }

      case "t64:voice:on-intent": {
        if (voiceUnlistenersRef.current.intent) return;
        const bridgeToken = activeBridgeTokenRef.current;
        onVoiceIntent((payload) => {
          post({ type: "t64:voice:intent", payload });
        }).then((un) => {
          if (activeBridgeTokenRef.current !== bridgeToken || !enabledRef.current) { un(); return; }
          voiceUnlistenersRef.current.intent = un;
          updateWidgetResourceMetrics();
        }).catch(() => {});
        return;
      }

      case "t64:voice:on-partial": {
        if (voiceUnlistenersRef.current.partial) return;
        const bridgeToken = activeBridgeTokenRef.current;
        onVoicePartial((payload) => {
          post({ type: "t64:voice:partial", payload });
        }).then((un) => {
          if (activeBridgeTokenRef.current !== bridgeToken || !enabledRef.current) { un(); return; }
          voiceUnlistenersRef.current.partial = un;
          updateWidgetResourceMetrics();
        }).catch(() => {});
        return;
      }

      case "t64:voice:on-final": {
        if (voiceUnlistenersRef.current.final) return;
        const bridgeToken = activeBridgeTokenRef.current;
        onVoiceFinal((payload) => {
          post({ type: "t64:voice:final", payload });
        }).then((un) => {
          if (activeBridgeTokenRef.current !== bridgeToken || !enabledRef.current) { un(); return; }
          voiceUnlistenersRef.current.final = un;
          updateWidgetResourceMetrics();
        }).catch(() => {});
        return;
      }

      default:
        break;
    }

    if (msg.type === "t64:request-messages") {
      const sid = msg.payload?.sessionId;
      const session = useProviderSessionStore.getState().sessions[sid];
      if (session) {
        post({
          type: "t64:messages",
          payload: {
            sessionId: sid,
            messages: session.messages.map((m) => ({
              id: m.id,
              role: m.role,
              content: m.content,
              toolCalls: m.toolCalls?.map((tc) => ({
                id: tc.id,
                name: tc.name,
                input: tc.input,
                result: tc.result,
                isError: tc.isError,
              })),
            })),
          },
        });
      }
    }
  }, [
    bridgeTrafficRef,
    disposedRef,
    embeddedBrowserIdRef,
    lastBoundsRef,
    markEmbeddedBrowserInactive,
    metricsInstanceIdRef,
    panelRef,
    post,
    setBrowserActive,
    syncBrowserBounds,
    updateWidgetResourceMetrics,
    widgetId,
  ]);

  return { handleRequest, sendInit };
}
