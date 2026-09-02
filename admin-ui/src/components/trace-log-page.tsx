import { useEffect, useMemo, useRef, useState } from 'react'
import { toast } from 'sonner'
import {
  ScrollText,
  RefreshCw,
  ChevronRight,
  ChevronLeft,
  AlertTriangle,
  CheckCircle2,
  Unplug,
  Search,
  X,
  Copy,
} from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from '@/components/ui/tooltip'
import {
  Select as UiSelect,
  SelectTrigger as UiSelectTrigger,
  SelectValue as UiSelectValue,
  SelectContent as UiSelectContent,
  SelectItem as UiSelectItem,
} from '@/components/ui/select'
import { useTraces } from '@/hooks/use-traces'
import { useClientKeys } from '@/hooks/use-client-keys'
import { useGroupOptions } from '@/hooks/use-groups'
import { useUrlState } from '@/hooks/use-url-state'
import {
  ConsoleTable,
  type ConsoleColumn,
} from '@/components/console/data-table'
import { BulkBar } from '@/components/console/bulk-bar'
import {
  DetailDrawer,
  DrawerField,
  DrawerSection,
} from '@/components/console/detail-drawer'
import {
  TimeRangePicker,
  rangeToStartMs,
  type TimeRange,
} from '@/components/console/time-range'
import { outcomeTone, railDotClass, type RailTone } from '@/components/console/rail'
import type { TraceAttempt, TraceQuery, TraceRecord } from '@/types/api'

/** 失败分类 → 中文标签 + Badge 颜色 */
function outcomeStyle(outcome: string): {
  label: string
  variant: 'default' | 'secondary' | 'destructive' | 'outline' | 'success' | 'warning'
} {
  switch (outcome) {
    case 'success':
      return { label: '成功', variant: 'success' }
    case 'quota_exhausted':
      return { label: '额度耗尽', variant: 'warning' }
    case 'account_throttled':
      return { label: '账号风控', variant: 'warning' }
    case 'auth_failed':
      return { label: '鉴权失败', variant: 'destructive' }
    case 'transient':
      return { label: '瞬态错误', variant: 'outline' }
    case 'network_error':
      return { label: '网络错误', variant: 'destructive' }
    case 'bad_request':
      return { label: '请求错误', variant: 'destructive' }
    case 'stream_interrupted':
      return { label: '流中断', variant: 'warning' }
    default:
      return { label: outcome || '未知', variant: 'secondary' }
  }
}

/**
 * 失败分类 → 轨迹节点圆点色。
 *
 * 委托给共享的状态轨映射：日志行的左侧色轨、凭据行的状态、这里的链路节点用同一套
 * 四档语义，异常在三个页面里是同一个颜色。原先本页自带一份 switch，与凭据卡片各判
 * 一次，账号风控在一边是 amber、另一边是 orange。
 */
function outcomeDot(outcome: string): string {
  return railDotClass(outcomeTone(outcome))
}

/** 整条 trace 的严重度 → 左侧色轨 */
function traceTone(rec: TraceRecord): RailTone {
  if (rec.finalStatus === 'success') {
    // 成功但重试过：请求被救回来了，可池子里有凭据在失败 —— 值得看一眼，但不是故障
    return rec.totalAttempts > 1 ? 'warn' : 'none'
  }
  if (rec.finalStatus === 'interrupted') return 'warn'
  return outcomeTone(rec.errorType ?? '')
}

/** 最终状态 → 徽章 */
function StatusBadge({ status }: { status: string }) {
  if (status === 'success')
    return (
      <Badge variant="success">
        <CheckCircle2 className="mr-1 h-3 w-3" />
        成功
      </Badge>
    )
  if (status === 'interrupted')
    return (
      <Badge variant="warning">
        <Unplug className="mr-1 h-3 w-3" />
        中断
      </Badge>
    )
  return (
    <Badge variant="destructive">
      <AlertTriangle className="mr-1 h-3 w-3" />
      失败
    </Badge>
  )
}

function formatTime(ts: string): string {
  const d = new Date(ts)
  if (isNaN(d.getTime())) return ts
  return d.toLocaleString('zh-CN', { hour12: false })
}

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`
  return `${(ms / 1000).toFixed(2)}s`
}

function formatTokens(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(2) + 'M'
  if (n >= 1_000) return (n / 1_000).toFixed(1) + 'K'
  return String(n)
}

/** 千位分隔的完整数值（用于明细悬浮框） */
function formatTokenFull(n: number): string {
  return n.toLocaleString('en-US')
}

function credLabel(id: number, email?: string | null): string {
  if (id === 0) return '—'
  return email ? email : `#${id}`
}

function keyLabel(keyId: number, keyName?: string | null): string {
  if (keyName) return keyName
  return `#${keyId}`
}

const STATUS_OPTIONS = [
  { value: '', label: '全部状态' },
  { value: 'success', label: '成功' },
  { value: 'error', label: '失败' },
  { value: 'interrupted', label: '中断' },
]

const ERROR_TYPE_OPTIONS = [
  { value: '', label: '全部错误类型' },
  { value: 'quota_exhausted', label: '额度耗尽' },
  { value: 'account_throttled', label: '账号风控' },
  { value: 'auth_failed', label: '鉴权失败' },
  { value: 'transient', label: '瞬态错误' },
  { value: 'network_error', label: '网络错误' },
  { value: 'bad_request', label: '请求错误' },
  { value: 'stream_interrupted', label: '流中断' },
  { value: 'unknown', label: '未知' },
]

/** 单跳明细行 */
function AttemptRow({ a }: { a: TraceAttempt }) {
  const style = outcomeStyle(a.outcome)
  return (
    <div className="rounded-lg border border-border/50 bg-secondary/30 p-3">
      <div className="flex flex-wrap items-center gap-2 text-[13px]">
        <span className="font-mono text-muted-foreground">#{a.attempt}</span>
        <Badge variant={style.variant}>{style.label}</Badge>
        <span className="text-muted-foreground">凭据</span>
        <span className="font-medium">{credLabel(a.credentialId, a.email)}</span>
        {a.endpoint && <Badge variant="outline">{a.endpoint}</Badge>}
        <span className="text-muted-foreground">HTTP</span>
        <span className="font-mono">{a.httpStatus ?? '—'}</span>
        <span className="ml-auto font-mono text-muted-foreground">
          {formatDuration(a.durationMs)}
        </span>
      </div>
      {a.errorSnippet && (
        <pre className="mt-2 max-h-40 overflow-auto whitespace-pre-wrap break-all rounded-md bg-background/60 p-2 font-mono text-[11px] text-muted-foreground">
          {a.errorSnippet}
        </pre>
      )}
    </div>
  )
}

/** 可展开的链路行 */
/** Token 用量单元格：紧凑展示总量，hover 显示分项明细 */
function TokenCell({ rec }: { rec: TraceRecord }) {
  const input = rec.inputTokens ?? 0
  const output = rec.outputTokens ?? 0
  const cacheCreation = rec.cacheCreationTokens ?? 0
  const cacheRead = rec.cacheReadTokens ?? 0
  const total = rec.totalTokens ?? input + output + cacheCreation + cacheRead
  // 全 0（早期失败、未走到上游）时不显示明细，仅占位
  if (total === 0) {
    return <span className="text-muted-foreground">—</span>
  }
  const rows: Array<[string, number]> = [
    ['输入 Token', input],
    ['输出 Token', output],
  ]
  if (cacheCreation > 0) rows.push(['缓存创建 Token', cacheCreation])
  if (cacheRead > 0) rows.push(['缓存读取 Token', cacheRead])
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className="inline-flex items-center gap-1 font-mono tabular-nums cursor-default border-b border-dotted border-muted-foreground/40">
          <span className="text-emerald-600 dark:text-emerald-400">
            ↓{formatTokens(input + cacheCreation + cacheRead)}
          </span>
          <span className="text-violet-600 dark:text-violet-400">
            ↑{formatTokens(output)}
          </span>
        </span>
      </TooltipTrigger>
      <TooltipContent className="p-0">
        <div className="min-w-[180px] px-3 py-2">
          <div className="mb-1.5 text-[13px] font-semibold">Token 明细</div>
          <div className="space-y-1 text-[12px]">
            {rows.map(([label, val]) => (
              <div key={label} className="flex items-center justify-between gap-6">
                <span className="text-muted-foreground">{label}</span>
                <span className="font-mono tabular-nums">{formatTokenFull(val)}</span>
              </div>
            ))}
            <div className="mt-1 flex items-center justify-between gap-6 border-t border-border/50 pt-1">
              <span className="font-medium">总 Token</span>
              <span className="font-mono font-semibold tabular-nums">
                {formatTokenFull(total)}
              </span>
            </div>
          </div>
        </div>
      </TooltipContent>
    </Tooltip>
  )
}

/**
 * 故障转移轨迹（本页签名元素）：把一次请求的 attempts[] 画成横向重试链路，
 * 按每跳结果着色。单次成功只显示一个安静的圆点；重试/故障转移时展开为带凭据号
 * 的节点串，让"这次请求怎么被救回来的"一眼可读。顺序即尝试次序。
 */
function AttemptChain({ rec }: { rec: TraceRecord }) {
  const attempts = rec.attempts ?? []
  if (attempts.length === 0) {
    return <span className="text-muted-foreground/50">—</span>
  }
  if (attempts.length === 1 && rec.finalStatus === 'success') {
    return (
      <span className={`inline-block h-2 w-2 rounded-full ${outcomeDot(attempts[0].outcome)}`} />
    )
  }
  return (
    <span className="inline-flex items-center gap-1">
      {attempts.map((a, i) => (
        <span key={a.attempt} className="inline-flex items-center gap-1">
          {i > 0 && <span className="text-muted-foreground/40">→</span>}
          <Tooltip>
            <TooltipTrigger asChild>
              <span className="inline-flex cursor-default items-center gap-1 rounded border border-border/60 bg-secondary/40 px-1.5 py-0.5 font-mono text-[11px] tabular-nums">
                <span className={`h-1.5 w-1.5 rounded-full ${outcomeDot(a.outcome)}`} />
                {a.credentialId > 0 ? `#${a.credentialId}` : '—'}
              </span>
            </TooltipTrigger>
            <TooltipContent className="font-mono text-[11px]">
              第 {a.attempt + 1} 跳 · {outcomeStyle(a.outcome).label}
              {a.httpStatus != null ? ` · HTTP ${a.httpStatus}` : ''}
              {a.endpoint ? ` · ${a.endpoint}` : ''} · {formatDuration(a.durationMs)}
            </TooltipContent>
          </Tooltip>
        </span>
      ))}
    </span>
  )
}

/**
 * 计费面板：以 credit（上游 meteringEvent 的真实计费）为核心，直观体现缓存省钱效果。
 *
 * 关键指标「每千输入 token 的 credit」：Kiro 后端对命中缓存的输入按更低费率计费，
 * 所以同规模输入下，该比值越低 = 缓存命中越多 = 越省钱。冷启动请求（大前缀首次进
 * 缓存）该比值偏高，之后重复内容命中后明显下降——对比同类请求的这个值即可看出缓存
 * 有没有帮你省钱。
 *
 * 注：input token 为 contextUsage 推算的粗粒度估算；credit 是上游真实计费口径，
 * 是判断成本的可信信号（缓存的 cache_read token 为中转层估算，仅供参考）。
 */
function CacheBillingPanel({ rec }: { rec: TraceRecord }) {
  const credit = rec.credits ?? 0
  if (credit <= 0) return null
  // 总输入 = 未缓存输入 + 缓存创建 + 缓存读取。
  // 这是缓存拆分的不变量（split_against_total 保证三者之和 == 总 prompt），
  // 用作分母才稳定；rec.inputTokens 在有缓存拆分时只剩「未缓存那部分」，不能当分母。
  const freshInput = rec.inputTokens ?? 0
  const cacheCreation = rec.cacheCreationTokens ?? 0
  const cacheRead = rec.cacheReadTokens ?? 0
  const promptTotal = freshInput + cacheCreation + cacheRead
  const perK = promptTotal > 0 ? credit / (promptTotal / 1000) : null

  // boldness 只花在一处：credit（真实成本）为主角，其余中性陪衬。
  const items: Array<{ label: string; value: string; hint?: string; primary?: boolean }> = [
    { label: '真实计费', value: credit.toFixed(4), hint: 'credit（上游 metering）', primary: true },
    { label: '总输入 Token', value: formatTokens(promptTotal), hint: '含缓存命中·估算' },
  ]
  if (perK != null) {
    items.push({
      label: '每千输入 credit',
      value: perK.toFixed(4),
      hint: '越低=缓存命中越多',
    })
  }
  // 有缓存拆分时，额外展示「未缓存输入」（真正按全价计费的部分）
  if (cacheRead > 0 || cacheCreation > 0) {
    items.push({
      label: '未缓存输入',
      value: formatTokens(freshInput),
      hint: '按全价计费部分·估算',
    })
  }

  return (
    <div className="rounded-lg border border-border/50 bg-secondary/30 p-3">
      <div className="mb-2 flex items-center gap-2 text-[12px] font-medium text-muted-foreground">
        <span>计费与缓存效率</span>
        <span className="text-[11px] font-normal text-muted-foreground/70">
          credit 为上游真实计费，是判断缓存省钱的可信信号
        </span>
      </div>
      <div className="grid grid-cols-2 gap-x-6 gap-y-2 sm:grid-cols-3">
        {items.map((it) => (
          <div key={it.label} className="min-w-0">
            <div className="text-[11px] text-muted-foreground">{it.label}</div>
            <div
              className={
                it.primary
                  ? 'font-mono tabular-nums text-[15px] font-semibold text-sky-700 dark:text-sky-300'
                  : 'font-mono tabular-nums text-[15px] font-medium text-foreground/85'
              }
            >
              {it.value}
            </div>
            {it.hint && (
              <div className="text-[10px] text-muted-foreground/70">{it.hint}</div>
            )}
          </div>
        ))}
      </div>
    </div>
  )
}

/** 展开后的链路详情：计费/缓存效率 + 错误摘要 + 每跳时间线 */
function ExpandedDetail({ rec }: { rec: TraceRecord }) {
  return (
    <div className="space-y-3">
      <CacheBillingPanel rec={rec} />
      {rec.errorMessage && (
        <div className="rounded-lg border border-destructive/30 bg-destructive/5 p-3 text-[13px] text-destructive">
          {rec.errorMessage}
        </div>
      )}
      {rec.interruptedAfterBytes != null && (
        <div className="text-[12px] text-muted-foreground">
          中断前已发送 {rec.interruptedAfterBytes} 字节
        </div>
      )}
      <div className="text-[12px] font-medium text-muted-foreground">
        尝试链路（{rec.attempts.length} 次
        {rec.attempts.length > 1 ? `，含 ${rec.attempts.length - 1} 次重试` : "，未重试"}）
      </div>
      <div className="space-y-2">
        {rec.attempts.length === 0 ? (
          <div className="text-[13px] text-muted-foreground">无尝试记录（请求未到达上游）</div>
        ) : (
          rec.attempts.map((a) => <AttemptRow key={a.attempt} a={a} />)
        )}
      </div>
    </div>
  )
}

/** 下拉筛选器 */
function Select({
  value,
  onChange,
  options,
}: {
  value: string
  onChange: (v: string) => void
  options: { value: string; label: string }[]
}) {
  // radix Select 不允许空字符串 value，用哨兵 "__all__" 代表「空/全部」，对外透明。
  const SENTINEL = '__all__'
  return (
    <UiSelect
      value={value === '' ? SENTINEL : value}
      onValueChange={(v) => onChange(v === SENTINEL ? '' : v)}
    >
      <UiSelectTrigger className="h-8 w-auto min-w-[120px]">
        <UiSelectValue />
      </UiSelectTrigger>
      <UiSelectContent>
        {options.map((o) => (
          <UiSelectItem key={o.value} value={o.value === '' ? SENTINEL : o.value}>
            {o.label}
          </UiSelectItem>
        ))}
      </UiSelectContent>
    </UiSelect>
  )
}

const PAGE_SIZE = 50

/** 默认时间窗口：24 小时。够覆盖"昨天那次失败"，又不至于一上来就全表扫。 */
const DEFAULT_RANGE_MINUTES = '1440'

const URL_DEFAULTS = {
  status: '',
  errorType: '',
  keyId: '',
  group: '',
  q: '',
  range: DEFAULT_RANGE_MINUTES,
  page: '0',
}

/** 搜索输入防抖：输入过程中不打请求，停手 300ms 再查 */
function useDebounced<T>(value: T, delay = 300): T {
  const [debounced, setDebounced] = useState(value)
  useEffect(() => {
    const t = window.setTimeout(() => setDebounced(value), delay)
    return () => window.clearTimeout(t)
  }, [value, delay])
  return debounced
}

/** `/` 聚焦搜索框 —— 手不离键盘就能开始筛 */
function useSlashFocus(ref: React.RefObject<HTMLInputElement | null>) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== '/' || e.metaKey || e.ctrlKey || e.altKey) return
      const t = e.target as HTMLElement | null
      if (
        t &&
        (t.tagName === 'INPUT' ||
          t.tagName === 'TEXTAREA' ||
          t.isContentEditable)
      ) {
        return
      }
      e.preventDefault()
      ref.current?.focus()
      ref.current?.select()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [ref])
}

/** 表格列定义。默认 8 列，其余进列控制菜单 —— 12 列全摆开必然横向滚动。 */
function useTraceColumns(): ConsoleColumn<TraceRecord>[] {
  return useMemo(
    () => [
      {
        id: 'ts',
        header: '时间',
        cell: (r) => (
          <span className="console-num text-muted-foreground">
            {formatTime(r.ts)}
          </span>
        ),
      },
      {
        id: 'model',
        header: '模型',
        cell: (r) => (
          <span className="inline-flex max-w-[200px] items-center gap-1.5">
            <span className="truncate">{r.model}</span>
            {r.isStream && (
              <span
                className="shrink-0 text-[10px] text-muted-foreground"
                title="流式响应"
              >
                流
              </span>
            )}
          </span>
        ),
      },
      {
        id: 'status',
        header: '状态',
        cell: (r) => <StatusBadge status={r.finalStatus} />,
      },
      {
        id: 'credential',
        header: '最终凭据',
        cell: (r) => (
          <span className="inline-block max-w-[190px] truncate">
            {credLabel(r.finalCredentialId, r.finalEmail)}
          </span>
        ),
      },
      {
        id: 'chain',
        header: '故障转移',
        hint: '这次请求走过的重试链路，顺序即尝试次序',
        cell: (r) => <AttemptChain rec={r} />,
      },
      {
        id: 'tokens',
        header: 'Token',
        cell: (r) => <TokenCell rec={r} />,
      },
      {
        id: 'credits',
        header: '费用',
        align: 'right',
        hint: 'credit —— 上游 metering 的真实计费',
        cell: (r) => (
          <span className="console-num">
            {r.credits != null && r.credits > 0 ? r.credits.toFixed(4) : '—'}
          </span>
        ),
      },
      {
        id: 'duration',
        header: '耗时',
        align: 'right',
        cell: (r) => (
          <span className="console-num text-muted-foreground">
            {formatDuration(r.durationMs)}
          </span>
        ),
      },
      {
        id: 'key',
        header: '入口 Key',
        optional: true,
        cell: (r) => (
          <Badge variant="outline">{keyLabel(r.keyId, r.keyName)}</Badge>
        ),
      },
      {
        id: 'firstToken',
        header: '首 Token',
        optional: true,
        align: 'right',
        hint: '首个 token 到达耗时，仅流式有值',
        cell: (r) => (
          <span className="console-num text-muted-foreground">
            {r.firstTokenMs != null ? formatDuration(r.firstTokenMs) : '—'}
          </span>
        ),
      },
      {
        id: 'errorType',
        header: '错误类型',
        optional: true,
        cell: (r) => {
          if (!r.errorType) return <span className="text-muted-foreground">—</span>
          const s = outcomeStyle(r.errorType)
          return <Badge variant={s.variant}>{s.label}</Badge>
        },
      },
      {
        id: 'traceId',
        header: 'Trace ID',
        optional: true,
        cell: (r) => (
          <span className="console-num text-[11px] text-muted-foreground">
            {r.traceId.slice(0, 12)}
          </span>
        ),
      },
    ],
    [],
  )
}

export function TraceLogPage() {
  const [url, patchUrl, resetUrl] = useUrlState('traces', URL_DEFAULTS)
  const [searchDraft, setSearchDraft] = useState(url.q)
  const debouncedSearch = useDebounced(searchDraft)
  const searchRef = useRef<HTMLInputElement>(null)
  const [detail, setDetail] = useState<TraceRecord | null>(null)
  const [selectedTraceIds, setSelectedTraceIds] = useState<Set<number | string>>(new Set())
  const [now, setNow] = useState(() => Date.now())
  useSlashFocus(searchRef)

  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 30_000)
    return () => window.clearInterval(timer)
  }, [])

  // 搜索词稳定后才写进 URL / 触发查询
  useEffect(() => {
    if (debouncedSearch !== url.q) patchUrl({ q: debouncedSearch, page: '0' })
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [debouncedSearch])

  const page = Number(url.page) || 0
  const range: TimeRange = {
    minutes: url.range === '' ? null : Number(url.range),
  }

  const { data: keysData } = useClientKeys()
  const groupOptions = useGroupOptions()

  const keyOptions = [
    { value: '', label: '全部 Key' },
    ...(keysData?.keys ?? []).map((k) => ({ value: String(k.id), label: k.name })),
  ]
  const groupSelectOptions = [
    { value: '', label: '全部分组' },
    ...groupOptions.map((g) => ({ value: g, label: g })),
  ]

  // 时间窗口按分钟数换算成起始秒；随自动刷新时钟滑动，始终表示“最近 N 分钟”。
  const startTime = useMemo(() => {
    const ms = rangeToStartMs(range, now)
    return ms == null ? undefined : Math.floor(ms / 1000)
  }, [url.range, now])

  const query: TraceQuery = {
    status: url.status || undefined,
    errorType: url.errorType || undefined,
    keyId: url.keyId ? Number(url.keyId) : undefined,
    group: url.group || undefined,
    q: url.q || undefined,
    startTime,
    limit: PAGE_SIZE,
    offset: page * PAGE_SIZE,
  }
  const { data, isLoading, isFetching, refetch } = useTraces(query)
  const records = data?.records ?? []
  const total = data?.total ?? 0
  const totalPages = Math.max(1, Math.ceil(total / PAGE_SIZE))
  const columns = useTraceColumns()

  const filterCount = [url.status, url.errorType, url.keyId, url.group, url.q].filter(
    Boolean,
  ).length

  return (
    <div className="console-scope space-y-3">
      <div className="flex flex-wrap items-center gap-2">
        <ScrollText className="h-4 w-4 text-muted-foreground" />
        <h2 className="text-lg font-semibold tracking-tight">请求日志</h2>
        <span className="console-num text-[13px] text-muted-foreground">
          {total} 条
        </span>
        {filterCount > 0 && (
          <Button
            size="sm"
            variant="ghost"
            onClick={() => {
              resetUrl()
              setSearchDraft('')
            }}
            className="h-7 px-2 text-xs"
          >
            清除 {filterCount} 个筛选
          </Button>
        )}
        <span className="ml-auto text-[11px] text-muted-foreground">
          <kbd className="rounded border border-border/70 px-1">/</kbd> 搜索 ·{' '}
          <kbd className="rounded border border-border/70 px-1">j</kbd>
          <kbd className="ml-0.5 rounded border border-border/70 px-1">k</kbd> 移动 ·{' '}
          <kbd className="rounded border border-border/70 px-1">Enter</kbd> 详情
        </span>
      </div>

      {/* 筛选栏：时间范围在最前，因为排查的第一句话通常是"刚才那几分钟" */}
      <div className="flex flex-wrap items-center gap-2">
        <TimeRangePicker
          value={range}
          onChange={(next) =>
            patchUrl({
              range: next.minutes == null ? '' : String(next.minutes),
              page: '0',
            })
          }
        />
        <div className="relative">
          <Search className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
          <input
            ref={searchRef}
            type="text"
            value={searchDraft}
            onChange={(e) => setSearchDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Escape') {
                setSearchDraft('')
                e.currentTarget.blur()
              }
            }}
            placeholder="搜索模型 / 报错 / Trace ID"
            aria-label="搜索日志"
            className="console-num h-8 w-[min(15rem,52vw)] rounded-lg border border-border bg-card/60 pl-8 pr-7 text-[12.5px] backdrop-blur placeholder:font-sans placeholder:text-muted-foreground/70 focus-visible:border-ring focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/30"
          />
          {searchDraft && (
            <button
              type="button"
              onClick={() => setSearchDraft('')}
              title="清除搜索"
              className="absolute right-1.5 top-1/2 flex h-5 w-5 -translate-y-1/2 items-center justify-center rounded-full text-muted-foreground hover:bg-accent hover:text-foreground"
            >
              <X className="h-3 w-3" />
            </button>
          )}
        </div>
        <Select
          value={url.status}
          onChange={(v) => patchUrl({ status: v, page: '0' })}
          options={STATUS_OPTIONS}
        />
        <Select
          value={url.errorType}
          onChange={(v) => patchUrl({ errorType: v, page: '0' })}
          options={ERROR_TYPE_OPTIONS}
        />
        <Select
          value={url.keyId}
          onChange={(v) => patchUrl({ keyId: v, page: '0' })}
          options={keyOptions}
        />
        <Select
          value={url.group}
          onChange={(v) => patchUrl({ group: v, page: '0' })}
          options={groupSelectOptions}
        />
        <Button
          size="sm"
          variant="outline"
          onClick={() => refetch()}
          disabled={isFetching}
          title="立即刷新（每 30 秒自动刷新）"
        >
          <RefreshCw className={`h-3.5 w-3.5 ${isFetching ? 'animate-spin' : ''}`} />
        </Button>
      </div>

      <ConsoleTable
        rows={records}
        columns={columns}
        rowKey={(r) => r.traceId}
        tone={traceTone}
        selectable
        selected={selectedTraceIds}
        onSelectedChange={setSelectedTraceIds}
        onRowActivate={setDetail}
        columnsStorageKey="kiro.traces.columns"
        loading={isLoading}
        empty={
          filterCount > 0 || url.range !== ''
            ? '当前筛选条件下没有记录。放宽时间范围或清除筛选试试。'
            : '暂无记录。发起几次 /v1/messages 请求后即可看到链路。'
        }
      />

      {/* 吸底批量操作栏 */}
      <BulkBar
        count={selectedTraceIds.size}
        onClear={() => setSelectedTraceIds(new Set())}
        noun="条日志"
      >
        <Button
          onClick={() => {
            const list = Array.from(selectedTraceIds).join('\n')
            navigator.clipboard.writeText(list)
            toast.success(`已复制 ${selectedTraceIds.size} 个 Trace ID`)
          }}
          size="sm"
          variant="ghost"
          className="h-8 px-3 text-xs gap-1.5 rounded-full hover:bg-accent"
        >
          <Copy className="h-3.5 w-3.5" />
          复制 Trace ID
        </Button>
      </BulkBar>

      {total > PAGE_SIZE && (
        <div className="flex items-center justify-center gap-2">
          <Button
            variant="outline"
            size="sm"
            onClick={() => patchUrl({ page: String(Math.max(0, page - 1)) })}
            disabled={page === 0 || isFetching}
          >
            <ChevronLeft className="h-3.5 w-3.5" />
            上一页
          </Button>
          <div className="console-num px-3 text-[13px] text-muted-foreground">
            第 <span className="font-medium text-foreground">{page + 1}</span> /{' '}
            {totalPages} 页
          </div>
          <Button
            variant="outline"
            size="sm"
            onClick={() =>
              patchUrl({ page: String(Math.min(totalPages - 1, page + 1)) })
            }
            disabled={page >= totalPages - 1 || isFetching}
          >
            下一页
            <ChevronRight className="h-3.5 w-3.5" />
          </Button>
        </div>
      )}

      <TraceDetailDrawer rec={detail} onClose={() => setDetail(null)} />
    </div>
  )
}

/**
 * 详情抽屉，替掉原先的行展开。
 *
 * 行展开会把 34px 的行顶到几百 px，下方所有行位置突变、滚动位置跟着跳 —— 想对比
 * 相邻两条记录时格外难受。抽屉让表格布局始终稳定，左边列表和右边详情能同时看，
 * j/k 还能继续在列表里走。
 */
function TraceDetailDrawer({
  rec,
  onClose,
}: {
  rec: TraceRecord | null
  onClose: () => void
}) {
  return (
    <DetailDrawer
      open={rec != null}
      onOpenChange={(open) => {
        if (!open) onClose()
      }}
      title={rec?.model ?? ''}
      subtitle={rec ? `${formatTime(rec.ts)} · ${rec.traceId}` : undefined}
    >
      {rec && (
        <div className="space-y-1">
          <DrawerSection title="结果">
            <DrawerField label="状态">
              <StatusBadge status={rec.finalStatus} />
            </DrawerField>
            {rec.errorType && (
              <DrawerField label="错误类型">
                <Badge variant={outcomeStyle(rec.errorType).variant}>
                  {outcomeStyle(rec.errorType).label}
                </Badge>
              </DrawerField>
            )}
            <DrawerField label="最终凭据" mono>
              {credLabel(rec.finalCredentialId, rec.finalEmail)}
            </DrawerField>
            <DrawerField label="入口 Key" mono>
              {keyLabel(rec.keyId, rec.keyName)}
            </DrawerField>
            <DrawerField label="总耗时" mono>
              {formatDuration(rec.durationMs)}
            </DrawerField>
            {rec.firstTokenMs != null && (
              <DrawerField label="首 Token" mono>
                {formatDuration(rec.firstTokenMs)}
              </DrawerField>
            )}
            {rec.interruptedAfterBytes != null && (
              <DrawerField label="中断前已发送" mono>
                {rec.interruptedAfterBytes} 字节
              </DrawerField>
            )}
          </DrawerSection>

          <ExpandedDetail rec={rec} />
        </div>
      )}
    </DetailDrawer>
  )
}
