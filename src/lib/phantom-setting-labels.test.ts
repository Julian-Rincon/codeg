import { createTranslator } from "next-intl"
import { describe, expect, it } from "vitest"

import enMessages from "@/i18n/messages/en.json"
import {
  hasPluginAgentOptions,
  isDangerousSettingValue,
  localizeSessionSettings,
  parsePluginAgentName,
  pluginAwareOptionRows,
  type PhantomSettingLabelTranslator,
} from "@/lib/phantom-setting-labels"
import { buildSessionSettings } from "@/lib/session-settings"
import type {
  SessionConfigOptionInfo,
  SessionConfigSelectOptionInfo,
} from "@/lib/types"

const t = createTranslator({
  locale: "en",
  messages: enMessages,
  namespace: "Folder.chat.messageInput",
}) as unknown as PhantomSettingLabelTranslator

function select(
  id: string,
  name: string,
  category: string | null,
  current: string,
  options: (string | SessionConfigSelectOptionInfo)[],
  extra: Partial<SessionConfigOptionInfo> = {}
): SessionConfigOptionInfo {
  return {
    id,
    name,
    description: null,
    category,
    kind: {
      type: "select",
      current_value: current,
      options: options.map((option) =>
        typeof option === "string"
          ? { value: option, name: option, description: null }
          : option
      ),
      groups: [],
    },
    ...extra,
  }
}

function build(options: SessionConfigOptionInfo[]) {
  return buildSessionSettings({
    configOptions: options,
    modes: [],
    currentModeId: null,
    modeTitle: "Mode",
  })
}

describe("localizeSessionSettings — titles", () => {
  it("translates the well-known option ids and leaves unknown ones alone", () => {
    const settings = localizeSessionSettings(
      build([
        select("model", "Model", "model", "m", ["m"]),
        select("effort", "Effort", "thought_level", "high", ["high"]),
        select("mode", "Mode", "mode", "default", ["default", "plan"]),
        select("agent", "Agent", "agent", "main", ["main"]),
        select("fast", "Fast mode", "model_config", "off", ["on", "off"]),
        select("mystery", "Mystery knob", null, "a", ["a"]),
      ]),
      t
    )
    expect(settings.map((setting) => setting.title)).toEqual([
      "Model",
      "Effort",
      "Mode",
      "Agent",
      "Mystery knob",
      "Fast mode",
    ])
  })

  it("relabels the mode values and follows them in the summary", () => {
    const settings = localizeSessionSettings(
      build([
        select("mode", "Modo", "mode", "bypassPermissions", [
          { value: "default", name: "Manual", description: null },
          { value: "plan", name: "Plan", description: null },
          {
            value: "bypassPermissions",
            name: "Bypass Permissions",
            description: null,
          },
        ]),
      ]),
      t
    )
    const mode = settings[0]
    if (mode.kind === "switch") throw new Error("not a choice")
    expect(mode.groups[0].options.map((option) => option.name)).toEqual([
      "Manual",
      "Plan",
      "Bypass permissions",
    ])
    expect(mode.currentLabel).toBe("Bypass permissions")
    expect(mode.summary).toBe("Bypass permissions")
  })

  it("never rewrites model or plugin names", () => {
    const settings = localizeSessionSettings(
      build([
        select("model", "Model", "model", "opus", ["opus"]),
        select("agent", "Agent", "agent", "build", [
          "build",
          "impeccable:impeccable-asset-producer",
        ]),
      ]),
      t
    )
    expect(settings.map((setting) => setting.title)).toEqual(["Model", "Agent"])
    const agent = settings[1]
    if (agent.kind === "switch") throw new Error("not a choice")
    expect(agent.groups[0].options[1].name).toBe(
      "impeccable:impeccable-asset-producer"
    )
  })

  it("translates a switch's title and its on-state summary", () => {
    const [setting] = localizeSessionSettings(
      build([
        select("fast_mode", "Fast mode", "model_config", "on", ["off", "on"]),
      ]),
      t
    )
    expect(setting).toMatchObject({ kind: "switch", title: "Fast mode" })
  })
})

describe("parsePluginAgentName", () => {
  it("splits a plugin subagent id", () => {
    expect(
      parsePluginAgentName("impeccable:impeccable-asset-producer")
    ).toEqual({ plugin: "impeccable", agent: "impeccable-asset-producer" })
  })

  it("ignores plain names, prose and incomplete ids", () => {
    expect(parsePluginAgentName("Plan")).toBeNull()
    expect(parsePluginAgentName("Note: do this")).toBeNull()
    expect(parsePluginAgentName(":agent")).toBeNull()
    expect(parsePluginAgentName("plugin:")).toBeNull()
    expect(parsePluginAgentName("a:b:c")).toBeNull()
  })
})

describe("pluginAwareOptionRows", () => {
  function agentSetting(names: string[]) {
    const [setting] = build([
      select(
        "agent",
        "Agent",
        "agent",
        names[0],
        names.map((name) => ({ value: name, name, description: null }))
      ),
    ])
    if (setting.kind === "switch") throw new Error("not a choice")
    return setting
  }

  it("moves plugin subagents under a trailing Plugins header, agent first", () => {
    const setting = agentSetting([
      "impeccable:impeccable-asset-producer",
      "builtin",
      "vercel:deploy",
    ])
    expect(hasPluginAgentOptions(setting)).toBe(true)
    const rows = pluginAwareOptionRows(setting, "Plugins")
    expect(
      rows.map((row) =>
        row.kind === "header" ? `#${row.name}` : row.option.name
      )
    ).toEqual(["builtin", "#Plugins", "impeccable-asset-producer", "deploy"])
    const pluginRows = rows.filter((row) => row.kind === "option" && row.plugin)
    expect(
      pluginRows.map((row) => row.kind === "option" && row.plugin)
    ).toEqual(["impeccable", "vercel"])
  })

  it("leaves lists without plugins exactly as they were", () => {
    const setting = agentSetting(["build", "plan"])
    expect(hasPluginAgentOptions(setting)).toBe(false)
    expect(
      pluginAwareOptionRows(setting, "Plugins").map((row) => row.kind)
    ).toEqual(["option", "option"])
  })

  it("never groups plugin-looking model names", () => {
    const [setting] = build([
      select("model", "Model", "model", "a", ["a", "b"]),
    ])
    if (setting.kind === "switch") throw new Error("not a choice")
    setting.groups[0].options[1].name = "vendor:model"
    expect(hasPluginAgentOptions(setting)).toBe(false)
  })
})

describe("isDangerousSettingValue", () => {
  it("flags bypass-permissions by id", () => {
    expect(isDangerousSettingValue("bypassPermissions")).toBe(true)
    expect(isDangerousSettingValue("always-approve")).toBe(true)
  })

  it("flags an agent-branded bypass by its label", () => {
    expect(isDangerousSettingValue("yolo", "Bypass Permissions")).toBe(true)
    // Grok's config id is the bypass mode whatever the locale calls it.
    expect(isDangerousSettingValue("yolo", "Omitir permisos")).toBe(true)
    // A legacy mode list may use YOLO as a brand name of its own.
    expect(isDangerousSettingValue("yolo", "YOLO", true)).toBe(false)
  })

  it("leaves the normal modes alone", () => {
    expect(isDangerousSettingValue("default", "Manual")).toBe(false)
    expect(isDangerousSettingValue("plan", "Plan")).toBe(false)
    expect(isDangerousSettingValue("acceptEdits", "Accept edits")).toBe(false)
  })
})
