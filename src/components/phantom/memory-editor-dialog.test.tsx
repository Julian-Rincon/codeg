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

// Stand-in for `@monaco-editor/react`: a textarea with the same value/onChange
// contract, so a draft can be typed without the AMD editor in jsdom.
vi.mock("@monaco-editor/react", async () => {
  const { createElement } = await import("react")
  return {
    default: ({
      value,
      onChange,
    }: {
      value?: string
      onChange?: (v: string | undefined) => void
    }) =>
      createElement("textarea", {
        "data-testid": "monaco",
        value: value ?? "",
        onChange: (e: { target: { value: string } }) =>
          onChange?.(e.target.value),
      }),
  }
})

// The real module only calls `loader.config` on the (now mocked) library.
vi.mock("@/lib/monaco-local", () => ({}))

// Every memory path goes through the transport; the dialog is what is under
// test, so the calls are recorded instead.
const listMemoryFiles = vi.fn()
const readMemoryFile = vi.fn()
const writeMemoryFile = vi.fn()
vi.mock("@/lib/phantom-memory", async (orig) => ({
  ...(await orig<typeof import("@/lib/phantom-memory")>()),
  listMemoryFiles: (...a: unknown[]) => listMemoryFiles(...a),
  readMemoryFile: (...a: unknown[]) => readMemoryFile(...a),
  writeMemoryFile: (...a: unknown[]) => writeMemoryFile(...a),
}))

const toastSuccess = vi.fn()
const toastError = vi.fn()
vi.mock("sonner", () => ({
  toast: {
    success: (...a: unknown[]) => toastSuccess(...a),
    error: (...a: unknown[]) => toastError(...a),
  },
}))

import { AppearanceProvider } from "@/components/appearance-provider"
import { MemoryEditorDialog } from "./memory-editor-dialog"

const FOLDER = "/w/p"

const FILES = [
  {
    scope: "project_dot_claude",
    path: "/w/p/.claude/CLAUDE.md",
    exists: false,
  },
  { scope: "project", path: "/w/p/CLAUDE.md", exists: true, mtime_ms: 100 },
  { scope: "local", path: "/w/p/CLAUDE.local.md", exists: false },
  { scope: "user", path: "/home/u/.claude/CLAUDE.md", exists: false },
]

function renderDialog(onOpenChange = vi.fn()) {
  const utils = render(
    <NextIntlClientProvider locale="en" messages={enMessages}>
      <AppearanceProvider>
        <MemoryEditorDialog
          open
          onOpenChange={onOpenChange}
          folderPath={FOLDER}
        />
      </AppearanceProvider>
    </NextIntlClientProvider>
  )
  return { ...utils, onOpenChange }
}

async function typeInEditor(value: string) {
  const editor = await screen.findByTestId("monaco")
  fireEvent.change(editor, { target: { value } })
}

beforeEach(() => {
  localStorage.clear()
  listMemoryFiles.mockReset().mockResolvedValue(FILES)
  readMemoryFile.mockReset().mockResolvedValue({
    scope: "project",
    path: "/w/p/CLAUDE.md",
    exists: true,
    content: "# Notes\n",
    mtime_ms: 100,
  })
  writeMemoryFile.mockReset().mockResolvedValue({
    status: "saved",
    mtime_ms: 200,
  })
  toastSuccess.mockReset()
  toastError.mockReset()
})

afterEach(cleanup)

describe("MemoryEditorDialog", () => {
  it("lists the four memory files and opens the project one", async () => {
    renderDialog()
    await typeInEditor("# Notes\n")
    for (const path of FILES.map((f) => f.path)) {
      expect(screen.getByTitle(path)).toBeTruthy()
    }
    expect(readMemoryFile).toHaveBeenCalledWith(FOLDER, "project")
    expect((screen.getByTestId("monaco") as HTMLTextAreaElement).value).toBe(
      "# Notes\n"
    )
  })

  it("saves with the mtime it loaded and keeps the buffer after it", async () => {
    renderDialog()
    await typeInEditor("# Notes\n\n- use pnpm\n")
    fireEvent.click(screen.getByRole("button", { name: "Save" }))
    await act(async () => {})
    expect(writeMemoryFile).toHaveBeenCalledWith(
      FOLDER,
      "project",
      "# Notes\n\n- use pnpm\n",
      100
    )
    expect(toastSuccess).toHaveBeenCalledTimes(1)
    // Nothing left to save, so the button is disabled again.
    expect(
      (screen.getByRole("button", { name: "Save" }) as HTMLButtonElement)
        .disabled
    ).toBe(true)
  })

  it("writes nothing on a conflict and lets the user overwrite", async () => {
    writeMemoryFile.mockResolvedValueOnce({
      status: "conflict",
      current_mtime_ms: 555,
    })
    renderDialog()
    await typeInEditor("# mine\n")
    fireEvent.click(screen.getByRole("button", { name: "Save" }))
    const alert = await screen.findByRole("alert")
    expect(alert.textContent).toContain("changed on disk")
    expect(writeMemoryFile).toHaveBeenLastCalledWith(
      FOLDER,
      "project",
      "# mine\n",
      100
    )

    fireEvent.click(screen.getByRole("button", { name: "Overwrite" }))
    await act(async () => {})
    expect(writeMemoryFile).toHaveBeenLastCalledWith(
      FOLDER,
      "project",
      "# mine\n",
      555
    )
    expect(screen.queryByRole("alert")).toBeNull()
  })

  it("turns a missing file into a create, and shows why a read failed", async () => {
    readMemoryFile.mockResolvedValueOnce({
      scope: "project",
      path: "/w/p/CLAUDE.md",
      exists: false,
      content: "",
    })
    renderDialog()
    await typeInEditor("- first note\n")
    expect(
      screen.getByText("This file doesn't exist yet; saving creates it.")
    ).toBeTruthy()
    fireEvent.click(screen.getByRole("button", { name: "Create" }))
    await act(async () => {})
    expect(writeMemoryFile).toHaveBeenCalledWith(
      FOLDER,
      "project",
      "- first note\n",
      null
    )

    // A failed read is stated in the pane, not left on a spinner for good.
    readMemoryFile.mockRejectedValueOnce(new Error("permission denied"))
    fireEvent.click(screen.getByTitle("/w/p/CLAUDE.local.md"))
    expect(await screen.findByText(/permission denied/)).toBeTruthy()
    const retry = screen.getByRole("button", { name: "Try again" })
    readMemoryFile.mockResolvedValueOnce({
      scope: "local",
      path: "/w/p/CLAUDE.local.md",
      exists: false,
      content: "",
    })
    fireEvent.click(retry)
    await waitFor(() =>
      expect(readMemoryFile).toHaveBeenLastCalledWith(FOLDER, "local")
    )
    await waitFor(() =>
      expect(screen.queryByText(/permission denied/)).toBeNull()
    )
    expect(screen.getByTestId("monaco")).toBeTruthy()
  })
})
