import { useState } from 'react'
import {
  Plus,
  Trash2,
  Lock,
  Check,
  Loader2,
} from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Input } from '@/components/ui/input'
import { Button } from '@/components/ui/button'
import {
  useAccountThrottleConfig,
  useSetAccountThrottleConfig,
  useModelApiRetryConfig,
  useSetModelApiRetryConfig,
  useAccountRpmLimitConfig,
  useSetAccountRpmLimitConfig,
  useQuotaResetRecoveryConfig,
  useSetQuotaResetRecoveryConfig,
  useLoadBalancingMode,
  useSetLoadBalancingMode,
  useApiDomain,
  useSetApiDomain,
  useSelfHealConfig,
  useSetSelfHealConfig,
} from '@/hooks/use-credentials'
import {
  SettingGroup,
  SettingNumber,
  SettingReadout,
  SettingSegments,
  SettingSwitch,
  useFieldSaver,
} from '@/components/console/setting-row'
import { reportSaveError } from '@/components/settings/report-error'

const SECS_PER_MIN = 60
// 与后端 SetAccountRpmLimitConfigRequest 的校验区间保持一致
const MIN_RPM_LIMIT = 1
const MAX_RPM_LIMIT = 100000

/**
 * 调度分区：凭据怎么选、失败怎么转、禁用怎么恢复。
 *
 * 三组配置放在一屏里是有意的 —— 它们互相牵制，分开看容易配出自相矛盾的组合。
 * 最典型的是「自愈开着 + 冷却间隔为 0」：403 持续时会陷入 全禁 → 自愈 → 403 → 再禁
 * 的死循环（0.7.4 修的就是这个）。摆在一起，间隔和上限这两个刹车就跟自愈开关同时在视野里。
 */
export function DispatchSection() {
  return (
    <div className="space-y-6">
      <LoadBalancingGroup />
      <ApiDomainGroup />
      <RateLimitHandlingGroup />
      <RpmLimitGroup />
      <QuotaResetRecoveryGroup />
      <div id="dispatch-ban-detect">
        <AccountBanGroup />
      </div>
      <SelfHealGroup />
    </div>
  )
}

function LoadBalancingGroup() {
  const { data, isLoading } = useLoadBalancingMode()
  const { mutate } = useSetLoadBalancingMode()
  const saver = useFieldSaver(mutate, reportSaveError)

  return (
    <SettingGroup title="凭据选择">
      <SettingSegments
        label="负载均衡模式"
        hint={
          data?.mode === 'balanced'
            ? '按用量动态挑选凭据，把请求摊平到整个池子'
            : '按优先级数字从小到大用：先用完 0 号，再换 1 号'
        }
        value={data?.mode ?? 'priority'}
        options={[
          { value: 'priority', label: '按优先级', hint: '小数字先用，顺序耗尽' },
          { value: 'balanced', label: '均衡负载', hint: '按用量动态摊平' },
        ]}
        onChange={(next) => saver.save('mode', next)}
        pending={saver.isSaving('mode')}
        saved={saver.isSaved('mode')}
        disabled={isLoading}
      />
    </SettingGroup>
  )
}

function ApiDomainGroup() {
  const { data, isLoading } = useApiDomain()
  const { mutate } = useSetApiDomain()
  const saver = useFieldSaver(mutate, reportSaveError)

  return (
    <SettingGroup
      title="上游域名"
      description="请求发往哪个 Kiro 服务器；立即生效并写入 config.json，无需重启"
    >
      <SettingSegments
        label="API 域名"
        hint={
          data?.domain === 'kiro'
            ? '发往 runtime.{区域}.kiro.dev（新版 Kiro IDE / CLI 用的域名）'
            : '发往 q.{区域}.amazonaws.com（kiro-rs 原默认）'
        }
        value={data?.domain ?? 'amazonaws'}
        options={[
          { value: 'amazonaws', label: '老域名', hint: 'q.{区域}.amazonaws.com' },
          { value: 'kiro', label: '新域名', hint: 'runtime.{区域}.kiro.dev' },
        ]}
        onChange={(next) => saver.save('domain', next)}
        pending={saver.isSaving('domain')}
        saved={saver.isSaved('domain')}
        disabled={isLoading}
      />
    </SettingGroup>
  )
}

function RateLimitHandlingGroup() {
  const { data: throttle, isLoading: throttleLoading } = useAccountThrottleConfig()
  const { mutate: setThrottle } = useSetAccountThrottleConfig()
  const throttleSaver = useFieldSaver(setThrottle, reportSaveError)
  const { data: retry, isLoading: retryLoading } = useModelApiRetryConfig()
  const { mutate: setRetry } = useSetModelApiRetryConfig()
  const retrySaver = useFieldSaver(setRetry, reportSaveError)
  const failover = throttle?.failover ?? true
  const cooldownSecs = throttle?.cooldownSecs ?? 30 * SECS_PER_MIN
  const retryEnabled = retry?.enabled ?? true

  return (
    <SettingGroup
      title="上游 429 处理"
      description="账号级限流按需切换凭据；容量类限流可由中转退避重试"
    >
      <SettingSwitch
        label="故障转移"
        hint={
          failover
            ? '账号触发请求或 credits 限流时，冷却当前凭据并换号重试'
            : '账号级 429 直接返回客户端，不切换凭据'
        }
        checked={failover}
        onChange={(next) => throttleSaver.save('failover', { failover: next })}
        pending={throttleSaver.isSaving('failover')}
        saved={throttleSaver.isSaved('failover')}
        disabled={throttleLoading}
      />
      <SettingNumber
        label="账号冷却时长"
        hint="被账号级限流的凭据要静默多久才重新参与调度"
        value={cooldownSecs}
        toDisplay={(secs) => Math.round(secs / SECS_PER_MIN)}
        fromDisplay={(min) => min * SECS_PER_MIN}
        onCommit={(secs) =>
          throttleSaver.save('cooldown', { cooldownSecs: secs })
        }
        min={1}
        max={1440}
        unit="分钟"
        presets={[5, 15, 30, 60]}
        pending={throttleSaver.isSaving('cooldown')}
        saved={throttleSaver.isSaved('cooldown')}
        disabled={throttleLoading || !failover}
      />
      <SettingSwitch
        label="普通 429 自动重试"
        hint={
          retryEnabled
            ? 'capacity / high traffic 等无需换号的 429 由中转退避重试'
            : '无需换号的 429 首次失败就返回客户端，由客户端决定是否重试'
        }
        checked={retryEnabled}
        onChange={(next) => retrySaver.save('enabled', { enabled: next })}
        pending={retrySaver.isSaving('enabled')}
        saved={retrySaver.isSaved('enabled')}
        disabled={retryLoading}
      />
    </SettingGroup>
  )
}

/**
 * 单账号 RPM 主动限流。
 *
 * 紧跟「上游 429 处理」是有意的：两者都是账号级限速，区别只在谁先动手 ——
 * 故障转移是收到上游 429 后的补救，这里是本地提前限制请求频率。
 */
function RpmLimitGroup() {
  const { data, isLoading } = useAccountRpmLimitConfig()
  const { mutate } = useSetAccountRpmLimitConfig()
  const saver = useFieldSaver(mutate, reportSaveError)
  const enabled = data?.enabled ?? false
  const limit = data?.limit ?? 60

  return (
    <SettingGroup
      title="单账号限流"
      description="主动掐住单个账号的每分钟请求数，别等上游风控才反应"
    >
      <SettingSwitch
        label="启用 RPM 限流"
        hint={
          enabled
            ? '每个凭据独立计 60 秒滑动窗口，超限的临时跳过并切到下一个可用凭据'
            : '关闭时不计数、不影响调度'
        }
        checked={enabled}
        onChange={(next) => saver.save('enabled', { enabled: next })}
        pending={saver.isSaving('enabled')}
        saved={saver.isSaved('enabled')}
        disabled={isLoading}
      />
      <SettingNumber
        label="每分钟上限"
        hint="单个凭据 60 秒内最多放行多少请求。所有凭据都超限时请求返回 429"
        value={limit}
        onCommit={(n) => saver.save('limit', { limit: n })}
        min={MIN_RPM_LIMIT}
        max={MAX_RPM_LIMIT}
        unit="次/分钟"
        presets={[10, 30, 60, 120, 300]}
        pending={saver.isSaving('limit')}
        saved={saver.isSaved('limit')}
        disabled={isLoading || !enabled}
      />
    </SettingGroup>
  )
}

function QuotaResetRecoveryGroup() {
  const { data, isLoading } = useQuotaResetRecoveryConfig()
  const { mutate } = useSetQuotaResetRecoveryConfig()
  const saver = useFieldSaver(mutate, reportSaveError)
  const enabled = data?.enabled ?? false

  return (
    <SettingGroup
      title="月度额度恢复"
      description="因额度耗尽被禁用的凭据，是否在下个账期自动恢复调度"
    >
      <SettingSwitch
        label="自动恢复额度耗尽凭据"
        hint={
          enabled
            ? '到凭据额度重置时间后复查余额，确认恢复额度后重新加入调度'
            : '保持禁用，需在凭据管理中手动启用'
        }
        checked={enabled}
        onChange={(next) => saver.save('enabled', { enabled: next })}
        pending={saver.isSaving('enabled')}
        saved={saver.isSaved('enabled')}
        disabled={isLoading}
      />
    </SettingGroup>
  )
}

function AccountBanGroup() {
  const { data, isLoading } = useSelfHealConfig()
  const { mutate } = useSetSelfHealConfig()
  const saver = useFieldSaver(mutate, reportSaveError)
  const enabled = data?.suspendedDetectionEnabled ?? true
  const keywords = data?.suspendedBanKeywords ?? []

  return (
    <SettingGroup
      title="账号封禁治理"
      description="识别上游封号响应（如 TEMPORARILY_SUSPENDED、locked 等）并永久熔断，绝不参与自愈重试"
    >
      <SettingSwitch
        label="启用封号识别"
        hint="命中封号响应（不限 403/429）立即标记为「账号封禁」并禁用，且永久排除在凭据自愈之外"
        checked={enabled}
        onChange={(next) =>
          saver.save('suspended', { suspendedDetectionEnabled: next })
        }
        pending={saver.isSaving('suspended')}
        saved={saver.isSaved('suspended')}
        disabled={isLoading}
      />
      <BanRulesTable
        keywords={keywords}
        onCommit={(newKeywords) =>
          saver.save('keywords', { suspendedBanKeywords: newKeywords })
        }
        pending={saver.isSaving('keywords')}
        saved={saver.isSaved('keywords')}
        disabled={isLoading || !enabled}
      />
    </SettingGroup>
  )
}

function SelfHealGroup() {
  const { data, isLoading } = useSelfHealConfig()
  const { mutate } = useSetSelfHealConfig()
  const saver = useFieldSaver(mutate, reportSaveError)
  const enabled = data?.enabled ?? true

  return (
    <SettingGroup
      title="凭据自愈恢复"
      description="请求池全灭时自动批量恢复因临时失败被禁用的凭据（已封禁账号将被永久隔离，不参与自愈）"
    >
      <SettingSwitch
        label="启用自愈"
        hint="当前作用域内已无可用凭据时，按作用域批量恢复因失败过多被禁用的凭据"
        checked={enabled}
        onChange={(next) => saver.save('enabled', { enabled: next })}
        pending={saver.isSaving('enabled')}
        saved={saver.isSaved('enabled')}
        disabled={isLoading}
      />
      <SettingNumber
        label="自愈冷却间隔"
        hint="两次自愈之间的最小间隔。设 0 表示不冷却 —— 上游持续故障时这是唯一能打断死循环的刹车，不建议设 0"
        value={data?.minIntervalSecs ?? 0}
        toDisplay={(secs) => Math.round(secs / SECS_PER_MIN)}
        fromDisplay={(min) => min * SECS_PER_MIN}
        onCommit={(secs) => saver.save('interval', { minIntervalSecs: secs })}
        min={0}
        max={1440}
        unit="分钟"
        presets={[0, 1, 5, 15]}
        pending={saver.isSaving('interval')}
        saved={saver.isSaved('interval')}
        disabled={isLoading || !enabled}
      />
      <SettingNumber
        label="连续自愈上限"
        hint="连续自愈达到该轮数且期间无任何成功请求则停止自愈。0 = 不限"
        value={data?.maxConsecutiveRounds ?? 5}
        onCommit={(n) => saver.save('rounds', { maxConsecutiveRounds: n })}
        min={0}
        max={1000}
        unit="轮"
        pending={saver.isSaving('rounds')}
        saved={saver.isSaved('rounds')}
        disabled={isLoading || !enabled}
      />
      <SettingReadout
        label="运行状态"
        hint="当前连续自愈轮数 / 累计恢复凭据次数"
      >
        连续 {data?.consecutiveRounds ?? 0} 轮 · 累计恢复 {data?.totalCount ?? 0} 次
      </SettingReadout>
    </SettingGroup>
  )
}

function BanRulesTable({
  keywords,
  onCommit,
  pending,
  saved,
  disabled,
}: {
  keywords: string[]
  onCommit: (keywords: string[]) => void
  pending?: boolean
  saved?: boolean
  disabled?: boolean
}) {
  const [draft, setDraft] = useState('')

  const handleAdd = () => {
    const trimmed = draft.trim()
    if (!trimmed) return
    if (keywords.some((k) => k.toLowerCase() === trimmed.toLowerCase())) {
      setDraft('')
      return
    }
    onCommit([...keywords, trimmed])
    setDraft('')
  }

  const handleRemove = (index: number) => {
    onCommit(keywords.filter((_, i) => i !== index))
  }

  return (
    <div className="py-3.5 space-y-3">
      {/* 头部：说明与添加操作栏 */}
      <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-2.5">
        <div>
          <div className="flex items-center gap-2">
            <span className="text-[13.5px] font-medium text-foreground">封号特征识别规则表</span>
            {pending && <Loader2 className="h-3.5 w-3.5 animate-spin text-muted-foreground" />}
            {!pending && saved && <Check className="h-3.5 w-3.5 text-emerald-500" />}
            <Badge variant="outline" className="text-[10px] px-1.5 py-0 h-4 text-muted-foreground font-mono">
              共 {2 + keywords.length} 条规则
            </Badge>
          </div>
          <p className="mt-0.5 text-xs text-muted-foreground leading-relaxed">
            无论正常调度还是探测重试，响应体命中表中任一特征将立即熔断并标记为「账号封禁」，永久排除在自动自愈之外。
          </p>
        </div>

        {/* 添加自定义关键词 */}
        <div className="flex items-center gap-1.5 shrink-0">
          <Input
            value={draft}
            disabled={disabled || pending}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') {
                e.preventDefault()
                handleAdd()
              }
            }}
            placeholder="输入自定义封禁关键词..."
            className="h-8 w-48 sm:w-56 text-xs bg-background"
          />
          <Button
            type="button"
            size="sm"
            className="h-8 px-3 text-xs gap-1 shrink-0 cursor-pointer"
            disabled={disabled || pending || !draft.trim()}
            onClick={handleAdd}
          >
            <Plus className="h-3.5 w-3.5" />
            添加关键词
          </Button>
        </div>
      </div>

      {/* 规则表格 */}
      <div className="rounded-lg border border-border/70 overflow-hidden bg-card/60 shadow-2xs">
        <div className="overflow-x-auto">
          <table className="w-full text-left text-xs border-collapse">
            <thead>
              <tr className="border-b border-border/60 bg-muted/40 text-muted-foreground text-[11px]">
                <th className="py-2.5 px-3.5 font-medium w-24">规则来源</th>
                <th className="py-2.5 px-3.5 font-medium">匹配特征 / 关键词</th>
                <th className="py-2.5 px-3.5 font-medium w-48">匹配模式</th>
                <th className="py-2.5 px-3.5 font-medium w-36">处置策略</th>
                <th className="py-2.5 px-3.5 font-medium text-right w-20">操作</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-border/40 text-xs">
              {/* 内置规则 1 */}
              <tr className="hover:bg-muted/20 transition-colors">
                <td className="py-2.5 px-3.5">
                  <Badge variant="outline" className="border-rose-500/30 text-rose-600 dark:text-rose-400 bg-rose-500/10 text-[10.5px] px-1.5 py-0 font-medium">
                    内置规则
                  </Badge>
                </td>
                <td className="py-2.5 px-3.5">
                  <code className="font-mono text-[11px] font-semibold text-rose-600 dark:text-rose-400 bg-rose-500/5 border border-rose-500/20 px-1.5 py-0.5 rounded">
                    reason: "TEMPORARILY_SUSPENDED"
                  </code>
                </td>
                <td className="py-2.5 px-3.5 text-muted-foreground text-[11.5px]">
                  JSON 顶层或嵌套结构化字段精确比对
                </td>
                <td className="py-2.5 px-3.5 text-muted-foreground text-[11.5px]">
                  标记封禁 · 排除自愈
                </td>
                <td className="py-2.5 px-3.5 text-right">
                  <span className="inline-flex items-center gap-0.5 text-[10.5px] text-muted-foreground/70" title="内核原生只读规则">
                    <Lock className="h-3 w-3" />
                    系统锁定
                  </span>
                </td>
              </tr>

              {/* 内置规则 2 */}
              <tr className="hover:bg-muted/20 transition-colors">
                <td className="py-2.5 px-3.5">
                  <Badge variant="outline" className="border-rose-500/30 text-rose-600 dark:text-rose-400 bg-rose-500/10 text-[10.5px] px-1.5 py-0 font-medium">
                    内置规则
                  </Badge>
                </td>
                <td className="py-2.5 px-3.5">
                  <div className="flex items-center gap-1 font-mono text-[11px] flex-wrap">
                    <code className="bg-muted px-1.5 py-0.5 rounded border border-border/60 text-foreground">
                      "suspended"
                    </code>
                    <span className="text-muted-foreground font-bold text-[10px]">+</span>
                    <code className="bg-muted px-1.5 py-0.5 rounded border border-border/60 text-foreground">
                      "locked your account" / "locked it"
                    </code>
                  </div>
                </td>
                <td className="py-2.5 px-3.5 text-muted-foreground text-[11.5px]">
                  响应体双短语交叉组合包含
                </td>
                <td className="py-2.5 px-3.5 text-muted-foreground text-[11.5px]">
                  标记封禁 · 排除自愈
                </td>
                <td className="py-2.5 px-3.5 text-right">
                  <span className="inline-flex items-center gap-0.5 text-[10.5px] text-muted-foreground/70" title="内核原生只读规则">
                    <Lock className="h-3 w-3" />
                    系统锁定
                  </span>
                </td>
              </tr>

              {/* 自定义关键词行 */}
              {keywords.map((kw, idx) => (
                <tr key={idx} className="hover:bg-muted/20 transition-colors">
                  <td className="py-2.5 px-3.5">
                    <Badge variant="secondary" className="text-[10.5px] px-1.5 py-0 font-normal">
                      自定义
                    </Badge>
                  </td>
                  <td className="py-2.5 px-3.5">
                    <code className="font-mono text-[11.5px] font-medium text-foreground bg-muted/70 px-1.5 py-0.5 rounded border border-border/60">
                      {kw}
                    </code>
                  </td>
                  <td className="py-2.5 px-3.5 text-muted-foreground text-[11.5px]">
                    响应体子串匹配 (忽略大小写)
                  </td>
                  <td className="py-2.5 px-3.5 text-muted-foreground text-[11.5px]">
                    标记封禁 · 排除自愈
                  </td>
                  <td className="py-2.5 px-3.5 text-right">
                    <Button
                      type="button"
                      variant="ghost"
                      size="icon"
                      className="h-6 w-6 text-muted-foreground hover:text-destructive hover:bg-destructive/10 cursor-pointer"
                      onClick={() => handleRemove(idx)}
                      disabled={disabled || pending}
                      title="删除该关键词"
                    >
                      <Trash2 className="h-3.5 w-3.5" />
                    </Button>
                  </td>
                </tr>
              ))}

              {keywords.length === 0 && (
                <tr>
                  <td colSpan={5} className="py-4 px-3.5 text-center text-muted-foreground text-xs bg-muted/5">
                    暂未配置自定义关键词。若上游出现特定封号错误文案，可在上方输入框添加关键词进行补充拦截。
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  )
}
