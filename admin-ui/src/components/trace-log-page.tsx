import { useState } from 'react'
import { toast } from 'sonner'
import {
  ScrollText,
  RefreshCw,
  ChevronRight,
  ChevronLeft,
  ChevronDown,
  AlertTriangle,
  CheckCircle2,
  Unplug,
  Settings2,
} from 'lucide-react'
import { Card, CardContent } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuLabel,
} from '@/components/ui/dropdown-menu'
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
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
import {
  useLogGovernanceConfig,
  useSetLogGovernanceConfig,
} from '@/hooks/use-credentials'
import { extractErrorMessage } from '@/lib/utils'
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
  if (keyId === 0) return '管理员API密钥'
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
    <TooltipProvider delayDuration={150}>
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
    </TooltipProvider>
  )
}

function TraceRow({ rec }: { rec: TraceRecord }) {
  const [open, setOpen] = useState(false)
  const errStyle = rec.errorType ? outcomeStyle(rec.errorType) : null
  return (
    <>
      <tr
        className="cursor-pointer whitespace-nowrap border-b border-border/40 hover:bg-accent/40"
        onClick={() => setOpen((v) => !v)}
      >
        <td className="py-2.5 pl-3 pr-2">
          {open ? (
            <ChevronDown className="h-4 w-4 text-muted-foreground" />
          ) : (
            <ChevronRight className="h-4 w-4 text-muted-foreground" />
          )}
        </td>
        <td className="py-2.5 pr-3 text-[13px] tabular-nums text-muted-foreground whitespace-nowrap">
          {formatTime(rec.ts)}
        </td>
        <td className="py-2.5 pr-3 text-[13px]">
          <span className="inline-block max-w-[220px] truncate align-middle">{rec.model}</span>
          {rec.isStream && <Badge variant="outline" className="ml-1.5">流式</Badge>}
        </td>
        <td className="py-2.5 pr-3 text-[13px]">
          {rec.keyId === 0 ? (
            <span className="text-muted-foreground">{keyLabel(rec.keyId, rec.keyName)}</span>
          ) : (
            <Badge variant="outline">{keyLabel(rec.keyId, rec.keyName)}</Badge>
          )}
        </td>
        <td className="py-2.5 pr-3">
          <StatusBadge status={rec.finalStatus} />
        </td>
        <TraceCredentialCell rec={rec} />
        <td className="py-2.5 pr-3 text-[12px] tabular-nums">
          <TokenCell rec={rec} />
        </td>
        <td className="py-2.5 pr-3 text-[13px] tabular-nums">
          {rec.credits != null && rec.credits > 0 ? rec.credits.toFixed(4) : '—'}
        </td>
        <td className="py-2.5 pr-3 text-[13px] tabular-nums text-muted-foreground">
          {rec.firstTokenMs != null ? formatDuration(rec.firstTokenMs) : '—'}
        </td>
        <td className="py-2.5 pr-3">
          {errStyle ? <Badge variant={errStyle.variant}>{errStyle.label}</Badge> : '—'}
        </td>
        <td className="py-2.5 pr-3 text-[13px] tabular-nums">
          {Math.max(0, rec.totalAttempts - 1)}
        </td>
        <td className="py-2.5 pr-3 text-[13px] tabular-nums text-muted-foreground">
          {formatDuration(rec.durationMs)}
        </td>
      </tr>
      {open && <ExpandedTraceRow rec={rec} />}
    </>
  )
}

function TraceCredentialCell({ rec }: { rec: TraceRecord }) {
  return (
    <td className="py-2.5 pr-3 text-[13px]">
      <span className="inline-block max-w-[220px] truncate align-middle">
        {credLabel(rec.finalCredentialId, rec.finalEmail)}
      </span>
    </td>
  )
}

function ExpandedTraceRow({ rec }: { rec: TraceRecord }) {
  return (
    <tr className="border-b border-border/40 bg-secondary/20">
      <td colSpan={12} className="px-3 py-3">
        <ExpandedDetail rec={rec} />
      </td>
    </tr>
  )
}

/** 展开后的链路详情：错误摘要 + 每跳时间线 */
function ExpandedDetail({ rec }: { rec: TraceRecord }) {
  return (
    <div className="space-y-3">
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

/** 日志治理设置下拉：trace 启用开关 + trace 保留天数 + usage 保留天数 */
function GovernanceButton() {
  const [open, setOpen] = useState(false)
  const { data: cfg, isLoading } = useLogGovernanceConfig()
  const { mutate, isPending } = useSetLogGovernanceConfig()
  const [traceDays, setTraceDays] = useState('')
  const [usageDays, setUsageDays] = useState('')

  const enabled = cfg?.traceEnabled ?? true

  const save = (patch: Record<string, unknown>, ok: string) => {
    mutate(patch, {
      onSuccess: () => toast.success(ok),
      onError: (err) => toast.error('保存失败：' + extractErrorMessage(err)),
    })
  }

  const submitDays = (
    e: React.FormEvent,
    field: 'traceRetentionDays' | 'usageLogRetentionDays',
    raw: string,
    reset: () => void,
  ) => {
    e.preventDefault()
    const n = parseInt(raw, 10)
    if (isNaN(n) || n < 1 || n > 365) {
      toast.error('保留天数需在 1..=365')
      return
    }
    save({ [field]: n }, '保留天数已更新')
    reset()
  }

  return (
    <DropdownMenu open={open} onOpenChange={setOpen}>
      <DropdownMenuTrigger asChild>
        <Button size="sm" variant="outline">
          <Settings2 className="h-3.5 w-3.5" />
          治理设置
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-72">
        <DropdownMenuLabel>请求链路追踪</DropdownMenuLabel>
        <div className="px-2 pb-2">
          <div className="flex items-center justify-between gap-2 rounded-md bg-secondary/40 px-2.5 py-2">
            <div className="text-xs">
              <div className="font-medium text-foreground">
                {enabled ? '已启用' : '已关闭'}
              </div>
              <div className="leading-snug text-muted-foreground">
                {enabled
                  ? '记录每次请求的完整重试链路到 traces.db'
                  : '不再写入新链路（历史记录仍可查询）'}
              </div>
            </div>
            <Switch
              checked={enabled}
              disabled={isLoading || isPending}
              onCheckedChange={(v) =>
                save({ traceEnabled: v }, v ? '已开启链路追踪' : '已关闭链路追踪')
              }
            />
          </div>
        </div>
        <DropdownMenuLabel className="pt-1">
          trace 保留天数（当前 {cfg?.traceRetentionDays ?? '—'}）
        </DropdownMenuLabel>
        <form
          onSubmit={(e) => submitDays(e, 'traceRetentionDays', traceDays, () => setTraceDays(''))}
          className="flex items-center gap-1.5 px-2 pb-2"
        >
          <Input
            type="number"
            min={1}
            max={365}
            placeholder="天数"
            value={traceDays}
            onChange={(e) => setTraceDays(e.target.value)}
            disabled={isPending}
            className="h-7 text-xs"
          />
          <Button type="submit" size="sm" variant="outline" className="h-7 text-xs" disabled={isPending || !traceDays.trim()}>
            保存
          </Button>
        </form>
        <DropdownMenuLabel className="pt-1">
          usage 日志保留天数（当前 {cfg?.usageLogRetentionDays ?? '—'}）
        </DropdownMenuLabel>
        <form
          onSubmit={(e) => submitDays(e, 'usageLogRetentionDays', usageDays, () => setUsageDays(''))}
          className="flex items-center gap-1.5 px-2 pb-2"
        >
          <Input
            type="number"
            min={1}
            max={365}
            placeholder="天数"
            value={usageDays}
            onChange={(e) => setUsageDays(e.target.value)}
            disabled={isPending}
            className="h-7 text-xs"
          />
          <Button type="submit" size="sm" variant="outline" className="h-7 text-xs" disabled={isPending || !usageDays.trim()}>
            保存
          </Button>
        </form>
      </DropdownMenuContent>
    </DropdownMenu>
  )
}


const PAGE_SIZE = 50

export function TraceLogPage() {
  const [status, setStatus] = useState('')
  const [errorType, setErrorType] = useState('')
  const [keyId, setKeyId] = useState('')
  const [onlyFailed, setOnlyFailed] = useState(false)
  const [page, setPage] = useState(0)

  const { data: keysData } = useClientKeys()
  const keyOptions = [
    { value: '', label: '全部 Key' },
    { value: '0', label: 'master' },
    ...(keysData?.keys ?? []).map((k) => ({ value: String(k.id), label: k.name })),
  ]

  // 筛选条件变化时回到第一页
  const resetTo = <T,>(setter: (v: T) => void) => (v: T) => {
    setter(v)
    setPage(0)
  }

  const query: TraceQuery = {
    status: status || undefined,
    errorType: errorType || undefined,
    keyId: keyId ? Number(keyId) : undefined,
    onlyFailed: onlyFailed || undefined,
    limit: PAGE_SIZE,
    offset: page * PAGE_SIZE,
  }
  const { data, isLoading, isFetching, refetch } = useTraces(query)
  const records = data?.records ?? []
  const total = data?.total ?? 0
  const totalPages = Math.max(1, Math.ceil(total / PAGE_SIZE))

  return (
    <div className="space-y-5">
      {/* 筛选栏 */}
      <div className="flex flex-wrap items-center gap-3">
        <div className="flex items-center gap-2">
          <ScrollText className="h-5 w-5 text-muted-foreground" />
          <h2 className="text-lg font-semibold tracking-tight">请求日志</h2>
          {total > 0 && <Badge variant="secondary">{total}</Badge>}
        </div>
        <div className="ml-auto flex flex-wrap items-center gap-2">
          <Select value={keyId} onChange={resetTo(setKeyId)} options={keyOptions} />
          <Select value={status} onChange={resetTo(setStatus)} options={STATUS_OPTIONS} />
          <Select
            value={errorType}
            onChange={resetTo(setErrorType)}
            options={ERROR_TYPE_OPTIONS}
          />
          <Button
            size="sm"
            variant={onlyFailed ? 'default' : 'outline'}
            onClick={() => {
              setOnlyFailed((v) => !v)
              setPage(0)
            }}
          >
            只看失败
          </Button>
          <GovernanceButton />
          <Button size="sm" variant="outline" onClick={() => refetch()} disabled={isFetching}>
            <RefreshCw className={`h-3.5 w-3.5 ${isFetching ? 'animate-spin' : ''}`} />
            刷新
          </Button>
        </div>
      </div>

      <Card>
        <CardContent className="p-0">
          {isLoading ? (
            <div className="p-6 text-sm text-muted-foreground">加载中…</div>
          ) : records.length === 0 ? (
            <div className="p-6 text-sm text-muted-foreground">
              暂无记录。发起几次 /v1/messages 请求后即可看到链路。
            </div>
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full min-w-[1080px] text-left">
                <thead>
                  <tr className="whitespace-nowrap border-b border-border/60 text-[12px] uppercase tracking-wider text-muted-foreground">
                    <th className="py-2 pl-3 pr-2 font-medium"></th>
                    <th className="py-2 pr-3 font-medium">时间</th>
                    <th className="py-2 pr-3 font-medium">模型</th>
                    <th className="py-2 pr-3 font-medium">客户端 Key</th>
                    <th className="py-2 pr-3 font-medium">状态</th>
                    <th className="py-2 pr-3 font-medium">最终凭据</th>
                    <th className="py-2 pr-3 font-medium">Token</th>
                    <th className="py-2 pr-3 font-medium">费用</th>
                    <th className="py-2 pr-3 font-medium">首Token</th>
                    <th className="py-2 pr-3 font-medium">错误类型</th>
                    <th className="py-2 pr-3 font-medium">重试</th>
                    <th className="py-2 pr-3 font-medium">耗时</th>
                  </tr>
                </thead>
                <tbody>
                  {records.map((rec) => (
                    <TraceRow key={rec.traceId} rec={rec} />
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </CardContent>
      </Card>

      {total > PAGE_SIZE && (
        <div className="flex items-center justify-center gap-2">
          <Button
            variant="outline"
            size="sm"
            onClick={() => setPage((p) => Math.max(0, p - 1))}
            disabled={page === 0 || isFetching}
          >
            <ChevronLeft className="h-3.5 w-3.5" />
            上一页
          </Button>
          <div className="px-3 text-sm tabular-nums text-muted-foreground">
            第 <span className="font-medium text-foreground">{page + 1}</span> /{' '}
            {totalPages} 页
            <span className="mx-1.5 text-muted-foreground/50">·</span>共 {total} 条
          </div>
          <Button
            variant="outline"
            size="sm"
            onClick={() => setPage((p) => Math.min(totalPages - 1, p + 1))}
            disabled={page >= totalPages - 1 || isFetching}
          >
            下一页
            <ChevronRight className="h-3.5 w-3.5" />
          </Button>
        </div>
      )}
    </div>
  )
}




