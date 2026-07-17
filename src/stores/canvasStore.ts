import { create } from "zustand";
import { v4 as uuidv4 } from "uuid";
import {
  DEFAULT_BORDER_COLOR,
  DEFAULT_TERMINAL_WIDTH,
  DEFAULT_TERMINAL_HEIGHT,
  MIN_TERMINAL_WIDTH,
  MIN_TERMINAL_HEIGHT,
} from "../lib/constants";
import type { SnapGuide } from "../lib/snapUtils";

const CANVAS_STORAGE_KEY = "widget64-workspace-v1";
const LEGACY_CANVAS_STORAGE_KEY = "terminal64-session";
const CANVAS_SAVE_DEBOUNCE_MS = 1000;

export type PanelType = "terminal" | "claude" | "shared-chat" | "widget" | "browser";

export interface CanvasTerminal {
  id: string;
  terminalId: string;
  x: number;
  y: number;
  width: number;
  height: number;
  zIndex: number;
  title: string;
  borderColor: string;
  poppedOut: boolean;
  cwd: string;
  panelType: PanelType;
  claudeSkipPermissions: boolean;
  widgetId?: string;
  browserUrl?: string;
}

/** Get the center of the current viewport in canvas-space coordinates. */
function getViewportCenter(state: { panX: number; panY: number; zoom: number }): { x: number; y: number } {
  const el = document.querySelector(".canvas");
  const rect = el?.getBoundingClientRect() ?? { width: window.innerWidth, height: window.innerHeight };
  return {
    x: (rect.width / 2 - state.panX) / state.zoom,
    y: (rect.height / 2 - state.panY) / state.zoom,
  };
}

const SPAWN_GAP = 20;

/** Push a rect away from all existing windows using AABB separation. */
function findNonOverlappingPosition(
  x: number, y: number, w: number, h: number,
  existing: CanvasTerminal[],
): { x: number; y: number } {
  const rects = existing.filter((t) => !t.poppedOut);
  let nx = x, ny = y;

  for (let iter = 0; iter < 25; iter++) {
    let overlapper: CanvasTerminal | null = null;
    for (const r of rects) {
      if (nx < r.x + r.width + SPAWN_GAP && nx + w + SPAWN_GAP > r.x &&
          ny < r.y + r.height + SPAWN_GAP && ny + h + SPAWN_GAP > r.y) {
        overlapper = r;
        break;
      }
    }
    if (!overlapper) break;

    const r = overlapper;
    // Compute push distance in each cardinal direction
    const options = [
      { dist: Math.abs((r.x + r.width + SPAWN_GAP) - nx), dx: (r.x + r.width + SPAWN_GAP) - nx, dy: 0 },
      { dist: Math.abs((r.x - SPAWN_GAP - w) - nx), dx: (r.x - SPAWN_GAP - w) - nx, dy: 0 },
      { dist: Math.abs((r.y + r.height + SPAWN_GAP) - ny), dx: 0, dy: (r.y + r.height + SPAWN_GAP) - ny },
      { dist: Math.abs((r.y - SPAWN_GAP - h) - ny), dx: 0, dy: (r.y - SPAWN_GAP - h) - ny },
    ];
    options.sort((a, b) => a.dist - b.dist);
    const best = options[0]!;
    nx += best.dx;
    ny += best.dy;
  }

  return { x: nx, y: ny };
}

interface CanvasState {
  terminals: CanvasTerminal[];
  panX: number;
  panY: number;
  zoom: number;
  nextZ: number;
  activeTerminalId: string | null;
  focusedTerminalId: string | null;
  snapGuides: SnapGuide[];

  addTerminal: (x?: number, y?: number, cwd?: string, width?: number, height?: number, title?: string) => CanvasTerminal;
  addClaudeTerminal: (cwd: string, skipPermissions: boolean, sessionName?: string, existingSessionId?: string) => void;
  addClaudeTerminalAt: (cwd: string, skipPermissions: boolean, sessionName?: string, existingSessionId?: string, x?: number, y?: number, width?: number, height?: number) => CanvasTerminal;
  addWidgetTerminal: (widgetId: string, widgetName?: string) => CanvasTerminal;
  addSharedChatPanel: (groupId: string, x: number, y: number, width: number, height: number) => CanvasTerminal;
  addBrowserPanel: (url: string, title?: string) => CanvasTerminal;
  removeTerminal: (id: string) => void;
  moveTerminal: (id: string, x: number, y: number) => void;
  resizeTerminal: (id: string, width: number, height: number) => void;
  setTerminalFrame: (id: string, x: number, y: number, width: number, height: number) => void;
  bringToFront: (id: string) => void;
  setTitle: (id: string, title: string) => void;
  setCwd: (id: string, cwd: string) => void;
  setBorderColor: (id: string, color: string) => void;
  popOut: (id: string) => void;
  popIn: (terminalId: string) => void;
  setActive: (id: string) => void;
  toggleFocusedTerminal: (terminalId: string) => void;
  clearFocusedTerminal: () => void;
  setSnapGuides: (guides: SnapGuide[]) => void;
  clearSnapGuides: () => void;
  pan: (dx: number, dy: number) => void;
  setZoom: (zoom: number) => void;
  zoomAtPoint: (newZoom: number, cx: number, cy: number) => void;
  centerView: (viewportW: number, viewportH: number) => void;
  saveSession: () => void;
}

interface SerializedTerminal {
  x?: number;
  y?: number;
  width?: number;
  height?: number;
  title?: string;
  borderColor?: string;
  cwd?: string;
  panelType?: PanelType;
  terminalId?: string;
  claudeSkipPermissions?: boolean;
  widgetId?: string;
  browserUrl?: string;
}

interface SerializedCanvasSession {
  terminals: SerializedTerminal[];
  panX: number;
  panY: number;
  zoom: number;
}

let lastSavedSessionJson: string | null = null;

function deserializeTerminals(items: SerializedTerminal[]): CanvasTerminal[] {
  return items.map((t, i) => {
    const panelType: PanelType = t.panelType ?? "terminal";
    const terminalId = (panelType !== "terminal" && t.terminalId) ? t.terminalId : uuidv4();
    return {
      id: uuidv4(),
      terminalId,
      x: t.x ?? 60,
      y: t.y ?? 60,
      width: t.width ?? DEFAULT_TERMINAL_WIDTH,
      height: t.height ?? DEFAULT_TERMINAL_HEIGHT,
      zIndex: i + 1,
      title: t.title ?? "Terminal",
      borderColor: t.borderColor ?? DEFAULT_BORDER_COLOR,
      poppedOut: false,
      cwd: t.cwd ?? "",
      panelType,
      claudeSkipPermissions: t.claudeSkipPermissions ?? false,
      ...(t.widgetId !== undefined && { widgetId: t.widgetId }),
      ...(t.browserUrl !== undefined && { browserUrl: t.browserUrl }),
    };
  });
}

function makeTerminal(zIndex: number, overrides: Partial<CanvasTerminal> = {}): CanvasTerminal {
  return {
    id: uuidv4(),
    terminalId: uuidv4(),
    x: 60,
    y: 60,
    width: DEFAULT_TERMINAL_WIDTH,
    height: DEFAULT_TERMINAL_HEIGHT,
    zIndex,
    title: "Terminal",
    borderColor: DEFAULT_BORDER_COLOR,
    poppedOut: false,
    cwd: "",
    panelType: "terminal",
    claudeSkipPermissions: false,
    ...overrides,
  };
}

// Load saved session at init time (before any components mount)
function getInitialState() {
  try {
    const current = localStorage.getItem(CANVAS_STORAGE_KEY);
    const raw = current ?? localStorage.getItem(LEGACY_CANVAS_STORAGE_KEY);
    if (raw) {
      if (current) lastSavedSessionJson = raw;
      const session = JSON.parse(raw);
      if (session.terminals?.length) {
        const terminals = deserializeTerminals(session.terminals).filter(
          (panel) => panel.panelType === "widget" && panel.widgetId,
        );
        if (terminals.length > 0) {
          return {
            terminals,
            panX: session.panX ?? 0,
            panY: session.panY ?? 0,
            zoom: session.zoom ?? 1,
            nextZ: terminals.length + 1,
            activeTerminalId: terminals[0]?.terminalId ?? null,
            focusedTerminalId: null,
            snapGuides: [],
          };
        }
      }
    }
  } catch (e) {
    console.warn("[canvasStore] Failed to load session from localStorage:", e);
  }

  return {
    terminals: [],
    panX: 0,
    panY: 0,
    zoom: 1,
    nextZ: 1,
    activeTerminalId: null,
    focusedTerminalId: null,
    snapGuides: [],
  };
}

let dirty = false;
let saveTimer: ReturnType<typeof setTimeout> | null = null;
let idleSaveHandle: number | null = null;

function scheduleIdle(callback: () => void): number {
  const win = window as Window & {
    requestIdleCallback?: (cb: IdleRequestCallback, opts?: IdleRequestOptions) => number;
  };
  if (typeof win.requestIdleCallback === "function") {
    return win.requestIdleCallback(callback, { timeout: 1500 });
  }
  return window.setTimeout(callback, 0);
}

function cancelIdle(handle: number) {
  const win = window as Window & {
    cancelIdleCallback?: (handle: number) => void;
  };
  if (typeof win.cancelIdleCallback === "function") {
    win.cancelIdleCallback(handle);
  } else {
    clearTimeout(handle);
  }
}

export const useCanvasStore = create<CanvasState>((set, get) => {
  const initial = getInitialState();

  const flushScheduledSave = () => {
    saveTimer = null;
    if (!dirty) return;
    if (idleSaveHandle !== null) return;
    idleSaveHandle = scheduleIdle(() => {
      idleSaveHandle = null;
      if (!dirty) return;
      try {
        useCanvasStore.getState().saveSession();
        dirty = false;
      } catch (e) {
        console.warn("[canvasStore] Auto-save failed:", e);
      }
    });
  };

  // Clean up on HMR to avoid stacking intervals
  if (import.meta.hot) {
    import.meta.hot.dispose(() => {
      if (saveTimer) {
        clearTimeout(saveTimer);
        saveTimer = null;
      }
      if (idleSaveHandle !== null) {
        cancelIdle(idleSaveHandle);
        idleSaveHandle = null;
      }
    });
  }

  const markDirty = () => {
    dirty = true;
    if (saveTimer) clearTimeout(saveTimer);
    saveTimer = setTimeout(flushScheduledSave, CANVAS_SAVE_DEBOUNCE_MS);
  };

  /** Shared helper: position a new panel, add it to state, mark dirty. */
  function addPanel(
    overrides: Partial<CanvasTerminal>,
    opts?: { x?: number | undefined; y?: number | undefined; width?: number | undefined; height?: number | undefined; setActive?: boolean },
  ): CanvasTerminal {
    const state = get();
    const w = opts?.width || overrides.width || DEFAULT_TERMINAL_WIDTH;
    const h = opts?.height || overrides.height || DEFAULT_TERMINAL_HEIGHT;
    let px: number, py: number;
    if (opts?.x != null && opts?.y != null) {
      px = opts.x; py = opts.y;
    } else {
      const vc = getViewportCenter(state);
      px = vc.x - w / 2;
      py = vc.y - h / 2;
    }
    const pos = findNonOverlappingPosition(px, py, w, h, state.terminals);
    const newTerm = makeTerminal(state.nextZ, { ...overrides, x: pos.x, y: pos.y, width: w, height: h });
    set({
      terminals: [...state.terminals, newTerm],
      nextZ: state.nextZ + 1,
      ...(opts?.setActive !== false ? { activeTerminalId: newTerm.terminalId } : {}),
    });
    markDirty();
    return newTerm;
  }

  return {
    ...initial,

    addTerminal: (x?: number, y?: number, cwd?: string, width?: number, height?: number, title?: string) => {
      return addPanel(
        { ...(cwd ? { cwd } : {}), ...(title ? { title } : {}) },
        { x, y, width, height },
      );
    },

    addClaudeTerminal: (cwd: string, skipPermissions: boolean, sessionName?: string, existingSessionId?: string) => {
      get().addClaudeTerminalAt(cwd, skipPermissions, sessionName, existingSessionId);
    },

    addClaudeTerminalAt: (cwd, skipPermissions, sessionName, existingSessionId, x, y, width, height) => {
      return addPanel({
        title: sessionName || "Claude",
        borderColor: "#cba6f7",
        cwd,
        panelType: "claude",
        claudeSkipPermissions: skipPermissions,
        ...(existingSessionId ? { terminalId: existingSessionId } : {}),
      }, { x, y, width, height });
    },

    addWidgetTerminal: (widgetId, widgetName) => {
      return addPanel({
        title: widgetName || widgetId,
        borderColor: "#78a7ff",
        panelType: "widget",
        widgetId,
      }, { width: 500, height: 400 });
    },

    addSharedChatPanel: (groupId, x, y, width, height) => {
      return addPanel({
        title: "Team Chat",
        borderColor: "#94e2d5",
        cwd: "",
        panelType: "shared-chat",
        terminalId: `shared-chat-${groupId}`,
      }, { x, y, width, height, setActive: false });
    },

    addBrowserPanel: (url, title) => {
      return addPanel({
        title: title || "Browser",
        borderColor: "#89b4fa",
        panelType: "browser",
        terminalId: `browser-${uuidv4().slice(0, 8)}`,
        browserUrl: url,
      }, { width: 900, height: 600 });
    },

    removeTerminal: (id: string) => {
      set((s) => {
        const removed = s.terminals.find((t) => t.id === id);
        const newTerminals = s.terminals.filter((t) => t.id !== id);
        return {
          terminals: newTerminals,
          activeTerminalId:
            removed?.terminalId === s.activeTerminalId
              ? newTerminals[newTerminals.length - 1]?.terminalId ?? null
              : s.activeTerminalId,
          focusedTerminalId:
            removed?.terminalId === s.focusedTerminalId
              ? null
              : s.focusedTerminalId,
        };
      });
      markDirty();
    },

    moveTerminal: (id: string, x: number, y: number) => {
      const nx = Math.round(x);
      const ny = Math.round(y);
      const current = get().terminals.find((t) => t.id === id);
      if (!current || (current.x === nx && current.y === ny)) return;
      set((s) => ({
        terminals: s.terminals.map((t) =>
          t.id === id ? { ...t, x: nx, y: ny } : t
        ),
      }));
      markDirty();
    },

    resizeTerminal: (id: string, width: number, height: number) => {
      const nextWidth = Math.round(Math.max(MIN_TERMINAL_WIDTH, width));
      const nextHeight = Math.round(Math.max(MIN_TERMINAL_HEIGHT, height));
      const current = get().terminals.find((t) => t.id === id);
      if (!current || (current.width === nextWidth && current.height === nextHeight)) return;
      set((s) => ({
        terminals: s.terminals.map((t) =>
          t.id === id
            ? {
                ...t,
                width: nextWidth,
                height: nextHeight,
              }
            : t
        ),
      }));
      markDirty();
    },

    setTerminalFrame: (id: string, x: number, y: number, width: number, height: number) => {
      const nx = Math.round(x);
      const ny = Math.round(y);
      const nextWidth = Math.round(Math.max(MIN_TERMINAL_WIDTH, width));
      const nextHeight = Math.round(Math.max(MIN_TERMINAL_HEIGHT, height));
      const current = get().terminals.find((t) => t.id === id);
      if (
        !current ||
        (current.x === nx &&
          current.y === ny &&
          current.width === nextWidth &&
          current.height === nextHeight)
      ) {
        return;
      }
      set((s) => ({
        terminals: s.terminals.map((t) =>
          t.id === id
            ? { ...t, x: nx, y: ny, width: nextWidth, height: nextHeight }
            : t
        ),
      }));
      markDirty();
    },

    bringToFront: (id: string) => {
      const state = get();
      const term = state.terminals.find((t) => t.id === id);
      if (!term || term.zIndex === state.nextZ - 1) return; // Already on top

      // Rebalance z-indices when they get too high to prevent rendering issues
      if (state.nextZ > 10000) {
        const sorted = [...state.terminals].sort((a, b) => a.zIndex - b.zIndex);
        const zMap = new Map<string, number>();
        sorted.forEach((t, i) => zMap.set(t.id, i + 1));
        // The target panel gets the top slot
        zMap.set(id, sorted.length + 1);
        set({
          terminals: state.terminals.map((t) => ({
            ...t,
            zIndex: zMap.get(t.id)!,
          })),
          nextZ: sorted.length + 2,
          activeTerminalId: term.terminalId,
        });
      } else {
        set({
          terminals: state.terminals.map((t) =>
            t.id === id ? { ...t, zIndex: state.nextZ } : t
          ),
          nextZ: state.nextZ + 1,
          activeTerminalId: term.terminalId,
        });
      }
      markDirty();
    },

    setTitle: (id: string, title: string) => {
      set((s) => ({
        terminals: s.terminals.map((t) =>
          t.id === id ? { ...t, title } : t
        ),
      }));
      markDirty();
    },

    setCwd: (id: string, cwd: string) => {
      const current = get().terminals.find((t) => t.id === id);
      if (current?.cwd === cwd) return; // No-op if unchanged
      set((s) => ({
        terminals: s.terminals.map((t) =>
          t.id === id ? { ...t, cwd } : t
        ),
      }));
      markDirty();
    },

    setBorderColor: (id: string, color: string) => {
      set((s) => ({
        terminals: s.terminals.map((t) =>
          t.id === id ? { ...t, borderColor: color } : t
        ),
      }));
      markDirty();
    },

    popOut: (id: string) => {
      set((s) => ({
        terminals: s.terminals.map((t) =>
          t.id === id ? { ...t, poppedOut: true } : t
        ),
      }));
      markDirty();
    },

    popIn: (terminalId: string) => {
      set((s) => ({
        terminals: s.terminals.map((t) =>
          t.terminalId === terminalId ? { ...t, poppedOut: false } : t
        ),
      }));
      markDirty();
    },

    setActive: (id: string) => {
      set({ activeTerminalId: id });
    },

    toggleFocusedTerminal: (terminalId: string) => {
      set((s) => ({
        focusedTerminalId: s.focusedTerminalId === terminalId ? null : terminalId,
        activeTerminalId: terminalId,
      }));
    },

    clearFocusedTerminal: () => {
      set({ focusedTerminalId: null });
    },

    setSnapGuides: (guides: SnapGuide[]) => {
      set({ snapGuides: guides });
    },

    clearSnapGuides: () => {
      set({ snapGuides: [] });
    },

    pan: (dx: number, dy: number) => {
      set((s) => ({ panX: s.panX + dx, panY: s.panY + dy }));
      markDirty();
    },

    setZoom: (zoom: number) => {
      set({ zoom: Math.max(0.1, Math.min(5, zoom)) });
      markDirty();
    },

    zoomAtPoint: (newZoom: number, cx: number, cy: number) => {
      const s = get();
      const clamped = Math.max(0.1, Math.min(5, newZoom));
      const ratio = clamped / s.zoom;
      set({
        zoom: clamped,
        panX: cx - (cx - s.panX) * ratio,
        panY: cy - (cy - s.panY) * ratio,
      });
      markDirty();
    },

    centerView: (viewportW: number, viewportH: number) => {
      const terms = get().terminals.filter((t) => !t.poppedOut);
      if (terms.length === 0) return;
      // Compute bounding box of all visible terminals
      let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
      for (const t of terms) {
        minX = Math.min(minX, t.x);
        minY = Math.min(minY, t.y);
        maxX = Math.max(maxX, t.x + t.width);
        maxY = Math.max(maxY, t.y + t.height);
      }
      const contentW = maxX - minX;
      const contentH = maxY - minY;
      if (contentW <= 0 || contentH <= 0) return;
      const pad = 40; // padding around content
      const zoom = Math.max(0.1, Math.min(1, Math.min(
        (viewportW - pad * 2) / contentW,
        (viewportH - pad * 2) / contentH,
      )));
      const panX = (viewportW - contentW * zoom) / 2 - minX * zoom;
      const panY = (viewportH - contentH * zoom) / 2 - minY * zoom;
      set({ panX, panY, zoom });
      markDirty();
    },

    saveSession: () => {
      const s = get();
      const session: SerializedCanvasSession = {
        terminals: s.terminals
          .filter((t) => !t.poppedOut)
          .map((t) => ({
            x: t.x,
            y: t.y,
            width: t.width,
            height: t.height,
            title: t.title,
            borderColor: t.borderColor,
            cwd: t.cwd,
            panelType: t.panelType,
            claudeSkipPermissions: t.claudeSkipPermissions,
            ...(t.panelType !== "terminal" ? { terminalId: t.terminalId } : {}),
            ...(t.widgetId ? { widgetId: t.widgetId } : {}),
            ...(t.browserUrl ? { browserUrl: t.browserUrl } : {}),
          })),
        panX: s.panX,
        panY: s.panY,
        zoom: s.zoom,
      };
      try {
        const startedAt = performance.now();
        const json = JSON.stringify(session);
        if (json === lastSavedSessionJson) return;
        localStorage.setItem(CANVAS_STORAGE_KEY, json);
        lastSavedSessionJson = json;
        const elapsed = performance.now() - startedAt;
        if (elapsed > 25) {
          console.warn(`[canvasStore] saveSession took ${Math.round(elapsed)}ms for ${json.length} bytes`);
        }
      } catch (e) {
        console.warn("[canvasStore] Failed to save session:", e);
      }
    },
  };
});
