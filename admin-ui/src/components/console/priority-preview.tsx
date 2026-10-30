import { useCredentials, useLoadBalancingMode } from '@/hooks/use-credentials'

/**
 * 优先级编辑时的队列位置预览 —— 优先级这件事的签名元素。
 *
 * 要解决的困惑很具体：面对一个写着 `0` 的输入框，用户无法判断改成 `5` 是让这个号
 * 更早还是更晚被用到。界面从头到尾没说过方向。
 *
 * 常规做法是在标签上补一句「（数字越小越优先）」。但那只是把规则背给用户听，而且
 * 每一行都要重复一遍；用户仍然得自己在脑子里把规则套到当前这堆数字上，才知道
 * 「我这个 5 到底排第几」。
 *
 * 这里换个路子：不解释规则，直接显示后果。输入 5 的当下就告诉他「排在 bob@x.com
 * 之后 · 第 3 / 8 位」。方向感是从相邻是谁里读出来的，不需要先理解规则。
 *
 * 排序口径与后端 `select_by_priority` 对齐：priority 升序，同值再按 id 升序。
 * 已禁用的凭据不参与调度，因此不计入队列。
 */

/** 邻居的展示名。与凭据行一致地带上 #id，指认的是同一个东西。 */
function label(email: string | undefined, id: number): string {
  return email ? `#${id} ${email}` : `#${id}`
}

export function PriorityPreview({
  credentialId,
  draft,
  disabled,
}: {
  credentialId: number
  /** 输入框里的原始字符串，未必是合法数字 */
  draft: string
  /** 该凭据当前是否已禁用 */
  disabled: boolean
}) {
  // 读 react-query 缓存里的凭据全集（与列表同一个 queryKey，不会额外发请求）
  const { data } = useCredentials()
  const { data: lbData } = useLoadBalancingMode()
  const invert = lbData?.invertPriority ?? false

  const n = Number(draft)
  if (draft.trim() === '' || !Number.isInteger(n) || n < 0) {
    return (
      <span className="text-[11px] text-muted-foreground">
        填 0 或更大的整数
      </span>
    )
  }

  if (disabled) {
    return (
      <span className="text-[11px] text-muted-foreground">
        已禁用，暂不参与排队
      </span>
    )
  }

  const all = data?.credentials ?? []
  // 把草稿值套进去重排，算出这次改动之后它落在第几位
  const queue = all
    .filter((c) => !c.disabled)
    .map((c) => (c.id === credentialId ? { ...c, priority: n } : c))
    .sort((a, b) => {
      const pDiff = invert ? b.priority - a.priority : a.priority - b.priority
      return pDiff || a.id - b.id
    })

  const idx = queue.findIndex((c) => c.id === credentialId)
  if (idx < 0) {
    return null
  }

  const prev = queue[idx - 1]
  const ahead = idx // 排在它前面的个数

  return (
    <span className="text-[11px] leading-tight text-muted-foreground">
      {ahead === 0 ? (
        <span className="font-medium text-emerald-600 dark:text-emerald-400">
          最先使用
        </span>
      ) : (
        <>
          排在 <span className="text-foreground">{label(prev.email, prev.id)}</span>{' '}
          之后
        </>
      )}
      <span className="mx-1 text-muted-foreground/50">·</span>
      第 <span className="console-num text-foreground">{idx + 1}</span> /{' '}
      <span className="console-num">{queue.length}</span> 位
    </span>
  )
}
