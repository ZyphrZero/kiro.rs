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

function SelfHealGroup() {
  const { data, isLoading } = useSelfHealConfig()
  const { mutate } = useSetSelfHealConfig()
  const saver = useFieldSaver(mutate, reportSaveError)
  const enabled = data?.enabled ?? true

  return (
    <SettingGroup
      title="凭据自愈"
      description="请求池全灭时自动把禁用的凭据放回来重试"
    >
      <SettingSwitch
        label="启用自愈"
        hint="当前作用域内已无可用凭据时，按作用域批量恢复被禁用的凭据"
        checked={enabled}
        onChange={(next) => saver.save('enabled', { enabled: next })}
        pending={saver.isSaving('enabled')}
        saved={saver.isSaved('enabled')}
        disabled={isLoading}
      />
      <SettingSwitch
        label="403 封禁识别"
        hint="命中封禁文案的 403 直接禁用且不参与自愈，避免为已封账号反复重试"
        checked={data?.suspendedDetectionEnabled ?? true}
        onChange={(next) =>
          saver.save('suspended', { suspendedDetectionEnabled: next })
        }
        pending={saver.isSaving('suspended')}
        saved={saver.isSaved('suspended')}
        disabled={isLoading}
      />
      <SettingNumber
        label="自愈冷却间隔"
        hint="两次自愈之间的最小间隔。设 0 表示不冷却 —— 上游持续 403 时这是唯一能打断死循环的刹车，不建议设 0"
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
