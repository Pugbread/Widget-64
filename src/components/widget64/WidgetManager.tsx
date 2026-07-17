import { useEffect, useMemo, useRef, useState } from "react";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { toast } from "sonner";
import {
  deleteWidgetFolder,
  installWidgetZip,
  listWidgetFolders,
  openWidgetFolder,
  scaffoldWidgetProject,
} from "../../lib/tauriApi";
import type { WidgetInfo } from "../../lib/types";
import { isTauriRuntime } from "../../lib/runtime";
import { useCanvasStore } from "../../stores/canvasStore";
import "./WidgetManager.css";

interface WidgetManagerProps {
  open: boolean;
  onClose: () => void;
}

function normalizeWidgetId(value: string) {
  return value.trim().toLowerCase().replace(/[^a-z0-9-_]/g, "-").replace(/-+/g, "-").replace(/^-|-$/g, "");
}

function WidgetTileIcon() {
  return (
    <span className="manager-widget-icon" aria-hidden="true">
      <i /><i /><i /><i />
    </span>
  );
}

function openOnCanvas(widgetId: string, title?: string) {
  const store = useCanvasStore.getState();
  const existing = store.terminals.find((panel) => panel.panelType === "widget" && panel.widgetId === widgetId);
  if (existing) {
    store.bringToFront(existing.id);
    return existing;
  }
  return store.addWidgetTerminal(widgetId, title);
}

export default function WidgetManager({ open, onClose }: WidgetManagerProps) {
  const [name, setName] = useState("");
  const [widgets, setWidgets] = useState<WidgetInfo[]>([]);
  const [loading, setLoading] = useState(false);
  const [creating, setCreating] = useState(false);
  const [dragging, setDragging] = useState(false);
  const [createdPath, setCreatedPath] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement | null>(null);
  const widgetId = useMemo(() => normalizeWidgetId(name), [name]);

  const refresh = () => {
    setLoading(true);
    listWidgetFolders()
      .then(setWidgets)
      .catch((error) => toast.error("Could not load widgets", { description: String(error) }))
      .finally(() => setLoading(false));
  };

  useEffect(() => {
    if (!open) return;
    setName("");
    setCreatedPath(null);
    if (isTauriRuntime) {
      refresh();
    } else {
      const previewWidgets = useCanvasStore.getState().terminals
        .filter((panel) => panel.panelType === "widget" && panel.widgetId)
        .map((panel) => ({ widget_id: panel.widgetId!, has_index: true, modified: 0 }));
      setWidgets(Array.from(new Map(previewWidgets.map((widget) => [widget.widget_id, widget])).values()));
    }
    const focusTimer = window.setTimeout(() => inputRef.current?.focus(), 120);
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => {
      window.clearTimeout(focusTimer);
      window.removeEventListener("keydown", onKeyDown);
    };
  }, [open, onClose]);

  useEffect(() => {
    if (!open || !isTauriRuntime) return;
    let unlisten: (() => void) | undefined;
    getCurrentWebviewWindow().onDragDropEvent(async (event) => {
      if (event.payload.type === "enter") {
        setDragging(event.payload.paths.some((path) => path.toLowerCase().endsWith(".zip")));
        return;
      }
      if (event.payload.type === "leave") {
        setDragging(false);
        return;
      }
      if (event.payload.type !== "drop") return;
      setDragging(false);
      const archives = event.payload.paths.filter((path) => path.toLowerCase().endsWith(".zip"));
      for (const archive of archives) {
        try {
          const installedId = await installWidgetZip(archive);
          openOnCanvas(installedId);
          toast.success("Widget installed", { description: installedId });
        } catch (error) {
          toast.error("Install failed", { description: String(error) });
        }
      }
      if (archives.length > 0) refresh();
    }).then((dispose) => { unlisten = dispose; }).catch(() => {});
    return () => unlisten?.();
  }, [open]);

  if (!open) return null;

  const createWidget = async () => {
    if (!widgetId || creating) return;
    setCreating(true);
    setCreatedPath(null);
    try {
      const path = await scaffoldWidgetProject(widgetId, name.trim());
      openOnCanvas(widgetId, name.trim());
      setCreatedPath(path);
      setName("");
      refresh();
      toast.success("Widget ready", { description: "Tauri project and AGENTS.md created." });
    } catch (error) {
      toast.error("Could not create widget", { description: String(error) });
    } finally {
      setCreating(false);
    }
  };

  const removeWidget = async (widget: WidgetInfo) => {
    const confirmed = window.confirm(`Delete “${widget.widget_id}” and all files in its widget folder?`);
    if (!confirmed) return;
    try {
      await deleteWidgetFolder(widget.widget_id);
      const store = useCanvasStore.getState();
      store.terminals
        .filter((panel) => panel.panelType === "widget" && panel.widgetId === widget.widget_id)
        .forEach((panel) => store.removeTerminal(panel.id));
      refresh();
      toast.success("Widget deleted");
    } catch (error) {
      toast.error("Could not delete widget", { description: String(error) });
    }
  };

  return (
    <div className="manager-backdrop" onMouseDown={(event) => event.target === event.currentTarget && onClose()}>
      <section className="manager-dialog" role="dialog" aria-modal="true" aria-labelledby="widget-manager-title">
        <header className="manager-header">
          <div>
            <p>Workspace library</p>
            <h2 id="widget-manager-title">Widgets</h2>
          </div>
          <button className="manager-close" type="button" onClick={onClose} aria-label="Close widget manager">
            <svg viewBox="0 0 14 14" aria-hidden="true"><path d="m3.5 3.5 7 7m0-7-7 7" /></svg>
          </button>
        </header>

        <div className="manager-body">
          <section className="manager-create" aria-labelledby="create-widget-title">
            <div className="manager-section-heading">
              <div>
                <h3 id="create-widget-title">Create a widget</h3>
                <p>A complete Vite + Tauri project, ready here and standalone.</p>
              </div>
              <span className="manager-runtime-badge"><i />Tauri 2</span>
            </div>
            <div className="manager-create-row">
              <label>
                <span>Widget name</span>
                <input
                  ref={inputRef}
                  value={name}
                  onChange={(event) => setName(event.target.value)}
                  onKeyDown={(event) => event.key === "Enter" && createWidget()}
                  placeholder="Build monitor"
                  autoComplete="off"
                />
              </label>
              <button type="button" onClick={createWidget} disabled={!widgetId || creating}>
                {creating ? "Creating…" : "Create widget"}
              </button>
            </div>
            <div className="manager-scaffold-note">
              <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M8 2.5 13 5.25v5.5L8 13.5l-5-2.75v-5.5zM3.3 5.5 8 8l4.7-2.5M8 8v5.2" /></svg>
              <span><strong>LLM-ready by default.</strong> New folders include `AGENTS.md`, the Widget 64 bridge contract, and standalone fallbacks.</span>
            </div>
            {createdPath && (
              <button className="manager-created-path" type="button" onClick={() => openWidgetFolder(createdPath).catch(() => {})}>
                <span>Created at</span>
                <code>{createdPath}</code>
                <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M6 3H3.5A1.5 1.5 0 0 0 2 4.5v8A1.5 1.5 0 0 0 3.5 14h8a1.5 1.5 0 0 0 1.5-1.5V10M9 2h5v5M8 8l6-6" /></svg>
              </button>
            )}
          </section>

          <section className="manager-library" aria-labelledby="installed-widgets-title">
            <div className="manager-section-heading">
              <div>
                <h3 id="installed-widgets-title">Installed</h3>
                <p>{loading ? "Reading your library…" : `${widgets.length} ${widgets.length === 1 ? "widget" : "widgets"} available`}</p>
              </div>
              <span className="manager-folder-hint">~/.terminal64/widgets</span>
            </div>

            {widgets.length === 0 && !loading ? (
              <div className="manager-library-empty">Create your first widget above, or drop a `.zip` here to install one.</div>
            ) : (
              <div className="manager-grid">
                {widgets.map((widget) => (
                  <article key={widget.widget_id} className="manager-tile">
                    <button
                      className="manager-tile__open"
                      type="button"
                      onClick={() => {
                        openOnCanvas(widget.widget_id);
                        onClose();
                      }}
                    >
                      <WidgetTileIcon />
                      <span className="manager-tile__copy">
                        <strong>{widget.widget_id}</strong>
                        <small>{widget.has_index ? "Ready to run" : "Missing index.html"}</small>
                      </span>
                      <svg className="manager-tile__arrow" viewBox="0 0 16 16" aria-hidden="true"><path d="m6 3 5 5-5 5" /></svg>
                    </button>
                    <button className="manager-tile__delete" type="button" onClick={() => removeWidget(widget)} aria-label={`Delete ${widget.widget_id}`}>
                      <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3.5 5h9M6 2.5h4l.5 2.5h-5zM5 7.5v5m3-5v5m3-5v5M4.25 5l.5 9h6.5l.5-9" /></svg>
                    </button>
                  </article>
                ))}
              </div>
            )}
          </section>
        </div>

        <div className={dragging ? "manager-dropzone is-active" : "manager-dropzone"}>
          <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 3v12m0-12L7.5 7.5M12 3l4.5 4.5M4 14v5a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-5" /></svg>
          <strong>Drop widget archive</strong>
          <span>Release to install the `.zip`</span>
        </div>
      </section>
    </div>
  );
}
