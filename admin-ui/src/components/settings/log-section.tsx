import {
  useLogGovernanceConfig,
  useSetLogGovernanceConfig,
} from '@/hooks/use-credentials'
import {
  SettingGroup,
  SettingNumber,
  SettingSwitch,
  useFieldSaver,
} from '@/components/console/setting-row'
import { reportSaveError } from '@/components/settings/report-error'

/**
 * 日志分区：链路追踪开关与两类日志的保留期。
 */
export function LogSection() {
  const { data, isLoading } = useLogGovernanceConfig()
  const { mutate } = useSetLogGovernanceConfig()
  const saver = useFieldSaver(mutate, reportSaveError)
  const enabled = data?.traceEnabled ?? true

  return (
    <div className="space-y-6">
      <SettingGroup title="链路追踪">
        <SettingSwitch
          label="记录请求链路"
          hint={
            enabled
              ? '每次请求的完整重试链路写入 traces.db，请求日志页据此展示故障转移过程'
              : '不再写入新链路；已有记录仍可查询'
          }
          checked={enabled}
          onChange={(next) => saver.save('traceEnabled', { traceEnabled: next })}
          pending={saver.isSaving('traceEnabled')}
          saved={saver.isSaved('traceEnabled')}
          disabled={isLoading}
        />
        <SettingNumber
          label="链路保留期"
          hint="超过天数的 trace 由后台任务定期清理"
          value={data?.traceRetentionDays ?? 7}
          onCommit={(n) => saver.save('traceDays', { traceRetentionDays: n })}
          min={1}
          max={365}
          unit="天"
          presets={[3, 7, 30]}
          pending={saver.isSaving('traceDays')}
          saved={saver.isSaved('traceDays')}
          disabled={isLoading}
        />
      </SettingGroup>

      <SettingGroup
        title="用量日志"
        description="按凭据 / 模型聚合的统计数据，概览页的图表来源"
      >
        <SettingNumber
          label="用量日志保留期"
          hint="影响概览页能回看多久的趋势"
          value={data?.usageLogRetentionDays ?? 30}
          onCommit={(n) => saver.save('usageDays', { usageLogRetentionDays: n })}
          min={1}
          max={365}
          unit="天"
          presets={[7, 30, 90]}
          pending={saver.isSaving('usageDays')}
          saved={saver.isSaved('usageDays')}
          disabled={isLoading}
        />
      </SettingGroup>
    </div>
  )
}
