// Decides whether a transcribed utterance is something the user said to the
// agent, or noise the VAD picked up: a cough, the keyboard, the agent's own
// voice leaking back from the speakers, or one of Whisper's stock
// hallucinations on near-silence ("Thank you.", "Subtítulos por…").
//
// It matters most while the agent is working: the mic stays open so the user
// can interrupt, and a false utterance sent then cuts the turn short — which
// is exactly how voice replies used to vanish before a single word was spoken.

/** Whisper's well-known outputs for silence/noise, normalised (lowercase, no
 *  punctuation). Matched as the WHOLE utterance only. */
const HALLUCINATIONS = new Set([
  "",
  "you",
  "thank you",
  "thanks",
  "thanks for watching",
  "thank you for watching",
  "bye",
  "okay",
  "ok",
  "music",
  "gracias",
  "muchas gracias",
  "adiós",
  "chau",
  "subtítulos realizados por la comunidad de amaraorg",
  "subtítulos por la comunidad de amaraorg",
  "amaraorg",
])

export interface UtteranceCheck {
  text: string
  /** Language Whisper detected ("es", "en", …). */
  language: string
  durationMs: number
  /** The agent is thinking or speaking: the bar for a real interruption. */
  agentBusy: boolean
  /** The language the user speaks to Phantom in (from the UI locale). */
  expectedLang: "es" | "en"
}

export function normaliseUtterance(text: string): string {
  return text
    .toLowerCase()
    .normalize("NFC")
    .replace(/[^\p{L}\p{N}\s]/gu, "")
    .replace(/\s+/g, " ")
    .trim()
}

export function shouldSendUtterance(u: UtteranceCheck): boolean {
  const norm = normaliseUtterance(u.text)
  if (HALLUCINATIONS.has(norm)) return false
  const words = norm.split(" ").filter(Boolean).length
  if (words === 0) return false

  // A short burst in another language than the user speaks is almost always
  // noise that Whisper forced into words.
  const lang = u.language.toLowerCase().slice(0, 2)
  if (lang && lang !== u.expectedLang && words < 4) return false

  // Interrupting a working agent needs a real sentence, not a "mm" or a
  // stray word — that is what used to cancel replies mid-turn.
  if (u.agentBusy && (words < 3 || u.durationMs < 1200)) return false

  return true
}
