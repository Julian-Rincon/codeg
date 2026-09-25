"use client"

// The "Gemini Live"-style full-panel overlay: opened over the conversation
// area (portaled to the nearest `[data-conversation-shell-root]` ancestor —
// see `message-input.tsx` — never a browser-level/body-level modal), showing
// the galaxy orb, live captions, and voice controls.

import { useEffect, useMemo, useRef, useState } from "react"
import { useTranslations } from "next-intl"
import { Mic, MicOff, Square, X } from "lucide-react"
import { Button } from "@/components/ui/button"
import { AgentIcon } from "@/components/agent-icon"
import { cn } from "@/lib/utils"
import type { AgentType } from "@/lib/types"
import type { UseVoiceLiveResult, VoicePhase } from "@/lib/voice/use-voice-live"
import {
  VoiceGalaxyOrb,
  type VoiceOrbState,
} from "@/components/voice/voice-galaxy-orb"

export interface VoiceLiveOverlayProps {
  voiceLive: UseVoiceLiveResult
  agentType?: AgentType | null
  agentName?: string | null
}

const ORB_STATE_BY_PHASE: Record<VoicePhase, VoiceOrbState> = {
  "checking-health": "idle",
  unavailable: "idle",
  listening: "listening",
  transcribing: "listening",
  thinking: "thinking",
  speaking: "speaking",
}

const FOCUSABLE_SELECTOR =
  'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])'

// The orb is the hero of the Live view: as large as the panel allows while
// leaving room for captions and controls.
function useOrbSize(): number {
  const compute = () =>
    typeof window === "undefined"
      ? 320
      : Math.round(
          Math.max(
            200,
            Math.min(window.innerWidth * 0.55, window.innerHeight * 0.48, 440)
          )
        )
  const [size, setSize] = useState(compute)
  useEffect(() => {
    const onResize = () => setSize(compute())
    window.addEventListener("resize", onResize)
    return () => window.removeEventListener("resize", onResize)
  }, [])
  return size
}

export function VoiceLiveOverlay({
  voiceLive,
  agentType,
  agentName,
}: VoiceLiveOverlayProps) {
  const t = useTranslations("Folder.chat.messageInput.voice")
  const orbSize = useOrbSize()
  const panelRef = useRef<HTMLDivElement | null>(null)

  const {
    phase,
    muted,
    toggleMute,
    stopSpeaking,
    cancelTurn,
    endVoiceMode,
    lastUserTranscript,
    spokenCaption,
    awaitingUserAction,
    unavailableReason,
    micLevelRef,
    ttsLevelRef,
  } = voiceLive

  // Focus trap + Escape-to-close + focus restore on close.
  useEffect(() => {
    const previouslyFocused = document.activeElement as HTMLElement | null
    panelRef.current?.focus()

    function handleKeyDown(e: KeyboardEvent) {
      if (e.key === "Escape") {
        e.preventDefault()
        endVoiceMode()
        return
      }
      if (e.key !== "Tab") return
      const panel = panelRef.current
      if (!panel) return
      const focusables = Array.from(
        panel.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR)
      )
      if (focusables.length === 0) return
      const first = focusables[0]
      const last = focusables[focusables.length - 1]
      if (e.shiftKey && document.activeElement === first) {
        e.preventDefault()
        last.focus()
      } else if (!e.shiftKey && document.activeElement === last) {
        e.preventDefault()
        first.focus()
      }
    }

    document.addEventListener("keydown", handleKeyDown)
    return () => {
      document.removeEventListener("keydown", handleKeyDown)
      previouslyFocused?.focus?.()
    }
  }, [endVoiceMode])

  const stateLabel = useMemo(() => {
    switch (phase) {
      case "checking-health":
        return t("stateChecking")
      case "unavailable":
        return t("stateUnavailable")
      case "listening":
        return t("stateListening")
      case "transcribing":
        return t("stateTranscribing")
      case "thinking":
        return t("stateThinking")
      case "speaking":
        return t("stateSpeaking")
    }
  }, [phase, t])

  const orbState = ORB_STATE_BY_PHASE[phase]

  return (
    <div
      ref={panelRef}
      role="dialog"
      aria-modal="true"
      aria-label={t("title")}
      tabIndex={-1}
      className="absolute inset-0 z-50 flex flex-col bg-background/97 backdrop-blur-md outline-none"
    >
      <div className="flex items-center justify-between gap-2 px-4 pt-4">
        {agentType ? (
          <div className="flex items-center gap-2 rounded-full border border-border/60 bg-card/60 px-3 py-1.5 text-xs text-muted-foreground">
            <AgentIcon agentType={agentType} className="size-4" />
            <span className="truncate">{agentName ?? agentType}</span>
          </div>
        ) : (
          <span />
        )}
        <Button
          onClick={endVoiceMode}
          variant="ghost"
          size="icon"
          className="h-8 w-8 shrink-0"
          title={t("endVoiceMode")}
          aria-label={t("endVoiceMode")}
        >
          <X className="size-4" />
        </Button>
      </div>

      <div className="flex flex-1 min-h-0 flex-col items-center justify-center gap-6 px-6">
        {phase === "unavailable" ? (
          <div className="flex max-w-sm flex-col items-center gap-3 text-center">
            <VoiceGalaxyOrb
              state="idle"
              muted
              micLevelRef={micLevelRef}
              ttsLevelRef={ttsLevelRef}
              size={Math.round(orbSize * 0.6)}
            />
            <p className="text-sm text-muted-foreground">
              {unavailableReason === "mic-permission-denied"
                ? t("unavailableMicDenied")
                : t("unavailableServiceDown")}
            </p>
          </div>
        ) : (
          <>
            <VoiceGalaxyOrb
              state={orbState}
              muted={muted}
              micLevelRef={micLevelRef}
              ttsLevelRef={ttsLevelRef}
              size={orbSize}
            />
            <div
              role="status"
              aria-live="polite"
              className="text-sm font-medium text-[color:var(--phantom-accent)]"
            >
              {stateLabel}
            </div>

            {awaitingUserAction && (
              <div className="rounded-lg border border-[color:var(--phantom-accent)]/40 bg-[color:var(--phantom-accent-soft)] px-3 py-1.5 text-xs text-foreground">
                {t("permissionPendingCue")}
              </div>
            )}

            <div
              aria-live="polite"
              className="flex w-full max-w-lg flex-col items-center gap-2 text-center"
            >
              {lastUserTranscript && (
                <p className="text-sm text-muted-foreground">
                  <span className="sr-only">{t("youSaidLabel")} </span>
                  {lastUserTranscript}
                </p>
              )}
              {spokenCaption && (
                <p className="text-base text-foreground">
                  <span className="sr-only">{t("assistantSaysLabel")} </span>
                  {spokenCaption}
                </p>
              )}
            </div>
          </>
        )}
      </div>

      <div className="flex items-center justify-center gap-3 pb-8 pt-2">
        <Button
          onClick={toggleMute}
          variant={muted ? "default" : "outline"}
          size="icon"
          className="h-11 w-11 rounded-full"
          title={muted ? t("unmute") : t("mute")}
          aria-label={muted ? t("unmute") : t("mute")}
          aria-pressed={muted}
        >
          {muted ? <MicOff className="size-4" /> : <Mic className="size-4" />}
        </Button>
        <Button
          onClick={stopSpeaking}
          variant="outline"
          size="icon"
          className="h-11 w-11 rounded-full"
          title={t("stopSpeaking")}
          aria-label={t("stopSpeaking")}
          disabled={phase !== "speaking"}
        >
          <Square className="size-4" />
        </Button>
        <Button
          onClick={cancelTurn}
          variant="destructive"
          size="icon"
          className={cn(
            "h-11 w-11 rounded-full",
            phase !== "thinking" && phase !== "speaking" && "opacity-50"
          )}
          title={t("stopAgent")}
          aria-label={t("stopAgent")}
        >
          <X className="size-4" />
        </Button>
      </div>
    </div>
  )
}
