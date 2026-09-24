import type { AgentType, SessionConfigOptionInfo } from "@/lib/types"
import {
  MODEL_CONFIG_OPTION_ID,
  isModelConfigOption,
} from "@/lib/model-config-groups"

/** The public product name. Internal protocol/data keys intentionally remain `codeg` for compatibility. */
export const PHANTOM_UI_NAME = "Phantom"
export const PHANTOM_UI_SHORT_NAME = "Phantom"

export type PhantomAccentId =
  | "general-blue"
  | "claude-amber"
  | "openai-teal"
  | "gemini-violet"
  | "deepseek-blue"
  | "qwen-violet"
  | "grok-rose"
  | "llama-indigo"
  | "mistral-orange"
  | "kimi-pink"
  | "hermes-cyan"

/** One mode's accent pair; both sides are checked for WCAG AA in the unit tests. */
export interface PhantomAccentShade {
  /** Main accent used for primary actions, focus rings and active selection. */
  color: string
  /** Text color that remains readable on top of `color`. */
  foreground: string
}

export interface PhantomAccentPalette {
  /** Used on light surfaces: dark enough to read as text on white. */
  light: PhantomAccentShade
  /** Used under `.dark`: light enough to read as text on the dark background. */
  dark: PhantomAccentShade
}

export interface PhantomAccent extends PhantomAccentPalette {
  id: PhantomAccentId
  label: string
  /** The model id that produced the accent, if one is known. */
  modelId: string | null
  /** The agent used only as a fallback when no model id is available. */
  agentType: AgentType | string | null
}

/** Last applied accent, read by the pre-hydration script to avoid a blue flash. */
export const STORAGE_KEY_PHANTOM_ACCENT = "phantom-ui-accent"

const LIGHT_FOREGROUND = "#ffffff"
const DARK_FOREGROUND = "#0a0a0a"

function palette(light: string, dark: string): PhantomAccentPalette {
  return {
    light: { color: light, foreground: LIGHT_FOREGROUND },
    dark: { color: dark, foreground: DARK_FOREGROUND },
  }
}

export const PHANTOM_ACCENTS: Record<PhantomAccentId, PhantomAccentPalette> = {
  "general-blue": palette("#1d4ed8", "#60a5fa"),
  "claude-amber": palette("#b4532a", "#e08a68"),
  "openai-teal": palette("#0f766e", "#2dd4bf"),
  "gemini-violet": palette("#6d28d9", "#a78bfa"),
  "deepseek-blue": palette("#1e40af", "#7ea8ff"),
  "qwen-violet": palette("#7e22ce", "#c084fc"),
  "grok-rose": palette("#be123c", "#fb7185"),
  "llama-indigo": palette("#4338ca", "#818cf8"),
  "mistral-orange": palette("#c2410c", "#fb923c"),
  "kimi-pink": palette("#be185d", "#f472b6"),
  "hermes-cyan": palette("#0e7490", "#22d3ee"),
}

export function isPhantomAccentId(value: unknown): value is PhantomAccentId {
  return (
    typeof value === "string" &&
    Object.prototype.hasOwnProperty.call(PHANTOM_ACCENTS, value)
  )
}

const ACCENT_LABELS: Record<PhantomAccentId, string> = {
  "general-blue": "Azul General",
  "claude-amber": "Claude",
  "openai-teal": "OpenAI",
  "gemini-violet": "Gemini",
  "deepseek-blue": "DeepSeek",
  "qwen-violet": "Qwen",
  "grok-rose": "Grok",
  "llama-indigo": "Llama",
  "mistral-orange": "Mistral",
  "kimi-pink": "Kimi",
  "hermes-cyan": "Hermes",
}

const MODEL_PATTERNS: ReadonlyArray<[PhantomAccentId, RegExp]> = [
  ["claude-amber", /claude|anthropic|sonnet|opus|haiku|fable/],
  ["openai-teal", /openai|gpt|^o[134](?:[-_]|$)|codex/],
  ["gemini-violet", /gemini|google/],
  ["deepseek-blue", /deepseek/],
  ["qwen-violet", /qwen|qwq/],
  ["grok-rose", /grok|xai/],
  ["llama-indigo", /llama|meta/],
  ["mistral-orange", /mistral|mixtral|codestral/],
  ["kimi-pink", /kimi|moonshot/],
  ["hermes-cyan", /hermes/],
]

const AGENT_ACCENTS: Record<string, PhantomAccentId> = {
  claude_code: "claude-amber",
  hermes: "hermes-cyan",
  open_code: "general-blue",
}

function normalize(value: string | null | undefined): string {
  return value?.trim().toLowerCase() ?? ""
}

function accentFor(
  id: PhantomAccentId,
  modelId: string | null,
  agentType: AgentType | string | null
): PhantomAccent {
  return {
    id,
    label: ACCENT_LABELS[id],
    ...PHANTOM_ACCENTS[id],
    modelId: modelId?.trim() || null,
    agentType,
  }
}

/**
 * Resolve the iconic color from the model actually selected on the active ACP
 * connection. The model id wins; its display label is included because some
 * agents expose opaque ids (for example `qfmodel`) and only the label carries
 * the family name. An unknown model deliberately stays in the stable Phantom
 * blue rather than inventing a color that could fluctuate after reconnects.
 */
export function resolvePhantomAccent({
  modelId,
  modelLabel,
  agentType,
}: {
  modelId?: string | null
  modelLabel?: string | null
  agentType?: AgentType | string | null
}): PhantomAccent {
  const rawModel = [modelId, modelLabel].filter(Boolean).join(" ")
  const normalized = normalize(rawModel)

  for (const [id, pattern] of MODEL_PATTERNS) {
    if (pattern.test(normalized))
      return accentFor(id, modelId ?? null, agentType ?? null)
  }

  // An explicit but unknown model is still a model choice: keep the stable
  // product blue instead of borrowing the agent's color and implying a family
  // match that the model id did not actually tell us.
  if (rawModel.trim()) {
    return accentFor("general-blue", modelId ?? null, agentType ?? null)
  }

  const agentKey = normalize(agentType)
  return accentFor(
    AGENT_ACCENTS[agentKey] ?? "general-blue",
    modelId ?? null,
    agentType ?? null
  )
}

/** Read the model selector's current value without assuming one agent's option id. */
export function currentModelId(
  options: SessionConfigOptionInfo[] | null | undefined
): string | null {
  if (!options?.length) return null
  const option =
    options.find((candidate) => candidate.id === MODEL_CONFIG_OPTION_ID) ??
    options.find(isModelConfigOption)
  if (!option || option.kind.type !== "select") return null
  return option.kind.current_value.trim() || null
}

/**
 * Apply the accent to the document root. This deliberately writes only Phantom
 * variables; globals.css maps them onto shadcn tokens only on the neutral base
 * preset, so an explicit color preset or custom token override still wins.
 * Both mode shades are written so a light/dark switch needs no re-apply.
 */
export function applyPhantomAccent(
  accent: Pick<PhantomAccent, "id" | "light" | "dark"> & {
    modelId?: string | null
  },
  root: HTMLElement
): void {
  root.dataset.phantomAccent = accent.id
  root.dataset.phantomModel = accent.modelId ?? "default"
  root.style.setProperty("--phantom-accent-light", accent.light.color)
  root.style.setProperty(
    "--phantom-accent-light-foreground",
    accent.light.foreground
  )
  root.style.setProperty("--phantom-accent-dark", accent.dark.color)
  root.style.setProperty(
    "--phantom-accent-dark-foreground",
    accent.dark.foreground
  )

  // Chromium/WebKit use this for the page's browser-chrome tint where the
  // platform supports it. It is a progressive enhancement: a missing head or a
  // browser that ignores it must not affect the React/UI accent.
  const doc = root.ownerDocument
  const head = doc.head
  if (!head) return
  let meta = head.querySelector<HTMLMetaElement>('meta[name="theme-color"]')
  if (!meta) {
    meta = doc.createElement("meta")
    meta.name = "theme-color"
    head.appendChild(meta)
  }
  meta.content = root.classList.contains("dark")
    ? accent.dark.color
    : accent.light.color
}

/** Remember the accent for the next cold start; storage failures are harmless. */
export function persistPhantomAccent(id: PhantomAccentId): void {
  try {
    localStorage.setItem(STORAGE_KEY_PHANTOM_ACCENT, id)
  } catch {
    // Private mode / disabled storage: the accent simply starts blue next time.
  }
}

/**
 * Synchronous pre-hydration script (injected by layout.tsx right after the
 * appearance script, so the `.dark` class is already settled). It restores the
 * last model accent before first paint; without it every cold start flashes
 * the general blue until the ACP connection reports its model. The palette is
 * embedded at build time so this file stays the single source of truth.
 */
export const PHANTOM_ACCENT_INIT_SCRIPT = `
(function() {
  try {
    var ACCENTS = ${JSON.stringify(PHANTOM_ACCENTS)};
    var id = localStorage.getItem(${JSON.stringify(STORAGE_KEY_PHANTOM_ACCENT)});
    if (!id || !Object.prototype.hasOwnProperty.call(ACCENTS, id)) return;
    var accent = ACCENTS[id];
    var root = document.documentElement;
    root.setAttribute("data-phantom-accent", id);
    root.setAttribute("data-phantom-model", "default");
    root.style.setProperty("--phantom-accent-light", accent.light.color);
    root.style.setProperty("--phantom-accent-light-foreground", accent.light.foreground);
    root.style.setProperty("--phantom-accent-dark", accent.dark.color);
    root.style.setProperty("--phantom-accent-dark-foreground", accent.dark.foreground);
  } catch (e) {
    // Storage unavailable: fall back to the CSS default accent.
  }
})();
`
