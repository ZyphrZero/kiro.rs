/**
 * 状态色轨 —— 凭据行 / 日志行 / 设置项共用的一套异常语言。
 *
 * 设计前提：异常在三个页面里必须是同一个视觉信号，否则用户要为每个页面
 * 重新学一遍配色。四档的划分不是随意的深浅，每档对应一种**处置期待**：
 *
 * - `ok`   健康，无需处置
 * - `warn` 需要人介入，且不会自行恢复（超额、额度耗尽）
 * - `cool` 会自行恢复，等待即可（账号级风控冷却）
 * - `dead` 已失效，必须处置才能复用（鉴权失败、禁用、网络错误）
 *
 * `warn` 与 `cool` 分色的信息量就在这里：橙色代表"等它自己好"，
 * 琥珀代表"你得去做点什么"。CSS 变量定义见 index.css 的 `--rail-*`。
 */
export type RailTone = 'ok' | 'warn' | 'cool' | 'dead' | 'none'

const RAIL_CLASS: Record<RailTone, string> = {
  ok: 'console-rail rail-ok',
  warn: 'console-rail rail-warn',
  cool: 'console-rail rail-cool',
  dead: 'console-rail rail-dead',
  none: '',
}

/** 色轨 class（用在 `<tr>` 的首个 `<td>` 或整行容器上） */
export function railClass(tone: RailTone): string {
  return RAIL_CLASS[tone]
}

const BORDER_CLASS: Record<RailTone, string> = {
  ok: 'border-l-[3px] border-l-emerald-500',
  warn: 'border-l-[3px] border-l-amber-500',
  cool: 'border-l-[3px] border-l-orange-500',
  dead: 'border-l-[3px] border-l-red-500',
  none: 'border-l-[3px] border-l-transparent',
}

/**
 * 色轨的 border 变体。
 *
 * `.console-rail` 走 inset box-shadow，在表格单元格里最省事；但用在已经带 `ring-1`
 * 的容器上会打架 —— ring 也是 box-shadow 实现的，两者互相覆盖。这个变体用 border-left，
 * 与 ring 可以共存。凭据列表行用它，表格单元格用 `railClass`。
 */
export function railBorderClass(tone: RailTone): string {
  return BORDER_CLASS[tone]
}

const DOT_CLASS: Record<RailTone, string> = {
  ok: 'bg-emerald-500',
  warn: 'bg-amber-500',
  cool: 'bg-orange-500',
  dead: 'bg-red-500',
  none: 'bg-muted-foreground/40',
}

/** 同一语义的圆点色（用于紧凑位置，如链路节点） */
export function railDotClass(tone: RailTone): string {
  return DOT_CLASS[tone]
}

const CHIP_CLASS: Record<RailTone, string> = {
  ok: 'border-emerald-500/30 bg-emerald-500/10 text-emerald-700 dark:text-emerald-400',
  warn: 'border-amber-500/30 bg-amber-500/10 text-amber-700 dark:text-amber-400',
  cool: 'border-orange-500/30 bg-orange-500/10 text-orange-700 dark:text-orange-400',
  dead: 'border-red-500/30 bg-red-500/10 text-red-700 dark:text-red-400',
  none: 'border-border bg-secondary/60 text-muted-foreground',
}

/**
 * 色轨的「实心块」变体：淡底 + 同色描边 + 同色文字。
 *
 * 存在的理由是尺度而非样式 —— 3px 的左边框在网格卡片里太细，扫读时得聚焦才能读出
 * 状态。同一个 tone 在边框、序号章、余额条上各出现一次，眼睛在任一尺度上都能接住。
 */
export function railChipClass(tone: RailTone): string {
  return CHIP_CLASS[tone]
}

const TEXT_CLASS: Record<RailTone, string> = {
  ok: 'text-emerald-600 dark:text-emerald-400',
  warn: 'text-amber-600 dark:text-amber-400',
  cool: 'text-orange-600 dark:text-orange-400',
  dead: 'text-red-600 dark:text-red-400',
  none: 'text-muted-foreground',
}

/** 同一语义的文字色 */
export function railTextClass(tone: RailTone): string {
  return TEXT_CLASS[tone]
}

/**
 * 日志失败分类 → 色轨。与 trace 的 `outcome` / `errorType` 取值对齐。
 */
export function outcomeTone(outcome: string): RailTone {
  switch (outcome) {
    case 'success':
      return 'ok'
    case 'quota_exhausted':
    case 'stream_interrupted':
    case 'abandoned_before_headers':
      return 'warn'
    case 'account_throttled':
    case 'local_rate_limited':
      return 'cool'
    case 'auth_failed':
    case 'network_error':
    case 'bad_request':
      return 'dead'
    case 'transient':
      return 'none'
    default:
      return 'none'
  }
}
