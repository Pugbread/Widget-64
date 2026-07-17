import { useEffect, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { Toaster } from "sonner";
import WidgetManager from "./components/widget64/WidgetManager";
import WidgetWorkspace, { type WorkspaceView } from "./components/widget64/WidgetWorkspace";
import { isTauriRuntime } from "./lib/runtime";
import "./App.css";

const VIEW_STORAGE_KEY = "widget64-active-view";

const viewOptions: Array<{ id: WorkspaceView; label: string; shortcut: string }> = [
  { id: "canvas", label: "Canvas", shortcut: "1" },
  { id: "overview", label: "Overview", shortcut: "2" },
  { id: "gallery", label: "Gallery", shortcut: "3" },
];

function initialView(): WorkspaceView {
  const stored = localStorage.getItem(VIEW_STORAGE_KEY);
  return stored === "overview" || stored === "gallery" ? stored : "canvas";
}

function ViewIcon({ view }: { view: WorkspaceView }) {
  if (view === "canvas") {
    return (
      <svg viewBox="0 0 16 16" aria-hidden="true">
        <path d="M3 5.5V3h2.5M10.5 3H13v2.5M13 10.5V13h-2.5M5.5 13H3v-2.5" />
        <rect x="5.25" y="5.25" width="5.5" height="5.5" rx="1" />
      </svg>
    );
  }
  if (view === "overview") {
    return (
      <svg viewBox="0 0 16 16" aria-hidden="true">
        <rect x="2.5" y="3" width="4.25" height="4" rx=".75" />
        <rect x="8.5" y="3" width="5" height="4" rx=".75" />
        <rect x="2.5" y="9" width="5" height="4" rx=".75" />
        <rect x="9.25" y="9" width="4.25" height="4" rx=".75" />
      </svg>
    );
  }
  return (
    <svg viewBox="0 0 16 16" aria-hidden="true">
      <rect x="1.75" y="3.25" width="4.5" height="9.5" rx="1" />
      <rect x="7.25" y="3.25" width="7" height="9.5" rx="1" />
    </svg>
  );
}

function App() {
  const [view, setViewState] = useState<WorkspaceView>(initialView);
  const [managerOpen, setManagerOpen] = useState(false);
  const [isMaximized, setIsMaximized] = useState(false);

  const setView = (nextView: WorkspaceView) => {
    setViewState(nextView);
    localStorage.setItem(VIEW_STORAGE_KEY, nextView);
  };

  useEffect(() => {
    const handleKeyDown = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const isTyping = target?.matches("input, textarea, [contenteditable='true']");
      if (isTyping || event.metaKey || event.ctrlKey || event.altKey) return;
      if (event.key === "1") setView("canvas");
      if (event.key === "2") setView("overview");
      if (event.key === "3") setView("gallery");
      if (event.key.toLowerCase() === "n") setManagerOpen(true);
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, []);

  const minimize = () => {
    if (isTauriRuntime) getCurrentWindow().minimize().catch(() => {});
  };
  const maximize = async () => {
    if (!isTauriRuntime) return;
    const appWindow = getCurrentWindow();
    await appWindow.toggleMaximize().catch(() => {});
    setIsMaximized(await appWindow.isMaximized().catch(() => false));
  };
  const close = () => {
    if (isTauriRuntime) getCurrentWindow().close().catch(() => {});
  };

  return (
    <main className="app-shell">
      <header className="titlebar">
        <div className="brand" data-tauri-drag-region>
          <span className="brand-mark" aria-hidden="true">
            <i />
            <i />
            <i />
            <i />
          </span>
          <span className="brand-name">Widget 64</span>
          <span className="brand-edition">spatial runtime</span>
        </div>

        <nav className="view-switcher" aria-label="Workspace view">
          {viewOptions.map((option) => (
            <button
              key={option.id}
              type="button"
              className={view === option.id ? "view-switcher__button is-active" : "view-switcher__button"}
              onClick={() => setView(option.id)}
              aria-pressed={view === option.id}
              title={`${option.label} (${option.shortcut})`}
            >
              <ViewIcon view={option.id} />
              <span>{option.label}</span>
            </button>
          ))}
        </nav>

        <div className="titlebar-drag" data-tauri-drag-region />

        <button className="new-widget-button" type="button" onClick={() => setManagerOpen(true)}>
          <svg viewBox="0 0 16 16" aria-hidden="true">
            <path d="M8 3v10M3 8h10" />
          </svg>
          <span>New widget</span>
          <kbd>N</kbd>
        </button>

        <div className="window-controls" aria-label="Window controls">
          <button type="button" onClick={minimize} aria-label="Minimize window">
            <svg viewBox="0 0 12 12" aria-hidden="true"><path d="M2 6.5h8" /></svg>
          </button>
          <button type="button" onClick={maximize} aria-label={isMaximized ? "Restore window" : "Maximize window"}>
            <svg viewBox="0 0 12 12" aria-hidden="true">
              {isMaximized ? <path d="M4 2.5h5.5V8M2.5 4H8v5.5H2.5z" /> : <rect x="2.5" y="2.5" width="7" height="7" rx=".5" />}
            </svg>
          </button>
          <button className="window-close" type="button" onClick={close} aria-label="Close window">
            <svg viewBox="0 0 12 12" aria-hidden="true"><path d="m3 3 6 6M9 3 3 9" /></svg>
          </button>
        </div>
      </header>

      <WidgetWorkspace view={view} onViewChange={setView} onOpenManager={() => setManagerOpen(true)} />
      <WidgetManager open={managerOpen} onClose={() => setManagerOpen(false)} />
      <Toaster position="bottom-right" theme="dark" richColors closeButton />
    </main>
  );
}

export default App;
