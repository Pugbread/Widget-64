import { resolveSkillPrompt } from "./tauriApi";
import type { SlashCommand } from "./types";

const SKILL_SOURCES = new Set(["user", "project", "Terminal 64"]);
const MAX_CATALOG_SKILLS = 30;
const MAX_AUTO_LOADED_SKILLS = 2;
const MAX_TOTAL_SKILL_BODY_CHARS = 12_000;
const MAX_SINGLE_SKILL_BODY_CHARS = 8_000;
const STOP_WORDS = new Set([
  "about",
  "after",
  "again",
  "against",
  "also",
  "and",
  "any",
  "are",
  "but",
  "can",
  "could",
  "for",
  "from",
  "have",
  "how",
  "into",
  "make",
  "need",
  "not",
  "our",
  "please",
  "should",
  "that",
  "the",
  "then",
  "this",
  "use",
  "using",
  "was",
  "what",
  "when",
  "where",
  "with",
  "work",
  "would",
  "you",
]);

export function isSkillSlashCommand(command: SlashCommand, builtinNames: ReadonlySet<string>): boolean {
  if (builtinNames.has(command.name)) return false;
  if (command.kind) return command.kind === "skill";
  if (command.source === "built-in" || command.source === "builtin") return false;
  return SKILL_SOURCES.has(command.source) || command.source.trim().length > 0;
}

function tokenize(text: string): Set<string> {
  const tokens = text
    .toLowerCase()
    .split(/[^a-z0-9_]+/g)
    .map((part) => part.trim())
    .filter((part) => part.length >= 3 && !STOP_WORDS.has(part));
  return new Set(tokens);
}

function commandTokens(command: SlashCommand): Set<string> {
  return tokenize(`${command.name.replace(/[-_:./]/g, " ")} ${command.description}`);
}

function scoreSkill(promptTokens: ReadonlySet<string>, promptLower: string, command: SlashCommand): number {
  let score = 0;
  const nameLower = command.name.toLowerCase();
  if (promptLower.includes(nameLower)) score += 8;
  if (promptLower.includes(`/${nameLower}`)) score += 12;

  for (const token of commandTokens(command)) {
    if (promptTokens.has(token)) score += command.name.toLowerCase().includes(token) ? 4 : 2;
  }
  return score;
}

function escapeSystemText(text: string): string {
  return text.split("</system-reminder>").join("<\\/system-reminder>");
}

function clipBody(body: string, remainingBudget: number): string {
  const limit = Math.max(0, Math.min(MAX_SINGLE_SKILL_BODY_CHARS, remainingBudget));
  if (body.length <= limit) return body;
  return `${body.slice(0, limit)}\n\n[Terminal 64: skill instructions truncated to fit the prompt budget.]`;
}

export async function buildSkillAugmentedPrompt({
  prompt,
  cwd,
  slashCommands,
  builtinNames,
}: {
  prompt: string;
  cwd?: string | undefined;
  slashCommands: SlashCommand[];
  builtinNames: ReadonlySet<string>;
}): Promise<string> {
  const skills = slashCommands
    .filter((command) => isSkillSlashCommand(command, builtinNames))
    .sort((a, b) => a.name.localeCompare(b.name));

  if (skills.length === 0) return prompt;

  const promptLower = prompt.toLowerCase();
  const promptTokens = tokenize(prompt);
  const scored = skills
    .map((command) => ({ command, score: scoreSkill(promptTokens, promptLower, command) }))
    .filter(({ score }) => score > 0)
    .sort((a, b) => b.score - a.score || a.command.name.localeCompare(b.command.name))
    .slice(0, MAX_AUTO_LOADED_SKILLS);

  const loadedSkillBlocks: string[] = [];
  let remainingBudget = MAX_TOTAL_SKILL_BODY_CHARS;
  for (const { command } of scored) {
    if (remainingBudget <= 0) break;
    try {
      const resolved = await resolveSkillPrompt(command.name, "", cwd);
      const clipped = clipBody(resolved.body, remainingBudget);
      remainingBudget -= clipped.length;
      loadedSkillBlocks.push([
        `<skill name="${resolved.name}">`,
        escapeSystemText(clipped),
        "</skill>",
      ].join("\n"));
    } catch (err) {
      console.warn("[skill] Failed to auto-load skill:", command.name, err);
    }
  }

  const catalog = skills.slice(0, MAX_CATALOG_SKILLS).map((skill) => {
    const description = skill.description.trim() || "No description provided.";
    return `- /${skill.name}: ${description}`;
  });
  if (skills.length > MAX_CATALOG_SKILLS) {
    catalog.push(`- ... ${skills.length - MAX_CATALOG_SKILLS} more skills available via /skills.`);
  }

  const reminder = [
    "<system-reminder>",
    "Terminal 64 skills are available. Use them proactively when a skill matches the user's task; do not wait for the user to type the slash command.",
    "Available skills:",
    ...catalog,
    loadedSkillBlocks.length > 0
      ? [
          "",
          "Auto-loaded skill instructions for this turn:",
          ...loadedSkillBlocks,
          "Follow any auto-loaded skill instructions that apply to this request.",
        ].join("\n")
      : "No skill body was auto-loaded for this turn. If a listed skill is essential, say which /skill should be run.",
    "</system-reminder>",
  ].join("\n");

  return `${reminder}\n\n${prompt}`;
}
