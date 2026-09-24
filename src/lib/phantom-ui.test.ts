import { beforeEach, describe, expect, it } from "vitest"
import {
  applyPhantomAccent,
  currentModelId,
  isPhantomAccentId,
  persistPhantomAccent,
  PHANTOM_ACCENT_INIT_SCRIPT,
  PHANTOM_ACCENTS,
  PHANTOM_UI_NAME,
  resolvePhantomAccent,
  STORAGE_KEY_PHANTOM_ACCENT,
} from "@/lib/phantom-ui"

// Neutral preset surfaces from globals.css, converted from oklch(L 0 0):
// light background/card are white; dark background is oklch(0.145 0 0) and
// dark card/sidebar is oklch(0.205 0 0), which is the harder case.
const LIGHT_SURFACES = ["#ffffff"]
const DARK_SURFACES = ["#0a0a0a", "#171717"]

function channel(value: number): number {
  const normalized = value / 255
  return normalized <= 0.03928
    ? normalized / 12.92
    : ((normalized + 0.055) / 1.055) ** 2.4
}

function luminance(hex: string): number {
  const value = hex.replace("#", "")
  const r = channel(Number.parseInt(value.slice(0, 2), 16))
  const g = channel(Number.parseInt(value.slice(2, 4), 16))
  const b = channel(Number.parseInt(value.slice(4, 6), 16))
  return 0.2126 * r + 0.7152 * g + 0.0722 * b
}

function contrast(a: string, b: string): number {
  const light = Math.max(luminance(a), luminance(b))
  const dark = Math.min(luminance(a), luminance(b))
  return (light + 0.05) / (dark + 0.05)
}

describe("Phantom identity", () => {
  it("uses the requested public product name", () => {
    expect(PHANTOM_UI_NAME).toBe("Phantom")
  })

  it("maps known model families to stable iconic accents", () => {
    expect(resolvePhantomAccent({ modelId: "claude-opus-5" }).id).toBe(
      "claude-amber"
    )
    expect(resolvePhantomAccent({ modelId: "gpt-4o" }).id).toBe("openai-teal")
    expect(resolvePhantomAccent({ modelId: "provider/gemini-3-pro" }).id).toBe(
      "gemini-violet"
    )
    expect(resolvePhantomAccent({ modelId: "deepseek-reasoner" }).id).toBe(
      "deepseek-blue"
    )
  })

  it("uses the agent only as a fallback and keeps unknown models blue", () => {
    expect(resolvePhantomAccent({ agentType: "claude_code" }).id).toBe(
      "claude-amber"
    )
    expect(
      resolvePhantomAccent({ modelId: "private-model-42", agentType: "hermes" })
        .id
    ).toBe("general-blue")
  })

  it("reads the current model from the ACP model option", () => {
    const options = [
      {
        id: "reasoning",
        name: "Reasoning",
        kind: {
          type: "select" as const,
          current_value: "high",
          options: [],
          groups: [],
        },
      },
      {
        id: "model",
        name: "Model",
        kind: {
          type: "select" as const,
          current_value: "gpt-4o",
          options: [],
          groups: [],
        },
      },
    ]
    expect(currentModelId(options)).toBe("gpt-4o")
  })

  it("keeps every accent at WCAG AA against its foreground and surfaces", () => {
    for (const [id, palette] of Object.entries(PHANTOM_ACCENTS)) {
      for (const [mode, shade, surfaces] of [
        ["light", palette.light, LIGHT_SURFACES],
        ["dark", palette.dark, DARK_SURFACES],
      ] as const) {
        expect(
          contrast(shade.color, shade.foreground),
          `${id} ${mode}: button text on accent`
        ).toBeGreaterThanOrEqual(4.5)
        for (const surface of surfaces) {
          expect(
            contrast(shade.color, surface),
            `${id} ${mode}: accent as text on ${surface}`
          ).toBeGreaterThanOrEqual(4.5)
        }
      }
    }
  })

  it("resolves every probe model to a known palette entry", () => {
    const probes = [
      "claude-opus-5",
      "gpt-4o",
      "gemini-3-pro",
      "deepseek-reasoner",
      "qwen-max",
      "grok-4",
      "llama-4",
      "mistral-large",
      "kimi-k2",
      "hermes-default",
      "private-model-42",
    ]
    for (const modelId of probes) {
      expect(isPhantomAccentId(resolvePhantomAccent({ modelId }).id)).toBe(true)
    }
  })

  it("writes only Phantom variables and exposes the active model id", () => {
    const root = document.createElement("html")
    const accent = resolvePhantomAccent({ modelId: "claude-sonnet-5" })
    applyPhantomAccent(accent, root)
    expect(root.dataset.phantomAccent).toBe("claude-amber")
    expect(root.dataset.phantomModel).toBe("claude-sonnet-5")
    expect(root.style.getPropertyValue("--phantom-accent-light")).toBe(
      accent.light.color
    )
    expect(root.style.getPropertyValue("--phantom-accent-dark")).toBe(
      accent.dark.color
    )
    expect(root.style.getPropertyValue("--primary")).toBe("")
    expect(
      root.ownerDocument
        .querySelector('meta[name="theme-color"]')
        ?.getAttribute("content")
    ).toBe(accent.light.color)
  })
})

describe("PHANTOM_ACCENT_INIT_SCRIPT", () => {
  function runInitScript() {
    ;(0, eval)(PHANTOM_ACCENT_INIT_SCRIPT)
  }

  beforeEach(() => {
    localStorage.clear()
    const root = document.documentElement
    root.removeAttribute("style")
    root.removeAttribute("data-phantom-accent")
    root.removeAttribute("data-phantom-model")
  })

  it("restores the persisted accent before hydration", () => {
    persistPhantomAccent("gemini-violet")
    runInitScript()
    const root = document.documentElement
    expect(root.dataset.phantomAccent).toBe("gemini-violet")
    expect(root.style.getPropertyValue("--phantom-accent-light")).toBe(
      PHANTOM_ACCENTS["gemini-violet"].light.color
    )
    expect(root.style.getPropertyValue("--phantom-accent-dark")).toBe(
      PHANTOM_ACCENTS["gemini-violet"].dark.color
    )
  })

  it("ignores unknown or tampered stored values", () => {
    localStorage.setItem(STORAGE_KEY_PHANTOM_ACCENT, "toString")
    runInitScript()
    expect(document.documentElement.dataset.phantomAccent).toBeUndefined()
    expect(document.documentElement.getAttribute("style")).toBeNull()
  })

  it("does nothing on a first run with empty storage", () => {
    runInitScript()
    expect(document.documentElement.dataset.phantomAccent).toBeUndefined()
  })
})
