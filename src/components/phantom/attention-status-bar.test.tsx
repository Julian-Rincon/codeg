import { act, cleanup, fireEvent, render, screen } from "@testing-library/react"
import { NextIntlClientProvider } from "next-intl"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import type { ConnectionState } from "@/contexts/acp-connections-context"
import { useTabStore, type TabItem } from "@/stores/tab-store"
import enMessages from "@/i18n/messages/en.json"

const h = vi.hoisted(() => ({
  conns: new Map<string, unknown>(),
  listeners: new Map<string, Set<() => void>>(),
  openConversations: vi.fn(),
}))

vi.mock("@/contexts/acp-connections-context", () => ({
  useOptionalConnectionStore: () => ({
    getConnection: (k: string) => h.conns.get(k),
    subscribeKey: (k: string, cb: () => void) => {
      const set = h.listeners.get(k) ?? new Set()
      set.add(cb)
      h.listeners.set(k, set)
      return () => set.delete(cb)
    },
  }),
}))
vi.mock("@/contexts/workbench-route-context", () => ({
  useOptionalWorkbenchRoute: () => ({
    openConversations: h.openConversations,
  }),
}))

import { PhantomAttentionStatusBar } from "./attention-status-bar"

const BLOCKED = {
  status: "prompting",
  pendingPermission: { request_id: "r" },
} as unknown as ConnectionState

function setConn(k: string, conn: ConnectionState | undefined) {
  if (conn) h.conns.set(k, conn)
  else h.conns.delete(k)
  h.listeners.get(k)?.forEach((cb) => cb())
}

function tab(id: string): TabItem {
  return {
    id,
    kind: "conversation",
    folderId: 1,
    conversationId: null,
    agentType: "claude_code",
    title: id,
    isPinned: true,
  }
}

const switchTab = vi.fn((id: string) => {
  useTabStore.setState({ activeTabId: id })
})

function mount() {
  return render(
    <NextIntlClientProvider locale="en" messages={enMessages}>
      <PhantomAttentionStatusBar />
    </NextIntlClientProvider>
  )
}

beforeEach(() => {
  h.conns.clear()
  h.listeners.clear()
  h.openConversations.mockReset()
  switchTab.mockClear()
  useTabStore.setState({
    tabs: [tab("t1"), tab("t2"), tab("t3")],
    activeTabId: null,
    switchTab,
  })
})

afterEach(() => {
  cleanup()
})

describe("PhantomAttentionStatusBar", () => {
  it("renders nothing while no tab is blocked", () => {
    setConn("t1", { status: "prompting" } as ConnectionState)
    const { container } = mount()
    expect(container).toBeEmptyDOMElement()
  })

  it("shows the blocked count with an accessible label", () => {
    setConn("t1", BLOCKED)
    setConn("t3", BLOCKED)
    mount()
    const button = screen.getByRole("button", {
      name: "2 conversations are waiting for you. Go to the next one",
    })
    expect(button).toHaveTextContent("2 waiting")
  })

  it("appears and disappears as tabs block and unblock", () => {
    mount()
    expect(screen.queryByRole("button")).toBeNull()
    act(() => setConn("t2", BLOCKED))
    expect(screen.getByRole("button")).toHaveTextContent("1 waiting")
    act(() => setConn("t2", undefined))
    expect(screen.queryByRole("button")).toBeNull()
  })

  it("cycles through the blocked tabs on each click", () => {
    setConn("t1", BLOCKED)
    setConn("t3", BLOCKED)
    mount()
    const button = screen.getByRole("button")

    fireEvent.click(button)
    expect(switchTab).toHaveBeenLastCalledWith("t1")
    expect(h.openConversations).toHaveBeenCalledTimes(1)

    fireEvent.click(button)
    expect(switchTab).toHaveBeenLastCalledWith("t3")

    fireEvent.click(button)
    expect(switchTab).toHaveBeenLastCalledWith("t1")
  })
})
