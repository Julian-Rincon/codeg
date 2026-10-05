// "Listen" for a finished reply: read a written answer aloud with the voice of
// the agent that wrote it, outside voice mode. The text is cut into sentence
// groups small enough for the cloned-voice engine (which caps a request at
// ~600 characters), synthesized one ahead of playback, and played in order.
// One reading at a time app-wide: starting another stops the current one.

import { SentenceChunker } from "@/lib/voice/sentence-chunker"
import { synthesizeSpeech, type TtsRequest } from "@/lib/voice/voice-client"

export type ReadAloudState = "idle" | "loading" | "playing" | "error"

/** Below the 600-char cap of the cloned-voice engine, with room for the
 *  server-side normalization that lengthens technical terms. */
const DEFAULT_MAX = 400

interface SplitOptions {
  max?: number
  codeBlockNote?: string
}

function hardSplit(sentence: string, max: number): string[] {
  const out: string[] = []
  let cur = ""
  for (const word of sentence.split(/\s+/)) {
    if (!word) continue
    if (cur && cur.length + 1 + word.length > max) {
      out.push(cur)
      cur = ""
    }
    cur = cur ? `${cur} ${word}` : word.slice(0, max)
  }
  if (cur) out.push(cur)
  return out
}

/** Sentence groups of at most `max` characters, code blocks replaced by a note. */
export function splitForSpeech(
  text: string,
  { max = DEFAULT_MAX, codeBlockNote }: SplitOptions = {}
): string[] {
  const chunker = new SentenceChunker({ codeBlockNote })
  const sentences = [...chunker.push(text)]
  const tail = chunker.flush()
  if (tail) sentences.push(tail)

  const chunks: string[] = []
  let cur = ""
  for (const raw of sentences) {
    const sentence = raw.trim()
    if (!sentence) continue
    for (const piece of sentence.length > max
      ? hardSplit(sentence, max)
      : [sentence]) {
      if (cur && cur.length + 1 + piece.length > max) {
        chunks.push(cur)
        cur = ""
      }
      cur = cur ? `${cur} ${piece}` : piece
    }
  }
  if (cur) chunks.push(cur)
  return chunks
}

export interface ReadAloudOptions extends SplitOptions {
  lang: TtsRequest["lang"]
  persona?: string | null
  conversationId?: number | null
}

interface ReadAloudDeps {
  synthesize: (req: TtsRequest) => Promise<Blob>
  createAudio: () => HTMLAudioElement
  createUrl: (blob: Blob) => string
  revokeUrl: (url: string) => void
}

export function createReadAloud(deps: ReadAloudDeps) {
  let generation = 0
  let current: HTMLAudioElement | null = null
  let notifyCurrent: ((state: ReadAloudState) => void) | null = null

  function stop() {
    generation++
    current?.pause()
    current = null
    // The reading being cut off must not stay "playing" in its button.
    notifyCurrent?.("idle")
    notifyCurrent = null
  }

  function playBlob(blob: Blob, isLive: () => boolean): Promise<void> {
    return new Promise((resolve, reject) => {
      if (!isLive()) return resolve()
      const url = deps.createUrl(blob)
      const audio = deps.createAudio()
      current = audio
      const done = () => {
        deps.revokeUrl(url)
        resolve()
      }
      audio.onended = done
      audio.onerror = () => {
        deps.revokeUrl(url)
        reject(new Error("playback failed"))
      }
      audio.src = url
      audio.play().catch((err) => {
        deps.revokeUrl(url)
        reject(err)
      })
    })
  }

  async function play(
    text: string,
    options: ReadAloudOptions,
    onState: (state: ReadAloudState) => void = () => {}
  ): Promise<void> {
    stop()
    const mine = ++generation
    notifyCurrent = onState
    const isLive = () => mine === generation
    const chunks = splitForSpeech(text, options)
    if (chunks.length === 0) return onState("idle")

    const request = (chunk: string): TtsRequest => ({
      text: chunk,
      lang: options.lang,
      ...(options.persona ? { persona: options.persona } : {}),
      ...(options.conversationId != null
        ? { conversationId: options.conversationId }
        : {}),
    })

    onState("loading")
    try {
      let next: Promise<Blob> = deps.synthesize(request(chunks[0]))
      for (let i = 0; i < chunks.length; i++) {
        const blob = await next
        if (!isLive()) return
        // Synthesize the following chunk while this one plays.
        if (i + 1 < chunks.length)
          next = deps.synthesize(request(chunks[i + 1]))
        onState("playing")
        await playBlob(blob, isLive)
        if (!isLive()) return
      }
      onState("idle")
    } catch {
      if (isLive()) onState("error")
    } finally {
      if (isLive()) {
        current = null
        notifyCurrent = null
      }
    }
  }

  return { play, stop }
}

/** App-wide reader backed by phantom-voice. */
export const readAloud = createReadAloud({
  synthesize: (req) => synthesizeSpeech(req),
  createAudio: () => new Audio(),
  createUrl: (blob) => URL.createObjectURL(blob),
  revokeUrl: (url) => URL.revokeObjectURL(url),
})
