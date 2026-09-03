import { cn } from '@/lib/utils'
import { railDotClass, type RailTone } from './rail'

export interface StatusSegment {
  label: string
  count: number
  tone: RailTone
  onClick?: () => void
  active?: boolean
  hint?: string
}

export function StatusStrip({
  segments,
  className,
  trailing,
}: {
  segments: StatusSegment[]
  className?: string
  trailing?: React.ReactNode
}) {
  return (
    <div
      className={cn(
        'console-scope flex flex-wrap items-center justify-between gap-3',
        className,
      )}
    >
      <div className="flex max-w-full flex-wrap items-center gap-2">
        {segments.map((s) => {
          const clickable = !!s.onClick

          const inner = (
            <>
              {s.tone !== 'none' && (
                <span
                  className={cn(
                    'h-2 w-2 shrink-0 rounded-full',
                    s.active ? 'bg-current' : railDotClass(s.tone),
                    !s.active && s.count === 0 && 'opacity-30',
                  )}
                />
              )}
              <span className="text-xs font-semibold tabular-nums">
                {s.count}
              </span>
              <span>{s.label}</span>
            </>
          )

          if (!clickable) {
            return (
              <span
                key={s.label}
                title={s.hint}
                className="inline-flex items-center gap-1.5 px-3 h-8 text-xs text-muted-foreground"
              >
                {inner}
              </span>
            )
          }

          return (
            <button
              key={s.label}
              type="button"
              onClick={s.onClick}
              title={s.hint}
              aria-pressed={s.active}
              className={cn(
                'inline-flex items-center gap-1.5 px-2.5 h-7 text-xs font-medium rounded border transition-colors select-none cursor-pointer active:scale-[0.96] transition-transform',
                'focus:outline-none focus-visible:ring-1 focus-visible:ring-ring',
                s.active
                  ? 'bg-card border-border text-foreground shadow-xs font-semibold'
                  : 'bg-transparent border-border/70 text-muted-foreground hover:bg-accent hover:text-foreground',
              )}
            >
              {inner}
            </button>
          )
        })}
      </div>

      {trailing && (
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          {trailing}
        </div>
      )}
    </div>
  )
}
