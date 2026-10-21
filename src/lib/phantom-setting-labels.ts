/**
 * Phantom-owned display labels for the composer's session settings.
 *
 * ACP agents ship their config option titles and values as already-rendered
 * English strings ("Mode", "Effort", "Bypass permissions", …). Upstream shows
 * them verbatim, which is right for the agent's own vocabulary but leaks English
 * into a localized UI. This module maps the stable, well-known ids — never the
 * provider text — onto Phantom's own message keys and leaves everything else
 * (models, plugin agents, a new option an agent adds tomorrow) untouched: a
 * missing translation must degrade to the agent's words, not to a raw id.
 *
 * Nothing here changes a value sent to the agent. Only display strings.
 *
 * Kept out of `session-settings.ts` on purpose so the regular upstream merge
 * stays cheap: the picker applies these labels at render time.
 */

import type {
  SessionChoiceSetting,
  SessionSetting,
} from "@/lib/session-settings"
import type { SessionConfigSelectOptionInfo } from "@/lib/types"

/** Every message key this module can ask for, under `Folder.chat.messageInput`. */
export type PhantomSettingLabelKey =
  | "settingModel"
  | "settingEffort"
  | "settingMode"
  | "settingAgent"
  | "settingFastMode"
  | "modeOptionDefault"
  | "modeOptionAcceptEdits"
  | "modeOptionPlan"
  | "modeOptionDontAsk"
  | "modeOptionBypassPermissions"
  | "modeOptionAuto"
  | "modeOptionYolo"

/**
 * `useTranslations("Folder.chat.messageInput")` narrowed to what this module
 * uses. A translator accepting the whole namespace is assignable to it, so
 * callers just pass `t`.
 */
export type PhantomSettingLabelTranslator = (
  key: PhantomSettingLabelKey
) => string

// Config option ids (lower-cased) whose TITLE we translate. These are the ids
// agents publish today: claude-agent-acp's model/effort/mode/fast, codex-acp's
// reasoning_effort/fast_mode, Cline's provider/mode, DeepSeek's mode. An
// unknown id keeps the agent's own title.
const SETTING_TITLES: Record<string, PhantomSettingLabelKey> = {
  model: "settingModel",
  effort: "settingEffort",
  reasoning_effort: "settingEffort",
  thought_level: "settingEffort",
  mode: "settingMode",
  permission_mode: "settingMode",
  agent: "settingAgent",
  fast: "settingFastMode",
  fast_mode: "settingFastMode",
  model_config: "settingFastMode",
}

// Claude Code / Grok permission-mode values. `default` is deliberately shown as
// "Manual" (claude-agent-acp's own name for it): it is the mode that asks first.
// Legacy mode lists (`target.type === "mode"`) are left alone — Gemini calls the
// same baseline "Default", so one canonical word would mislabel one of them.
const MODE_VALUES: Record<string, PhantomSettingLabelKey> = {
  default: "modeOptionDefault",
  acceptedits: "modeOptionAcceptEdits",
  plan: "modeOptionPlan",
  dontask: "modeOptionDontAsk",
  bypasspermissions: "modeOptionBypassPermissions",
  auto: "modeOptionAuto",
  yolo: "modeOptionYolo",
}

// Values that disable the permission prompts, compared lower-cased.
// `always-approve` is codeg's legacy marker for the same mode; Grok's `yolo`
// is handled separately below because a legacy mode list may use "YOLO" as a
// brand name for something else.
const DANGEROUS_VALUES = new Set(["bypasspermissions", "always-approve"])
const DANGEROUS_NAME = /\bbypass(?:es)?\b/i

/**
 * Whether picking this option hands the agent unguarded access. `value` is the
 * stable signal; `name` catches agent-branded bypass modes. A legacy mode list
 * (`legacyMode`) may use `YOLO` as its own brand name, so only a config option's
 * `yolo` id is treated as the bypass mode there.
 */
export function isDangerousSettingValue(
  value: string,
  name?: string | null,
  legacyMode = false
): boolean {
  const id = value.trim().toLowerCase()
  if (DANGEROUS_VALUES.has(id)) return true
  if (!legacyMode && id === "yolo") return true
  return name != null && DANGEROUS_NAME.test(name)
}

function lookup<T>(table: Record<string, T>, id: string): T | undefined {
  const key = id.toLowerCase()
  return Object.prototype.hasOwnProperty.call(table, key)
    ? table[key]
    : undefined
}

function configIdOf(setting: SessionSetting): string | null {
  return setting.target.type === "config" ? setting.target.configId : null
}

// Only a config option that lists session modes has values worth translating.
// `target.type === "mode"` is the legacy list (see `MODE_VALUES`).
function isModeValuesSetting(setting: SessionChoiceSetting): boolean {
  const id = configIdOf(setting)
  return id !== null && (id === "mode" || id === "permission_mode")
}

function localizeTitle(
  setting: SessionSetting,
  t: PhantomSettingLabelTranslator
): string {
  const id = configIdOf(setting)
  const entry = id === null ? undefined : lookup(SETTING_TITLES, id)
  return entry ? t(entry) : setting.title
}

function localizeOptionName(
  setting: SessionChoiceSetting,
  option: SessionConfigSelectOptionInfo,
  t: PhantomSettingLabelTranslator
): string {
  if (!isModeValuesSetting(setting)) return option.name
  const entry = lookup(MODE_VALUES, option.value)
  return entry ? t(entry) : option.name
}

function localizeGroups(
  setting: SessionChoiceSetting,
  t: PhantomSettingLabelTranslator
) {
  if (!isModeValuesSetting(setting)) return setting.groups
  return setting.groups.map((group) => ({
    ...group,
    options: group.options.map((option) => ({
      ...option,
      name: localizeOptionName(setting, option, t),
    })),
  }))
}

/**
 * The settings the picker renders, with Phantom's localized titles and mode
 * values. Unknown ids and every model/plugin name pass through untouched, and
 * the displayed `currentLabel`/`summary` follow the localized options so the
 * chip and the popover stay in agreement.
 */
export function localizeSessionSettings(
  settings: SessionSetting[],
  t: PhantomSettingLabelTranslator
): SessionSetting[] {
  return settings.map((setting) => {
    const title = localizeTitle(setting, t)
    if (setting.kind === "switch") {
      if (title === setting.title) return setting
      return {
        ...setting,
        title,
        summary: setting.summary === null ? null : title,
      }
    }
    const groups = localizeGroups(setting, t)
    if (groups === setting.groups && title === setting.title) return setting
    const current = groups
      .flatMap((group) => group.options)
      .find((option) => option.value === setting.currentValue)
    const currentLabel = current?.name ?? setting.currentLabel
    return {
      ...setting,
      title,
      groups,
      currentLabel,
      summary: setting.summary === null ? null : currentLabel,
    }
  })
}

/** A plugin subagent: ACP agents name those `"<plugin>:<agent>"`. */
export interface PluginAgentRef {
  plugin: string
  agent: string
}

// Plugin and agent ids are slug-like (`impeccable-asset-producer`,
// `my_plugin.agent`). Requiring both sides keeps prose with a colon, model
// names and Windows-style `C:` values from being mistaken for plugins.
const PLUGIN_AGENT = /^([A-Za-z0-9][\w.-]*):([A-Za-z0-9][\w.-]*)$/

/** Parse a plugin subagent display name, or `null` for anything else. */
export function parsePluginAgentName(name: string): PluginAgentRef | null {
  const match = PLUGIN_AGENT.exec(name.trim())
  if (!match) return null
  return { plugin: match[1], agent: match[2] }
}

/** A rendered row: a group header, or an option (with its plugin, if any). */
export type PhantomSettingRow =
  | { kind: "header"; key: string; name: string }
  | {
      kind: "option"
      key: string
      groupKey: string
      option: SessionConfigSelectOptionInfo
      /** The owning plugin, shown muted beside the agent's name. */
      plugin?: string
    }

function hasPluginOption(setting: SessionChoiceSetting): boolean {
  return setting.groups.some((group) =>
    group.options.some((option) => parsePluginAgentName(option.name) !== null)
  )
}

/** Whether this setting lists plugin subagents (and needs a plain, grouped list). */
export function hasPluginAgentOptions(setting: SessionChoiceSetting): boolean {
  return !setting.isModel && hasPluginOption(setting)
}

/**
 * Flatten a list setting into rows, moving plugin subagents into a trailing
 * "Plugins" group and showing just the agent name there (the plugin becomes the
 * muted suffix). Values are never rewritten; only labels change.
 */
export function pluginAwareOptionRows(
  setting: SessionChoiceSetting,
  pluginsLabel: string
): PhantomSettingRow[] {
  if (!hasPluginAgentOptions(setting)) {
    return setting.groups.flatMap((group) => {
      const rows: PhantomSettingRow[] = []
      if (group.name !== null) {
        rows.push({
          kind: "header",
          key: `header:${group.key}`,
          name: group.name,
        })
      }
      for (const option of group.options) {
        rows.push({
          kind: "option",
          key: `option:${group.key}:${option.value}`,
          groupKey: group.key,
          option,
        })
      }
      return rows
    })
  }

  const builtins: PhantomSettingRow[] = []
  const plugins: PhantomSettingRow[] = []
  for (const group of setting.groups) {
    const rows: PhantomSettingRow[] = []
    if (group.name !== null) {
      rows.push({
        kind: "header",
        key: `header:${group.key}`,
        name: group.name,
      })
    }
    for (const option of group.options) {
      const parsed = parsePluginAgentName(option.name)
      const key = `option:${group.key}:${option.value}`
      if (parsed) {
        plugins.push({
          kind: "option",
          key,
          groupKey: group.key,
          option: { ...option, name: parsed.agent },
          plugin: parsed.plugin,
        })
      } else {
        rows.push({
          kind: "option",
          key,
          groupKey: group.key,
          option,
        })
      }
    }
    builtins.push(...rows)
  }

  // A server group whose options were all plugins would otherwise leave a bare
  // header behind (headers here are followed by their own options only).
  const withoutEmptyHeaders = builtins.filter((row, index) => {
    if (row.kind !== "header") return true
    return builtins[index + 1]?.kind === "option"
  })

  if (plugins.length > 0) {
    withoutEmptyHeaders.push({
      kind: "header",
      key: "header:__phantom_plugins__",
      name: pluginsLabel,
    })
    withoutEmptyHeaders.push(...plugins)
  }
  return withoutEmptyHeaders
}
