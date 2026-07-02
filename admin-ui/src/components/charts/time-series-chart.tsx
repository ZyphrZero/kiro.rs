import { memo, useMemo } from 'react'
import {
  LineChart,
  Line,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from 'recharts'
import type { TimeSeriesPoint, StatsGranularity } from '@/types/api'
import { tooltipCursorStyle } from './tooltip-style'
import { formatCredits, formatNumber } from '@/lib/utils'

interface Props {
  data: TimeSeriesPoint[]
  granularity: StatsGranularity
}

const COLORS = {
  input: '#3b82f6',
  output: '#10b981',
  cacheCreation: '#f59e0b',
  cacheRead: '#06b6d4',
  cacheHitRate: '#a855f7',
  credits: '#ec4899',
} as const

const SERIES = [
  { key: 'inputTokens', name: '输入', color: COLORS.input, axis: 'left' as const, kind: 'tokens' as const },
  { key: 'outputTokens', name: '输出', color: COLORS.output, axis: 'left' as const, kind: 'tokens' as const },
  { key: 'cacheCreationTokens', name: '缓存写', color: COLORS.cacheCreation, axis: 'left' as const, kind: 'tokens' as const },
  { key: 'cacheReadTokens', name: '缓存读', color: COLORS.cacheRead, axis: 'left' as const, kind: 'tokens' as const },
  { key: 'cacheHitRate', name: '命中率', color: COLORS.cacheHitRate, axis: 'right' as const, kind: 'percent' as const },
]

interface ChartPoint extends TimeSeriesPoint {
  label: string
  cacheHitRate: number
}

function formatTs(ts: string, granularity: StatsGranularity): string {
  const d = new Date(ts)
  const md = `${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`
  if (granularity === 'day') return `${d.getFullYear()}-${md}`
  return `${d.getFullYear()}-${md} ${String(d.getHours()).padStart(2, '0')}:00`
}

/**
 * 命中率 = cacheRead /(input + cacheCreation + cacheRead)，无可计量 token 时为 0。
 *
 * 「这次请求里有多大比例是便宜地从缓存读到的」。creation 是溢价写入（最贵的桶，
 * 为将来命中而付费写入缓存），经济上算未命中——只进分母不进分子，所以首轮（全是写入、
 * 零读取）如实显示 ≈0%，写入重的轮次命中率被正确拉低；长对话稳态读占绝对多数时趋近高位。
 */
function calcHitRate(p: TimeSeriesPoint): number {
  const denom = p.inputTokens + p.cacheCreationTokens + p.cacheReadTokens
  if (denom <= 0) return 0
  return (p.cacheReadTokens / denom) * 100
}

function pickXAxisInterval(len: number): number | 'preserveStartEnd' {
  if (len <= 12) return 0
  if (len <= 48) return Math.ceil(len / 12)
  return Math.ceil(len / 16)
}

function ChartTooltip({ active, payload, label }: {
  active?: boolean
  payload?: ReadonlyArray<{
    dataKey?: string | number
    value?: number
    color?: string
    payload?: ChartPoint
  }>
  label?: string
}) {
  if (!active || !payload?.length) return null
  const map = new Map<string, number>()
  payload.forEach((p) => {
    if (typeof p.dataKey === 'string' && typeof p.value === 'number') {
      map.set(p.dataKey, p.value)
    }
  })
  const credits = payload[0]?.payload?.credits ?? 0
  return (
    <div style={TOOLTIP_STYLE}>
      <div style={{ fontWeight: 600, marginBottom: 6, color: 'rgba(255,255,255,0.92)' }}>{label}</div>
      {SERIES.map((s) => <TooltipRow key={s.key} entry={s} value={map.get(s.key)} />)}
      {credits > 0 && <CreditTooltipRow credits={credits} />}
    </div>
  )
}

const TOOLTIP_STYLE: React.CSSProperties = {
  background: 'rgba(20,20,20,0.94)',
  border: '1px solid rgba(255,255,255,0.08)',
  borderRadius: 10,
  boxShadow: '0 8px 24px rgba(0,0,0,0.25)',
  color: '#fff',
  fontSize: 12,
  minWidth: 180,
  padding: '10px 14px',
}

const TOOLTIP_ROW_STYLE: React.CSSProperties = {
  alignItems: 'center',
  display: 'flex',
  gap: 8,
  padding: '2px 0',
}

const TOOLTIP_SWATCH_BASE_STYLE: React.CSSProperties = {
  borderRadius: 2,
  display: 'inline-block',
  height: 10,
  width: 10,
}

const TOOLTIP_VALUE_STYLE: React.CSSProperties = {
  fontVariantNumeric: 'tabular-nums',
}

function TooltipRow({
  entry,
  value,
}: {
  entry: (typeof SERIES)[number]
  value?: number
}) {
  if (value == null) return null
  const valueStr = entry.kind === 'percent' ? `${value.toFixed(1)}%` : formatNumber(value)
  return (
    <div style={TOOLTIP_ROW_STYLE}>
      <span style={{ ...TOOLTIP_SWATCH_BASE_STYLE, background: entry.color }} />
      <span style={{ flex: 1 }}>{entry.name}:</span>
      <span style={TOOLTIP_VALUE_STYLE}>{valueStr}</span>
    </div>
  )
}

function CreditTooltipRow({ credits }: { credits: number }) {
  return (
    <div style={CREDIT_ROW_STYLE}>
      <span style={{ ...TOOLTIP_SWATCH_BASE_STYLE, background: COLORS.credits }} />
      <span style={{ flex: 1 }}>Credit:</span>
      <span style={TOOLTIP_VALUE_STYLE}>{formatCredits(credits)}</span>
    </div>
  )
}

const CREDIT_ROW_STYLE: React.CSSProperties = {
  ...TOOLTIP_ROW_STYLE,
  borderTop: '1px solid rgba(255,255,255,0.08)',
  marginTop: 4,
  padding: '4px 0 0',
}

function TimeSeriesChartImpl({ data, granularity }: Props) {
  const formatted = useMemo<ChartPoint[]>(
    () =>
      data.map((p) => ({
        ...p,
        label: formatTs(p.ts, granularity),
        cacheHitRate: calcHitRate(p),
      })),
    [data, granularity],
  )
  const interval = useMemo(() => pickXAxisInterval(formatted.length), [formatted.length])
  // 全零时强制让左轴显示 0 刻度，避免空白
  const leftAllZero = useMemo(
    () =>
      formatted.every(
        (p) =>
          p.inputTokens === 0 &&
          p.outputTokens === 0 &&
          p.cacheCreationTokens === 0 &&
          p.cacheReadTokens === 0,
      ),
    [formatted],
  )

  return (
    <div className="h-[260px] sm:h-[320px]">
      <ResponsiveContainer width="100%" height="100%">
        <LineChart data={formatted} margin={{ top: 16, right: 6, left: -12, bottom: 0 }}>
          {chartAxes({ interval, leftAllZero })}
          <Tooltip content={<ChartTooltip />} cursor={tooltipCursorStyle} />
          {chartLegend()}
          {chartLines()}
        </LineChart>
      </ResponsiveContainer>
    </div>
  )
}

function chartAxes({
  interval,
  leftAllZero,
}: {
  interval: number | 'preserveStartEnd'
  leftAllZero: boolean
}) {
  return [
    <CartesianGrid key="grid" strokeDasharray="3 3" className="stroke-border/50" />,
    <XAxis
      key="x"
      dataKey="label"
      tick={{ fontSize: 11 }}
      className="fill-muted-foreground"
      interval={interval}
    />,
    <YAxis
      key="left"
      yAxisId="left"
      tick={{ fontSize: 11 }}
      className="fill-muted-foreground"
      tickFormatter={(v: number) => formatNumber(v)}
      width={48}
      domain={leftAllZero ? [0, 1] : [0, 'auto']}
      ticks={leftAllZero ? [0] : undefined}
      allowDecimals={false}
    />,
    <YAxis
      key="right"
      yAxisId="right"
      orientation="right"
      tick={{ fontSize: 11, fill: COLORS.cacheHitRate }}
      domain={[0, 100]}
      ticks={[0, 20, 40, 60, 80, 100]}
      tickFormatter={(v: number) => `${v}%`}
      width={36}
    />,
  ]
}

function chartLegend() {
  return <Legend verticalAlign="top" align="center" iconType="circle" wrapperStyle={LEGEND_STYLE} />
}

const LEGEND_STYLE: React.CSSProperties = {
  fontSize: 12,
  paddingBottom: 8,
}

function chartLines() {
  return SERIES.map((s) => (
    <Line
      key={s.key}
      yAxisId={s.axis}
      type="monotone"
      dataKey={s.key}
      stroke={s.color}
      name={s.name}
      dot={false}
      strokeWidth={s.kind === 'percent' ? 1.8 : 2}
      strokeDasharray={s.kind === 'percent' ? '4 4' : undefined}
      isAnimationActive
      animationDuration={550}
      animationEasing="ease-out"
    />
  ))
}

export const TimeSeriesChart = memo(TimeSeriesChartImpl)
