import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import { getQuotaAlerts, setQuotaAlerts, testQuotaAlerts } from '@/api/credentials'
import { SettingGroup, SettingNumber, SettingRow, SettingSwitch, useFieldSaver } from '@/components/console/setting-row'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Badge } from '@/components/ui/badge'
import { extractErrorMessage } from '@/lib/utils'
import type { QuotaAccountUsage, QuotaAlertSettings } from '@/types/quota-alerts'
import { reportSaveError } from './report-error'

const queryKey = ['quota-alerts']
const statusLabels: Record<QuotaAccountUsage['status'], string> = {
  valid: '有效', missing: '缺少本轮数据', failed: '本轮查询失败', stale: '数据过期', invalid: '额度无效',
}

function formatTime(timestamp: number | null) {
  return timestamp === null ? '尚无记录' : new Date(timestamp * 1000).toLocaleString('zh-CN')
}

export function QuotaAlertsSection() {
  const queryClient = useQueryClient()
  const { data, isLoading, error, refetch } = useQuery({
    queryKey, queryFn: getQuotaAlerts, refetchInterval: 5000,
  })
  const mutation = useMutation({
    mutationFn: (settings: QuotaAlertSettings) => setQuotaAlerts({ settings }),
    onSuccess: (response) => queryClient.setQueryData(queryKey, response),
  })
  const saver = useFieldSaver(mutation.mutate, reportSaveError)
  const [webhook, setWebhook] = useState('')
  const [signingSecret, setSigningSecret] = useState('')
  const [channelPending, setChannelPending] = useState(false)
  const test = useMutation({
    mutationFn: testQuotaAlerts,
    onSuccess: () => {
      toast.info('测试已在后台排队，请查看最近发送结果')
      void queryClient.invalidateQueries({ queryKey })
    },
    onError: reportSaveError,
  })

  if (isLoading) return <p className="text-sm text-muted-foreground">正在读取告警设置…</p>
  if (!data) return (
    <div role="alert" className="space-y-3 text-sm">
      <p>读取额度告警失败：{extractErrorMessage(error)}</p>
      <Button variant="outline" onClick={() => void refetch()}>重试</Button>
    </div>
  )

  const { settings, overview } = data
  const disabled = saver.busy || channelPending
  const anomalies = overview.accounts.filter((account) => account.status !== 'valid' || account.nextResetAt === null)
  const saveChannel = async (clear?: 'webhook' | 'secret') => {
    setChannelPending(true)
    try {
      const response = await setQuotaAlerts({
        settings,
        webhook: clear === 'webhook' ? '' : webhook.trim() || undefined,
        signingSecret: clear === 'secret' ? '' : signingSecret.trim() || undefined,
      })
      queryClient.setQueryData(queryKey, response)
      setWebhook('')
      setSigningSecret('')
      toast.success('渠道已更新，仅在当前进程内生效')
    } catch (saveError) {
      reportSaveError(saveError)
    } finally {
      setChannelPending(false)
    }
  }

  return (
    <div className="space-y-6">
      <div className="rounded-lg border border-border bg-muted/30 p-3 text-xs leading-relaxed text-muted-foreground">
        仅统计启用账号的订阅、有效试用和有效奖励额度，不计超额付费额度。开启 overage 的账号正常参与；达到 100% 不一定无法继续使用。
        阈值人数累计计算，≥90% 包含 ≥100%。消息仅在一轮后台余额刷新结束后判断。
      </div>
      {error && <p role="alert" className="text-sm text-destructive">状态刷新失败，以下为上次结果：{extractErrorMessage(error)}</p>}
      {data.error && <p role="alert" className="text-sm text-destructive">{data.error}</p>}
      {!data.webhookConfigured && (settings.account.enabled || settings.pool.enabled) && <p role="alert" className="text-sm text-amber-600">告警模式已开启，但当前进程未配置 Webhook，暂时无法投递。请注入渠道环境变量或重新输入临时渠道。</p>}
      {!data.notifierEnabled && <p role="status" className="text-sm text-amber-600">当前实例关闭了通知发送（KIRO_RS_QUOTA_ALERT_NOTIFIER=false）。</p>}
      {data.refreshIntervalSecs === 0 && <p role="status" className="text-sm text-amber-600">后台余额刷新已关闭，自动告警不会执行。请调整 balanceRefreshIntervalSecs 后重启。</p>}

      <SettingGroup title="飞书自定义机器人" description="Webhook 和可选签名密钥均为只写字段，接口不返回明文。">
        <div className="space-y-3 py-3">
          <div className="flex flex-wrap gap-2">
            <Badge variant="outline">Webhook：{data.webhookConfigured ? '已配置 · 已隐藏' : '未配置'}</Badge>
            <Badge variant="outline">签名密钥：{data.signingSecretConfigured ? '已配置 · 已隐藏' : '未配置'}</Badge>
          </div>
          <p className="text-xs leading-relaxed text-muted-foreground">
            长期配置请通过部署环境注入 KIRO_RS_FEISHU_WEBHOOK 和 KIRO_RS_FEISHU_SECRET。
            下方输入仅保存在服务端内存，重启后恢复环境变量值，不写入配置、数据库或浏览器存储。留空表示不修改。
            远程管理请使用 HTTPS。机器人若启用了关键词校验，请允许「Kiro」。
          </p>
          <form className="space-y-3" onSubmit={(event) => { event.preventDefault(); void saveChannel() }}>
            <div className="space-y-1">
              <label htmlFor="quota-webhook" className="text-sm">Webhook 地址</label>
              <Input id="quota-webhook" type="password" autoComplete="new-password" value={webhook}
                placeholder="输入新的飞书 / Lark 官方机器人 Webhook" disabled={disabled}
                onChange={(event) => setWebhook(event.target.value)} />
            </div>
            <div className="space-y-1">
              <label htmlFor="quota-signing-secret" className="text-sm">签名密钥（可选）</label>
              <Input id="quota-signing-secret" type="password" autoComplete="new-password" value={signingSecret}
                placeholder="留空保留现有密钥" disabled={disabled}
                onChange={(event) => setSigningSecret(event.target.value)} />
            </div>
            <div className="flex flex-wrap gap-2">
              <Button type="submit" size="sm" disabled={disabled || (!webhook.trim() && !signingSecret.trim())}>更新内存渠道</Button>
              <Button type="button" size="sm" variant="outline" disabled={disabled || !data.signingSecretConfigured}
                onClick={() => void saveChannel('secret')}>清除签名密钥</Button>
              <Button type="button" size="sm" variant="outline"
                disabled={disabled || !data.webhookConfigured || settings.account.enabled || settings.pool.enabled}
                onClick={() => void saveChannel('webhook')}>清除 Webhook</Button>
              <Button type="button" size="sm" variant="outline"
                disabled={disabled || test.isPending || data.sending || !data.webhookConfigured || !data.notifierEnabled}
                onClick={() => test.mutate()}>发送测试通知</Button>
            </div>
          </form>
        </div>
      </SettingGroup>

      {(['account', 'pool'] as const).map((mode) => (
        <SettingGroup key={mode} title={mode === 'account' ? '单账号告警' : '账号池整体告警'}
          description={mode === 'account' ? '每账号、额度周期、阈值持久化去重；一次跨越多档只发送最高档。周期未知时暂停该账号告警。' : '按汇总已用 / 汇总额度计算，不平均百分比；数据不完整时暂停，回落后允许再次跨越触发。'}>
          <SettingSwitch label={mode === 'account' ? '启用单账号告警' : '启用整体告警'}
            checked={settings[mode].enabled} disabled={disabled} pending={saver.isSaving(mode)} saved={saver.isSaved(mode)}
            onChange={(enabled) => saver.save(mode, { ...settings, [mode]: { ...settings[mode], enabled } })} />
          <SettingRow label="使用率阈值" hint="1–100 的递增整数，用逗号分隔，最多 10 档。"
            pending={saver.isSaving(`${mode}-thresholds`)} saved={saver.isSaved(`${mode}-thresholds`)}>
            <ThresholdInput key={`${mode}-${settings[mode].thresholds.join(',')}`} values={settings[mode].thresholds}
              label={mode === 'account' ? '单账号使用率阈值' : '账号池使用率阈值'} disabled={disabled}
              onCommit={(thresholds) => saver.save(`${mode}-thresholds`, { ...settings, [mode]: { ...settings[mode], thresholds } })} />
          </SettingRow>
          {mode === 'pool' && <SettingNumber label="整体告警冷却" hint="冷却内的新跨越延迟至后续完整刷新轮次发送；持续高位不重复发送。"
            value={settings.poolCooldownSecs} min={60} max={604800} unit="秒" disabled={disabled}
            pending={saver.isSaving('cooldown')} saved={saver.isSaved('cooldown')}
            onCommit={(poolCooldownSecs) => saver.save('cooldown', { ...settings, poolCooldownSecs })} />}
        </SettingGroup>
      ))}

      <SettingGroup title="数据新鲜度">
        <SettingNumber label="数据有效期" hint={`后台每轮完成后等待 ${data.refreshIntervalSecs} 秒。账号多时需覆盖整轮查询耗时，过期数据不会作为低用量参与统计。`}
          value={settings.maxDataAgeSecs} min={60} max={604800} unit="秒" disabled={disabled}
          pending={saver.isSaving('freshness')} saved={saver.isSaved('freshness')}
          onCommit={(maxDataAgeSecs) => saver.save('freshness', { ...settings, maxDataAgeSecs })} />
      </SettingGroup>

      <SettingGroup title="账号池概况" description={`最近完整轮次：${formatTime(overview.completedAt)}（浏览器本地时间）`}>
        <div className="space-y-3 py-3 text-sm">
          <p>启用账号：{overview.enabledAccounts} · 有效数据：{overview.validAccounts} · Overage 已开启：{overview.overageEnabled} · 状态未知：{overview.overageUnknown}</p>
          <p className="font-medium">整体使用率：{overview.percentage === null ? '不可用，暂停整体判断' : `${overview.percentage.toFixed(2)}%`}</p>
          <p className="text-muted-foreground">{overview.complete ? '汇总额度' : '仅有效数据小计（非完整池统计）'}：{overview.used.toFixed(2)} / {overview.limit.toFixed(2)} credits</p>
          <div className="flex flex-wrap gap-2">
            {Object.entries(overview.thresholdCounts).map(([threshold, count]) => <Badge variant="outline" key={threshold}>≥{threshold}%：{count} 个</Badge>)}
          </div>
          <p className="text-xs text-muted-foreground">缺失 {overview.missingAccounts} · 查询失败 {overview.failedAccounts} · 过期 {overview.staleAccounts} · 额度无效 {overview.invalidAccounts} · 周期未知 {overview.unknownCycleAccounts}</p>
          {anomalies.length > 0 && <details>
            <summary className="cursor-pointer text-amber-600">查看异常或周期未知账号（{anomalies.length}）</summary>
            <ul className="mt-2 max-h-60 space-y-1 overflow-auto text-xs">
              {anomalies.map((account) => <li key={account.id}>
                #{account.id} {account.name} — {account.status === 'valid' ? '额度周期未知' : statusLabels[account.status]} · 查询时间：{formatTime(account.queriedAt)}
              </li>)}
            </ul>
          </details>}
        </div>
      </SettingGroup>

      <SettingGroup title="最近发送结果">
        <div className="space-y-2 py-3 text-sm" aria-live="polite">
          {data.sending && <p>正在后台发送…</p>}
          {data.lastDelivery ? <>
            <p>{data.lastDelivery.success ? '成功' : '失败'} · {data.lastDelivery.target} · {formatTime(data.lastDelivery.at)}</p>
            <p className={data.lastDelivery.success ? 'text-muted-foreground' : 'text-destructive'}>{data.lastDelivery.message}</p>
          </> : <p className="text-muted-foreground">尚无发送记录</p>}
          <p className="text-xs text-muted-foreground">每次请求超时 10 秒，最多尝试 3 次。超时重试可能重复投递；测试通知不会消耗告警档位。多副本请仅指定一个通知实例。</p>
        </div>
      </SettingGroup>
    </div>
  )
}

function ThresholdInput({ values, label, disabled, onCommit }: {
  values: number[], label: string, disabled: boolean, onCommit: (values: number[]) => void,
}) {
  const [draft, setDraft] = useState(values.join(', '))
  const commit = () => {
    const parts = draft.split(/[,，]/).map((part) => part.trim())
    const next = parts.map(Number)
    if (parts.some((part) => !/^\d+$/.test(part)) || next.length > 10
      || next.some((value, index) => value < 1 || value > 100 || (index > 0 && value <= next[index - 1]))) {
      toast.error('阈值须为 1–100 的递增整数，最多 10 档，不能重复')
      setDraft(values.join(', '))
      return
    }
    if (next.join(',') !== values.join(',')) onCommit(next)
  }
  return <Input aria-label={label} value={draft} disabled={disabled} className="h-8 w-full sm:w-56"
    onChange={(event) => setDraft(event.target.value)} onBlur={commit}
    onKeyDown={(event) => { if (event.key === 'Enter') event.currentTarget.blur() }} />
}
