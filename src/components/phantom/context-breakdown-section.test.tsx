import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  within,
} from "@testing-library/react"
import { NextIntlClientProvider } from "next-intl"
import { afterEach, describe, expect, it, vi } from "vitest"
import enMessages from "@/i18n/messages/en.json"
import { parseContextReport } from "@/lib/phantom-context-breakdown"
import {
  type ConversationRuntimeSession,
  useConversationRuntimeStore,
} from "@/stores/conversation-runtime-store"
import {
  ContextBreakdownSection,
  ContextBreakdownView,
  categorySlot,
} from "./context-breakdown-section"

const REPORT = `## Context Usage

**Model:** claude-sonnet-4-5
**Tokens:** 15.2k / 200k (8%)

### Estimated usage by category

| Category | Tokens | Percentage |
|----------|--------|------------|
| System prompt | 2.9k | 1.5% |
| System tools | 11.4k | 5.7% |
| Memory files | 868 | 0.4% |
| Messages | 8 | 0.0% |
| Free space | 139.8k | 69.9% |
| Autocompact buffer | 45.0k | 22.5% |

### Memory Files

| Type | Path | Tokens |
|------|------|--------|
| Project | /w/p/CLAUDE.md | 868 |
`

function wrap(ui: React.ReactNode) {
  return render(
    <NextIntlClientProvider locale="en" messages={enMessages}>
      {ui}
    </NextIntlClientProvider>
  )
}

function seed(turnText: string | null) {
  const session = {
    conversationId: 7,
    liveMessage: null,
    localTurns:
      turnText == null
        ? []
        : [
            {
              id: "a",
              role: "assistant",
              blocks: [{ type: "text", text: turnText }],
              timestamp: "2026-10-28T00:00:00Z",
            },
          ],
    detail: null,
  } as unknown as ConversationRuntimeSession
  act(() => {
    useConversationRuntimeStore.setState({
      byConversationId: new Map([[7, session]]),
    })
  })
}

afterEach(() => {
  cleanup()
  act(() => {
    useConversationRuntimeStore.setState({ byConversationId: new Map() })
  })
})

describe("ContextBreakdownView", () => {
  it("draws one segment per in-window category and a full table", () => {
    wrap(
      <ContextBreakdownView
        breakdown={parseContextReport(REPORT)!}
        windowSize={null}
      />
    )
    // Used categories + the buffer; free space is the bar's empty track.
    const segments = screen.getAllByTestId("context-segment")
    expect(segments).toHaveLength(5)
    expect(segments[0].getAttribute("title")).toBe("System prompt: 2.9K (1.5%)")
    expect(parseFloat(segments[0].style.width)).toBeCloseTo(1.45)

    const rows = within(screen.getByRole("table")).getAllByRole("row")
    // header + 6 categories
    expect(rows).toHaveLength(7)
    expect(rows[5].textContent).toContain("Free space")
    expect(rows[5].textContent).toContain("139.8K")

    expect(screen.getByText("Autocompacts at").nextSibling?.textContent).toBe(
      "155K"
    )
    expect(screen.getByText("/w/p/CLAUDE.md")).toBeTruthy()
  })

  it("keeps a category's colour slot fixed by name", () => {
    expect(categorySlot("System prompt")).toBe(0)
    expect(categorySlot("messages")).toBe(6)
    expect(categorySlot("Something new")).toBe(7)
  })
})

describe("ContextBreakdownSection", () => {
  it("offers /context on click when no report exists, never by itself", () => {
    seed(null)
    const onSendCommand = vi.fn()
    wrap(
      <ContextBreakdownSection
        conversationId={7}
        windowSize={200_000}
        isPrompting={false}
        onSendCommand={onSendCommand}
      />
    )
    expect(onSendCommand).not.toHaveBeenCalled()
    fireEvent.click(screen.getByRole("button", { name: "Show breakdown" }))
    expect(onSendCommand).toHaveBeenCalledWith("/context")
    // Waiting for the reply: the button can't fire a second /context.
    expect(
      (
        screen.getByRole("button", {
          name: "Show breakdown",
        }) as HTMLButtonElement
      ).disabled
    ).toBe(true)

    seed(REPORT)
    expect(screen.getAllByTestId("context-segment")).toHaveLength(5)
  })

  it("renders the newest report from the conversation", () => {
    seed(`Here you go\n\n${REPORT}`)
    wrap(
      <ContextBreakdownSection
        conversationId={7}
        windowSize={200_000}
        isPrompting={false}
        onSendCommand={vi.fn()}
      />
    )
    expect(screen.getByText("System tools")).toBeTruthy()
    expect(
      screen.getByRole("button", { name: "Refresh breakdown (/context)" })
    ).toBeTruthy()
  })

  it("sends /compact with optional instructions and says it costs tokens", () => {
    seed(null)
    const onSendCommand = vi.fn()
    wrap(
      <ContextBreakdownSection
        conversationId={7}
        windowSize={200_000}
        isPrompting={false}
        onSendCommand={onSendCommand}
      />
    )
    expect(screen.getByText(/this uses tokens/)).toBeTruthy()
    fireEvent.click(screen.getByRole("button", { name: "Compact now" }))
    expect(onSendCommand).toHaveBeenLastCalledWith("/compact")

    fireEvent.change(
      screen.getByRole("textbox", {
        name: "Compaction instructions (optional)",
      }),
      { target: { value: "  keep the   API notes " } }
    )
    fireEvent.click(screen.getByRole("button", { name: "Compact now" }))
    expect(onSendCommand).toHaveBeenLastCalledWith(
      "/compact keep the API notes"
    )
  })

  it("is disabled while the agent is responding", () => {
    seed(null)
    const onSendCommand = vi.fn()
    wrap(
      <ContextBreakdownSection
        conversationId={7}
        windowSize={200_000}
        isPrompting
        onSendCommand={onSendCommand}
      />
    )
    for (const name of ["Show breakdown", "Compact now"]) {
      const button = screen.getByRole("button", { name }) as HTMLButtonElement
      expect(button.disabled).toBe(true)
    }
    expect(
      screen.getByText("Available when the agent finishes responding.")
    ).toBeTruthy()
  })
})
