import { useState } from 'react'
import {
  Activity,
  RefreshCw,
  UploadCloud,
  MoreHorizontal,
  ShieldAlert,
  ShieldCheck,
  Boxes,
  HeartPulse,
  HeartCrack,
} from 'lucide-react'
import { useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import { Button } from '@/components/ui/button'
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
} from '@/components/ui/dropdown-menu'
import {
  useLoadBalancingMode,
  useSetLoadBalancingMode,
  useAccountThrottleConfig,
  useSetAccountThrottleConfig,
  useSelfHealConfig,
  useSetSelfHealConfig,
} from '@/hooks/use-credentials'
import { useUpdateCheck } from '@/hooks/use-update-check'
import { extractErrorMessage } from '@/lib/utils'
import { ImageUpdateDialog } from '@/components/image-update-dialog'
import { AvailableModelsDialog } from '@/components/available-models-dialog'

/**
 * 顶栏工具区：三个调度开关 + 三个动作按钮。
 *
 * 这里此前塞了完整的配置面板 —— 冷却时长的 5 个预设按钮、自定义分钟输入、自愈连续
 * 上限输入、登录密钥表单，还各写了 compact / full 两套。下拉菜单里放数字输入框本来
 * 就不是它该干的事：菜单是"选一个动作"的容器，不是表单容器。
 *
 * 现在的分工：**顶栏只放一次点击就能完成的开关**（这三个是运维高频动作，不该退化成
 * "进设置页找"），所有参数调整归「设置」Tab。compact 与 full 因此收敛成同一份
 * 开关定义，窄屏只是把它们折进一个菜单。
 */
interface TopbarToolsProps {
  compact?: boolean
}

// 顶栏“刷新数据”只刷新数据查询；配置查询有各自的保存/轮询语义。
const NON_DATA_QUERY_ROOTS = new Set([
  'loadBalancingMode',
  'accountThrottleConfig',
  'accountRpmLimitConfig',
  'selfHealConfig',
  'logGovernanceConfig',
  'global-proxy',
  'custom-models',
  'update-config',
  'system-update-check',
])

/** 一个开关的完整描述，compact / full 两种排布共用 */
interface ToggleSpec {
  key: string
  /** 当前是否开启 */
  on: boolean
  busy: boolean
  /** full 模式的按钮文案 */
  label: string
  /** compact 模式的菜单项文案（说明这次点击会做什么） */
  menuLabel: string
  title: string
  icon: React.ReactNode
  onToggle: () => void
}

export function TopbarTools({ compact = false }: TopbarToolsProps) {
  const queryClient = useQueryClient()
  const { data: lbData, isLoading: lbLoading } = useLoadBalancingMode()
  const { mutate: setLb, isPending: lbSaving } = useSetLoadBalancingMode()
  const { data: throttle, isLoading: thLoading } = useAccountThrottleConfig()
  const { mutate: setThrottle, isPending: thSaving } = useSetAccountThrottleConfig()
  const { data: selfHeal, isLoading: shLoading } = useSelfHealConfig()
  const { mutate: setSelfHeal, isPending: shSaving } = useSetSelfHealConfig()
  const { data: updateCheck } = useUpdateCheck()

  const [imageUpdateOpen, setImageUpdateOpen] = useState(false)
  const [modelsOpen, setModelsOpen] = useState(false)

  const handleRefresh = () => {
    // 刷新所有数据查询；配置查询不属于“刷新数据”，也不会因此触发上游检查。
    queryClient.invalidateQueries({
      predicate: ({ queryKey }) => {
        const root = queryKey[0]
        return typeof root === 'string' && !NON_DATA_QUERY_ROOTS.has(root)
      },
    })
    toast.success('已刷新')
  }

  const onError = (err: unknown) =>
    toast.error('切换失败：' + extractErrorMessage(err))

  const balanced = lbData?.mode === 'balanced'
  const failover = throttle?.failover ?? true
  const healing = selfHeal?.enabled ?? true
  const cooldownMin = Math.round((throttle?.cooldownSecs ?? 1800) / 60)

  const toggles: ToggleSpec[] = [
    {
      key: 'lb',
      on: balanced,
      busy: lbLoading || lbSaving,
      label: lbLoading ? '加载中…' : balanced ? '均衡负载' : '按优先级',
      menuLabel: balanced ? '切换到按优先级' : '切换到均衡负载',
      title: balanced
        ? '调度模式：均衡负载 —— 按用量动态摊平到整个池子'
        : '调度模式：按优先级 —— 数字越小越先用，用完再换下一个',
      icon: <Activity className="h-3.5 w-3.5" />,
      onToggle: () =>
        setLb(balanced ? 'priority' : 'balanced', {
          onSuccess: () =>
            toast.success(
              balanced ? '已切换到按优先级调度' : '已切换到均衡负载',
            ),
          onError,
        }),
    },
    {
      key: 'failover',
      on: failover,
      busy: thLoading || thSaving,
      label: thLoading ? '加载中…' : failover ? `故障转移 · ${cooldownMin}m` : '不切换',
      menuLabel: failover ? '关闭故障转移' : '开启故障转移',
      title: failover
        ? `账号级风控故障转移：开启（冷却 ${cooldownMin} 分钟，可在设置页调整）`
        : '账号级风控故障转移：关闭',
      icon: failover ? (
        <ShieldCheck className="h-3.5 w-3.5 text-emerald-600" />
      ) : (
        <ShieldAlert className="h-3.5 w-3.5 text-amber-500" />
      ),
      onToggle: () =>
        setThrottle(
          { failover: !failover },
          {
            onSuccess: () =>
              toast.success(failover ? '已关闭故障转移' : '已开启故障转移'),
            onError,
          },
        ),
    },
    {
      key: 'selfheal',
      on: healing,
      busy: shLoading || shSaving,
      label: shLoading ? '加载中…' : healing ? '自愈开' : '自愈关',
      menuLabel: healing ? '关闭凭据自愈' : '开启凭据自愈',
      title: healing
        ? `凭据自愈：已启用（当前连续 ${selfHeal?.consecutiveRounds ?? 0} 轮，参数见设置页）`
        : '凭据自愈：已关闭',
      icon: healing ? (
        <HeartPulse className="h-3.5 w-3.5 text-emerald-600" />
      ) : (
        <HeartCrack className="h-3.5 w-3.5 text-amber-500" />
      ),
      onToggle: () =>
        setSelfHeal(
          { enabled: !healing },
          {
            onSuccess: () =>
              toast.success(healing ? '已关闭凭据自愈' : '已开启凭据自愈'),
            onError,
          },
        ),
    },
  ]

  return (
    <>
      {compact ? (
        <CompactTools
          toggles={toggles}
          hasUpdate={!!updateCheck?.hasUpdate}
          onRefresh={handleRefresh}
          onOpenModels={() => setModelsOpen(true)}
          onOpenImageUpdate={() => setImageUpdateOpen(true)}
        />
      ) : (
        <FullTools
          toggles={toggles}
          updateCheck={updateCheck}
          onRefresh={handleRefresh}
          onOpenModels={() => setModelsOpen(true)}
          onOpenImageUpdate={() => setImageUpdateOpen(true)}
        />
      )}
      <ImageUpdateDialog open={imageUpdateOpen} onOpenChange={setImageUpdateOpen} />
      <AvailableModelsDialog open={modelsOpen} onOpenChange={setModelsOpen} />
    </>
  )
}

interface ToolsProps {
  toggles: ToggleSpec[]
  onRefresh: () => void
  onOpenModels: () => void
  onOpenImageUpdate: () => void
}

function FullTools({
  toggles,
  updateCheck,
  onRefresh,
  onOpenModels,
  onOpenImageUpdate,
}: ToolsProps & {
  updateCheck?: { hasUpdate: boolean; latestVersion: string; currentVersion: string }
}) {
  return (
    <>
      {toggles.map((t) => (
        <Button
          key={t.key}
          variant="outline"
          size="sm"
          onClick={t.onToggle}
          disabled={t.busy}
          title={t.title}
        >
          {t.icon}
          <span className="hidden md:inline">{t.label}</span>
        </Button>
      ))}
      <Button variant="ghost" size="icon" onClick={onOpenModels} title="可用模型">
        <Boxes className="h-4 w-4" />
      </Button>
      <Button variant="ghost" size="icon" onClick={onRefresh} title="刷新数据">
        <RefreshCw className="h-4 w-4" />
      </Button>
      <Button
        variant="ghost"
        size="icon"
        onClick={onOpenImageUpdate}
        title={
          updateCheck?.hasUpdate
            ? `发现新版本 v${updateCheck.latestVersion}（当前 v${updateCheck.currentVersion}）`
            : '镜像在线更新'
        }
        className="relative"
      >
        <UploadCloud className="h-4 w-4" />
        {updateCheck?.hasUpdate && <UpdateDot />}
      </Button>
    </>
  )
}

function CompactTools({
  toggles,
  hasUpdate,
  onRefresh,
  onOpenModels,
  onOpenImageUpdate,
}: ToolsProps & { hasUpdate: boolean }) {
  return (
    <DropdownMenu modal={false}>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" size="icon" title="更多操作" className="relative">
          <MoreHorizontal className="h-4 w-4" />
          {hasUpdate && <UpdateDot />}
        </Button>
      </DropdownMenuTrigger>
      {/* 窄屏兜底：菜单项随调度开关增加时不撑出视口，超出即在菜单内滚动 */}
      <DropdownMenuContent
        align="end"
        className="max-h-[calc(100dvh-4.5rem)] w-56 max-w-[calc(100dvw-1rem)] overflow-x-hidden overflow-y-auto overscroll-contain"
      >
        <DropdownMenuLabel>调度</DropdownMenuLabel>
        {toggles.map((t) => (
          <DropdownMenuItem key={t.key} disabled={t.busy} onSelect={t.onToggle}>
            {t.icon}
            {t.menuLabel}
          </DropdownMenuItem>
        ))}
        <DropdownMenuLabel>操作</DropdownMenuLabel>
        <DropdownMenuItem onSelect={onRefresh}>
          <RefreshCw />
          刷新数据
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={onOpenModels}>
          <Boxes />
          可用模型
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={onOpenImageUpdate}>
          <UploadCloud />
          镜像在线更新
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenu>
  )
}

function UpdateDot() {
  return (
    <span className="absolute right-1 top-1 inline-flex h-2 w-2 items-center justify-center">
      <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-red-400 opacity-75" />
      <span className="relative inline-flex h-2 w-2 rounded-full bg-red-500" />
    </span>
  )
}
