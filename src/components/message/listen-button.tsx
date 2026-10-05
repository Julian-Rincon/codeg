"use client"

import { useEffect, useState } from "react"
import { Loader2Icon, SquareIcon, Volume2Icon } from "lucide-react"
import { useLocale, useTranslations } from "next-intl"

import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip"
import { readAloud, type ReadAloudState } from "@/lib/voice/read-aloud"
import { useVoiceIdentity } from "@/lib/voice/voice-identity-context"
import { resolveTtsLang } from "@/lib/voice/voice-client"

/** Reads a finished reply aloud with the voice of the agent that wrote it. */
export function ListenButton({
  text,
  className,
}: {
  text: string
  className: string
}) {
  const t = useTranslations("Folder.chat.messageList")
  const locale = useLocale()
  const { persona, conversationId } = useVoiceIdentity()
  const [state, setState] = useState<ReadAloudState>("idle")
  const active = state === "loading" || state === "playing"

  // Leaving the conversation must not keep reading in the background.
  useEffect(() => () => readAloud.stop(), [])

  const onClick = () => {
    if (active) {
      readAloud.stop()
      return
    }
    void readAloud.play(
      text,
      {
        lang: resolveTtsLang(locale),
        persona,
        conversationId,
        codeBlockNote: t("listenCodeBlock"),
      },
      setState
    )
  }

  const label = active
    ? t("stopListening")
    : state === "error"
      ? t("listenUnavailable")
      : t("listen")

  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          onClick={onClick}
          className={className}
          aria-label={label}
          aria-pressed={active}
        >
          {state === "loading" ? (
            <Loader2Icon
              aria-hidden="true"
              className="h-3.5 w-3.5 animate-spin"
            />
          ) : state === "playing" ? (
            <SquareIcon aria-hidden="true" className="h-3.5 w-3.5" />
          ) : (
            <Volume2Icon aria-hidden="true" className="h-3.5 w-3.5" />
          )}
        </button>
      </TooltipTrigger>
      <TooltipContent side="top">{label}</TooltipContent>
    </Tooltip>
  )
}
