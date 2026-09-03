import { Check, RefreshCw } from 'lucide-react'
import { Button } from '@/components/ui/button'
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu'
import {
  AUTO_REFRESH_INTERVAL_OPTIONS,
  useAutoRefresh,
  type AutoRefreshInterval,
} from '@/hooks/use-auto-refresh'

interface AutoRefreshControlProps {
  onRefresh: () => void | Promise<unknown>
  isRefreshing?: boolean
  defaultInterval?: AutoRefreshInterval
  resourceLabel?: string
  className?: string
}

/** 可复用的自动刷新菜单：开关、间隔、倒计时与一次性手动刷新。 */
export function AutoRefreshControl({
  onRefresh,
  isRefreshing = false,
  defaultInterval = 30,
  resourceLabel = '数据',
  className = 'flex min-w-0 items-center gap-1',
}: AutoRefreshControlProps) {
  const autoRefresh = useAutoRefresh({
    onRefresh,
    isRefreshing,
    defaultInterval,
  })
  const label = autoRefresh.isEnabled
    ? `自动刷新 · ${autoRefresh.secondsRemaining}s`
    : '自动刷新 · 关闭'

  return (
    <div className={className}>
      <DropdownMenu modal={false}>
        <DropdownMenuTrigger asChild>
          <Button
            type="button"
            variant="outline"
            size="sm"
            className="h-8 min-w-[8.75rem] justify-start px-2.5 text-xs"
            aria-label={label}
          >
            <RefreshCw className={`h-3.5 w-3.5 ${isRefreshing ? 'animate-spin' : ''}`} />
            <span className="truncate">{label}</span>
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="w-44">
          <DropdownMenuLabel>刷新设置</DropdownMenuLabel>
          <DropdownMenuItem
            role="menuitemcheckbox"
            aria-checked={autoRefresh.isEnabled}
            onSelect={autoRefresh.toggle}
          >
            <span>启用自动刷新</span>
            {autoRefresh.isEnabled ? <Check className="ml-auto" /> : null}
          </DropdownMenuItem>
          <DropdownMenuSeparator />
          <DropdownMenuLabel>刷新间隔</DropdownMenuLabel>
          {AUTO_REFRESH_INTERVAL_OPTIONS.map((next) => (
            <DropdownMenuItem
              key={next}
              role="menuitemradio"
              aria-checked={autoRefresh.interval === next}
              onSelect={() => autoRefresh.updateInterval(next)}
            >
              <span>{next} 秒</span>
              {autoRefresh.interval === next ? <Check className="ml-auto" /> : null}
            </DropdownMenuItem>
          ))}
        </DropdownMenuContent>
      </DropdownMenu>
      <Button
        type="button"
        variant="outline"
        size="icon"
        className="h-8 w-8"
        disabled={isRefreshing}
        onClick={() => {
          autoRefresh.reset()
          void onRefresh()
        }}
        aria-label={`立即刷新${resourceLabel}`}
        title="立即刷新"
      >
        <RefreshCw className={`h-3.5 w-3.5 ${isRefreshing ? 'animate-spin' : ''}`} />
      </Button>
    </div>
  )
}
