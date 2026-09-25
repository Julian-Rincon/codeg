// The voice-live state machine: idle -> listening -> transcribing ->
// thinking -> speaking -> listening (continuous conversation), driven by a
// real mic (VAD) and the local phantom-voice STT/TTS backend.
//
// Decoupled from the composer's send path on purpose: the caller supplies
// `sendText`, which the composer wires to the EXACT SAME `onSend` callback
// Enter/the send button already use (see `message-input.tsx`), so a voice
// turn behaves identically to a typed one (same agent, permissions, history).
// Likewise `liveAssistantText`/`isAgentBusy` are read by the caller from the
// conversation runtime store and passed in, rather than this hook reaching
// into app state itself — keeps it testable and reusable.
//
// Audio levels (`micLevelRef`/`ttsLevelRef`) are exposed as REFS, not React
// state: they update on every animation frame and are meant to be read
// directly by the orb's own rAF loop, not to trigger a React re-render per
// frame.

"use client"

import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type RefObject,
} from "react"
import {
  checkVoiceHealth,
  resolveTtsLang,
  synthesizeSpeech,
  transcribeAudio,
  type VoiceClientOptions,
} from "@/lib/voice/voice-client"
import {
  computeRms,
  createInitialVadState,
  stepVad,
  type VadEvent,
  type VadState,
} from "@/lib/voice/vad"
import { SentenceChunker } from "@/lib/voice/sentence-chunker"

export type VoicePhase =
  | "checking-health"
  | "unavailable"
  | "listening"
  | "transcribing"
  | "thinking"
  | "speaking"

export type VoiceUnavailableReason =
  | "service-unreachable"
  | "mic-permission-denied"

export interface UseVoiceLiveOptions {
  /** App locale (next-intl `useLocale()`). Drives TTS language selection. */
  locale: string
  /** `true` while the active connection has a turn in flight
   *  (`conn.status === "prompting"`). */
  isAgentBusy: boolean
  /** Accumulated plain text of the assistant's current streaming turn ("" when
   *  no turn is live). The hook diffs this against what it already spoke. */
  liveAssistantText: string
  /** A permission/question/plan-approval is blocking the agent. */
  awaitingUserAction: boolean
  /** Deliver transcribed text through the SAME path as typing + Enter. */
  sendText: (text: string) => void
  /** The composer's existing cancel/stop-turn function, if the session has
   *  one. Wired to the explicit "stop" UI action only — barge-in never calls
   *  this automatically. */
  onCancelTurn?: () => void
  /** Spoken in place of a fenced code block ("Code block.", localized). */
  codeBlockNote: string
  /** Spoken once when the agent blocks on a permission/question
   *  ("I need your permission on screen.", localized). */
  permissionCue: string
  /** Override for tests / alternate deployments. */
  baseUrl?: string
}

export interface UseVoiceLiveResult {
  isOpen: boolean
  open: () => void
  close: () => void
  /** Alias for `close`, named for the UI's "end voice mode" control / Esc. */
  endVoiceMode: () => void
  phase: VoicePhase
  /** 0..1, updated every animation frame — read directly, not reactive. */
  micLevelRef: RefObject<number>
  /** 0..1, updated every animation frame — read directly, not reactive. */
  ttsLevelRef: RefObject<number>
  muted: boolean
  toggleMute: () => void
  /** Stop TTS playback/queue immediately; does not end voice mode or cancel
   *  the agent turn. */
  stopSpeaking: () => void
  /** Stop TTS AND cancel the in-flight agent turn (via `onCancelTurn`), when
   *  one is available. */
  cancelTurn: () => void
  lastUserTranscript: string
  /** The sentence currently being spoken (or about to play next). */
  spokenCaption: string
  awaitingUserAction: boolean
  unavailableReason: VoiceUnavailableReason | null
}

interface TtsQueueItem {
  id: string
  text: string
  blobPromise?: Promise<Blob | null>
}

let ttsIdCounter = 0
function nextTtsId(): string {
  ttsIdCounter += 1
  return `voice-tts-${ttsIdCounter}`
}

function isAbortError(err: unknown): boolean {
  return err instanceof DOMException && err.name === "AbortError"
}

// Scales AnalyserNode RMS (typically well under 1.0 for normal speech) into
// a roomier 0..1 range for visual feedback (orb pulse), rather than the raw
// signal, which tends to only occupy the low end of the range visually.
const MIC_LEVEL_GAIN = 6
const TTS_LEVEL_GAIN = 3.5
const MIC_POLL_FFT_SIZE = 1024
const TTS_POLL_FFT_SIZE = 512

type AudioCtxCtor = typeof AudioContext

function getAudioContextCtor(): AudioCtxCtor | null {
  if (typeof window === "undefined") return null
  return (
    window.AudioContext ||
    (window as unknown as { webkitAudioContext?: AudioCtxCtor })
      .webkitAudioContext ||
    null
  )
}

function pickRecorderMimeType(): string | undefined {
  if (typeof MediaRecorder === "undefined") return undefined
  if (MediaRecorder.isTypeSupported?.("audio/webm;codecs=opus")) {
    return "audio/webm;codecs=opus"
  }
  return undefined
}

export function useVoiceLive(options: UseVoiceLiveOptions): UseVoiceLiveResult {
  const {
    locale,
    isAgentBusy,
    liveAssistantText,
    awaitingUserAction,
    sendText,
    onCancelTurn,
    codeBlockNote,
    permissionCue,
    baseUrl,
  } = options

  const [isOpen, setIsOpen] = useState(false)
  const [phase, setPhase] = useState<VoicePhase>("checking-health")
  const [muted, setMuted] = useState(false)
  const [lastUserTranscript, setLastUserTranscript] = useState("")
  const [spokenCaption, setSpokenCaption] = useState("")
  const [unavailableReason, setUnavailableReason] =
    useState<VoiceUnavailableReason | null>(null)

  // Mirrors of state/props read from async callbacks and the rAF loop, which
  // would otherwise close over stale values.
  const isOpenRef = useRef(false)
  const phaseRef = useRef<VoicePhase>("checking-health")
  const mutedRef = useRef(false)
  const isAgentBusyRef = useRef(isAgentBusy)
  const localeRef = useRef(locale)
  const sendTextRef = useRef(sendText)
  const onCancelTurnRef = useRef(onCancelTurn)
  const codeBlockNoteRef = useRef(codeBlockNote)
  const permissionCueRef = useRef(permissionCue)
  const suppressSpeechForTurnRef = useRef(false)
  const prevAwaitingRef = useRef(false)
  const prevBusyRef = useRef(isAgentBusy)

  useEffect(() => {
    isAgentBusyRef.current = isAgentBusy
  }, [isAgentBusy])
  useEffect(() => {
    localeRef.current = locale
  }, [locale])
  useEffect(() => {
    sendTextRef.current = sendText
  }, [sendText])
  useEffect(() => {
    onCancelTurnRef.current = onCancelTurn
  }, [onCancelTurn])
  useEffect(() => {
    codeBlockNoteRef.current = codeBlockNote
  }, [codeBlockNote])
  useEffect(() => {
    permissionCueRef.current = permissionCue
  }, [permissionCue])

  const setPhaseState = useCallback((next: VoicePhase) => {
    phaseRef.current = next
    setPhase(next)
  }, [])

  // Mic capture + VAD
  const micStreamRef = useRef<MediaStream | null>(null)
  const audioCtxRef = useRef<AudioContext | null>(null)
  const micAnalyserRef = useRef<AnalyserNode | null>(null)
  const micDataRef = useRef<Float32Array | null>(null)
  const micRafRef = useRef<number | null>(null)
  const vadStateRef = useRef<VadState>(createInitialVadState())
  const micLevelRef = useRef(0)

  // Utterance recording
  const mediaRecorderRef = useRef<MediaRecorder | null>(null)
  const recordedChunksRef = useRef<Blob[]>([])
  const sttAbortRef = useRef<AbortController | null>(null)

  // Sentence chunking + TTS playback
  const chunkerRef = useRef<SentenceChunker | null>(null)
  const ttsQueueRef = useRef<TtsQueueItem[]>([])
  const ttsRunningRef = useRef(false)
  const ttsAbortRef = useRef<AbortController | null>(null)
  const ttsAudioElRef = useRef<HTMLAudioElement | null>(null)
  const ttsAudioCtxRef = useRef<AudioContext | null>(null)
  const ttsAnalyserRef = useRef<AnalyserNode | null>(null)
  const ttsDataRef = useRef<Float32Array | null>(null)
  const ttsRafRef = useRef<number | null>(null)
  const ttsLevelRef = useRef(0)

  const clientOpts = useCallback(
    (signal?: AbortSignal): VoiceClientOptions => ({ baseUrl, signal }),
    [baseUrl]
  )

  // --- TTS level metering (own rAF loop, paused when nothing is playing) ---
  const stopTtsLevelLoop = useCallback(() => {
    if (ttsRafRef.current != null) {
      cancelAnimationFrame(ttsRafRef.current)
      ttsRafRef.current = null
    }
    ttsLevelRef.current = 0
  }, [])

  const runTtsLevelLoop = useCallback(() => {
    const analyser = ttsAnalyserRef.current
    const data = ttsDataRef.current
    if (!analyser || !data) return
    analyser.getFloatTimeDomainData(data)
    const rms = computeRms(data)
    ttsLevelRef.current = Math.min(1, rms * TTS_LEVEL_GAIN)
    ttsRafRef.current = requestAnimationFrame(runTtsLevelLoop)
  }, [])

  const ensureTtsAudioGraph = useCallback((): HTMLAudioElement | null => {
    if (ttsAudioElRef.current) return ttsAudioElRef.current
    if (typeof Audio === "undefined") return null
    const audioEl = new Audio()
    ttsAudioElRef.current = audioEl
    const Ctor = getAudioContextCtor()
    if (!Ctor) return audioEl
    try {
      const ctx = ttsAudioCtxRef.current ?? new Ctor()
      ttsAudioCtxRef.current = ctx
      // A given <audio> element can only ever be wrapped by ONE
      // MediaElementAudioSourceNode — the element is created once and reused
      // across the whole voice session (src is swapped per clip) precisely
      // so this only runs once.
      const source = ctx.createMediaElementSource(audioEl)
      const analyser = ctx.createAnalyser()
      analyser.fftSize = TTS_POLL_FFT_SIZE
      source.connect(analyser)
      analyser.connect(ctx.destination)
      ttsAnalyserRef.current = analyser
      ttsDataRef.current = new Float32Array(analyser.fftSize)
    } catch {
      // Web Audio graph is a progressive enhancement for the orb's level
      // meter; playback itself (below) doesn't depend on it.
    }
    return audioEl
  }, [])

  const playBlob = useCallback(
    (blob: Blob, signal: AbortSignal): Promise<void> => {
      const audioEl = ensureTtsAudioGraph()
      if (!audioEl) return Promise.resolve()
      const url = URL.createObjectURL(blob)
      audioEl.src = url
      runTtsLevelLoop()
      return new Promise((resolve) => {
        let settled = false
        const finish = () => {
          if (settled) return
          settled = true
          audioEl.removeEventListener("ended", onEnded)
          audioEl.removeEventListener("error", onEnded)
          signal.removeEventListener("abort", onAbort)
          URL.revokeObjectURL(url)
          stopTtsLevelLoop()
          resolve()
        }
        const onEnded = () => finish()
        const onAbort = () => {
          audioEl.pause()
          finish()
        }
        audioEl.addEventListener("ended", onEnded)
        audioEl.addEventListener("error", onEnded)
        signal.addEventListener("abort", onAbort)
        void audioEl.play().catch(() => finish())
      })
    },
    [ensureTtsAudioGraph, runTtsLevelLoop, stopTtsLevelLoop]
  )

  const stopSpeakingInternal = useCallback(() => {
    ttsAbortRef.current?.abort()
    ttsQueueRef.current = []
    const audioEl = ttsAudioElRef.current
    if (audioEl) {
      audioEl.pause()
      audioEl.removeAttribute("src")
    }
    stopTtsLevelLoop()
    setSpokenCaption("")
  }, [stopTtsLevelLoop])

  const fetchSentenceAudio = useCallback(
    (text: string): Promise<Blob | null> =>
      synthesizeSpeech(
        { text, lang: resolveTtsLang(localeRef.current) },
        clientOpts()
      ).catch(() => null),
    [clientOpts]
  )

  const prefetchNext = useCallback(() => {
    const next = ttsQueueRef.current[1]
    if (next && !next.blobPromise) {
      next.blobPromise = fetchSentenceAudio(next.text)
    }
  }, [fetchSentenceAudio])

  const runTtsQueue = useCallback(async () => {
    if (ttsRunningRef.current) return
    ttsRunningRef.current = true
    try {
      while (ttsQueueRef.current.length > 0) {
        if (mutedRef.current || suppressSpeechForTurnRef.current) {
          ttsQueueRef.current = []
          break
        }
        const item = ttsQueueRef.current[0]
        setSpokenCaption(item.text)
        setPhaseState("speaking")
        const controller = new AbortController()
        ttsAbortRef.current = controller
        const blob = await (item.blobPromise ?? fetchSentenceAudio(item.text))
        if (controller.signal.aborted) break
        // Drop the item we just handled, then kick off the next prefetch
        // immediately so it's ready by the time THIS sentence finishes.
        ttsQueueRef.current = ttsQueueRef.current.slice(1)
        prefetchNext()
        if (!blob) continue // synth failed for this sentence: skip, try next
        await playBlob(blob, controller.signal)
      }
    } finally {
      ttsRunningRef.current = false
      if (isOpenRef.current && !suppressSpeechForTurnRef.current) {
        if (ttsQueueRef.current.length === 0) {
          setPhaseState(isAgentBusyRef.current ? "thinking" : "listening")
          setSpokenCaption("")
        }
      }
    }
  }, [fetchSentenceAudio, playBlob, prefetchNext, setPhaseState])

  const enqueueSentence = useCallback(
    (text: string) => {
      const trimmed = text.trim()
      if (!trimmed) return
      ttsQueueRef.current = [
        ...ttsQueueRef.current,
        { id: nextTtsId(), text: trimmed },
      ]
      if (ttsQueueRef.current.length === 2) prefetchNext()
      void runTtsQueue()
    },
    [prefetchNext, runTtsQueue]
  )

  // --- Utterance recording (one MediaRecorder per detected utterance) ---
  const startRecordingUtterance = useCallback(() => {
    const stream = micStreamRef.current
    if (!stream || typeof MediaRecorder === "undefined") return
    try {
      const mimeType = pickRecorderMimeType()
      const recorder = mimeType
        ? new MediaRecorder(stream, { mimeType })
        : new MediaRecorder(stream)
      recordedChunksRef.current = []
      recorder.ondataavailable = (e: BlobEvent) => {
        if (e.data.size > 0) recordedChunksRef.current.push(e.data)
      }
      mediaRecorderRef.current = recorder
      recorder.start()
    } catch {
      // Recorder failed to start (unsupported format, device gone mid-call
      // etc.) — the VAD keeps running and simply won't capture this
      // utterance; the user can just speak again.
    }
  }, [])

  const transcribeUtterance = useCallback(
    async (blob: Blob) => {
      setPhaseState("transcribing")
      const controller = new AbortController()
      sttAbortRef.current = controller
      try {
        const result = await transcribeAudio(blob, {
          lang: "auto",
          ...clientOpts(controller.signal),
        })
        const text = result.text.trim()
        if (!isOpenRef.current) return
        if (!text) {
          setPhaseState("listening")
          return
        }
        setLastUserTranscript(text)
        chunkerRef.current = new SentenceChunker({
          codeBlockNote: codeBlockNoteRef.current,
        })
        suppressSpeechForTurnRef.current = false
        setSpokenCaption("")
        setPhaseState("thinking")
        sendTextRef.current(text)
      } catch (err) {
        if (isAbortError(err)) return
        if (isOpenRef.current) setPhaseState("listening")
      }
    },
    [clientOpts, setPhaseState]
  )

  const stopRecordingUtterance = useCallback(
    (shouldTranscribe: boolean) => {
      const recorder = mediaRecorderRef.current
      if (!recorder || recorder.state === "inactive") return
      recorder.onstop = () => {
        const blob = new Blob(recordedChunksRef.current, {
          type: recorder.mimeType || "audio/webm;codecs=opus",
        })
        recordedChunksRef.current = []
        if (shouldTranscribe && blob.size > 0) void transcribeUtterance(blob)
      }
      try {
        recorder.stop()
      } catch {
        // Already stopped/inactive — nothing to clean up.
      }
    },
    [transcribeUtterance]
  )

  const handleVadEvent = useCallback(
    (event: VadEvent) => {
      if (!event) return
      if (event.type === "speech-start") {
        // Barge-in: talking over the assistant stops playback immediately
        // and returns to listening, WITHOUT cancelling the agent's turn.
        if (phaseRef.current === "speaking" || ttsQueueRef.current.length > 0) {
          stopSpeakingInternal()
          suppressSpeechForTurnRef.current = true
          setPhaseState("listening")
        }
        startRecordingUtterance()
      } else if (event.type === "speech-end") {
        stopRecordingUtterance(true)
      } else if (event.type === "speech-rejected") {
        stopRecordingUtterance(false)
      }
    },
    [
      setPhaseState,
      startRecordingUtterance,
      stopRecordingUtterance,
      stopSpeakingInternal,
    ]
  )

  // --- Mic level + VAD poll loop ---
  const runMicLoop = useCallback(() => {
    const analyser = micAnalyserRef.current
    const data = micDataRef.current
    if (!analyser || !data) return
    analyser.getFloatTimeDomainData(data)
    const rms = computeRms(data)
    if (mutedRef.current) {
      micLevelRef.current = 0
    } else {
      micLevelRef.current = Math.min(1, rms * MIC_LEVEL_GAIN)
      const { state, event } = stepVad(
        vadStateRef.current,
        rms,
        typeof performance !== "undefined" ? performance.now() : Date.now()
      )
      vadStateRef.current = state
      handleVadEvent(event)
    }
    micRafRef.current = requestAnimationFrame(runMicLoop)
  }, [handleVadEvent])

  const startMic = useCallback(async () => {
    const stream = await navigator.mediaDevices.getUserMedia({ audio: true })
    micStreamRef.current = stream
    const Ctor = getAudioContextCtor()
    if (Ctor) {
      const ctx = audioCtxRef.current ?? new Ctor()
      if (ctx.state === "suspended") await ctx.resume().catch(() => {})
      audioCtxRef.current = ctx
      const source = ctx.createMediaStreamSource(stream)
      const analyser = ctx.createAnalyser()
      analyser.fftSize = MIC_POLL_FFT_SIZE
      source.connect(analyser)
      micAnalyserRef.current = analyser
      micDataRef.current = new Float32Array(analyser.fftSize)
    }
    vadStateRef.current = createInitialVadState()
    chunkerRef.current = new SentenceChunker({
      codeBlockNote: codeBlockNoteRef.current,
    })
    suppressSpeechForTurnRef.current = false
    micRafRef.current = requestAnimationFrame(runMicLoop)
  }, [runMicLoop])

  const teardown = useCallback(() => {
    if (micRafRef.current != null) cancelAnimationFrame(micRafRef.current)
    micRafRef.current = null
    stopTtsLevelLoop()
    if (
      mediaRecorderRef.current &&
      mediaRecorderRef.current.state !== "inactive"
    ) {
      mediaRecorderRef.current.onstop = null
      try {
        mediaRecorderRef.current.stop()
      } catch {
        // Already stopped.
      }
    }
    mediaRecorderRef.current = null
    recordedChunksRef.current = []
    micStreamRef.current?.getTracks().forEach((track) => track.stop())
    micStreamRef.current = null
    micAnalyserRef.current = null
    micDataRef.current = null
    sttAbortRef.current?.abort()
    ttsAbortRef.current?.abort()
    ttsQueueRef.current = []
    const audioEl = ttsAudioElRef.current
    if (audioEl) {
      audioEl.pause()
      audioEl.removeAttribute("src")
    }
    micLevelRef.current = 0
    ttsLevelRef.current = 0
    suppressSpeechForTurnRef.current = false
    chunkerRef.current = null
  }, [stopTtsLevelLoop])

  const open = useCallback(() => {
    if (isOpenRef.current) return
    isOpenRef.current = true
    setIsOpen(true)
    setUnavailableReason(null)
    setLastUserTranscript("")
    setSpokenCaption("")
    setPhaseState("checking-health")

    void (async () => {
      try {
        await checkVoiceHealth(clientOpts())
      } catch {
        if (!isOpenRef.current) return
        setUnavailableReason("service-unreachable")
        setPhaseState("unavailable")
        return
      }
      try {
        await startMic()
      } catch {
        if (!isOpenRef.current) return
        setUnavailableReason("mic-permission-denied")
        setPhaseState("unavailable")
        return
      }
      if (!isOpenRef.current) return
      setPhaseState("listening")
    })()
  }, [clientOpts, setPhaseState, startMic])

  const close = useCallback(() => {
    if (!isOpenRef.current) {
      setIsOpen(false)
      return
    }
    isOpenRef.current = false
    teardown()
    setIsOpen(false)
    setPhaseState("checking-health")
  }, [setPhaseState, teardown])

  const toggleMute = useCallback(() => {
    mutedRef.current = !mutedRef.current
    setMuted(mutedRef.current)
    if (mutedRef.current) micLevelRef.current = 0
  }, [])

  const stopSpeaking = useCallback(() => {
    stopSpeakingInternal()
    suppressSpeechForTurnRef.current = true
    if (isOpenRef.current) setPhaseState("listening")
  }, [setPhaseState, stopSpeakingInternal])

  /** Explicit "stop" UI action: stops TTS AND cancels the in-flight agent
   *  turn via the composer's existing cancel function, when one is wired.
   *  Unlike barge-in (which only stops playback), this is a deliberate user
   *  action — safe to actually interrupt the agent. */
  const cancelTurn = useCallback(() => {
    stopSpeakingInternal()
    suppressSpeechForTurnRef.current = true
    onCancelTurnRef.current?.()
    if (isOpenRef.current) setPhaseState("listening")
  }, [setPhaseState, stopSpeakingInternal])

  // Feed the sentence chunker as the assistant's turn streams in.
  useEffect(() => {
    if (!isOpen) return
    if (suppressSpeechForTurnRef.current) return
    if (phaseRef.current !== "thinking" && phaseRef.current !== "speaking") {
      return
    }
    const chunker = chunkerRef.current
    if (!chunker) return
    for (const sentence of chunker.push(liveAssistantText)) {
      enqueueSentence(sentence)
    }
  }, [isOpen, liveAssistantText, enqueueSentence])

  // Flush the trailing sentence fragment when the agent's turn ends.
  useEffect(() => {
    if (prevBusyRef.current && !isAgentBusy && isOpenRef.current) {
      if (!suppressSpeechForTurnRef.current) {
        const tail = chunkerRef.current?.flush() ?? null
        if (tail) enqueueSentence(tail)
        else if (ttsQueueRef.current.length === 0 && !ttsRunningRef.current) {
          setPhaseState("listening")
        }
      }
    }
    prevBusyRef.current = isAgentBusy
  }, [isAgentBusy, enqueueSentence, setPhaseState])

  // Speak the permission/question cue once per rising edge.
  useEffect(() => {
    if (!isOpen) return
    if (awaitingUserAction && !prevAwaitingRef.current) {
      enqueueSentence(permissionCueRef.current)
    }
    prevAwaitingRef.current = awaitingUserAction
  }, [isOpen, awaitingUserAction, enqueueSentence])

  // Full teardown on unmount (e.g. the composer/tab closes with voice mode
  // still open).
  useEffect(() => {
    return () => {
      if (isOpenRef.current) teardown()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps -- teardown-only cleanup, intentionally runs once
  }, [])

  return useMemo(
    () => ({
      isOpen,
      open,
      close,
      endVoiceMode: close,
      phase,
      micLevelRef,
      ttsLevelRef,
      muted,
      toggleMute,
      stopSpeaking,
      cancelTurn,
      lastUserTranscript,
      spokenCaption,
      awaitingUserAction,
      unavailableReason,
    }),
    [
      isOpen,
      open,
      close,
      phase,
      muted,
      toggleMute,
      stopSpeaking,
      cancelTurn,
      lastUserTranscript,
      spokenCaption,
      awaitingUserAction,
      unavailableReason,
    ]
  )
}
