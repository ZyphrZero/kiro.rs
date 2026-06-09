import { memo, useMemo } from 'react'
import { PieChart, Pie, Cell, Tooltip, ResponsiveContainer, Legend } from 'recharts'
import type { ModelDistribution } from '@/types/api'
import { tooltipContentStyle, tooltipItemStyle, tooltipLabelStyle } from './tooltip-style'
import { formatNumber } from '@/lib/utils'

interface Props {
  data: ModelDistribution[]
}

interface ChartDatum {
  inputTokens: number
  name: string
  outputTokens: number
  value: number
}

const PALETTE = [
  '#3b82f6', '#10b981', '#a855f7', '#f59e0b', '#ec4899',
  '#06b6d4', '#84cc16', '#f97316', '#6366f1', '#14b8a6',
]

function ModelPieChartImpl({ data }: Props) {
  const { chartData, total } = useMemo(() => buildChartData(data), [data])

  if (data.length === 0) {
    return <EmptyModelChart />
  }

  return <ModelChartContent chartData={chartData} total={total} />
}

function buildChartData(data: ModelDistribution[]) {
  const total = data.reduce((s, d) => s + d.calls, 0) || 1
  const chartData = data.map((d) => ({
    inputTokens: d.inputTokens,
    name: d.model,
    outputTokens: d.outputTokens,
    value: d.calls,
  }))
  return { chartData, total }
}

function EmptyModelChart() {
  return (
    <div className="flex h-[180px] items-center justify-center text-sm text-muted-foreground sm:h-[260px]">
      暂无数据
    </div>
  )
}

function ModelChartContent({
  chartData,
  total,
}: {
  chartData: ChartDatum[]
  total: number
}) {
  return (
    <div className="h-[220px] sm:h-[260px]">
      <ResponsiveContainer width="100%" height="100%">
        <PieChart>
          <Pie
            data={chartData}
            dataKey="value"
            nameKey="name"
            cx="50%"
            cy="50%"
            outerRadius="72%"
            innerRadius="40%"
            paddingAngle={2}
            isAnimationActive={false}
          >
          {chartData.map((_, i) => (
            <Cell key={i} fill={PALETTE[i % PALETTE.length]} />
          ))}
        </Pie>
          <Tooltip
            contentStyle={tooltipContentStyle}
            labelStyle={tooltipLabelStyle}
            itemStyle={tooltipItemStyle}
            cursor={false}
            formatter={(value: number, _name, item) =>
              formatTooltipValue({ item, total, value })}
          />
          <Legend wrapperStyle={{ fontSize: 11 }} iconSize={8} />
        </PieChart>
      </ResponsiveContainer>
    </div>
  )
}

function formatTooltipValue({
  item,
  total,
  value,
}: {
  item?: { payload?: ChartDatum }
  total: number
  value: number
}) {
  const pct = ((value / total) * 100).toFixed(1)
  const payload = item?.payload
  const input = formatNumber(payload?.inputTokens ?? 0)
  const output = formatNumber(payload?.outputTokens ?? 0)
  return [`${formatNumber(value)} 次（${pct}%）  in ${input} / out ${output}`, payload?.name ?? '']
}

export const ModelPieChart = memo(ModelPieChartImpl)
