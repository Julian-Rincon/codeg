import { cleanup, fireEvent, render, screen } from "@testing-library/react"
import { NextIntlClientProvider } from "next-intl"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import enMessages from "@/i18n/messages/en.json"
import { recordPrompt } from "@/lib/phantom-prompt-history"
import { PromptHistorySearch } from "./prompt-history-search"

function mount(props: Partial<Parameters<typeof PromptHistorySearch>[0]> = {}) {
  const onPick = vi.fn()
  const onClose = vi.fn()
  render(
    <NextIntlClientProvider locale="en" messages={enMessages}>
      <PromptHistorySearch
        folder="/repo"
        onPick={onPick}
        onClose={onClose}
        {...props}
      />
    </NextIntlClientProvider>
  )
  return { onPick, onClose, input: screen.getByRole("combobox") }
}

describe("PromptHistorySearch", () => {
  beforeEach(() => localStorage.clear())
  afterEach(cleanup)

  it("lists session prompts first, filters, and picks with Enter", () => {
    recordPrompt("deploy the api", "/other", 1)
    const { input, onPick } = mount({ getSessionHistory: () => ["fix tests"] })
    expect(screen.getAllByRole("option").map((o) => o.textContent)).toEqual([
      "fix tests",
      "deploy the api",
    ])
    fireEvent.change(input, { target: { value: "deploy" } })
    fireEvent.keyDown(input, { key: "Enter" })
    expect(onPick).toHaveBeenCalledWith("deploy the api")
  })

  it("closes on Escape and says when there is nothing yet", () => {
    const { input, onClose } = mount()
    expect(screen.getByText("No prompts sent yet")).toBeTruthy()
    fireEvent.keyDown(input, { key: "Escape" })
    expect(onClose).toHaveBeenCalled()
  })
})
