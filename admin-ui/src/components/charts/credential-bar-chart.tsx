import { memo, useMemo } from 'react'
import { BarChart, Bar, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer, Legend } from 'recharts'
import type { CredentialDistribution } from '@/types/api'
import { tooltipContentStyle, tooltipCursorStyle, tooltipItemStyle, tooltipLabelStyle } from './tooltip-style'
import { formatNumber } from '@/lib/utils'

interface Props {
  data: CredentialDistribution[]
}

interface ChartDatum {
  calls: number
  errors: number
  fullLabel: string
  inputTokens: number
  label: string
  outputTokens: number
}

function CredentialBarChartImpl({ data }: Props) {
  const formatted = useMemo(() => buildChartData(data), [data])

  if (data.length === 0) {
    return <EmptyCredentialChart />
  }

  return <CredentialChartContent data={formatted} />
}

function buildChartData(data: CredentialDistribution[]): ChartDatum[] {
  return data.slice(0, 12).map((d) => {
    const fullLabel = d.email ?? `#${d.credentialId}`
    return {
      calls: d.calls,
      errors: d.errors,
      fullLabel,
      inputTokens: d.inputTokens,
      label: d.email ? truncateEmail(d.email) : fullLabel,
      outputTokens: d.outputTokens,
    }
  })
}

function EmptyCredentialChart() {
  return (
    <div className="flex h-[180px] items-center justify-center text-sm text-muted-foreground sm:h-[260px]">
      暂无数据
    </div>
  )
}

function CredentialChartContent({ data }: { data: ChartDatum[] }) {
  return (
    <div className="h-[280px] sm:h-[340px]">
      <ResponsiveContainer width="100%" height="100%">
        <BarChart data={data} margin={{ top: 8, right: 8, left: -10, bottom: 52 }}>
          {credentialChartAxes()}
          {credentialChartTooltip()}
          <Legend verticalAlign="top" align="right" height={28} wrapperStyle={{ fontSize: 12 }} />
          {credentialChartBars()}
        </BarChart>
      </ResponsiveContainer>
    </div>
  )
}

function credentialChartAxes() {
  return [
    <CartesianGrid key="grid" strokeDasharray="3 3" className="stroke-border/50" />,
    <XAxis
      key="x"
      dataKey="label"
      tick={{ fontSize: 10 }}
      angle={-30}
      textAnchor="end"
      interval={0}
      height={64}
    />,
    <YAxis key="y" tick={{ fontSize: 11 }} tickFormatter={(v: number) => formatNumber(v)} width={42} />,
  ]
}

function credentialChartTooltip() {
  return (
    <Tooltip
      contentStyle={tooltipContentStyle}
      labelStyle={tooltipLabelStyle}
      itemStyle={tooltipItemStyle}
      cursor={tooltipCursorStyle}
      formatter={(value: number) => formatNumber(value)}
      labelFormatter={formatTooltipLabel}
    />
  )
}

function formatTooltipLabel(label: string, payload?: ReadonlyArray<{ payload?: ChartDatum }>) {
  return payload?.[0]?.payload?.fullLabel ?? label
}

function credentialChartBars() {
  return [
    <Bar key="input" dataKey="inputTokens" name="输入" stackId="a" fill="#3b82f6" isAnimationActive={false} />,
    <Bar key="output" dataKey="outputTokens" name="输出" stackId="a" fill="#10b981" isAnimationActive={false} />,
  ]
}

export const CredentialBarChart = memo(CredentialBarChartImpl)

/** 仅用于 X 轴展示：保留 @ 后域名前 1-2 段，整体最长 22 字符 */
function truncateEmail(email: string): string {
  if (email.length <= 22) return email
  const at = email.indexOf('@')
  if (at < 0) return email.slice(0, 20) + '…'
  const name = email.slice(0, at)
  const domain = email.slice(at + 1)
  const shortName = name.length > 12 ? name.slice(0, 11) + '…' : name
  return `${shortName}@${domain}`
}
