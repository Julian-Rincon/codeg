import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react"
import { NextIntlClientProvider } from "next-intl"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import enMessages from "@/i18n/messages/en.json"
import type { AgentType, PromptDraft } from "@/lib/types"

const appendMemoryNote = vi.fn()
vi.mock("@/lib/phantom-memory", async (orig) => ({
  ...(await orig<typeof import("@/lib/phantom-memory")>()),
  appendMemoryNote: (...args: unknown[]) => appendMemoryNote(...args),
}))
const toastSuccess = vi.fn()
const toastError = vi.fn()
vi.mock("sonner", () => ({
  toast: {
    success: (...a: unknown[]) => toastSuccess(...a),
    error: (...a: unknown[]) => toastError(...a),
  },
}))
// The editor dialog (Monaco) is irrelevant here.
vi.mock("next/dynamic", () => ({ default: () => () => null }))

import { setMemoryQuickAddEnabled } from "@/lib/phantom-memory"
import { usePhantomMemoryComposer } from "./memory-composer"

const draft = (text: string): PromptDraft => ({
  blocks: [{ type: "text", text }],
  displayText: text,
})

function mount(agentType: AgentType | null = "claude_code") {
  const onNoteSaved = vi.fn()
  const onDismiss = vi.fn()
  const results: boolean[] = []
  function Harness({ text }: { text: string }) {
    const memory = usePhantomMemoryComposer({
      agentType,
      folderPath: "/w/p",
      onNoteSaved,
      onDismiss,
    })
    return (
      <div data-has-editor={String(memory.openEditor != null)}>
        <button
          type="button"
          onClick={() => results.push(memory.interceptDraft(draft(text)))}
        >
          submit
        </button>
        {memory.element}
      </div>
    )
  }
  const utils = render(
    <NextIntlClientProvider locale="en" messages={enMessages}>
      <Harness text="# use pnpm, not npm" />
    </NextIntlClientProvider>
  )
  const submit = (text?: string) => {
    if (text != null) {
      utils.rerender(
        <NextIntlClientProvider locale="en" messages={enMessages}>
          <Harness text={text} />
        </NextIntlClientProvider>
      )
    }
    fireEvent.click(screen.getByText("submit"))
    return results[results.length - 1]
  }
  return {
    submit,
    onNoteSaved,
    onDismiss,
    hasEditor: () =>
      utils.container.firstElementChild?.getAttribute("data-has-editor") ===
      "true",
  }
}

describe("usePhantomMemoryComposer (# quick-add)", () => {
  beforeEach(() => {
    localStorage.clear()
    appendMemoryNote.mockReset()
    appendMemoryNote.mockResolvedValue({
      scope: "project",
      path: "/w/p/CLAUDE.md",
      mtime_ms: 1,
      created: false,
    })
    toastSuccess.mockReset()
    toastError.mockReset()
  })
  afterEach(cleanup)

  it("intercepts a # draft and saves it to the project with P", async () => {
    const { submit, onNoteSaved } = mount()
    expect(submit()).toBe(true)
    const chooser = screen.getByRole("dialog", { name: "Save to memory" })
    expect(chooser.textContent).toContain("- use pnpm, not npm")
    // Focus lands on the chooser, so Enter/keys don't go back to the editor.
    expect(document.activeElement?.textContent).toContain("Project")

    fireEvent.keyDown(chooser, { key: "p" })
    await waitFor(() => expect(onNoteSaved).toHaveBeenCalledTimes(1))
    expect(appendMemoryNote).toHaveBeenCalledWith(
      "/w/p",
      "project",
      "use pnpm, not npm"
    )
    expect(toastSuccess).toHaveBeenCalledWith("Saved to CLAUDE.md")
    expect(screen.queryByRole("dialog")).toBeNull()
  })

  it("U picks the user CLAUDE.md", async () => {
    const { submit } = mount()
    submit()
    fireEvent.keyDown(screen.getByRole("dialog"), { key: "U" })
    await waitFor(() =>
      expect(appendMemoryNote).toHaveBeenCalledWith(
        "/w/p",
        "user",
        "use pnpm, not npm"
      )
    )
  })

  it("Escape cancels without writing and hands focus back", () => {
    const { submit, onDismiss, onNoteSaved } = mount()
    submit()
    fireEvent.keyDown(screen.getByRole("dialog"), { key: "Escape" })
    expect(onDismiss).toHaveBeenCalledTimes(1)
    expect(onNoteSaved).not.toHaveBeenCalled()
    expect(appendMemoryNote).not.toHaveBeenCalled()
    expect(screen.queryByRole("dialog")).toBeNull()
  })

  it("ignores keys while an IME composition is active", () => {
    const { submit } = mount()
    submit()
    fireEvent.keyDown(screen.getByRole("dialog"), {
      key: "p",
      isComposing: true,
    })
    expect(appendMemoryNote).not.toHaveBeenCalled()
  })

  it("keeps the chooser and the text when saving fails", async () => {
    appendMemoryNote.mockRejectedValueOnce(new Error("disk full"))
    const { submit, onNoteSaved } = mount()
    submit()
    fireEvent.click(screen.getByRole("button", { name: /Project/ }))
    await waitFor(() => expect(toastError).toHaveBeenCalledTimes(1))
    expect(onNoteSaved).not.toHaveBeenCalled()
    expect(screen.getByRole("dialog")).toBeTruthy()
  })

  it("lets ordinary messages through", () => {
    const { submit } = mount()
    expect(submit("fix #12 please")).toBe(false)
    expect(submit("# Title\nbody")).toBe(false)
    expect(submit("/context")).toBe(false)
    expect(screen.queryByRole("dialog")).toBeNull()
  })

  it("does nothing for other agents", () => {
    const { submit, hasEditor } = mount("codex")
    expect(submit()).toBe(false)
    expect(hasEditor()).toBe(false)
  })

  it("respects the setting", () => {
    const { submit, hasEditor } = mount()
    act(() => setMemoryQuickAddEnabled(false))
    expect(submit()).toBe(false)
    // The editor stays reachable from the menu.
    expect(hasEditor()).toBe(true)
    act(() => setMemoryQuickAddEnabled(true))
    expect(submit()).toBe(true)
  })
})
