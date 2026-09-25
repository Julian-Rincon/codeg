import { describe, expect, it } from "vitest"
import { SentenceChunker, splitCompleteSentences } from "./sentence-chunker"

describe("splitCompleteSentences", () => {
  it("returns nothing for text with no terminator", () => {
    expect(splitCompleteSentences("Hello there")).toEqual({
      sentences: [],
      consumed: 0,
    })
  })

  it("splits a single terminated sentence", () => {
    expect(splitCompleteSentences("Hello world.")).toEqual({
      sentences: ["Hello world."],
      consumed: 12,
    })
  })

  it("splits multiple sentences in one segment", () => {
    const { sentences } = splitCompleteSentences("Hi! How are you? Great.")
    expect(sentences).toEqual(["Hi!", "How are you?", "Great."])
  })

  it("does not split a decimal number", () => {
    const { sentences, consumed } = splitCompleteSentences("Pi is 3.14 roughly")
    expect(sentences).toEqual([])
    expect(consumed).toBe(0)
  })

  it("treats an ellipsis as a terminator", () => {
    const { sentences } = splitCompleteSentences("Well… anyway.")
    expect(sentences).toEqual(["Well…", "anyway."])
  })

  it("treats a run of newlines as a boundary", () => {
    const { sentences } = splitCompleteSentences("line one\nline two\n")
    expect(sentences).toEqual(["line one", "line two"])
  })

  it("keeps a terminator immediately followed by a closing quote", () => {
    const { sentences } = splitCompleteSentences('She said "hi." Then left.')
    expect(sentences).toEqual(['She said "hi."', "Then left."])
  })
})

describe("SentenceChunker incremental streaming", () => {
  it("emits nothing until the first sentence terminates", () => {
    const chunker = new SentenceChunker()
    expect(chunker.push("Hello")).toEqual([])
    expect(chunker.push("Hello wor")).toEqual([])
  })

  it("emits a sentence exactly once, the instant it completes", () => {
    const chunker = new SentenceChunker()
    expect(chunker.push("Hello wor")).toEqual([])
    expect(chunker.push("Hello world.")).toEqual(["Hello world."])
    // Growing further without a new terminator emits nothing more.
    expect(chunker.push("Hello world. How")).toEqual([])
    // Never re-emits the already-spoken first sentence.
    expect(chunker.push("Hello world. How are you?")).toEqual(["How are you?"])
  })

  it("emits several sentences completed between two pushes", () => {
    const chunker = new SentenceChunker()
    expect(chunker.push("First. Second. Third")).toEqual(["First.", "Second."])
    expect(chunker.push("First. Second. Third.")).toEqual(["Third."])
  })

  it("speaks each list item as its own chunk", () => {
    const chunker = new SentenceChunker()
    expect(chunker.push("- item one\n")).toEqual(["- item one"])
    expect(chunker.push("- item one\n- item two\n")).toEqual(["- item two"])
  })

  it("holds back a fenced code block and speaks a note once it closes", () => {
    const chunker = new SentenceChunker({ codeBlockNote: "Code snippet." })
    expect(chunker.push("Here is code:\n```js\n")).toEqual(["Here is code:"])
    // Still inside the fence — nothing new to speak.
    expect(chunker.push("Here is code:\n```js\nconst x = 1;\n")).toEqual([])
    expect(
      chunker.push("Here is code:\n```js\nconst x = 1;\n```\nDone.")
    ).toEqual(["Code snippet.", "Done."])
  })

  it("never speaks the code fence markers or code content", () => {
    const chunker = new SentenceChunker({ codeBlockNote: "[code]" })
    const spoken = [
      ...chunker.push("```py\nprint('hi')\n```"),
      chunker.flush(),
    ].filter((s): s is string => s !== null)
    expect(spoken.join(" ")).not.toContain("```")
    expect(spoken.join(" ")).not.toContain("print")
    expect(spoken).toEqual(["[code]"])
  })

  it("never re-emits text already returned, across many small pushes", () => {
    const chunker = new SentenceChunker()
    const full = "One. Two. Three. Four."
    const seen: string[] = []
    for (let i = 1; i <= full.length; i++) {
      seen.push(...chunker.push(full.slice(0, i)))
    }
    expect(seen).toEqual(["One.", "Two.", "Three.", "Four."])
  })

  it("flush returns the trailing unterminated fragment", () => {
    const chunker = new SentenceChunker()
    chunker.push("First. Second, still going")
    expect(chunker.flush()).toBe("Second, still going")
  })

  it("flush returns null when nothing is left to speak", () => {
    const chunker = new SentenceChunker()
    chunker.push("Complete sentence.")
    expect(chunker.flush()).toBeNull()
  })

  it("flush emits the code-block note when the turn ends mid-fence", () => {
    const chunker = new SentenceChunker({ codeBlockNote: "[code]" })
    chunker.push("Before.\n```py\nprint(1)")
    expect(chunker.flush()).toBe("[code]")
    // A second flush is a no-op.
    expect(chunker.flush()).toBeNull()
  })

  it("reset clears state so the chunker can start a fresh turn", () => {
    const chunker = new SentenceChunker()
    chunker.push("First turn.")
    chunker.reset()
    expect(chunker.push("First turn. Second sentence.")).toEqual([
      "First turn.",
      "Second sentence.",
    ])
  })

  it("auto-resets when fed text shorter than what was already consumed", () => {
    const chunker = new SentenceChunker()
    // Consume past index 6 ("First." emitted), so emittedIndex > 0.
    expect(chunker.push("First. Continuing without end")).toEqual(["First."])
    // A new turn starts, its accumulator is shorter than the consumed index.
    expect(chunker.push("Hi.")).toEqual(["Hi."])
  })
})
