// Thin fetch client for the local `phantom-voice` speech backend (STT/TTS).
// This is a SEPARATE local service from the codeg server, but reuses the same
// bearer token the web UI already stores for codeg itself (`codeg_token`) —
// see the contract note in `use-voice-live.ts`. Every call takes an
// AbortSignal so the caller (the voice state machine) can cancel a stale
// request on barge-in / voice-mode close without leaking a pending fetch.

import { getCodegToken } from "@/lib/transport/web-auth"

/** Default local endpoint for the voice backend. Kept as a single constant
 *  (rather than scattering the literal) so a future settings toggle only
 *  needs to change the resolution of this one value. */
export const PHANTOM_VOICE_BASE_URL = "http://127.0.0.1:3091"

export class VoiceServiceError extends Error {
  readonly status?: number
  readonly cause?: unknown
  constructor(message: string, options?: { status?: number; cause?: unknown }) {
    super(message)
    this.name = "VoiceServiceError"
    this.status = options?.status
    this.cause = options?.cause
  }
}

export interface VoiceHealth {
  status: string
  stt: { model: string; device: string }
  tts: { engine: string; voices: { es: string; en: string } }
}

export type VoiceLang = "es" | "en" | "auto"

export interface SttResult {
  text: string
  language: string
  duration_ms: number
  elapsed_ms: number
}

export interface TtsRequest {
  text: string
  lang: "es" | "en"
  voice?: string
  speed?: number
}

export interface VoiceClientOptions {
  baseUrl?: string
  signal?: AbortSignal
}

function authHeaders(): Record<string, string> {
  const token = getCodegToken()
  return token ? { Authorization: `Bearer ${token}` } : {}
}

function resolveBaseUrl(baseUrl?: string): string {
  return baseUrl ?? PHANTOM_VOICE_BASE_URL
}

/** GET /health — used to gate opening voice mode: if this fails, the UI must
 *  show a clear "local voice service isn't running" message, never crash. */
export async function checkVoiceHealth(
  options: VoiceClientOptions = {}
): Promise<VoiceHealth> {
  let res: Response
  try {
    res = await fetch(`${resolveBaseUrl(options.baseUrl)}/health`, {
      method: "GET",
      headers: authHeaders(),
      signal: options.signal,
    })
  } catch (err) {
    throw new VoiceServiceError("Voice service unreachable", { cause: err })
  }
  if (!res.ok) {
    throw new VoiceServiceError(`Voice service health check failed`, {
      status: res.status,
    })
  }
  return (await res.json()) as VoiceHealth
}

/** POST /stt — `audio` must be `audio/webm;codecs=opus` (what
 *  `MediaRecorder` produces by default in Chromium/Brave). */
export async function transcribeAudio(
  audio: Blob,
  options: VoiceClientOptions & { lang?: VoiceLang } = {}
): Promise<SttResult> {
  const url = new URL(`${resolveBaseUrl(options.baseUrl)}/stt`)
  if (options.lang) url.searchParams.set("lang", options.lang)

  let res: Response
  try {
    res = await fetch(url, {
      method: "POST",
      headers: {
        ...authHeaders(),
        "Content-Type": audio.type || "audio/webm;codecs=opus",
      },
      body: audio,
      signal: options.signal,
    })
  } catch (err) {
    throw new VoiceServiceError("Speech-to-text request failed", {
      cause: err,
    })
  }
  if (!res.ok) {
    throw new VoiceServiceError("Speech-to-text request failed", {
      status: res.status,
    })
  }
  return (await res.json()) as SttResult
}

/** POST /tts — returns the raw `audio/wav` body as a Blob, ready for an
 *  `HTMLAudioElement` src via `URL.createObjectURL`. */
export async function synthesizeSpeech(
  request: TtsRequest,
  options: VoiceClientOptions = {}
): Promise<Blob> {
  let res: Response
  try {
    res = await fetch(`${resolveBaseUrl(options.baseUrl)}/tts`, {
      method: "POST",
      headers: {
        ...authHeaders(),
        "Content-Type": "application/json",
      },
      body: JSON.stringify(request),
      signal: options.signal,
    })
  } catch (err) {
    throw new VoiceServiceError("Text-to-speech request failed", {
      cause: err,
    })
  }
  if (!res.ok) {
    throw new VoiceServiceError("Text-to-speech request failed", {
      status: res.status,
    })
  }
  return await res.blob()
}

/** Map the app's next-intl locale to the voice backend's TTS language code.
 *  Only `es`/`en` are supported server-side, so every other UI locale falls
 *  back to English (matches the spec: "es -> es, else en"). STT itself
 *  always requests `lang: "auto"` regardless of UI locale — the user may
 *  speak either language and the backend detects it. */
export function resolveTtsLang(locale: string): "es" | "en" {
  return locale.toLowerCase().startsWith("es") ? "es" : "en"
}
