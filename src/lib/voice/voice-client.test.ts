import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import {
  checkVoiceHealth,
  PHANTOM_VOICE_BASE_URL,
  resolveTtsLang,
  synthesizeSpeech,
  transcribeAudio,
  VoiceServiceError,
} from "./voice-client"

const originalFetch = globalThis.fetch

beforeEach(() => {
  localStorage.setItem("codeg_token", "test-token")
})

afterEach(() => {
  globalThis.fetch = originalFetch
  localStorage.removeItem("codeg_token")
  vi.restoreAllMocks()
})

describe("checkVoiceHealth", () => {
  it("resolves with the parsed health payload on 200", async () => {
    const payload = {
      status: "ok",
      stt: { model: "whisper", device: "cuda" },
      tts: { engine: "piper", voices: { es: "es-voice", en: "en-voice" } },
    }
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => payload,
    })
    globalThis.fetch = fetchMock as unknown as typeof fetch

    const health = await checkVoiceHealth()
    expect(health).toEqual(payload)
    expect(fetchMock).toHaveBeenCalledWith(
      `${PHANTOM_VOICE_BASE_URL}/health`,
      expect.objectContaining({
        method: "GET",
        headers: expect.objectContaining({
          Authorization: "Bearer test-token",
        }),
      })
    )
  })

  it("throws VoiceServiceError when the service is unreachable (network error)", async () => {
    globalThis.fetch = vi
      .fn()
      .mockRejectedValue(
        new TypeError("Failed to fetch")
      ) as unknown as typeof fetch

    await expect(checkVoiceHealth()).rejects.toBeInstanceOf(VoiceServiceError)
  })

  it("throws VoiceServiceError on a non-ok response", async () => {
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: false,
      status: 503,
      json: async () => ({}),
    }) as unknown as typeof fetch

    await expect(checkVoiceHealth()).rejects.toMatchObject({
      status: 503,
    })
  })

  it("omits the Authorization header when no token is stored", async () => {
    localStorage.removeItem("codeg_token")
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => ({ status: "ok" }),
    })
    globalThis.fetch = fetchMock as unknown as typeof fetch

    await checkVoiceHealth()
    const [, init] = fetchMock.mock.calls[0] as [string, RequestInit]
    expect(init.headers).not.toHaveProperty("Authorization")
  })
})

describe("transcribeAudio", () => {
  it("posts the raw blob and appends the lang query param", async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => ({
        text: "hola",
        language: "es",
        duration_ms: 1200,
        elapsed_ms: 300,
      }),
    })
    globalThis.fetch = fetchMock as unknown as typeof fetch

    const blob = new Blob(["fake-audio"], { type: "audio/webm;codecs=opus" })
    const result = await transcribeAudio(blob, { lang: "auto" })

    expect(result.text).toBe("hola")
    const [url, init] = fetchMock.mock.calls[0] as [URL, RequestInit]
    expect(url.toString()).toBe(`${PHANTOM_VOICE_BASE_URL}/stt?lang=auto`)
    expect(init.method).toBe("POST")
    expect(init.body).toBe(blob)
  })

  it("throws VoiceServiceError on a non-ok response", async () => {
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: false,
      status: 400,
      json: async () => ({}),
    }) as unknown as typeof fetch

    const blob = new Blob(["x"], { type: "audio/webm;codecs=opus" })
    await expect(transcribeAudio(blob)).rejects.toBeInstanceOf(
      VoiceServiceError
    )
  })
})

describe("synthesizeSpeech", () => {
  it("posts JSON and returns the wav blob", async () => {
    const wavBlob = new Blob(["riff-wav-bytes"], { type: "audio/wav" })
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      blob: async () => wavBlob,
    })
    globalThis.fetch = fetchMock as unknown as typeof fetch

    const result = await synthesizeSpeech({ text: "Hola", lang: "es" })
    expect(result).toBe(wavBlob)
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit]
    expect(url).toBe(`${PHANTOM_VOICE_BASE_URL}/tts`)
    expect(JSON.parse(init.body as string)).toEqual({
      text: "Hola",
      lang: "es",
    })
  })

  it("throws VoiceServiceError on a non-ok response", async () => {
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: false,
      status: 500,
      blob: async () => new Blob([]),
    }) as unknown as typeof fetch

    await expect(
      synthesizeSpeech({ text: "hi", lang: "en" })
    ).rejects.toBeInstanceOf(VoiceServiceError)
  })
})

describe("resolveTtsLang", () => {
  it("maps Spanish-family locales to es", () => {
    expect(resolveTtsLang("es")).toBe("es")
    expect(resolveTtsLang("es-CO")).toBe("es")
    expect(resolveTtsLang("ES")).toBe("es")
  })

  it("falls back to en for every other locale", () => {
    expect(resolveTtsLang("en")).toBe("en")
    expect(resolveTtsLang("ja")).toBe("en")
    expect(resolveTtsLang("zh-CN")).toBe("en")
    expect(resolveTtsLang("ar")).toBe("en")
  })
})
