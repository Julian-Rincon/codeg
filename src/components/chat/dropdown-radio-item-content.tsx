"use client"

import { Badge } from "@/components/ui/badge"

interface DropdownRadioItemContentProps {
  label: string
  description?: string | null
  /** Localized "recommended" chip text, or null/absent for the usual row. Set it
   *  only on the value the AGENT recommends (ACP `recommendedValue`) — a claim
   *  independent of what is currently selected, which is the checkmark's job, so
   *  both can land on the same row or on different ones.
   *
   *  Passed in rather than translated here on purpose: every other label this
   *  leaf renders arrives the same way, and the callers all hold a translator
   *  already. */
  recommendedLabel?: string | null
  /** Localized short category chips ("Edit", "Fast", …) for the categories this
   *  model measurably wins at (`ModelScorecardEntry.strengths`), or absent when
   *  there's no scorecard data for this row. Rendered the same way as
   *  `recommendedLabel` — a claim about the model, not about selection state. */
  scorecardBadges?: string[] | null
  /** Localized "not available" chip text, shown when the backend reports
   *  `available === false` for this model — the model stays listed (never
   *  hidden), just visibly flagged. */
  unavailableLabel?: string | null
  /** Muted line under the description: either the measured metric summary
   *  ("14s/turn · 56 tok/s · 3% errors · 262k ctx") or a localized "not enough
   *  data yet" note. Absent when there's nothing to show at all. */
  scorecardLine?: string | null
}

export function DropdownRadioItemContent({
  label,
  description,
  recommendedLabel,
  scorecardBadges,
  unavailableLabel,
  scorecardLine,
}: DropdownRadioItemContentProps) {
  const normalizedDescription = description?.trim()
  const badge = recommendedLabel?.trim()
  const unavailable = unavailableLabel?.trim()
  const strengthBadges = (scorecardBadges ?? []).filter((b) => b.trim())
  const line = scorecardLine?.trim()

  return (
    <div className="w-full min-w-0 pr-2" title={label}>
      {/* The badges are `shrink-0` (Badge's own base class) so a long model
          name truncates instead of squeezing them away. */}
      <div className="flex min-w-0 flex-wrap items-center gap-1.5">
        <p className="min-w-0 truncate">{label}</p>
        {badge ? (
          <Badge variant="outline" className="px-1 text-3xs font-normal">
            {badge}
          </Badge>
        ) : null}
        {strengthBadges.map((chip) => (
          <Badge
            key={chip}
            variant="outline"
            className="px-1 text-3xs font-normal text-[var(--phantom-accent)]"
          >
            {chip}
          </Badge>
        ))}
        {unavailable ? (
          <Badge
            variant="outline"
            className="px-1 text-3xs font-normal text-muted-foreground"
          >
            {unavailable}
          </Badge>
        ) : null}
      </div>
      {normalizedDescription ? (
        <p className="text-muted-foreground mt-0.5 text-xs leading-snug whitespace-pre-wrap wrap-break-word">
          {normalizedDescription}
        </p>
      ) : null}
      {line ? (
        <p className="text-muted-foreground mt-0.5 truncate text-3xs leading-snug">
          {line}
        </p>
      ) : null}
    </div>
  )
}
