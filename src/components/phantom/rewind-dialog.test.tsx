import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react"
import { NextIntlClientProvider } from "next-intl"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"

import enMessages from "@/i18n/messages/en.json"
import type {
  CheckpointPreview,
  PhantomRewindContext,
} from "@/lib/phantom-checkpoints"
import type { MessageTurn } from "@/lib/types"

const transportCall = vi.hoisted(() => vi.fn())
vi.mock("@/lib/transport", () => ({
  getTransport: () => ({ call: transportCall }),
  getShellTransport: () => ({ call: transportCall }),
  isDesktop: () => false,
  isRemoteDesktopMode: () => false,
  getActiveRemoteConnectionId: () => null,
  notifyRemoteDesktopUnauthorized: vi.fn(),
}))

const notifyMock = vi.hoisted(() => vi.fn())
vi.mock("@/lib/notify", () => ({ notify: notifyMock }))

import { RewindDialog } from "./rewind-dialog"

const turn: MessageTurn = {
  id: "turn-9",
  role: "user",
  blocks: [],
  timestamp: "2026-10-28T12:00:00Z",
}

const readyPreview: CheckpointPreview = {
  status: "ready",
  checkpoint: {
    id: 41,
    seq: 3,
    kind: "prompt",
    createdAt: "2026-10-28T11:59:59Z",
    changedFiles: 2,
    skippedReason: null,
  },
  files: [
    { path: "src/app.ts", status: "modified", additions: 4, deletions: 1 },
    { path: "notes.md", status: "added", additions: 6, deletions: null },
  ],
  moreFiles: 0,
  busy: false,
}

function makeContext(
  overrides: Partial<PhantomRewindContext> = {}
): PhantomRewindContext {
  return {
    conversationId: 7,
    canFork: true,
    rewindConversation: vi.fn(async () => true),
    resolveForkTarget: () => ({ kind: "fork", turnId: "turn-4" }),
    ...overrides,
  }
}

function mount({
  context = makeContext(),
  preview = readyPreview,
  restore = { safetyCheckpointId: 42, written: 1, deleted: 1 },
  busy = false,
  onOpenChange = vi.fn(),
}: {
  context?: PhantomRewindContext
  preview?: CheckpointPreview
  restore?: unknown
  busy?: boolean
  onOpenChange?: (open: boolean) => void
} = {}) {
  transportCall.mockImplementation((name: string) => {
    if (name === "phantom_checkpoint_preview") return Promise.resolve(preview)
    if (name === "phantom_checkpoint_restore") {
      return restore instanceof Error
        ? Promise.reject(restore)
        : Promise.resolve(restore)
    }
    return Promise.reject(new Error(`unexpected call ${name}`))
  })
  render(
    <NextIntlClientProvider locale="en" messages={enMessages}>
      <RewindDialog
        open
        onOpenChange={onOpenChange}
        context={context}
        itemKey="m1"
        turn={turn}
        text="fix the login"
        busy={busy}
      />
    </NextIntlClientProvider>
  )
  return { context, onOpenChange }
}

function radio(name: string): HTMLButtonElement {
  return screen.getByRole("radio", { name }) as HTMLButtonElement
}

describe("RewindDialog", () => {
  afterEach(cleanup)

  beforeEach(() => {
    transportCall.mockReset()
    notifyMock.mockReset()
  })

  it("previews what restoring the code would change", async () => {
    mount()
    expect(await screen.findByText("src/app.ts")).toBeTruthy()
    expect(screen.getByText("notes.md")).toBeTruthy()
    expect(screen.getByText("Restoring the code changes")).toBeTruthy()
    expect(screen.getByText("2 files, +10 −1")).toBeTruthy()
    expect(transportCall).toHaveBeenCalledWith("phantom_checkpoint_preview", {
      conversationId: 7,
      messageId: "turn-9",
      turnTimestamp: "2026-10-28T12:00:00Z",
      turnText: "fix the login",
    })
    // Both halves are available, so both is the default.
    expect(radio("Code and conversation").getAttribute("aria-checked")).toBe(
      "true"
    )
  })

  it("restores the code and forks the conversation on confirm", async () => {
    const context = makeContext()
    const onOpenChange = vi.fn()
    mount({ context, onOpenChange })

    fireEvent.click(await screen.findByRole("button", { name: "Rewind" }))

    await waitFor(() =>
      expect(context.rewindConversation).toHaveBeenCalledWith(
        { mode: "fork", turnId: "turn-4" },
        "fix the login"
      )
    )
    expect(transportCall).toHaveBeenCalledWith("phantom_checkpoint_restore", {
      conversationId: 7,
      checkpointId: 41,
    })
    const notice = notifyMock.mock.calls[notifyMock.mock.calls.length - 1]?.[0]
    expect(notice.title).toBe("Code restored")
    expect(notice.description).toBe(
      "1 file written, 1 deleted. Git history was not touched."
    )
    expect(onOpenChange).toHaveBeenCalledWith(false)
  })

  it("undoes a restore through the safety checkpoint it reported", async () => {
    mount()
    fireEvent.click(await screen.findByRole("button", { name: "Rewind" }))
    await waitFor(() => expect(notifyMock).toHaveBeenCalled())

    const undo = notifyMock.mock.calls[0][0].actions[0]
    expect(undo.label).toBe("Undo rewind")
    undo.onClick()

    await waitFor(() =>
      expect(transportCall).toHaveBeenCalledWith("phantom_checkpoint_restore", {
        conversationId: 7,
        checkpointId: 42,
      })
    )
    await waitFor(() =>
      expect(
        notifyMock.mock.calls[notifyMock.mock.calls.length - 1]?.[0].title
      ).toBe("Rewind undone")
    )
  })

  it("offers conversation only when this message has no code checkpoint", async () => {
    const context = makeContext()
    mount({
      context,
      preview: {
        status: "no_checkpoint",
        checkpoint: null,
        files: [],
        moreFiles: 0,
        busy: false,
      },
    })

    expect(
      await screen.findByText("No code checkpoint was saved for this message.")
    ).toBeTruthy()
    expect(radio("Code only").disabled).toBe(true)
    expect(radio("Code and conversation").disabled).toBe(true)
    expect(radio("Conversation only").getAttribute("aria-checked")).toBe("true")

    fireEvent.click(screen.getByRole("button", { name: "Rewind" }))
    await waitFor(() =>
      expect(context.rewindConversation).toHaveBeenCalledTimes(1)
    )
    expect(transportCall).not.toHaveBeenCalledWith(
      "phantom_checkpoint_restore",
      expect.anything()
    )
  })

  it("explains a non-git folder and still forks the conversation", async () => {
    const context = makeContext({ canFork: false })
    mount({
      context,
      preview: {
        status: "not_git",
        checkpoint: null,
        files: [],
        moreFiles: 0,
        busy: false,
      },
    })
    expect(
      await screen.findByText(
        "This folder is not a git repository, so there is no code checkpoint. Only the conversation can be rewound."
      )
    ).toBeTruthy()
    expect(
      screen.getByText(
        "This agent cannot fork at a message, so a new conversation opens with this message's text in the composer."
      )
    ).toBeTruthy()
    expect(radio("Code only").disabled).toBe(true)
  })

  it("refuses while a turn is running", async () => {
    mount({ busy: true })
    expect(await screen.findByRole("alert")).toBeTruthy()
    expect(screen.getByRole("alert").textContent).toContain("A turn is running")
    const confirm = screen.getByRole("button", {
      name: "Rewind",
    }) as HTMLButtonElement
    expect(confirm.disabled).toBe(true)
    fireEvent.click(confirm)
    expect(transportCall).not.toHaveBeenCalledWith(
      "phantom_checkpoint_restore",
      expect.anything()
    )
  })

  it("reports a restore refused because a turn started meanwhile", async () => {
    mount({
      restore: Object.assign(new Error("busy"), { code: "turn_in_progress" }),
    })
    fireEvent.click(await screen.findByRole("button", { name: "Rewind" }))
    await waitFor(() =>
      expect(screen.getAllByRole("alert").map((a) => a.textContent)).toContain(
        "A turn is running. Stop it or wait for it to finish before rewinding."
      )
    )
  })
})
