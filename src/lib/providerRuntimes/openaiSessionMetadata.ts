import {
  resolveProviderRuntimeResumeId,
  useProviderSessionStore,
} from "../../stores/providerSessionStore";

export function getOpenAiThreadIdForSession(sessionId: string): string | null {
  const session = useProviderSessionStore.getState().sessions[sessionId];
  return resolveProviderRuntimeResumeId(session, "openai") ?? session?.codexThreadId ?? null;
}

export function setOpenAiThreadIdForSession(sessionId: string, threadId: string | null): void {
  useProviderSessionStore.getState().setCodexThreadId(sessionId, threadId);
}

export function clearOpenAiThreadIdAndSeedTranscript(sessionId: string): void {
  const store = useProviderSessionStore.getState();
  const session = store.sessions[sessionId];
  store.setCodexThreadId(sessionId, null);
  if (session?.messages.length) {
    store.setSeedTranscript(sessionId, session.messages);
  }
}
