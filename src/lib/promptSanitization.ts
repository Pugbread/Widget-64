const SYSTEM_REMINDER_BLOCK_RE = /<system-reminder>[\s\S]*?(?:<\/system-reminder>|$)\s*/gi;

export function stripSystemReminderBlocks(text: string): string {
  if (!text.includes("<system-reminder>")) return text;
  return text.replace(SYSTEM_REMINDER_BLOCK_RE, "").trimStart();
}
