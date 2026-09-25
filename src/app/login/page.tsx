"use client"

import { useEffect, useId, useState } from "react"
import Image from "next/image"
import { useRouter } from "next/navigation"
import { useTranslations } from "next-intl"
import { Eye, EyeOff, Loader2 } from "lucide-react"
import { AgentIcon } from "@/components/agent-icon"
import { isDesktop } from "@/lib/platform"
import { PHANTOM_EMBLEM_SRC } from "@/lib/phantom-ui"
import type { AgentType } from "@/lib/types"

// The three first-class agents, in the selector's order. Shown as identity,
// not as a status claim: the login page does not know which ones are online.
const LOGIN_AGENTS: ReadonlyArray<{ type: AgentType; name: string }> = [
  { type: "open_code" as AgentType, name: "OpenCode" },
  { type: "claude_code" as AgentType, name: "Claude Code" },
  { type: "hermes" as AgentType, name: "Hermes" },
]

export default function LoginPage() {
  const router = useRouter()
  const t = useTranslations("LoginPage")
  const [token, setToken] = useState("")
  const [reveal, setReveal] = useState(false)
  const [error, setError] = useState("")
  const [loading, setLoading] = useState(false)
  const inputId = useId()
  const errorId = useId()

  useEffect(() => {
    document.title = t("documentTitle")
  }, [t])

  // Desktop users skip login entirely
  if (isDesktop()) {
    router.replace("/workspace")
    return null
  }

  async function handleSubmit(e: React.FormEvent) {
    e.preventDefault()
    setError("")
    setLoading(true)

    try {
      // Validate token by calling a lightweight API endpoint
      const res = await fetch("/api/health", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Authorization: `Bearer ${token.trim()}`,
        },
        body: "{}",
      })

      if (res.ok) {
        localStorage.setItem("codeg_token", token.trim())
        router.replace("/workspace")
      } else if (res.status === 401) {
        setError(t("invalidToken"))
      } else {
        setError(t("connectionFailed", { status: res.status }))
      }
    } catch {
      setError(t("networkError"))
    } finally {
      setLoading(false)
    }
  }

  return (
    <main className="relative grid min-h-dvh place-items-center overflow-hidden bg-background px-4 py-10">
      {/* One light source from above, tinted by the active accent: the page's
          only atmospheric element, echoing the emblem's glow. */}
      <div
        aria-hidden
        className="pointer-events-none absolute inset-x-0 top-0 h-[60vh] bg-[radial-gradient(60%_70%_at_50%_0%,color-mix(in_srgb,var(--primary)_16%,transparent),transparent_70%)]"
      />

      <div className="relative w-full max-w-[22rem]">
        <header className="flex flex-col items-center text-center">
          <Image
            src={PHANTOM_EMBLEM_SRC}
            alt=""
            width={96}
            height={96}
            priority
            className="size-24 rounded-[1.75rem] drop-shadow-[0_10px_28px_color-mix(in_srgb,var(--primary)_35%,transparent)]"
          />
          <h1 className="mt-6 font-wordmark text-[1.625rem] leading-none text-foreground">
            {t("brand")}
          </h1>
          <p className="mt-3 text-[0.6875rem] font-medium uppercase tracking-[0.32em] text-primary">
            {t("tagline")}
          </p>
          <p className="mt-5 text-sm text-balance text-muted-foreground">
            {t("subtitle")}
          </p>
        </header>

        <form onSubmit={handleSubmit} className="mt-8 space-y-3" noValidate>
          <label
            htmlFor={inputId}
            className="block text-xs font-medium text-muted-foreground"
          >
            {t("tokenLabel")}
          </label>
          <div className="relative">
            <input
              id={inputId}
              type={reveal ? "text" : "password"}
              value={token}
              onChange={(e) => setToken(e.target.value)}
              placeholder={t("tokenPlaceholder")}
              autoFocus
              autoComplete="current-password"
              spellCheck={false}
              aria-invalid={error ? true : undefined}
              aria-describedby={error ? errorId : undefined}
              className="flex h-11 w-full rounded-lg border border-input bg-card pr-11 pl-3.5 font-mono text-sm tracking-wide text-foreground shadow-[0_1px_2px_rgb(0_0_0/0.06)] transition-[border-color,box-shadow] placeholder:font-sans placeholder:tracking-normal placeholder:text-muted-foreground focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/25 focus-visible:outline-none aria-invalid:border-destructive"
            />
            <button
              type="button"
              onClick={() => setReveal((v) => !v)}
              aria-label={reveal ? t("hideToken") : t("showToken")}
              aria-pressed={reveal}
              className="absolute inset-y-0 right-0 grid w-11 place-items-center rounded-r-lg text-muted-foreground transition-colors hover:text-foreground focus-visible:text-foreground focus-visible:outline-none"
            >
              {reveal ? (
                <EyeOff className="size-4" aria-hidden />
              ) : (
                <Eye className="size-4" aria-hidden />
              )}
            </button>
          </div>

          {error && (
            <p id={errorId} role="alert" className="text-sm text-destructive">
              {error}
            </p>
          )}

          <button
            type="submit"
            disabled={!token.trim() || loading}
            className="inline-flex h-11 w-full items-center justify-center gap-2 rounded-lg bg-primary px-4 text-sm font-medium text-primary-foreground shadow-[0_6px_18px_-6px_color-mix(in_srgb,var(--primary)_60%,transparent)] transition-[background-color,box-shadow,opacity] hover:bg-primary/90 focus-visible:ring-3 focus-visible:ring-ring/35 focus-visible:outline-none disabled:pointer-events-none disabled:opacity-45 disabled:shadow-none"
          >
            {loading && <Loader2 className="size-4 animate-spin" aria-hidden />}
            {loading ? t("connecting") : t("connect")}
          </button>
        </form>

        <p className="mt-4 text-center text-xs leading-relaxed text-balance text-muted-foreground">
          {t("helpText")}
        </p>

        <ul
          aria-label={t("agentsLabel")}
          className="mt-10 flex items-center justify-center gap-x-4 border-t border-border pt-5 text-xs text-muted-foreground"
        >
          {LOGIN_AGENTS.map((agent) => (
            <li key={agent.type} className="flex items-center gap-1.5">
              <AgentIcon agentType={agent.type} className="size-4" />
              <span>{agent.name}</span>
            </li>
          ))}
        </ul>
      </div>
    </main>
  )
}
