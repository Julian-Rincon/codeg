import { act, cleanup, render, renderHook } from "@testing-library/react"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import type {
  ConnectionState,
  ConnectionStoreApi,
} from "@/contexts/acp-connections-context"
import { useTabStore, type TabItem } from "@/stores/tab-store"

const h = vi.hoisted(() => ({ store: null as unknown }))

vi.mock("@/contexts/acp-connections-context", () => ({
  useOptionalConnectionStore: () => h.store,
}))

import {
  attentionOf,
  nextBlockedTabId,
  usePhantomAttention,
  useTabAttention,
} from "./use-phantom-attention"
import {
  PhantomAttentionTitle,
  withAttentionPrefix,
} from "@/components/phantom/attention-title"

type Conn = Partial<ConnectionState>

/** A minimal keyed store with the same subscribe/notify contract. */
function makeFakeStore() {
  const conns = new Map<string, Conn>()
  const listeners = new Map<string, Set<() => void>>()
  const api = {
    getConnection: (k: string) => conns.get(k) as ConnectionState | undefined,
    subscribeKey: (k: string, cb: () => void) => {
      const set = listeners.get(k) ?? new Set()
      set.add(cb)
      listeners.set(k, set)
      return () => set.delete(cb)
    },
    getConnectPending: () => undefined,
    getConnectError: () => undefined,
    getActiveKey: () => null,
    subscribeActiveKey: () => () => {},
  } satisfies ConnectionStoreApi
  return {
    api,
    set(k: string, conn: Conn | undefined) {
      if (conn) conns.set(k, conn)
      else conns.delete(k)
      listeners.get(k)?.forEach((cb) => cb())
    },
    listenerCount: (k: string) => listeners.get(k)?.size ?? 0,
  }
}

function tab(id: string, title = id): TabItem {
  return {
    id,
    kind: "conversation",
    folderId: 1,
    conversationId: Number(id.replace(/\D/g, "")) || null,
    agentType: "claude_code",
    title,
    isPinned: true,
  }
}

const permission = { request_id: "r1" } as unknown as Conn["pendingPermission"]
const BLOCKED: Conn = { status: "prompting", pendingPermission: permission }
const RUNNING: Conn = { status: "prompting" }

let fake: ReturnType<typeof makeFakeStore>

beforeEach(() => {
  fake = makeFakeStore()
  h.store = fake.api
  useTabStore.setState({ tabs: [], activeTabId: null })
})

afterEach(() => {
  cleanup()
})

describe("attentionOf", () => {
  it("is null without a connection or while idle", () => {
    expect(attentionOf(undefined)).toBeNull()
    expect(attentionOf({ status: "connected" } as ConnectionState)).toBeNull()
  })

  it("reports running while prompting and blocked over running", () => {
    expect(attentionOf(RUNNING as ConnectionState)).toBe("running")
    expect(attentionOf(BLOCKED as ConnectionState)).toBe("blocked")
  })

  it("ignores a question set with no questions", () => {
    const conn = {
      status: "connected",
      pendingAskQuestion: { question_id: "q", questions: [] },
    } as unknown as ConnectionState
    expect(attentionOf(conn)).toBeNull()
  })
})

describe("usePhantomAttention", () => {
  it("splits open tabs into blocked and running, in tab order", () => {
    useTabStore.setState({ tabs: [tab("t1"), tab("t2"), tab("t3")] })
    fake.set("t1", RUNNING)
    fake.set("t3", BLOCKED)
    const { result } = renderHook(() => usePhantomAttention())
    expect(result.current.blocked.map((t) => t.id)).toEqual(["t3"])
    expect(result.current.running.map((t) => t.id)).toEqual(["t1"])
  })

  it("re-renders when a tab's connection changes", () => {
    useTabStore.setState({ tabs: [tab("t1"), tab("t2")] })
    const { result } = renderHook(() => usePhantomAttention())
    expect(result.current.blocked).toEqual([])

    act(() => fake.set("t2", BLOCKED))
    expect(result.current.blocked.map((t) => t.id)).toEqual(["t2"])

    act(() => fake.set("t2", RUNNING))
    expect(result.current.blocked).toEqual([])
    expect(result.current.running.map((t) => t.id)).toEqual(["t2"])
  })

  it("follows tabs being opened and closed", () => {
    useTabStore.setState({ tabs: [tab("t1")] })
    const { result } = renderHook(() => usePhantomAttention())
    expect(fake.listenerCount("t1")).toBe(1)

    act(() => useTabStore.setState({ tabs: [tab("t1"), tab("t2")] }))
    expect(fake.listenerCount("t2")).toBe(1)
    act(() => fake.set("t2", BLOCKED))
    expect(result.current.blocked.map((t) => t.id)).toEqual(["t2"])

    act(() => useTabStore.setState({ tabs: [tab("t1")] }))
    expect(fake.listenerCount("t2")).toBe(0)
    expect(result.current.blocked).toEqual([])
  })

  it("is empty outside the connections provider", () => {
    h.store = null
    useTabStore.setState({ tabs: [tab("t1")] })
    const { result } = renderHook(() => usePhantomAttention())
    expect(result.current).toEqual({ blocked: [], running: [] })
  })
})

describe("useTabAttention", () => {
  it("tracks one tab's state", () => {
    const { result } = renderHook(() => useTabAttention("t1"))
    expect(result.current).toBeNull()
    act(() => fake.set("t1", RUNNING))
    expect(result.current).toBe("running")
    act(() => fake.set("t1", BLOCKED))
    expect(result.current).toBe("blocked")
    act(() => fake.set("t1", undefined))
    expect(result.current).toBeNull()
  })
})

describe("nextBlockedTabId", () => {
  const blocked = [{ id: "a" }, { id: "b" }, { id: "c" }]
  it("starts at the first when the active tab is not blocked", () => {
    expect(nextBlockedTabId(blocked, null)).toBe("a")
    expect(nextBlockedTabId(blocked, "zzz")).toBe("a")
  })
  it("advances and wraps", () => {
    expect(nextBlockedTabId(blocked, "a")).toBe("b")
    expect(nextBlockedTabId(blocked, "c")).toBe("a")
  })
  it("is null with nothing blocked", () => {
    expect(nextBlockedTabId([], "a")).toBeNull()
  })
})

describe("PhantomAttentionTitle", () => {
  it("sets, replaces and strips the (N) prefix", () => {
    expect(withAttentionPrefix("Repo - Phantom", 2)).toBe("(2) Repo - Phantom")
    expect(withAttentionPrefix("(2) Repo - Phantom", 3)).toBe(
      "(3) Repo - Phantom"
    )
    expect(withAttentionPrefix("(3) Repo - Phantom", 0)).toBe("Repo - Phantom")
  })

  it("prefixes document.title while tabs are blocked and restores it", () => {
    document.title = "Repo - Phantom"
    useTabStore.setState({ tabs: [tab("t1"), tab("t2")] })
    render(<PhantomAttentionTitle />)
    expect(document.title).toBe("Repo - Phantom")

    act(() => {
      fake.set("t1", BLOCKED)
      fake.set("t2", BLOCKED)
    })
    expect(document.title).toBe("(2) Repo - Phantom")

    act(() => fake.set("t1", RUNNING))
    expect(document.title).toBe("(1) Repo - Phantom")

    act(() => fake.set("t2", undefined))
    expect(document.title).toBe("Repo - Phantom")
  })
})
