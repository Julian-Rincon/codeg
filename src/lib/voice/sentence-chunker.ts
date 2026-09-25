// Incremental sentence splitter for voice-live TTS. `SentenceChunker.push`
// is fed the FULL accumulated assistant text on every streaming update (not
// a delta) and returns only the newly-completed sentences since the last
// call, so the TTS queue can start speaking a sentence the instant it's
// finished without waiting for the whole turn.
//
// Fenced code blocks are never spoken verbatim: a chunker holds the block
// back entirely and, once it closes, emits a single short spoken note in its
// place (the note text is caller-supplied so it can be localized).

/** Matches a run of sentence terminators (., !, ?, …) optionally followed by
 *  a closing quote/paren, when followed by whitespace or end-of-string — or
 *  a run of newlines (paragraph breaks, list-item lines). The whitespace
 *  lookahead is what keeps "3.14" or "v1.2" from splitting: the `.` there is
 *  followed by a digit, not whitespace. */
const TERMINATOR_RE = /[.!?…]+["')\]]*(?=\s|$)|\n+/g

const CODE_FENCE = "```"

export interface SentenceChunkerOptions {
  /** Spoken in place of a fenced code block once it closes (or at flush, if
   *  the turn ends mid-block). Defaults to a generic placeholder; callers
   *  should pass a localized string. */
  codeBlockNote?: string
}

const DEFAULT_CODE_BLOCK_NOTE = "Code block."

/** Split a fence-free text segment into complete, terminator-ended
 *  sentences, plus how many characters were consumed doing so (the
 *  remainder is an incomplete trailing fragment, left for the next call). */
export function splitCompleteSentences(segment: string): {
  sentences: string[]
  consumed: number
} {
  const sentences: string[] = []
  let consumed = 0
  TERMINATOR_RE.lastIndex = 0
  let match: RegExpExecArray | null
  let cursor = 0
  while ((match = TERMINATOR_RE.exec(segment))) {
    const endIdx = match.index + match[0].length
    const chunk = segment.slice(cursor, endIdx).trim()
    if (chunk) sentences.push(chunk)
    consumed = endIdx
    cursor = endIdx
  }
  return { sentences, consumed }
}

export class SentenceChunker {
  private emittedIndex = 0
  private fenceOpenIndex: number | null = null
  private lastText = ""
  private readonly codeBlockNote: string

  constructor(options: SentenceChunkerOptions = {}) {
    this.codeBlockNote = options.codeBlockNote ?? DEFAULT_CODE_BLOCK_NOTE
  }

  /** Feed the current FULL assistant text; returns sentences newly completed
   *  since the previous call. Never re-emits already-returned text. */
  push(fullText: string): string[] {
    if (fullText.length < this.emittedIndex) {
      // Text shrank — a new turn started reusing the same accumulator.
      this.reset()
    }
    this.lastText = fullText

    const sentences: string[] = []
    let pos = this.emittedIndex

    for (;;) {
      if (this.fenceOpenIndex !== null) {
        const searchFrom = Math.max(
          pos,
          this.fenceOpenIndex + CODE_FENCE.length
        )
        const closeIdx = fullText.indexOf(CODE_FENCE, searchFrom)
        if (closeIdx === -1) break // fence still open; wait for more text
        const fenceEnd = closeIdx + CODE_FENCE.length
        sentences.push(this.codeBlockNote)
        this.fenceOpenIndex = null
        this.emittedIndex = fenceEnd
        pos = fenceEnd
        continue
      }

      const nextFenceIdx = fullText.indexOf(CODE_FENCE, pos)
      const scanEnd = nextFenceIdx === -1 ? fullText.length : nextFenceIdx
      const segment = fullText.slice(pos, scanEnd)
      const { sentences: found, consumed } = splitCompleteSentences(segment)
      sentences.push(...found)
      this.emittedIndex = pos + consumed
      pos = this.emittedIndex

      if (nextFenceIdx === -1) break // no fence ahead; leftover tail buffers

      // A code fence is a hard break: force-emit any un-terminated leftover
      // fragment right before it rather than losing it inside the block.
      const leftover = fullText.slice(pos, nextFenceIdx).trim()
      if (leftover) sentences.push(leftover)
      this.fenceOpenIndex = nextFenceIdx
      this.emittedIndex = nextFenceIdx
      pos = nextFenceIdx
    }

    return sentences
  }

  /** Call once the turn has ended (no more text will arrive). Returns the
   *  final leftover fragment (or the code-block note, if the turn ended
   *  mid-fence) as one last sentence, or `null` if nothing is left. */
  flush(): string | null {
    if (this.fenceOpenIndex !== null) {
      this.fenceOpenIndex = null
      this.emittedIndex = this.lastText.length
      return this.codeBlockNote
    }
    const leftover = this.lastText.slice(this.emittedIndex).trim()
    this.emittedIndex = this.lastText.length
    return leftover || null
  }

  /** Reset for a brand-new turn. */
  reset(): void {
    this.emittedIndex = 0
    this.fenceOpenIndex = null
    this.lastText = ""
  }
}
