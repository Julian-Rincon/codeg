import { act, renderHook, waitFor } from "@testing-library/react"
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import { useVoiceLive } from "./use-voice-live"

const mockCheckVoiceHealth = vi.fn()
const mockTranscribeAudio = vi.fn()
const mockSynthesizeSpeech = vi.fn()

vi.mock("@/lib/voice/voice-client", async () => {
  const actual = await vi.importActual<
    typeof import("@/lib/voice/voice-client")
  >("@/lib/voice/voice-client")
  return {
    ...actual,
    checkVoiceHealth: (...args: unknown[]) => mockCheckVoiceHealth(...args),
    transcribeAudio: (...args: unknown[]) => mockTranscribeAudio(...args),
    synthesizeSpeech: (...args: unknown[]) => mockSynthesizeSpeech(...args),
  }
})

// --- Controllable browser API mocks ---------------------------------------

/** Shared amplitude the mock AnalyserNode reports on the NEXT frame pump —
 *  lets a test drive the VAD deterministically without a real mic. */
let mockAnalyserAmplitude = 0

class MockAnalyserNode {
  fftSize = 1024
  connect() {}
  getFloatTimeDomainData(arr: Float32Array) {
    arr.fill(mockAnalyserAmplitude)
  }
}

class MockAudioContext {
  state = "running"
  destination = {}
  createMediaStreamSource() {
    return { connect: () => {} }
  }
  createMediaElementSource() {
    return { connect: () => {} }
  }
  createAnalyser() {
    return new MockAnalyserNode()
  }
  resume() {
    return Promise.resolve()
  }
}

class MockAudioElement {
  src = ""
  private listeners: Record<string, Array<() => void>> = {}
  addEventListener(type: string, cb: () => void) {
    ;(this.listeners[type] ??= []).push(cb)
  }
  removeEventListener(type: string, cb: () => void) {
    this.listeners[type] = (this.listeners[type] ?? []).filter((l) => l !== cb)
  }
  removeAttribute() {
    this.src = ""
  }
  pause() {}
  play() {
    // Auto-"end" playback on the next microtask so queue-draining tests
    // don't hang waiting for a real audio element.
    queueMicrotask(() => this.listeners.ended?.forEach((cb) => cb()))
    return Promise.resolve()
  }
}

class MockMediaRecorder {
  static isTypeSupported() {
    return true
  }
  state: "inactive" | "recording" = "inactive"
  mimeType = "audio/webm;codecs=opus"
  ondataavailable: ((e: { data: Blob }) => void) | null = null
  onstop: (() => void) | null = null
  start() {
    this.state = "recording"
  }
  stop() {
    this.state = "inactive"
    this.ondataavailable?.({
      data: new Blob(["fake-audio"], { type: "audio/webm;codecs=opus" }),
    })
    this.onstop?.()
  }
}

let rafCallback: FrameRequestCallback | null = null
let rafId = 0
let clockMs = 0

function pumpFrame(amplitude: number, advanceMs: number) {
  mockAnalyserAmplitude = amplitude
  clockMs += advanceMs
  const cb = rafCallback
  rafCallback = null
  cb?.(clockMs)
}

function baseOptions(
  overrides: Partial<Parameters<typeof useVoiceLive>[0]> = {}
) {
  return {
    locale: "en",
    isAgentBusy: false,
    liveAssistantText: "",
    awaitingUserAction: false,
    sendText: vi.fn(),
    codeBlockNote: "Code block.",
    permissionCue: "I need your permission on screen.",
    ...overrides,
  }
}

beforeEach(() => {
  mockAnalyserAmplitude = 0
  rafCallback = null
  rafId = 0
  clockMs = 0
  // jsdom doesn't implement the Blob URL registry; the hook only needs a
  // unique-ish string to assign as the mock <audio>'s `src`.
  URL.createObjectURL ??= vi.fn(() => "blob:mock-url")
  URL.revokeObjectURL ??= vi.fn()
  mockCheckVoiceHealth.mockReset().mockResolvedValue({
    status: "ok",
    stt: { model: "m", device: "cpu" },
    tts: { engine: "e", voices: { es: "es", en: "en" } },
  })
  mockTranscribeAudio.mockReset().mockResolvedValue({
    text: "hola mundo",
    language: "es",
    duration_ms: 500,
    elapsed_ms: 100,
  })
  mockSynthesizeSpeech
    .mockReset()
    .mockResolvedValue(new Blob(["wav"], { type: "audio/wav" }))

  vi.stubGlobal("navigator", {
    mediaDevices: {
      getUserMedia: vi.fn().mockResolvedValue({
        getTracks: () => [{ stop: vi.fn() }],
      }),
    },
  })
  vi.stubGlobal("AudioContext", MockAudioContext)
  vi.stubGlobal("Audio", MockAudioElement)
  vi.stubGlobal("MediaRecorder", MockMediaRecorder)
  vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback): number => {
    rafCallback = cb
    rafId += 1
    return rafId
  })
  vi.stubGlobal("cancelAnimationFrame", () => {
    rafCallback = null
  })
  vi.spyOn(performance, "now").mockImplementation(() => clockMs)
})

afterEach(() => {
  vi.unstubAllGlobals()
  vi.restoreAllMocks()
})

describe("useVoiceLive — availability gating", () => {
  it("goes to unavailable/service-unreachable when the health check fails", async () => {
    mockCheckVoiceHealth.mockRejectedValue(new Error("offline"))
    const { result } = renderHook(() => useVoiceLive(baseOptions()))

    act(() => result.current.open())
    await waitFor(() => expect(result.current.phase).toBe("unavailable"))
    expect(result.current.unavailableReason).toBe("service-unreachable")
  })

  it("goes to unavailable/mic-permission-denied when getUserMedia is rejected", async () => {
    vi.stubGlobal("navigator", {
      mediaDevices: {
        getUserMedia: vi.fn().mockRejectedValue(new Error("denied")),
      },
    })
    const { result } = renderHook(() => useVoiceLive(baseOptions()))

    act(() => result.current.open())
    await waitFor(() => expect(result.current.phase).toBe("unavailable"))
    expect(result.current.unavailableReason).toBe("mic-permission-denied")
  })

  it("reaches listening once health + mic both succeed", async () => {
    const { result } = renderHook(() => useVoiceLive(baseOptions()))

    act(() => result.current.open())
    await waitFor(() => expect(result.current.phase).toBe("listening"))
    expect(result.current.isOpen).toBe(true)
  })
})

describe("useVoiceLive — open/close lifecycle", () => {
  it("stops mic tracks and resets phase on close", async () => {
    const stopTrack = vi.fn()
    vi.stubGlobal("navigator", {
      mediaDevices: {
        getUserMedia: vi
          .fn()
          .mockResolvedValue({ getTracks: () => [{ stop: stopTrack }] }),
      },
    })
    const { result } = renderHook(() => useVoiceLive(baseOptions()))

    act(() => result.current.open())
    await waitFor(() => expect(result.current.phase).toBe("listening"))

    act(() => result.current.close())
    expect(result.current.isOpen).toBe(false)
    expect(stopTrack).toHaveBeenCalled()
  })

  it("toggleMute flips the muted flag", async () => {
    const { result } = renderHook(() => useVoiceLive(baseOptions()))
    expect(result.current.muted).toBe(false)
    act(() => result.current.toggleMute())
    expect(result.current.muted).toBe(true)
    act(() => result.current.toggleMute())
    expect(result.current.muted).toBe(false)
  })
})

describe("useVoiceLive — full utterance -> send -> speak cycle", () => {
  it("detects speech via VAD, transcribes it, and sends it through sendText (the same path as typing + Enter)", async () => {
    const sendText = vi.fn()
    const { result } = renderHook(() => useVoiceLive(baseOptions({ sendText })))

    act(() => result.current.open())
    await waitFor(() => expect(result.current.phase).toBe("listening"))

    // Warm up the noise floor with a couple of silent frames, then speak.
    act(() => pumpFrame(0.0005, 50))
    act(() => pumpFrame(0.0005, 50))
    act(() => pumpFrame(0.5, 50)) // speech-start
    act(() => pumpFrame(0.5, 400)) // still speaking (400ms utterance)
    // Trailing silence past the 700ms hangover ends the utterance.
    act(() => pumpFrame(0.0005, 400))
    act(() => pumpFrame(0.0005, 400))

    await waitFor(() => expect(mockTranscribeAudio).toHaveBeenCalled())
    await waitFor(() => expect(sendText).toHaveBeenCalledWith("hola mundo"))
    expect(result.current.lastUserTranscript).toBe("hola mundo")
    expect(result.current.phase).toBe("thinking")
  })

  it("speaks completed sentences as the assistant's live text streams in, then returns to listening", async () => {
    const { result, rerender } = renderHook(
      (props: Partial<Parameters<typeof useVoiceLive>[0]>) =>
        useVoiceLive(baseOptions({ isAgentBusy: true, ...props })),
      { initialProps: {} }
    )

    act(() => result.current.open())
    await waitFor(() => expect(result.current.phase).toBe("listening"))

    // Drive the hook into "thinking" the same way a real utterance would:
    // warm noise floor, speak, then go silent past the hangover.
    act(() => pumpFrame(0.0005, 50))
    act(() => pumpFrame(0.5, 50))
    act(() => pumpFrame(0.5, 400))
    act(() => pumpFrame(0.0005, 400))
    act(() => pumpFrame(0.0005, 400))
    await waitFor(() => expect(result.current.phase).toBe("thinking"))

    // Assistant streams a first complete sentence.
    rerender({ isAgentBusy: true, liveAssistantText: "Hello there." })
    await waitFor(() => expect(result.current.phase).toBe("speaking"))
    expect(mockSynthesizeSpeech).toHaveBeenCalledWith(
      expect.objectContaining({ text: "Hello there." }),
      expect.anything()
    )

    // Turn ends — the hook flushes and, once the queue drains, returns to
    // listening for the next turn (continuous conversation).
    rerender({ isAgentBusy: false, liveAssistantText: "Hello there." })
    await waitFor(() => expect(result.current.phase).toBe("listening"))
  })
})

describe("useVoiceLive — cancelTurn", () => {
  it("invokes the composer's onCancelTurn and stops speaking", async () => {
    const onCancelTurn = vi.fn()
    const { result } = renderHook(() =>
      useVoiceLive(baseOptions({ onCancelTurn }))
    )
    act(() => result.current.open())
    await waitFor(() => expect(result.current.phase).toBe("listening"))

    act(() => result.current.cancelTurn())
    expect(onCancelTurn).toHaveBeenCalled()
  })
})
