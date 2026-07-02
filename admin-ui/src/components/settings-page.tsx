import { useState } from 'react'
import { toast } from 'sonner'
import { DatabaseZap, Filter, Shuffle, ShieldAlert, ScrollText } from 'lucide-react'
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import {
  useRuntimeGovernanceConfig,
  useSetRuntimeGovernanceConfig,
  usePromptFilterDefaults,
  useSetPromptFilterDefaults,
  useAccountThrottleConfig,
  useSetAccountThrottleConfig,
  useLoadBalancingMode,
  useSetLoadBalancingMode,
  useLogGovernanceConfig,
  useSetLogGovernanceConfig,
  useGlobalProxy,
  useSetGlobalProxy,
} from '@/hooks/use-credentials'
import { ModelMappingPanel } from '@/components/model-mapping-panel'
import { extractErrorMessage } from '@/lib/utils'

// 区块外壳：图标标题 + 描述 + 内容
function SettingSection({
  icon,
  title,
  desc,
  children,
}: {
  icon: React.ReactNode
  title: string
  desc: string
  children: React.ReactNode
}) {
  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          {icon}
          {title}
        </CardTitle>
        <CardDescription>{desc}</CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">{children}</CardContent>
    </Card>
  )
}

// 一行开关：标题 + 说明 + Switch
function ToggleRow({
  label,
  desc,
  checked,
  disabled,
  onChange,
}: {
  label: string
  desc: string
  checked: boolean
  disabled?: boolean
  onChange: (v: boolean) => void
}) {
  return (
    <div className="flex items-center justify-between gap-3 rounded-md bg-secondary/40 px-3 py-2.5">
      <div className="text-sm">
        <div className="font-medium text-foreground">{label}</div>
        <div className="leading-snug text-muted-foreground">{desc}</div>
      </div>
      <Switch checked={checked} disabled={disabled} onCheckedChange={onChange} />
    </div>
  )
}

// PLACEHOLDER_SECTIONS

// 缓存 / 配额治理
function CacheQuotaSection() {
  const { data: cfg, isLoading } = useRuntimeGovernanceConfig()
  const { mutate, isPending } = useSetRuntimeGovernanceConfig()
  const [threshold, setThreshold] = useState('')
  const [ttl, setTtl] = useState('')
  const [ratio, setRatio] = useState('')
  const [meterTtl, setMeterTtl] = useState('')

  const save = (patch: Record<string, unknown>, ok: string) =>
    mutate(patch, {
      onSuccess: () => toast.success(ok),
      onError: (err) => toast.error('保存失败：' + extractErrorMessage(err)),
    })

  const num =
    (raw: string, min: number, max: number, parse: (s: string) => number, field: string, ok: string, reset: () => void) =>
    (e: React.FormEvent) => {
      e.preventDefault()
      const n = parse(raw)
      if (isNaN(n) || n < min || n > max) {
        toast.error(`需在 ${min}..=${max}`)
        return
      }
      save({ [field]: n }, ok)
      reset()
    }

  return (
    <SettingSection
      icon={<DatabaseZap className="h-4 w-4 text-sky-500" />}
      title="缓存 / 配额治理"
      desc="配额自动禁用阈值、全局响应缓存默认、Prompt cache 计量。均运行时生效并持久化。"
    >
      <div>
        <div className="mb-1.5 text-sm font-medium">
          配额自动禁用阈值（当前 {cfg ? `${cfg.quotaDisableThreshold}%` : '—'}）
        </div>
        <div className="mb-2 text-[11px] leading-snug text-muted-foreground">
          账号用量达此百分比自动禁用，回落后自动恢复；设为 100 则关闭。
        </div>
        <form onSubmit={num(threshold, 1, 100, parseFloat, 'quotaDisableThreshold', '配额阈值已更新', () => setThreshold(''))} className="flex items-center gap-1.5">
          <Input type="number" min={1} max={100} step="0.1" placeholder="百分比" value={threshold} onChange={(e) => setThreshold(e.target.value)} disabled={isPending} className="h-8 max-w-[160px] text-xs" />
          <Button type="submit" size="sm" variant="outline" className="h-8 text-xs" disabled={isPending || !threshold.trim()}>保存</Button>
        </form>
      </div>

      <ToggleRow
        label={cfg?.responseCacheEnabled ? '响应缓存：已启用' : '响应缓存：已关闭'}
        desc={cfg?.responseCacheEnabled ? '相同请求命中即回放、跳过上游' : '全局默认关闭，可被各 Key 单独覆盖'}
        checked={cfg?.responseCacheEnabled ?? false}
        disabled={isLoading || isPending}
        onChange={(v) => save({ responseCacheEnabled: v }, v ? '已开启响应缓存' : '已关闭响应缓存')}
      />

      <div>
        <div className="mb-1.5 text-sm font-medium">响应缓存 TTL 秒（当前 {cfg?.responseCacheTtlSecs ?? '—'}）</div>
        <form onSubmit={num(ttl, 1, 86400, (s) => parseInt(s, 10), 'responseCacheTtlSecs', '缓存 TTL 已更新', () => setTtl(''))} className="flex items-center gap-1.5">
          <Input type="number" min={1} max={86400} placeholder="秒" value={ttl} onChange={(e) => setTtl(e.target.value)} disabled={isPending} className="h-8 max-w-[160px] text-xs" />
          <Button type="submit" size="sm" variant="outline" className="h-8 text-xs" disabled={isPending || !ttl.trim()}>保存</Button>
        </form>
      </div>

      <div>
        <div className="mb-1.5 text-sm font-medium text-pink-600">Prompt cache 计量 · read 留存 R（当前 {cfg?.cacheReadRatio ?? '—'}）</div>
        <div className="mb-2 text-[11px] leading-snug text-muted-foreground">
          合成给下游的 token 计量旋钮（不缓存真实响应）。保留 read×R、其余按未命中推回 input，不触碰 creation。1=给足折扣，0=不给。可被各 Key 覆盖。
        </div>
        <form onSubmit={num(ratio, 0, 1, parseFloat, 'cacheReadRatio', '缓存命中率已更新', () => setRatio(''))} className="flex items-center gap-1.5">
          <Input type="number" min={0} max={1} step={0.05} placeholder="0 ~ 1" value={ratio} onChange={(e) => setRatio(e.target.value)} disabled={isPending} className="h-8 max-w-[160px] text-xs" />
          <Button type="submit" size="sm" variant="outline" className="h-8 text-xs" disabled={isPending || !ratio.trim()}>保存</Button>
        </form>
      </div>
      <div>
        <div className="mb-1.5 text-sm font-medium text-pink-600">Prompt cache 计量 · 缓存热度 TTL 秒（当前 {cfg?.cacheMeterTtlSecs ?? '—'}）</div>
        <div className="mb-2 text-[11px] leading-snug text-muted-foreground">
          会话首次出现、或距上次请求超此秒数（缓存已凉）→ 本轮判 cold：整段可缓存前缀按 creation（贵桶）重写计费、read=0，如同首轮。TTL 越短越多请求判 cold（creation 多、下游折扣少）；越长越易判 warm（更多 0.1× read 折扣）。默认 300（5min，对齐 Anthropic ephemeral）。
        </div>
        <form onSubmit={num(meterTtl, 1, 86400, (s) => parseInt(s, 10), 'cacheMeterTtlSecs', '缓存热度 TTL 已更新', () => setMeterTtl(''))} className="flex items-center gap-1.5">
          <Input type="number" min={1} max={86400} placeholder="秒" value={meterTtl} onChange={(e) => setMeterTtl(e.target.value)} disabled={isPending} className="h-8 max-w-[160px] text-xs" />
          <Button type="submit" size="sm" variant="outline" className="h-8 text-xs" disabled={isPending || !meterTtl.trim()}>保存</Button>
        </form>
      </div>
    </SettingSection>
  )
}

// 提示词过滤默认值（新建 Key 时继承）
function PromptFilterSection() {
  const { data: cfg, isLoading } = usePromptFilterDefaults()
  const { mutate, isPending } = useSetPromptFilterDefaults()
  const save = (patch: Record<string, unknown>, ok: string) =>
    mutate(patch, {
      onSuccess: () => toast.success(ok),
      onError: (err) => toast.error('保存失败：' + extractErrorMessage(err)),
    })
  const busy = isLoading || isPending

  return (
    <SettingSection
      icon={<Filter className="h-4 w-4 text-amber-500" />}
      title="提示词过滤默认值"
      desc="新建客户端 Key 时继承这些开关。现有 Key 不受影响，每把 Key 仍可在编辑里各自覆盖。"
    >
      <ToggleRow
        label="精简 CC 提示词"
        desc="去除 Claude Code 系统提示中的冗余段落"
        checked={cfg?.simplifyCcPrompt ?? false}
        disabled={busy}
        onChange={(v) => save({ simplifyCcPrompt: v }, '已更新默认值')}
      />
      <ToggleRow
        label="去除边界标记"
        desc="剥离请求里的边界/分隔标记噪声"
        checked={cfg?.stripBoundaryMarkers ?? false}
        disabled={busy}
        onChange={(v) => save({ stripBoundaryMarkers: v }, '已更新默认值')}
      />
      <ToggleRow
        label="去除环境噪声"
        desc="剥离环境信息等与任务无关的上下文"
        checked={cfg?.stripEnvNoise ?? false}
        disabled={busy}
        onChange={(v) => save({ stripEnvNoise: v }, '已更新默认值')}
      />
    </SettingSection>
  )
}

// PLACEHOLDER_SECTIONS2

const COOLDOWN_PRESETS = [60, 300, 900, 1800]

// 账号风控 / 负载均衡
function ThrottleLbSection() {
  const { data: throttle, isLoading: tl } = useAccountThrottleConfig()
  const setThrottle = useSetAccountThrottleConfig()
  const { data: lb, isLoading: ll } = useLoadBalancingMode()
  const setLb = useSetLoadBalancingMode()
  const [cooldown, setCooldown] = useState('')

  const saveThrottle = (patch: Record<string, unknown>, ok: string) =>
    setThrottle.mutate(patch, {
      onSuccess: () => toast.success(ok),
      onError: (err) => toast.error('保存失败：' + extractErrorMessage(err)),
    })

  const submitCooldown = (secs: number) => {
    if (isNaN(secs) || secs < 1 || secs > 86400) {
      toast.error('冷却秒数需在 1..=86400')
      return
    }
    saveThrottle({ cooldownSecs: secs }, '冷却时长已更新')
    setCooldown('')
  }

  const mode = lb?.mode ?? 'priority'

  return (
    <SettingSection
      icon={<ShieldAlert className="h-4 w-4 text-red-500" />}
      title="账号风控 / 负载均衡"
      desc="账号级风控故障转移与冷却时长、上游凭据负载均衡模式。"
    >
      <ToggleRow
        label="风控故障转移"
        desc="账号被风控（429/限流）时自动切换到其他可用账号"
        checked={throttle?.failover ?? false}
        disabled={tl || setThrottle.isPending}
        onChange={(v) => saveThrottle({ failover: v }, v ? '已开启故障转移' : '已关闭故障转移')}
      />

      <div>
        <div className="mb-1.5 text-sm font-medium">风控冷却时长（当前 {throttle ? `${throttle.cooldownSecs}s` : '—'}）</div>
        <div className="mb-2 text-[11px] leading-snug text-muted-foreground">被风控的账号在冷却期内不参与调度。</div>
        <div className="flex flex-wrap items-center gap-1.5">
          {COOLDOWN_PRESETS.map((s) => (
            <Button
              key={s}
              size="sm"
              variant={throttle?.cooldownSecs === s ? 'default' : 'outline'}
              className="h-8 text-xs"
              disabled={setThrottle.isPending}
              onClick={() => submitCooldown(s)}
            >
              {s >= 60 ? `${s / 60} 分` : `${s}s`}
            </Button>
          ))}
          <form
            onSubmit={(e) => {
              e.preventDefault()
              submitCooldown(parseInt(cooldown, 10))
            }}
            className="flex items-center gap-1.5"
          >
            <Input type="number" min={1} max={86400} placeholder="自定义秒" value={cooldown} onChange={(e) => setCooldown(e.target.value)} disabled={setThrottle.isPending} className="h-8 max-w-[120px] text-xs" />
            <Button type="submit" size="sm" variant="outline" className="h-8 text-xs" disabled={setThrottle.isPending || !cooldown.trim()}>保存</Button>
          </form>
        </div>
      </div>

      <div>
        <div className="mb-1.5 text-sm font-medium">负载均衡模式（当前 {mode === 'priority' ? '优先级' : '均衡负载'}）</div>
        <div className="mb-2 text-[11px] leading-snug text-muted-foreground">
          优先级：按优先级顺序用账号，高优先用尽再降级。均衡负载：在可用账号间轮询分摊。
        </div>
        <div className="flex items-center gap-1.5">
          {(['priority', 'balanced'] as const).map((m) => (
            <Button
              key={m}
              size="sm"
              variant={mode === m ? 'default' : 'outline'}
              className="h-8 text-xs"
              disabled={ll || setLb.isPending}
              onClick={() =>
                setLb.mutate(m, {
                  onSuccess: () => toast.success(`已切换到${m === 'priority' ? '优先级模式' : '均衡负载模式'}`),
                  onError: (err) => toast.error('切换失败：' + extractErrorMessage(err)),
                })
              }
            >
              {m === 'priority' ? '优先级' : '均衡负载'}
            </Button>
          ))}
        </div>
      </div>
    </SettingSection>
  )
}

// 日志治理 / 全局代理
function LogProxySection() {
  const { data: log, isLoading: gl } = useLogGovernanceConfig()
  const setLog = useSetLogGovernanceConfig()
  const { data: proxy, isLoading: pl } = useGlobalProxy()
  const setProxy = useSetGlobalProxy()
  const [traceDays, setTraceDays] = useState('')
  const [usageDays, setUsageDays] = useState('')
  const [proxyUrl, setProxyUrl] = useState('')

  const saveLog = (patch: Record<string, unknown>, ok: string) =>
    setLog.mutate(patch, {
      onSuccess: () => toast.success(ok),
      onError: (err) => toast.error('保存失败：' + extractErrorMessage(err)),
    })

  const submitDays = (raw: string, field: string, ok: string, reset: () => void) => (e: React.FormEvent) => {
    e.preventDefault()
    const n = parseInt(raw, 10)
    if (isNaN(n) || n < 0 || n > 3650) {
      toast.error('保留天数需在 0..=3650')
      return
    }
    saveLog({ [field]: n }, ok)
    reset()
  }

  return (
    <SettingSection
      icon={<ScrollText className="h-4 w-4 text-violet-500" />}
      title="日志治理 / 全局代理"
      desc="请求日志（trace）与用量日志保留期、全局出站代理。"
    >
      <ToggleRow
        label={log?.traceEnabled ? '请求日志：已启用' : '请求日志：已关闭'}
        desc="记录每次请求的链路信息，用于排障与统计"
        checked={log?.traceEnabled ?? false}
        disabled={gl || setLog.isPending}
        onChange={(v) => saveLog({ traceEnabled: v }, v ? '已开启请求日志' : '已关闭请求日志')}
      />

      <div>
        <div className="mb-1.5 text-sm font-medium">请求日志保留天数（当前 {log?.traceRetentionDays ?? '—'}）</div>
        <form onSubmit={submitDays(traceDays, 'traceRetentionDays', '请求日志保留期已更新', () => setTraceDays(''))} className="flex items-center gap-1.5">
          <Input type="number" min={0} max={3650} placeholder="天" value={traceDays} onChange={(e) => setTraceDays(e.target.value)} disabled={setLog.isPending} className="h-8 max-w-[160px] text-xs" />
          <Button type="submit" size="sm" variant="outline" className="h-8 text-xs" disabled={setLog.isPending || !traceDays.trim()}>保存</Button>
        </form>
      </div>

      <div>
        <div className="mb-1.5 text-sm font-medium">用量日志保留天数（当前 {log?.usageLogRetentionDays ?? '—'}）</div>
        <form onSubmit={submitDays(usageDays, 'usageLogRetentionDays', '用量日志保留期已更新', () => setUsageDays(''))} className="flex items-center gap-1.5">
          <Input type="number" min={0} max={3650} placeholder="天" value={usageDays} onChange={(e) => setUsageDays(e.target.value)} disabled={setLog.isPending} className="h-8 max-w-[160px] text-xs" />
          <Button type="submit" size="sm" variant="outline" className="h-8 text-xs" disabled={setLog.isPending || !usageDays.trim()}>保存</Button>
        </form>
      </div>

      <div>
        <div className="mb-1.5 text-sm font-medium">
          全局出站代理（当前 {pl ? '…' : proxy?.proxyUrl ? proxy.proxyUrl : '未设置'}）
        </div>
        <div className="mb-2 text-[11px] leading-snug text-muted-foreground">
          所有上游请求经此代理出站（未单独指定代理的凭据）。留空并保存即清除。
        </div>
        <form
          onSubmit={(e) => {
            e.preventDefault()
            const url = proxyUrl.trim()
            setProxy.mutate(
              { proxyUrl: url === '' ? null : url },
              {
                onSuccess: () => {
                  toast.success(url === '' ? '已清除全局代理' : '全局代理已更新')
                  setProxyUrl('')
                },
                onError: (err) => toast.error('保存失败：' + extractErrorMessage(err)),
              },
            )
          }}
          className="flex items-center gap-1.5"
        >
          <Input placeholder="http://host:port（留空清除）" value={proxyUrl} onChange={(e) => setProxyUrl(e.target.value)} disabled={setProxy.isPending} className="h-8 text-xs" />
          <Button type="submit" size="sm" variant="outline" className="h-8 text-xs" disabled={setProxy.isPending}>保存</Button>
        </form>
      </div>
    </SettingSection>
  )
}

export function SettingsPage() {
  return (
    <div>
      <div className="mb-6">
        <h1 className="text-[28px] font-semibold leading-tight tracking-tight">设置</h1>
        <p className="mt-1 text-sm text-muted-foreground">
          全局运行时设置，均即时生效并持久化到 config.json。per-key 设置仍在各自的 Key 编辑里。
        </p>
      </div>
      <div className="grid gap-5 lg:grid-cols-2">
        <CacheQuotaSection />
        <PromptFilterSection />
        <SettingSection
          icon={<Shuffle className="h-4 w-4 text-emerald-500" />}
          title="模型映射（OpenAI 端点）"
          desc="客户端请求的模型名按规则映射到目标 Claude 模型；未命中原样透传。全局生效、即时保存。"
        >
          <ModelMappingPanel />
        </SettingSection>
        <ThrottleLbSection />
        <LogProxySection />
      </div>
    </div>
  )
}

export default SettingsPage
