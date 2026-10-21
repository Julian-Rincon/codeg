"use client"

import { useEffect } from "react"
import { usePhantomAttention } from "@/hooks/use-phantom-attention"

const PREFIX_RE = /^\(\d+\) /

/** `title` with the "(N) " attention prefix set to `count` (removed at 0). */
export function withAttentionPrefix(title: string, count: number): string {
  const base = title.replace(PREFIX_RE, "")
  return count > 0 ? `(${count}) ${base}` : base
}

/**
 * Prefixes `document.title` with "(N) " while N tabs wait on the user, so a
 * backgrounded window or the phone PWA shows it. Other writers (the workspace
 * folder title) keep owning the rest of the title: a head observer re-applies
 * the prefix after they write, and dropping to zero strips only the prefix.
 */
export function PhantomAttentionTitle() {
  const count = usePhantomAttention().blocked.length

  useEffect(() => {
    const apply = () => {
      const next = withAttentionPrefix(document.title, count)
      if (next !== document.title) document.title = next
    }
    apply()
    if (count === 0) return
    const observer = new MutationObserver(apply)
    observer.observe(document.head, {
      subtree: true,
      childList: true,
      characterData: true,
    })
    return () => {
      observer.disconnect()
      document.title = withAttentionPrefix(document.title, 0)
    }
  }, [count])

  return null
}
