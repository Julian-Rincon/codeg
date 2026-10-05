import { describe, expect, it, vi } from "vitest"

import { createReadAloud, splitForSpeech } from "@/lib/voice/read-aloud"

describe("splitForSpeech", () => {
  it("agrupa frases sin pasar el máximo y sin perder texto", () => {
    const text = Array.from(
      { length: 30 },
      (_, i) => `Esta es la frase número ${i}.`
    ).join(" ")
    const chunks = splitForSpeech(text, { max: 120 })
    expect(chunks.length).toBeGreaterThan(1)
    expect(chunks.every((c) => c.length <= 120)).toBe(true)
    expect(chunks.join(" ").replace(/\s+/g, " ")).toBe(text)
  })

  it("cambia los bloques de código por una nota", () => {
    const chunks = splitForSpeech("Mira:\n```py\nprint(1)\n```\nListo.", {
      codeBlockNote: "Bloque de código.",
    })
    const joined = chunks.join(" ")
    expect(joined).toContain("Bloque de código.")
    expect(joined).not.toContain("print(1)")
  })

  it("parte una frase gigante por palabras", () => {
    const chunks = splitForSpeech("palabra ".repeat(200).trim() + ".", {
      max: 100,
    })
    expect(chunks.every((c) => c.length <= 100)).toBe(true)
  })

  it("texto vacío no produce nada", () => {
    expect(splitForSpeech("   ")).toEqual([])
  })
})

function fakeAudio() {
  const played: string[] = []
  class Audio {
    src = ""
    onended: (() => void) | null = null
    onerror: (() => void) | null = null
    play() {
      played.push(this.src)
      queueMicrotask(() => this.onended?.())
      return Promise.resolve()
    }
    pause() {}
  }
  return { Audio, played }
}

describe("createReadAloud", () => {
  it("sintetiza y reproduce cada trozo en orden con la identidad de voz", async () => {
    const synth = vi.fn(async (req: { text: string }) => new Blob([req.text]))
    const { Audio, played } = fakeAudio()
    let n = 0
    const player = createReadAloud({
      synthesize: synth,
      createAudio: () => new Audio() as unknown as HTMLAudioElement,
      createUrl: () => `blob:${n++}`,
      revokeUrl: () => {},
    })
    const states: string[] = []
    await player.play(
      "Uno. Dos.",
      {
        lang: "es",
        persona: "open_code",
        conversationId: 9,
        max: 5,
      },
      (s) => states.push(s)
    )
    expect(synth).toHaveBeenCalledTimes(2)
    expect(synth.mock.calls[0][0]).toMatchObject({
      text: "Uno.",
      lang: "es",
      persona: "open_code",
      conversationId: 9,
    })
    expect(played).toEqual(["blob:0", "blob:1"])
    expect(states[0]).toBe("loading")
    expect(states[states.length - 1]).toBe("idle")
  })

  it("stop corta la lectura y no sigue sintetizando", async () => {
    let release: (b: Blob) => void = () => {}
    const synth = vi.fn(
      () => new Promise<Blob>((resolve) => (release = resolve))
    )
    const { Audio, played } = fakeAudio()
    const player = createReadAloud({
      synthesize: synth,
      createAudio: () => new Audio() as unknown as HTMLAudioElement,
      createUrl: () => "blob:x",
      revokeUrl: () => {},
    })
    const done = player.play("Uno. Dos. Tres.", { lang: "es", max: 5 })
    player.stop()
    release(new Blob(["x"]))
    await done
    expect(played).toEqual([])
  })

  it("si la síntesis falla, termina en error sin lanzar", async () => {
    const player = createReadAloud({
      synthesize: async () => {
        throw new Error("down")
      },
      createAudio: () => fakeAudio().Audio as unknown as HTMLAudioElement,
      createUrl: () => "blob:x",
      revokeUrl: () => {},
    })
    const states: string[] = []
    await player.play("Hola.", { lang: "es" }, (s) => states.push(s))
    expect(states[states.length - 1]).toBe("error")
  })
})

describe("createReadAloud — una lectura a la vez", () => {
  it("empezar otra lectura deja la anterior en idle", async () => {
    const synth = vi.fn(() => new Promise<Blob>(() => {}))
    const { Audio } = fakeAudio()
    const player = createReadAloud({
      synthesize: synth,
      createAudio: () => new Audio() as unknown as HTMLAudioElement,
      createUrl: () => "blob:x",
      revokeUrl: () => {},
    })
    const first: string[] = []
    void player.play("Uno.", { lang: "es" }, (s) => first.push(s))
    void player.play("Dos.", { lang: "es" }, () => {})
    expect(first).toEqual(["loading", "idle"])
  })
})
