import type { BalanceResponse, CredentialStatusItem } from '@/types/api'
import type { RailTone } from './rail'

/**
 * 凭据状态判定 —— 凭据页「处置意图」列的大脑，也是本次重设计的签名元素。
 *
 * 改造前每行右侧是一个 `⋯` 菜单，8 个菜单项固定不变。这把判断责任推给了用户：
 * 面对一个"冷却中"的凭据，得先知道冷却是什么、再在菜单里认出「解除冷却」这一项。
 * 菜单是通用容器，它对每一行说的话都一样。
 *
 * 这里换成：每行状态唯一决定一个**具名动作**。冷却中就写「解除冷却」，超额禁用就写
 * 「查看余额」，鉴权失败就写「重新登录」。界面替用户完成了那一步判断。
 *
 * 这个映射之所以不是通用表格模式，是因为它来自凭据这个域里真实存在的几种状态
 * 和各自唯一的处置手段 —— 换个项目它就不成立。
 *
 * `⋯` 菜单保留为二级入口，只是不再是唯一入口。
 */
export type CredentialState =
  | 'current' // 当前优先，正在服务
  | 'throttled' // 账号级限流冷却中，会自行恢复
  | 'quotaDisabled' // 因超额被禁用
  | 'quotaExceeded' // 已超额但仍启用
  | 'authFailed' // 鉴权 / token 失效类禁用
  | 'suspended' // 账号封禁
  | 'refreshFailing' // token 刷新在失败，但还没到禁用阈值
  | 'manualDisabled' // 手动禁用
  | 'otherDisabled' // 其他原因禁用
  | 'healthy'

/** 处置动作的语义类型，由调用方映射到具体 handler */
export type DispositionAction =
  | 'clearThrottle'
  | 'viewBalance'
  | 'relogin'
  | 'enable'
  | 'refreshToken'
  | 'viewFailures'
  | 'none'

export interface CredentialDisposition {
  state: CredentialState
  tone: RailTone
  /** 状态的中文说明，用于徽章 / 悬浮提示 */
  stateLabel: string
  /** 处置按钮文案；null 表示无需处置 */
  actionLabel: string | null
  action: DispositionAction
  /** 处置按钮是否为破坏性操作 */
  destructive?: boolean
}

/** 鉴权 / token 类禁用原因 —— 这些必须重新登录才能恢复 */
const AUTH_REASONS = new Set([
  'InvalidRefreshToken',
  'TooManyRefreshFailures',
  'InvalidConfig',
])

function isQuotaExceeded(balance?: BalanceResponse | null): boolean {
  if (!balance) return false
  return balance.remaining <= 0 || balance.usagePercentage >= 100
}

/**
 * 判定一个凭据当前处于哪种状态、下一步该做什么。
 *
 * 判定顺序即优先级：先看"是否被禁用且为什么"，再看"是否冷却中"，最后才是软状态。
 * 一个凭据可能同时超额且冷却，此时禁用原因更需要人处置，排在前面。
 */
export function getDisposition(
  c: CredentialStatusItem,
  balance?: BalanceResponse | null,
  throttleRemaining = c.throttledRemainingSecs ?? 0,
): CredentialDisposition {
  const reason = c.disabledReason

  if (c.disabled) {
    if (reason === 'QuotaExceeded') {
      return {
        state: 'quotaDisabled',
        tone: 'warn',
        stateLabel: '已禁用 · 已超额',
        // 超额不会自己好，但也不是"修一下就能用"——先看清余额和重置时间再决定
        actionLabel: '查看余额',
        action: 'viewBalance',
      }
    }
    if (reason === 'Suspended') {
      return {
        state: 'suspended',
        tone: 'banned',
        stateLabel: '已封禁 · 账号锁定',
        actionLabel: '重新登录',
        action: 'relogin',
      }
    }
    if (reason && AUTH_REASONS.has(reason)) {
      return {
        state: 'authFailed',
        tone: 'disabled',
        stateLabel:
          reason === 'InvalidRefreshToken'
            ? '已禁用 · Token 失效'
            : reason === 'TooManyRefreshFailures'
              ? '已禁用 · 刷新失败过多'
              : '已禁用 · 配置无效',
        actionLabel: '重新登录',
        action: 'relogin',
      }
    }
    if (reason === 'TooManyFailures') {
      return {
        state: 'otherDisabled',
        tone: 'disabled',
        stateLabel: '已禁用 · 失败过多',
        // 失败原因未必是凭据本身的问题，先看失败日志再决定要不要放回去
        actionLabel: '查看失败',
        action: 'viewFailures',
      }
    }
    if (reason === 'Manual' || !reason) {
      return {
        state: 'manualDisabled',
        tone: 'disabled',
        stateLabel: reason ? '已禁用 · 手动禁用' : '已禁用',
        actionLabel: '启用',
        action: 'enable',
      }
    }
    return {
      state: 'otherDisabled',
      tone: 'disabled',
      stateLabel: `已禁用 · ${reason}`,
      actionLabel: '启用',
      action: 'enable',
    }
  }

  // 未禁用但在限流冷却中：会自己恢复，所以是 cool 而非 warn
  if (throttleRemaining > 0) {
    return {
      state: 'throttled',
      tone: 'cool',
      stateLabel: '限流冷却中',
      actionLabel: '解除冷却',
      action: 'clearThrottle',
    }
  }

  if (isQuotaExceeded(balance)) {
    return {
      state: 'quotaExceeded',
      tone: 'warn',
      stateLabel: '已超额（仍在调度）',
      actionLabel: '查看余额',
      action: 'viewBalance',
    }
  }

  // 刷新在失败但还没到禁用阈值 —— 这是唯一能提前干预的窗口
  if (c.refreshFailureCount > 0) {
    return {
      state: 'refreshFailing',
      tone: 'warn',
      stateLabel: `Token 刷新失败 ${c.refreshFailureCount} 次`,
      actionLabel: '刷新 Token',
      action: 'refreshToken',
    }
  }

  if (c.isCurrent) {
    return {
      state: 'current',
      tone: 'ok',
      stateLabel: '当前优先',
      actionLabel: null,
      action: 'none',
    }
  }

  return {
    state: 'healthy',
    tone: 'none',
    stateLabel: '正常',
    actionLabel: null,
    action: 'none',
  }
}

/** 状态账条的分段计数 */
export interface CredentialCounts {
  healthy: number
  current: number
  throttled: number
  quota: number
  dead: number
  suspended: number
  total: number
}

export function countByState(
  credentials: CredentialStatusItem[],
  balanceOf: (c: CredentialStatusItem) => BalanceResponse | null | undefined,
): CredentialCounts {
  const counts: CredentialCounts = {
    healthy: 0,
    current: 0,
    throttled: 0,
    quota: 0,
    dead: 0,
    suspended: 0,
    total: credentials.length,
  }
  for (const c of credentials) {
    const { state } = getDisposition(c, balanceOf(c))
    switch (state) {
      case 'current':
        counts.current += 1
        counts.healthy += 1
        break
      case 'healthy':
      case 'refreshFailing':
        counts.healthy += 1
        break
      case 'throttled':
        counts.throttled += 1
        break
      case 'quotaExceeded':
      case 'quotaDisabled':
        counts.quota += 1
        break
      case 'suspended':
        counts.suspended += 1
        break
      default:
        counts.dead += 1
    }
  }
  return counts
}

/** 状态账条各段对应的筛选键 */
export type StateFilter = '' | 'healthy' | 'throttled' | 'quota' | 'dead' | 'suspended'

/** 某凭据是否命中状态筛选 */
export function matchesStateFilter(
  c: CredentialStatusItem,
  balance: BalanceResponse | null | undefined,
  filter: StateFilter,
): boolean {
  if (!filter) return true
  const { state } = getDisposition(c, balance)
  switch (filter) {
    case 'healthy':
      return state === 'healthy' || state === 'current' || state === 'refreshFailing'
    case 'throttled':
      return state === 'throttled'
    case 'quota':
      return state === 'quotaExceeded' || state === 'quotaDisabled'
    case 'suspended':
      return state === 'suspended'
    case 'dead':
      return (
        state === 'authFailed' ||
        state === 'manualDisabled' ||
        state === 'otherDisabled'
      )
    default:
      return true
  }
}
