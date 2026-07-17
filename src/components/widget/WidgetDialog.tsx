import { useState, useEffect, useRef } from "react";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { listWidgetFolders, createWidgetFolder, deleteWidgetFolder, installWidgetZip, installBundledWidget, readWidgetManifest, readWidgetApproval, writeWidgetApproval } from "../../lib/tauriApi";
import { useCanvasStore } from "../../stores/canvasStore";
import { useProviderSessionStore } from "../../stores/providerSessionStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { pushToast } from "../../lib/notifications";
import { formatRelativeTime, openSystemFolder } from "../../lib/constants";
import type { WidgetInfo } from "../../lib/types";
import {
  validateManifest,
  requiresReconsent,
  permissionNames,
  labelForPermission,
  type PluginManifest,
  type ApprovalRecord,
} from "../../lib/pluginManifest";
import "./Widget.css";

interface WidgetDialogProps {
  isOpen: boolean;
  onClose: () => void;
}

interface ConsentTarget {
  widgetId: string;
  manifest: PluginManifest;
  manifestHash: string;
}

export default function WidgetDialog({ isOpen, onClose }: WidgetDialogProps) {
  const [widgets, setWidgets] = useState<WidgetInfo[]>([]);
  const [name, setName] = useState("");
  const [creating, setCreating] = useState(false);
  const [isDragOver, setIsDragOver] = useState(false);
  const [installing, setInstalling] = useState(false);
  const [consent, setConsent] = useState<ConsentTarget | null>(null);
  const [approving, setApproving] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  const openwolfEnabled = useSettingsStore((s) => s.openwolfEnabled);

  const refreshList = async () => {
    if (openwolfEnabled) {
      // Only install if not already present
      const existing = await listWidgetFolders().catch(() => [] as WidgetInfo[]);
      if (!existing.some((w) => w.widget_id === "project-intel")) {
        try { await installBundledWidget("project-intel"); } catch (_) {}
      }
      setWidgets(existing.some((w) => w.widget_id === "project-intel")
        ? existing
        : await listWidgetFolders().catch(() => [] as WidgetInfo[]));
      return;
    }
    listWidgetFolders().then(setWidgets).catch(() => {});
  };

  useEffect(() => {
    if (!isOpen) return;
    setName("");
    setIsDragOver(false);
    setInstalling(false);
    refreshList();
    setTimeout(() => inputRef.current?.focus(), 100);
  }, [isOpen]);

  // Listen for native file drops while dialog is open
  useEffect(() => {
    if (!isOpen) return;
    let unlisten: (() => void) | null = null;
    getCurrentWebviewWindow()
      .onDragDropEvent(async (event) => {
        const payload = event.payload;
        if (payload.type === "enter") {
          const hasZip = payload.paths.some((p) => p.toLowerCase().endsWith(".zip"));
          if (hasZip) setIsDragOver(true);
        } else if (payload.type === "leave") {
          setIsDragOver(false);
        } else if (payload.type === "drop") {
          setIsDragOver(false);
          const zipFiles = payload.paths.filter((p) => p.toLowerCase().endsWith(".zip"));
          if (zipFiles.length === 0) return;
          setInstalling(true);
          for (const zipPath of zipFiles) {
            try {
              const widgetId = await installWidgetZip(zipPath);
              useCanvasStore.getState().addWidgetTerminal(widgetId);
              pushToast("Widget installed", widgetId);
            } catch (err) {
              pushToast("Widget install failed", String(err));
            }
          }
          refreshList();
          setInstalling(false);
        }
      })
      .then((fn) => { unlisten = fn; })
      .catch((err) => console.warn("[widget-drop]", err));
    return () => { if (unlisten) unlisten(); };
  }, [isOpen]);

  useEffect(() => {
    if (!isOpen) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.stopPropagation(); onClose(); }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [isOpen, onClose]);

  if (!isOpen) return null;

  const handleCreate = async () => {
    const id = name.trim().toLowerCase().replace(/[^a-z0-9-_]/g, "-").replace(/-+/g, "-");
    if (!id) return;
    setCreating(true);
    try {
      const folderPath = await createWidgetFolder(id);
      const widgetName = name.trim();
      const sessionName = `Widget: ${widgetName}`;
      // Open widget panel on canvas
      useCanvasStore.getState().addWidgetTerminal(id, widgetName);
      // Open an empty, provider-unlocked chat. The first real user prompt
      // chooses and locks Claude or Codex via the empty-chat provider picker.
      useCanvasStore.getState().addClaudeTerminal(folderPath, false, sessionName);
      const panels = useCanvasStore.getState().terminals;
      const chatPanel = panels[panels.length - 1];
      if (chatPanel?.panelType === "claude") {
        useProviderSessionStore.getState().createSession(
          chatPanel.terminalId,
          sessionName,
          false,
          true,
          folderPath,
          undefined,
          false,
        );
      }
      onClose();
    } catch (err) {
      console.warn("[widget] Failed to create:", err);
    } finally {
      setCreating(false);
    }
  };

  const openWidgetOnCanvas = (widgetId: string) => {
    const existing = useCanvasStore.getState().terminals.find(
      (t) => t.panelType === "widget" && t.widgetId === widgetId,
    );
    if (existing) {
      useCanvasStore.getState().bringToFront(existing.id);
      onClose();
      return;
    }
    useCanvasStore.getState().addWidgetTerminal(widgetId);
    onClose();
  };

  /**
   * Open flow with manifest-aware consent:
   *   1. No `widget.json`    → legacy web widget, open immediately.
   *   2. Manifest kind=web   → no subprocess, no consent needed.
   *   3. Manifest kind=plugin|hybrid + prior approval covers the current
   *      permission set → auto-approve silently.
   *   4. Otherwise           → show the Review Permissions modal.
   */
  const handleOpen = async (widget: WidgetInfo) => {
    const id = widget.widget_id;
    try {
      const envelope = await readWidgetManifest(id);
      if (!envelope) {
        openWidgetOnCanvas(id);
        return;
      }
      const result = validateManifest(envelope.raw);
      if (!result.ok) {
        pushToast("Widget manifest invalid", result.errors.join("; "));
        return;
      }
      const manifest = result.manifest;
      if (manifest.kind === "web") {
        openWidgetOnCanvas(id);
        return;
      }
      const prior = (await readWidgetApproval(id).catch(() => null)) as
        | ApprovalRecord
        | null;
      if (!requiresReconsent(manifest, prior)) {
        openWidgetOnCanvas(id);
        return;
      }
      setConsent({ widgetId: id, manifest, manifestHash: envelope.hash });
    } catch (err) {
      pushToast("Failed to read widget manifest", String(err));
    }
  };

  const handleApproveConsent = async () => {
    if (!consent) return;
    setApproving(true);
    try {
      const record: ApprovalRecord = {
        manifestHash: consent.manifestHash,
        approvedAt: new Date().toISOString(),
        permissionNames: permissionNames(consent.manifest),
        apiVersion: consent.manifest.apiVersion,
      };
      await writeWidgetApproval(consent.widgetId, JSON.stringify(record, null, 2));
      const id = consent.widgetId;
      setConsent(null);
      openWidgetOnCanvas(id);
    } catch (err) {
      pushToast("Failed to save approval", String(err));
    } finally {
      setApproving(false);
    }
  };

  const handleDenyConsent = () => {
    setConsent(null);
  };

  const handleDelete = async (e: React.MouseEvent, widget: WidgetInfo) => {
    e.stopPropagation();
    try {
      await deleteWidgetFolder(widget.widget_id);
      // Also close any open widget panels for this widget
      const terminals = useCanvasStore.getState().terminals;
      for (const t of terminals) {
        if (t.panelType === "widget" && t.widgetId === widget.widget_id) {
          useCanvasStore.getState().removeTerminal(t.id);
        }
      }
      refreshList();
    } catch (err) {
      console.warn("[widget] Failed to delete:", err);
    }
  };

  if (consent) {
    return (
      <div className="wdg-dialog-overlay" onClick={handleDenyConsent}>
        <div className="wdg-dialog" onClick={(e) => e.stopPropagation()}>
          <div className="wdg-dialog-header">
            <span className="wdg-dialog-title">Review Permissions</span>
            <button className="wdg-dialog-close" onClick={handleDenyConsent}>
              <svg width="10" height="10" viewBox="0 0 10 10">
                <path d="M1 1L9 9M9 1L1 9" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" />
              </svg>
            </button>
          </div>
          <div className="wdg-dialog-body">
            <div className="wdg-consent-intro">
              <div className="wdg-consent-title">
                <strong>{consent.manifest.name}</strong>
                <span className="wdg-consent-version">v{consent.manifest.version}</span>
              </div>
              <div className="wdg-consent-subtitle">
                This {consent.manifest.kind === "hybrid" ? "hybrid plugin" : "plugin"} is
                requesting the following permissions:
              </div>
            </div>
            {consent.manifest.permissions.length === 0 ? (
              <div className="wdg-empty">
                No host permissions requested — this plugin runs sandboxed.
              </div>
            ) : (
              <ul className="wdg-consent-list">
                {consent.manifest.permissions.map((p) => (
                  <li key={p.name} className="wdg-consent-item">
                    <div className="wdg-consent-item-head">
                      <span className="wdg-consent-item-label">{labelForPermission(p.name)}</span>
                      <code className="wdg-consent-item-name">{p.name}</code>
                    </div>
                    <div className="wdg-consent-item-reason">{p.reason}</div>
                    {p.scopes && p.scopes.length > 0 && (
                      <div className="wdg-consent-item-scopes">
                        Scopes: {p.scopes.join(", ")}
                      </div>
                    )}
                  </li>
                ))}
              </ul>
            )}
            <div className="wdg-form-actions wdg-consent-actions">
              <button className="wdg-btn wdg-btn--cancel" onClick={handleDenyConsent}>Deny</button>
              <button
                className="wdg-btn wdg-btn--create"
                onClick={handleApproveConsent}
                disabled={approving}
              >
                {approving ? "Approving..." : "Approve"}
              </button>
            </div>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="wdg-dialog-overlay" onClick={onClose}>
      <div className="wdg-dialog" onClick={(e) => e.stopPropagation()}>
        <div className="wdg-dialog-header">
          <span className="wdg-dialog-title">Widgets</span>
          <button className="wdg-dialog-close" onClick={onClose}>
            <svg width="10" height="10" viewBox="0 0 10 10">
              <path d="M1 1L9 9M9 1L1 9" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" />
            </svg>
          </button>
        </div>

        <div className="wdg-dialog-body">
          <div className="wdg-form">
            <label>Create a new widget</label>
            <input
              ref={inputRef}
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="Widget name..."
              onKeyDown={(e) => {
                if (e.key === "Enter" && name.trim()) handleCreate();
                if (e.key === "Escape") onClose();
              }}
            />
            <div className="wdg-form-actions">
              <button className="wdg-btn wdg-btn--cancel" onClick={onClose}>Cancel</button>
              <button
                className="wdg-btn wdg-btn--create"
                onClick={handleCreate}
                disabled={!name.trim() || creating}
              >
                {creating ? "Creating..." : "Create"}
              </button>
            </div>
          </div>

          {widgets.length > 0 && (() => {
            const BUNDLED_IDS = new Set(["project-intel"]);
            const bundled = widgets.filter((w) => BUNDLED_IDS.has(w.widget_id));
            const user = widgets.filter((w) => !BUNDLED_IDS.has(w.widget_id));
            const sorted = [...bundled, ...user];
            return (
              <>
                <div className="wdg-section-label">Existing Widgets</div>
                <div className="wdg-list">
                  {sorted.map((w) => {
                    const isBundled = BUNDLED_IDS.has(w.widget_id);
                    return (
                      <div
                        key={w.widget_id}
                        className={`wdg-list-item ${isBundled ? "wdg-list-item--bundled" : ""}`}
                        onClick={() => handleOpen(w)}
                      >
                        <div className={`wdg-list-item-dot ${isBundled ? "wdg-list-item-dot--bundled" : w.has_index ? "wdg-list-item-dot--ready" : "wdg-list-item-dot--empty"}`} />
                        <span className="wdg-list-item-name">{w.widget_id}</span>
                        {isBundled && <span className="wdg-list-item-badge">built-in</span>}
                        <span className="wdg-list-item-time">{formatRelativeTime(w.modified)}</span>
                        {!isBundled && (
                          <button
                            className="wdg-list-item-delete"
                            onClick={(e) => handleDelete(e, w)}
                            title="Delete widget"
                          >
                            <svg width="10" height="10" viewBox="0 0 10 10">
                              <path d="M2 3H8M3 3V8.5H7V3M4 4.5V7M6 4.5V7M3.5 3L4 1.5H6L6.5 3" stroke="currentColor" strokeWidth="0.9" strokeLinecap="round" strokeLinejoin="round"/>
                            </svg>
                          </button>
                        )}
                      </div>
                    );
                  })}
                </div>
              </>
            );
          })()}

          {widgets.length === 0 && (
            <div className="wdg-empty">
              No widgets yet. Create one to get started.
            </div>
          )}

          <div className="wdg-section-label">Install Widget</div>
          <div className={`wdg-drop-zone ${isDragOver ? "wdg-drop-zone--active" : ""} ${installing ? "wdg-drop-zone--installing" : ""}`}>
            {installing ? (
              <>
                <div className="wdg-spinner" />
                <span className="wdg-drop-zone-text">Installing...</span>
              </>
            ) : (
              <>
                <svg className="wdg-drop-zone-icon" width="24" height="24" viewBox="0 0 24 24" fill="none">
                  <path d="M21 15V19C21 20.1 20.1 21 19 21H5C3.9 21 3 20.1 3 19V15" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round"/>
                  <path d="M7 10L12 15L17 10" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round"/>
                  <path d="M12 15V3" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round"/>
                </svg>
                <span className="wdg-drop-zone-text">
                  {isDragOver ? "Release to install" : "Drag & drop .zip to install"}
                </span>
              </>
            )}
          </div>

          <button
            className="wdg-open-folder"
            onClick={() => openSystemFolder("$HOME/.terminal64/widgets")}
          >
            Open Widgets Folder
          </button>
        </div>
      </div>
    </div>
  );
}
