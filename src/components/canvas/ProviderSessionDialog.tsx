import { useState, useRef, useEffect } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import {
  getDefaultAvailableProvider,
  isProviderAvailable,
  useSettingsStore,
} from "../../stores/settingsStore";
import {
  readProviderSessionMetadataSnapshot,
  resolveSessionProviderState,
  useProviderSessionStore,
} from "../../stores/providerSessionStore";
import { listDiskSessions, listCodexDiskSessions } from "../../lib/tauriApi";
import type { DiskSession } from "../../lib/types";
import { listProviderManifests, type ProviderId } from "../../lib/providers";
import { ProviderLogo } from "../ui/BrandLogos";
import { formatRelativeTime } from "../../lib/constants";
import "./ProviderSessionDialog.css";

export interface ProviderSessionDialogProps {
  isOpen: boolean;
  onClose: () => void;
  onConfirm: (cwd: string, skipPermissions: boolean, sessionName: string | undefined, provider: ProviderId) => void;
  onReopen: (sessionId: string, cwd: string, provider: ProviderId) => void;
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes}B`;
  if (bytes < 1048576) return `${(bytes / 1024).toFixed(0)}KB`;
  return `${(bytes / 1048576).toFixed(1)}MB`;
}

const diskSessionFetchers: Partial<Record<ProviderId, (cwd: string) => Promise<DiskSession[]>>> = {
  anthropic: listDiskSessions,
  openai: listCodexDiskSessions,
};

export function ProviderSessionDialog({ isOpen, onClose, onConfirm, onReopen }: ProviderSessionDialogProps) {
  const [dir, setDir] = useState("");
  const [name, setName] = useState("");
  const [provider, setProvider] = useState<ProviderId>("anthropic");
  const [diskSessions, setDiskSessions] = useState<DiskSession[]>([]);
  const [loading, setLoading] = useState(false);
  const recentDirs = useSettingsStore((s) => s.recentDirs);
  const addRecentDir = useSettingsStore((s) => s.addRecentDir);
  const providerAvailability = useSettingsStore((s) => s.providerAvailability);
  const sessions = useProviderSessionStore((s) => s.sessions);
  const inputRef = useRef<HTMLInputElement>(null);

  const namedSessions = getNamedSessions(sessions, dir, provider);
  const defaultProvider = getDefaultAvailableProvider(providerAvailability);
  const availableProviderManifests = listProviderManifests().filter((manifest) =>
    isProviderAvailable(manifest.id, providerAvailability)
  );

  useEffect(() => {
    if (isOpen) {
      setDir("");
      setName("");
      setProvider(defaultProvider);
      setDiskSessions([]);
      setTimeout(() => inputRef.current?.focus(), 50);
    }
  }, [defaultProvider, isOpen]);

  useEffect(() => {
    if (!isOpen || isProviderAvailable(provider, providerAvailability)) return;
    setProvider(defaultProvider);
  }, [defaultProvider, isOpen, provider, providerAvailability]);

  useEffect(() => {
    if (!dir.trim()) { setDiskSessions([]); return; }
    setLoading(true);
    const fetcher = diskSessionFetchers[provider] ?? listDiskSessions;
    fetcher(dir.trim()).then((s) => {
      setDiskSessions(s);
      setLoading(false);
    }).catch(() => { setDiskSessions([]); setLoading(false); });
  }, [dir, provider]);

  useEffect(() => {
    if (!isOpen) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.stopPropagation(); onClose(); }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [isOpen, onClose]);

  if (!isOpen) return null;

  const handleBrowse = async () => {
    const selected = await open({ directory: true, title: "Select project folder" });
    if (selected) setDir(selected as string);
  };

  const handleNewSession = () => {
    if (!dir.trim()) return;
    const selectedProvider = isProviderAvailable(provider, providerAvailability)
      ? provider
      : defaultProvider;
    addRecentDir(dir.trim());
    onConfirm(dir.trim(), false, name.trim() || undefined, selectedProvider);
    onClose();
  };

  const handleOpenSession = (sessionId: string) => {
    if (dir.trim()) addRecentDir(dir.trim());
    onReopen(sessionId, dir.trim(), provider);
    onClose();
  };

  const handleQuickDir = (d: string) => {
    setDir(d);
  };


  const hasDir = dir.trim().length > 0;
  // Named sessions (from our store) pinned at top
  const namedIds = new Set(namedSessions.map((s) => s.id));
  // Disk sessions that aren't already named
  const otherDiskSessions = diskSessions.filter((s) => !namedIds.has(s.id));

  return (
    <div className="claude-dialog-overlay" onClick={onClose}>
      <div className="claude-dialog" onClick={(e) => e.stopPropagation()}>
        <div className="claude-dialog-header">
          <span className="claude-dialog-title">&gt;_ Code Session</span>
          <button className="claude-dialog-close" onClick={onClose}>
            <svg width="9" height="9" viewBox="0 0 9 9">
              <path d="M1 1L8 8M8 1L1 8" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
            </svg>
          </button>
        </div>

        <div className="claude-dialog-body">
          {/* Step 0: Provider — pinned to the top so the lists below filter
              by it without the user having to scroll. */}
          <label className="claude-dialog-label">Provider</label>
          <div className="claude-dialog-provider-row">
            {availableProviderManifests.map((manifest) => (
              <button
                key={manifest.id}
                type="button"
                className={`claude-dialog-provider-chip ${provider === manifest.id ? "claude-dialog-provider-chip--active" : ""}`}
                onClick={() => setProvider(manifest.id)}
              >
                <ProviderLogo provider={manifest.id} size={11} />
                <span>{manifest.ui.label}</span>
              </button>
            ))}
          </div>

          {/* Step 1: Directory */}
          <label className="claude-dialog-label">Project Directory</label>
          <div className="claude-dialog-dir-row">
            <input
              ref={inputRef}
              className="claude-dialog-input"
              value={dir}
              onChange={(e) => setDir(e.target.value)}
              placeholder="Select or type a project path"
              onKeyDown={(e) => e.key === "Enter" && hasDir && handleNewSession()}
            />
            <button className="claude-dialog-browse" onClick={handleBrowse}>Browse</button>
          </div>

          {/* Recent dirs as chips */}
          {recentDirs.length > 0 && !hasDir && (
            <div className="claude-dialog-chips">
              {recentDirs.map((d) => (
                <button key={d} className="claude-dialog-chip" onClick={() => handleQuickDir(d)}>
                  {d.split(/[/\\]/).slice(-2).join("/")}
                </button>
              ))}
            </div>
          )}

          {/* Step 2: Sessions (shown after directory is selected) */}
          {hasDir && (
            <>
              {/* Named/saved sessions pinned at top */}
              {namedSessions.length > 0 && (
                <div className="claude-dialog-section">
                  <label className="claude-dialog-label">Saved Sessions</label>
                  {namedSessions.map((s) => (
                    <div key={s.id} className="claude-dialog-session-row">
                      <button className="claude-dialog-session claude-dialog-session--named" onClick={() => handleOpenSession(s.id)}>
                        <span className="claude-dialog-session-pin">&#9733;</span>
                        <span className="claude-dialog-session-name">{s.name}</span>
                        <span className="claude-dialog-session-meta">{s.messageCount} msgs</span>
                      </button>
                      <button
                        className="claude-dialog-session-delete"
                        onClick={() => useProviderSessionStore.getState().deleteSession(s.id)}
                        title="Delete session"
                      >
                        <svg width="10" height="10" viewBox="0 0 10 10">
                          <path d="M1 1L9 9M9 1L1 9" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" />
                        </svg>
                      </button>
                    </div>
                  ))}
                </div>
              )}

              {/* All disk sessions */}
              {otherDiskSessions.length > 0 && (
                <div className="claude-dialog-section">
                  <label className="claude-dialog-label">
                    Previous Sessions
                    <span className="claude-dialog-count">{otherDiskSessions.length}</span>
                  </label>
                  <div className="claude-dialog-session-list">
                    {otherDiskSessions.map((s) => (
                      <button key={s.id} className="claude-dialog-session" onClick={() => handleOpenSession(s.id)}>
                        <span className="claude-dialog-session-id">{s.summary || s.id.slice(0, 8)}</span>
                        <span className="claude-dialog-session-meta">{formatRelativeTime(s.modified * 1000)}</span>
                        <span className="claude-dialog-session-size">{formatSize(s.size)}</span>
                      </button>
                    ))}
                  </div>
                </div>
              )}

              {loading && (
                <div className="claude-dialog-loading">Scanning sessions...</div>
              )}

              {/* New session */}
              <div className="claude-dialog-section">
                <label className="claude-dialog-label">New Session</label>
                <input
                  className="claude-dialog-input"
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  placeholder="Session name (optional — saves it)"
                  onKeyDown={(e) => e.key === "Enter" && handleNewSession()}
                />
              </div>
            </>
          )}
        </div>

        <div className="claude-dialog-footer">
          <button className="claude-dialog-cancel" onClick={onClose}>Cancel</button>
          <button
            className="claude-dialog-confirm"
            onClick={handleNewSession}
            disabled={!hasDir}
          >
            New Session
          </button>
        </div>
      </div>
    </div>
  );
}

function normPath(p: string): string {
  return p.replace(/\\/g, "/").replace(/\/+$/, "");
}

function cwdMatch(a: string | undefined, b: string): boolean {
  if (!a || !b) return true;
  return normPath(a) === normPath(b);
}

function getNamedSessions(
  liveSessions: Record<string, any>,
  cwd: string,
  provider: ProviderId,
): { id: string; name: string; messageCount: number }[] {
  const results: { id: string; name: string; messageCount: number }[] = [];
  const seen = new Set<string>();
  const providerMatch = (session: any) => resolveSessionProviderState(session).provider === provider;

  // Live sessions
  for (const [id, s] of Object.entries(liveSessions)) {
    if (s.name && cwdMatch(s.cwd, cwd) && providerMatch(s)) {
      results.push({ id, name: s.name, messageCount: s.messages?.length || 0 });
      seen.add(id);
    }
  }

  // Persisted sessions
  try {
    const data = readProviderSessionMetadataSnapshot();
    for (const [id, session] of Object.entries(data)) {
      if (
        session.name &&
        !seen.has(id) &&
        cwdMatch(session.cwd, cwd) &&
        providerMatch(session)
      ) {
        results.push({ id, name: session.name, messageCount: session.localTranscript?.length || 0 });
      }
    }
  } catch (e) {
    console.warn("[provider-session-dialog] Failed to load persisted sessions:", e);
  }

  return results;
}

export default ProviderSessionDialog;
