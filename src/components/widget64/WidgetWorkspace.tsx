import { useEffect, useMemo, useRef, useState, type CSSProperties, type PointerEvent as ReactPointerEvent, type WheelEvent } from "react";
import WidgetPanel from "../widget/WidgetPanel";
import { useCanvasStore, type CanvasTerminal } from "../../stores/canvasStore";
import { isTauriRuntime } from "../../lib/runtime";
import "./WidgetWorkspace.css";

export type WorkspaceView = "canvas" | "overview" | "gallery";

interface WidgetWorkspaceProps {
  view: WorkspaceView;
  onViewChange: (view: WorkspaceView) => void;
  onOpenManager: () => void;
}

interface LayoutFrame {
  x: number;
  y: number;
  scale: number;
}

interface ViewportSize {
  width: number;
  height: number;
}

const OVERVIEW_EDGE = 48;
const OVERVIEW_GAP = 26;
const GALLERY_GAP = 34;

function bestOverviewLayout(widgets: CanvasTerminal[], viewport: ViewportSize): Map<string, LayoutFrame> {
  const result = new Map<string, LayoutFrame>();
  if (widgets.length === 0 || viewport.width <= 0 || viewport.height <= 0) return result;

  const availableWidth = Math.max(240, viewport.width - OVERVIEW_EDGE * 2);
  const availableHeight = Math.max(180, viewport.height - OVERVIEW_EDGE * 2);
  let winner: { columns: number; rows: number; score: number } | null = null;

  for (let columns = 1; columns <= widgets.length; columns += 1) {
    const rows = Math.ceil(widgets.length / columns);
    const cellWidth = availableWidth / columns;
    const cellHeight = availableHeight / rows;
    const scales = widgets.map((widget) => Math.min(
      1,
      Math.max(0.08, (cellWidth - OVERVIEW_GAP) / widget.width),
      Math.max(0.08, (cellHeight - OVERVIEW_GAP) / widget.height),
    ));
    const minimumScale = Math.min(...scales);
    const averageScale = scales.reduce((sum, scale) => sum + scale, 0) / scales.length;
    const emptyCells = columns * rows - widgets.length;
    const score = minimumScale * 10 + averageScale - emptyCells * 0.015;
    if (!winner || score > winner.score) winner = { columns, rows, score };
  }

  const columns = winner?.columns ?? 1;
  const rows = winner?.rows ?? 1;
  const cellWidth = availableWidth / columns;
  const cellHeight = availableHeight / rows;
  widgets.forEach((widget, index) => {
    const row = Math.floor(index / columns);
    const column = index % columns;
    const itemsInRow = Math.min(columns, widgets.length - row * columns);
    const rowOffset = (columns - itemsInRow) * cellWidth * 0.5;
    const scale = Math.min(
      1,
      Math.max(0.08, (cellWidth - OVERVIEW_GAP) / widget.width),
      Math.max(0.08, (cellHeight - OVERVIEW_GAP) / widget.height),
    );
    result.set(widget.id, {
      x: OVERVIEW_EDGE + rowOffset + column * cellWidth + (cellWidth - widget.width * scale) / 2,
      y: OVERVIEW_EDGE + row * cellHeight + (cellHeight - widget.height * scale) / 2,
      scale,
    });
  });
  return result;
}

function galleryLayout(widgets: CanvasTerminal[], viewport: ViewportSize) {
  const result = new Map<string, LayoutFrame>();
  const maxHeight = Math.max(220, viewport.height - 124);
  let cursor = 56;

  widgets.forEach((widget) => {
    const scale = Math.min(1, maxHeight / widget.height);
    result.set(widget.id, {
      x: cursor,
      y: Math.max(42, (viewport.height - widget.height * scale) / 2),
      scale,
    });
    cursor += widget.width * scale + GALLERY_GAP;
  });

  return { frames: result, width: Math.max(viewport.width, cursor + 22) };
}

function FrameMenuIcon({ kind }: { kind: "close" | "resize" }) {
  if (kind === "close") {
    return <svg viewBox="0 0 12 12" aria-hidden="true"><path d="m3 3 6 6M9 3 3 9" /></svg>;
  }
  return <svg viewBox="0 0 12 12" aria-hidden="true"><path d="M3 9h6V3M5.5 9 9 5.5" /></svg>;
}

function BrowserWidgetPreview({ widgetId }: { widgetId: string }) {
  const variant = widgetId.includes("focus") ? "focus" : widgetId.includes("signal") ? "signal" : "metrics";
  if (variant === "focus") {
    return (
      <div className="browser-widget browser-widget--focus">
        <p>Focus</p>
        <strong>42:18</strong>
        <span>Design review</span>
        <button type="button">Pause session</button>
      </div>
    );
  }
  if (variant === "signal") {
    return (
      <div className="browser-widget browser-widget--signal">
        <div><p>System signal</p><span>Live</span></div>
        <strong>All services nominal</strong>
        <ul><li><span>API</span><i /></li><li><span>Queue</span><i /></li><li><span>Database</span><i /></li></ul>
      </div>
    );
  }
  return (
    <div className="browser-widget browser-widget--metrics">
      <div><p>Build velocity</p><span>Last 7 days</span></div>
      <strong>94.8%</strong>
      <div className="browser-widget__chart" aria-hidden="true"><i /><i /><i /><i /><i /><i /><i /></div>
      <small>+8.4% from previous period</small>
    </div>
  );
}

function WidgetFrame({
  widget,
  view,
  frame,
  onFocus,
}: {
  widget: CanvasTerminal;
  view: WorkspaceView;
  frame: LayoutFrame;
  onFocus: (widget: CanvasTerminal) => void;
}) {
  const moveTerminal = useCanvasStore((state) => state.moveTerminal);
  const resizeTerminal = useCanvasStore((state) => state.resizeTerminal);
  const removeTerminal = useCanvasStore((state) => state.removeTerminal);
  const bringToFront = useCanvasStore((state) => state.bringToFront);
  const zoom = useCanvasStore((state) => state.zoom);
  const [interacting, setInteracting] = useState<"drag" | "resize" | null>(null);

  const startDrag = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (view !== "canvas" || event.button !== 0 || (event.target as HTMLElement).closest("button")) return;
    event.preventDefault();
    bringToFront(widget.id);
    setInteracting("drag");
    const origin = { clientX: event.clientX, clientY: event.clientY, x: widget.x, y: widget.y };
    const onMove = (moveEvent: PointerEvent) => {
      moveTerminal(
        widget.id,
        origin.x + (moveEvent.clientX - origin.clientX) / zoom,
        origin.y + (moveEvent.clientY - origin.clientY) / zoom,
      );
    };
    const onUp = () => {
      setInteracting(null);
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp, { once: true });
  };

  const startResize = (event: ReactPointerEvent<HTMLButtonElement>) => {
    if (view !== "canvas" || event.button !== 0) return;
    event.preventDefault();
    event.stopPropagation();
    bringToFront(widget.id);
    setInteracting("resize");
    const origin = { clientX: event.clientX, clientY: event.clientY, width: widget.width, height: widget.height };
    const onMove = (moveEvent: PointerEvent) => {
      resizeTerminal(
        widget.id,
        origin.width + (moveEvent.clientX - origin.clientX) / zoom,
        origin.height + (moveEvent.clientY - origin.clientY) / zoom,
      );
    };
    const onUp = () => {
      setInteracting(null);
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp, { once: true });
  };

  const style = {
    width: widget.width,
    height: widget.height,
    zIndex: view === "canvas" ? widget.zIndex : 1,
    transform: `translate3d(${frame.x}px, ${frame.y}px, 0) scale(${frame.scale})`,
  } satisfies CSSProperties;

  return (
    <article
      className={`widget-frame${interacting ? " is-interacting" : ""}`}
      style={style}
      onPointerDown={() => view === "canvas" && bringToFront(widget.id)}
      aria-label={`${widget.title} widget`}
    >
      <div className="widget-frame__bar" onPointerDown={startDrag} onDoubleClick={() => view !== "canvas" && onFocus(widget)}>
        <span className="widget-frame__identity">
          <span className="widget-frame__glyph" aria-hidden="true"><i /><i /><i /><i /></span>
          <span className="widget-frame__title">{widget.title}</span>
        </span>
        <span className="widget-frame__size">{Math.round(widget.width)} × {Math.round(widget.height)}</span>
        <button
          type="button"
          className="widget-frame__close"
          onClick={(event) => {
            event.stopPropagation();
            removeTerminal(widget.id);
          }}
          aria-label={`Remove ${widget.title} from workspace`}
          title="Remove from workspace"
        >
          <FrameMenuIcon kind="close" />
        </button>
      </div>
      <div className="widget-frame__content">
        {isTauriRuntime
          ? <WidgetPanel widgetId={widget.widgetId ?? widget.title} />
          : <BrowserWidgetPreview widgetId={widget.widgetId ?? widget.title} />}
      </div>
      {view === "overview" && (
        <button className="widget-frame__focus" type="button" onClick={() => onFocus(widget)} aria-label={`Focus ${widget.title}`} />
      )}
      {view === "canvas" && (
        <button className="widget-frame__resize" type="button" onPointerDown={startResize} aria-label={`Resize ${widget.title}`}>
          <FrameMenuIcon kind="resize" />
        </button>
      )}
    </article>
  );
}

export default function WidgetWorkspace({ view, onViewChange, onOpenManager }: WidgetWorkspaceProps) {
  const allPanels = useCanvasStore((state) => state.terminals);
  const panX = useCanvasStore((state) => state.panX);
  const panY = useCanvasStore((state) => state.panY);
  const zoom = useCanvasStore((state) => state.zoom);
  const pan = useCanvasStore((state) => state.pan);
  const setZoom = useCanvasStore((state) => state.setZoom);
  const zoomAtPoint = useCanvasStore((state) => state.zoomAtPoint);
  const centerView = useCanvasStore((state) => state.centerView);
  const surfaceRef = useRef<HTMLDivElement | null>(null);
  const [viewport, setViewport] = useState<ViewportSize>({ width: 0, height: 0 });
  const [transitioning, setTransitioning] = useState(false);
  const [panning, setPanning] = useState(false);
  const seededDemo = useRef(false);
  const widgets = useMemo(
    () => allPanels.filter((panel) => panel.panelType === "widget" && !panel.poppedOut),
    [allPanels],
  );

  useEffect(() => {
    if (!import.meta.env.DEV || seededDemo.current || !new URLSearchParams(window.location.search).has("demo")) return;
    seededDemo.current = true;
    const store = useCanvasStore.getState();
    if (store.terminals.some((panel) => panel.panelType === "widget")) return;
    const metrics = store.addWidgetTerminal("metrics", "Build velocity");
    store.setTerminalFrame(metrics.id, 80, 80, 660, 340);
    const focus = store.addWidgetTerminal("focus", "Focus timer");
    store.setTerminalFrame(focus.id, 790, 70, 380, 560);
    const signal = store.addWidgetTerminal("signal", "System signal");
    store.setTerminalFrame(signal.id, 180, 470, 500, 420);
  }, []);

  useEffect(() => {
    const surface = surfaceRef.current;
    if (!surface) return;
    const update = () => setViewport({ width: surface.clientWidth, height: surface.clientHeight });
    update();
    const observer = new ResizeObserver(update);
    observer.observe(surface);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    setTransitioning(true);
    const timeout = window.setTimeout(() => setTransitioning(false), 440);
    if (view !== "gallery" && surfaceRef.current) surfaceRef.current.scrollLeft = 0;
    return () => window.clearTimeout(timeout);
  }, [view]);

  const overview = useMemo(() => bestOverviewLayout(widgets, viewport), [widgets, viewport]);
  const gallery = useMemo(() => galleryLayout(widgets, viewport), [widgets, viewport]);

  const frameFor = (widget: CanvasTerminal): LayoutFrame => {
    if (view === "overview") return overview.get(widget.id) ?? { x: 0, y: 0, scale: 1 };
    if (view === "gallery") return gallery.frames.get(widget.id) ?? { x: 0, y: 0, scale: 1 };
    return { x: widget.x * zoom + panX, y: widget.y * zoom + panY, scale: zoom };
  };

  const handleWheel = (event: WheelEvent<HTMLDivElement>) => {
    if (view === "gallery") {
      if (Math.abs(event.deltaY) > Math.abs(event.deltaX) && surfaceRef.current) {
        surfaceRef.current.scrollLeft += event.deltaY;
      }
      return;
    }
    if (view !== "canvas") return;
    event.preventDefault();
    if (event.ctrlKey || event.metaKey) {
      const rect = surfaceRef.current?.getBoundingClientRect();
      const pointX = event.clientX - (rect?.left ?? 0);
      const pointY = event.clientY - (rect?.top ?? 0);
      const nextZoom = zoom * Math.exp(-event.deltaY * 0.006);
      zoomAtPoint(nextZoom, pointX, pointY);
    } else {
      pan(-event.deltaX, -event.deltaY);
    }
  };

  const startPan = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (view !== "canvas" || event.button !== 0 || (event.target as HTMLElement).closest(".widget-frame, button")) return;
    event.preventDefault();
    setPanning(true);
    const origin = { x: event.clientX, y: event.clientY };
    let last = origin;
    const onMove = (moveEvent: PointerEvent) => {
      pan(moveEvent.clientX - last.x, moveEvent.clientY - last.y);
      last = { x: moveEvent.clientX, y: moveEvent.clientY };
    };
    const onUp = () => {
      setPanning(false);
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp, { once: true });
  };

  const focusWidget = (widget: CanvasTerminal) => {
    const targetZoom = Math.min(1.15, Math.max(0.65, Math.min(
      (viewport.width - 160) / widget.width,
      (viewport.height - 140) / widget.height,
    )));
    setZoom(targetZoom);
    const state = useCanvasStore.getState();
    const nextPanX = (viewport.width - widget.width * targetZoom) / 2 - widget.x * targetZoom;
    const nextPanY = (viewport.height - widget.height * targetZoom) / 2 - widget.y * targetZoom;
    state.pan(nextPanX - state.panX, nextPanY - state.panY);
    state.bringToFront(widget.id);
    onViewChange("canvas");
  };

  const canvasBackground = view === "canvas" ? {
    backgroundPosition: `${panX}px ${panY}px`,
    backgroundSize: `${24 * zoom}px ${24 * zoom}px`,
  } : undefined;

  return (
    <section
      ref={surfaceRef}
      className={`workspace workspace--${view}${transitioning ? " is-transitioning" : ""}${panning ? " is-panning" : ""}`}
      style={canvasBackground}
      onWheel={handleWheel}
      onPointerDown={startPan}
    >
      <div className="workspace__plane" style={{ width: view === "gallery" ? gallery.width : "100%" }}>
        {widgets.map((widget) => (
          <WidgetFrame key={widget.id} widget={widget} view={view} frame={frameFor(widget)} onFocus={focusWidget} />
        ))}
      </div>

      {widgets.length === 0 && (
        <div className="empty-workspace">
          <span className="empty-workspace__mark" aria-hidden="true"><i /><i /><i /><i /></span>
          <p className="empty-workspace__eyebrow">Your canvas is clear</p>
          <h1>Make a space for what matters.</h1>
          <p className="empty-workspace__copy">Every widget starts as a standalone Tauri app and fits here without modification.</p>
          <button type="button" onClick={onOpenManager}>
            <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M8 3v10M3 8h10" /></svg>
            Create your first widget
          </button>
        </div>
      )}

      {view === "canvas" && widgets.length > 0 && (
        <div className="canvas-controls" aria-label="Canvas controls">
          <button type="button" onClick={() => setZoom(zoom - 0.1)} aria-label="Zoom out">−</button>
          <span>{Math.round(zoom * 100)}%</span>
          <button type="button" onClick={() => setZoom(zoom + 0.1)} aria-label="Zoom in">+</button>
          <i />
          <button type="button" className="canvas-controls__fit" onClick={() => centerView(viewport.width, viewport.height)}>Fit</button>
        </div>
      )}

      <div className="workspace-status" aria-live="polite">
        <span>{widgets.length} {widgets.length === 1 ? "widget" : "widgets"}</span>
        <i />
        <span>{view === "canvas" ? "Scroll to move · Pinch to zoom" : view === "overview" ? "Double-click a widget to focus" : "Scroll sideways to browse"}</span>
      </div>
    </section>
  );
}
