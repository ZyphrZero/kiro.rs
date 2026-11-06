import { useId, useState } from 'react'
import {
  AlertTriangle,
  ChevronDown,
  Database,
  Info,
  RefreshCw,
} from 'lucide-react'
import { Button } from '@/components/ui/button'
import { cn, formatNumber, maskEmail } from '@/lib/utils'
import {
  cacheHitRate,
  formatCacheHitRate,
  hasCacheCounters,
  sumCacheUsage,
} from '@/lib/cache-statistics'
import type { CredentialDistribution, StatsRange } from '@/types/api'

export const CACHE_RANGE_LABELS: Record<StatsRange, string> = {
  '24h': '近24小时',
  '7d': '近7天',
  '30d': '近30天',
}

export const CACHE_COUNTING_NOTE =
  '缓存读取 ÷（普通输入 + 缓存写入 + 缓存读取），先汇总 Token 再计算。包含成功与失败请求的用量记录，可能包含本地缓存估算，不等同于上游真实命中率。缺少上游用量的失败请求可能按估算输入计入，使命中率偏低。时间范围按小时或天汇总。'
const CACHE_STALE_NOTE = '刷新失败，显示上次数据'
const CACHE_UNAVAILABLE_NOTE =
  '缓存用量缺失或无效，暂无法计算；请确认后端版本及统计数据。'

interface CacheState {
  loading?: boolean
  failed?: boolean
  stale?: boolean
}

export function CredentialCacheMetric({
  stats,
  range,
  loading,
  failed,
  stale,
  compact = false,
}: CacheState & {
  stats?: CredentialDistribution
  range: StatsRange
  compact?: boolean
}) {
  const rate = !loading && !failed ? cacheHitRate(stats) : null
  const available = !loading && !failed && hasCacheCounters(stats)
  const empty = available && stats.calls === 0
  const unavailable = failed
    ? '缓存统计加载失败'
    : loading
      ? '正在加载缓存统计'
      : empty
        ? '该账号在所选时间内没有调用记录'
        : !available
          ? CACHE_UNAVAILABLE_NOTE
          : '没有可用于计算的输入 Token'

  return (
    <details
      className={cn(
        'group/cache rounded-lg border border-border/60 bg-secondary/25',
        compact && 'max-w-xl',
      )}
    >
      <summary
        className="flex cursor-pointer list-none flex-wrap items-center gap-x-2 gap-y-1 px-3 py-2.5 text-xs [&::-webkit-details-marker]:hidden"
        title={rate === null ? unavailable : CACHE_COUNTING_NOTE}
      >
        <Database
          className="h-3.5 w-3.5 shrink-0 text-muted-foreground"
          aria-hidden="true"
        />
        <span className="text-muted-foreground">缓存命中率</span>
        <strong
          className={cn(
            'tabular-nums font-semibold',
            failed
              ? 'text-destructive'
              : rate !== null && rate > 0
                ? 'text-primary'
                : 'text-foreground',
          )}
        >
          {formatCacheHitRate(rate)}
        </strong>
        {stale && (
          <span
            className="inline-flex items-center gap-1 text-amber-600 dark:text-amber-400"
            title={CACHE_STALE_NOTE}
          >
            <AlertTriangle className="h-3 w-3" aria-hidden="true" />
            上次数据
          </span>
        )}
        <span className="ml-auto text-[11px] text-muted-foreground">
          {CACHE_RANGE_LABELS[range]}
        </span>
        <ChevronDown
          className="h-3 w-3 shrink-0 text-muted-foreground transition-transform group-open/cache:rotate-180"
          aria-hidden="true"
        />
      </summary>
      <div className="border-t border-border/50 px-3 pb-3 pt-2.5">
        {available ? (
          <>
            <dl className="grid grid-cols-3 gap-2 text-[11px]">
              <TokenValue label="普通输入" value={stats.inputTokens} />
              <TokenValue label="缓存写入" value={stats.cacheCreationTokens} />
              <TokenValue
                label="缓存读取"
                value={stats.cacheReadTokens}
                accent
              />
            </dl>
            <p className="mt-2 text-[11px] text-muted-foreground">
              {empty ? '暂无调用' : callCountLabel(stats.calls, stats.errors)}
            </p>
            {rate === null && !empty && (
              <p className="mt-2 text-[11px] text-muted-foreground">
                没有可用于计算的输入 Token
              </p>
            )}
          </>
        ) : (
          <p className="text-[11px] text-muted-foreground">{unavailable}</p>
        )}
        {stale && (
          <p className="mt-2 text-[11px] text-amber-600 dark:text-amber-400">
            {CACHE_STALE_NOTE}
          </p>
        )}
        <p className="mt-2 text-[11px] leading-relaxed text-muted-foreground">
          {CACHE_COUNTING_NOTE}
        </p>
      </div>
    </details>
  )
}

function TokenValue({
  label,
  value,
  accent,
}: {
  label: string
  value: number | undefined
  accent?: boolean
}) {
  return (
    <div className="min-w-0">
      <dt className="truncate text-muted-foreground">{label}</dt>
      <dd
        className={cn('mt-1 font-mono tabular-nums', accent && 'text-primary')}
        title={value?.toLocaleString('zh-CN')}
      >
        {value === undefined ? '—' : formatNumber(value)}
      </dd>
    </div>
  )
}

export function AccountCacheSummary({
  rows,
  range,
  onRangeChange,
  loading,
  failed,
  stale,
  refreshing,
  onRefresh,
  privacyMode = false,
}: CacheState & {
  rows: CredentialDistribution[]
  range: StatsRange
  onRangeChange: (range: StatsRange) => void
  refreshing?: boolean
  onRefresh?: () => void
  privacyMode?: boolean
}) {
  const [expanded, setExpanded] = useState(false)
  const panelId = useId()
  const usage = sumCacheUsage(rows)
  const rate = !loading && !failed ? cacheHitRate(usage) : null
  const calls = rows.reduce((sum, row) => sum + row.calls, 0)
  const errors = rows.reduce((sum, row) => sum + row.errors, 0)
  const incomplete = !loading && !failed && rows.length > 0 && !usage
  const status = failed
    ? '加载失败，暂不显示统计'
    : loading
      ? '正在加载统计…'
      : incomplete
        ? CACHE_UNAVAILABLE_NOTE
        : rows.length === 0
          ? '所选时间内暂无调用'
          : `${rows.length} 个有调用的账号 · ${callCountLabel(calls, errors)}`

  return (
    <section
      className="mb-4 overflow-hidden rounded-xl border border-border bg-card shadow-xs"
      aria-label="账号缓存统计"
    >
      <div className="flex flex-wrap items-center gap-3 px-4 py-3 sm:gap-4">
        <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-primary/10 text-primary">
          <Database className="h-4 w-4" aria-hidden="true" />
        </span>
        <div className="min-w-0">
          <h2 className="text-sm font-semibold">账号缓存统计</h2>
          <p className="mt-0.5 text-[11px] text-muted-foreground">
            全部调用账号 · {status}
          </p>
          {stale && (
            <p
              role="status"
              className="mt-1 text-[11px] text-amber-600 dark:text-amber-400"
            >
              {CACHE_STALE_NOTE}
            </p>
          )}
        </div>
        <div
          className="flex items-baseline gap-2 sm:ml-2"
          title={CACHE_COUNTING_NOTE}
        >
          <span className="text-[11px] text-muted-foreground">加权命中率</span>
          <strong className="font-mono text-xl font-semibold tabular-nums tracking-tight text-primary">
            {formatCacheHitRate(rate)}
          </strong>
        </div>
        <div className="flex flex-wrap items-center gap-2 sm:ml-auto">
          <div
            role="group"
            className="inline-flex rounded-lg bg-secondary/65 p-0.5"
            aria-label="缓存统计时间范围"
          >
            {(['24h', '7d', '30d'] as const).map((value) => (
              <Button
                key={value}
                type="button"
                size="sm"
                variant="ghost"
                className={cn(
                  'h-7 px-2.5 text-[11px]',
                  range === value &&
                    'bg-background shadow-xs hover:bg-background',
                )}
                aria-pressed={range === value}
                onClick={() => onRangeChange(value)}
              >
                {value === '24h' ? '24小时' : value === '7d' ? '7天' : '30天'}
              </Button>
            ))}
          </div>
          {onRefresh && (
            <Button
              type="button"
              variant="ghost"
              size="icon"
              className="h-7 w-7"
              aria-label="刷新缓存统计"
              disabled={refreshing}
              onClick={onRefresh}
            >
              <RefreshCw
                className={cn('h-3.5 w-3.5', refreshing && 'animate-spin')}
              />
            </Button>
          )}
          <Button
            type="button"
            variant="ghost"
            size="sm"
            className="h-7 gap-1 px-2 text-xs"
            aria-expanded={expanded}
            aria-controls={expanded ? panelId : undefined}
            onClick={() => setExpanded((value) => !value)}
          >
            {expanded ? '收起明细' : '展开明细'}
            <ChevronDown
              className={cn(
                'h-3.5 w-3.5 transition-transform',
                expanded && 'rotate-180',
              )}
            />
          </Button>
        </div>
      </div>
      {expanded && (
        <div id={panelId} className="border-t border-border/70">
          {failed || loading ? (
            <p
              role="status"
              className="px-4 py-6 text-center text-sm text-muted-foreground"
            >
              {status}
            </p>
          ) : rows.length === 0 ? (
            <p className="px-4 py-6 text-center text-sm text-muted-foreground">
              暂无调用记录，命中率暂不计算。
            </p>
          ) : (
            <>
              <div
                className="hidden grid-cols-[minmax(0,1.5fr)_repeat(5,minmax(0,1fr))] gap-3 bg-secondary/20 px-4 py-2.5 text-right text-[11px] text-muted-foreground lg:grid"
                aria-hidden="true"
              >
                <span className="text-left">账号</span>
                <span>调用次数</span>
                <span>普通输入</span>
                <span>缓存写入</span>
                <span>缓存读取</span>
                <span>命中率</span>
              </div>
              <ul className="divide-y divide-border/60">
                {rows.map((row) => {
                  const rowRate = cacheHitRate(row)
                  const available = hasCacheCounters(row)
                  return (
                    <li
                      key={row.credentialId}
                      className="grid grid-cols-2 items-center gap-3 px-4 py-3 text-xs sm:grid-cols-3 lg:grid-cols-[minmax(0,1.5fr)_repeat(5,minmax(0,1fr))]"
                    >
                      <div className="col-span-2 min-w-0 sm:col-span-3 lg:col-span-1">
                        <p
                          className="truncate font-medium"
                          title={privacyMode ? undefined : row.email}
                        >
                          {privacyMode
                            ? maskEmail(row.email) ||
                              `账号 #${row.credentialId}`
                            : row.email || `账号 #${row.credentialId}`}
                        </p>
                        <span className="mt-0.5 block text-[11px] text-muted-foreground">
                          #{row.credentialId}
                        </span>
                      </div>
                      <SummaryCell
                        label="调用次数"
                        value={formatNumber(row.calls)}
                        note={
                          row.errors > 0
                            ? `含 ${formatNumber(row.errors)} 次失败`
                            : undefined
                        }
                      />
                      <SummaryCell
                        label="普通输入"
                        value={available ? formatNumber(row.inputTokens) : '—'}
                      />
                      <SummaryCell
                        label="缓存写入"
                        value={
                          available
                            ? formatNumber(row.cacheCreationTokens!)
                            : '—'
                        }
                      />
                      <SummaryCell
                        label="缓存读取"
                        value={
                          available ? formatNumber(row.cacheReadTokens!) : '—'
                        }
                      />
                      <SummaryCell
                        label="命中率"
                        value={formatCacheHitRate(rowRate)}
                        accent
                      />
                    </li>
                  )
                })}
              </ul>
            </>
          )}
          <p className="flex items-start gap-1.5 border-t border-border/60 bg-secondary/15 px-4 py-3 text-[11px] leading-relaxed text-muted-foreground">
            <Info className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden="true" />
            <span>{CACHE_COUNTING_NOTE}账号筛选不影响本汇总。</span>
          </p>
        </div>
      )}
    </section>
  )
}

function callCountLabel(calls: number, errors: number): string {
  return `${formatNumber(calls)} 次调用${errors > 0 ? `（含 ${formatNumber(errors)} 次失败）` : ''}`
}

function SummaryCell({
  label,
  value,
  note,
  accent,
}: {
  label: string
  value: string
  note?: string
  accent?: boolean
}) {
  return (
    <div className="min-w-0 lg:text-right">
      <span className="mb-1 block text-[11px] text-muted-foreground lg:hidden">
        {label}
      </span>
      <span
        className={cn(
          'font-mono tabular-nums',
          accent && 'font-semibold text-primary',
        )}
      >
        {value}
      </span>
      {note && (
        <span className="mt-1 block text-[11px] text-muted-foreground">
          {note}
        </span>
      )}
    </div>
  )
}
