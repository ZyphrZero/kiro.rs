import { forwardRef, useEffect, useState, type ComponentPropsWithoutRef } from 'react'
import {
  Activity, RefreshCw, UploadCloud, Settings, Key, Wand2, Eye, EyeOff, Copy,
  MoreHorizontal, ShieldAlert, ShieldCheck,
} from 'lucide-react'
import { useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import { storage } from '@/lib/storage'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter, DialogDescription,
} from '@/components/ui/dialog'
import {
  DropdownMenu, DropdownMenuTrigger, DropdownMenuContent,
  DropdownMenuItem, DropdownMenuLabel,
} from '@/components/ui/dropdown-menu'
import {
  useLoadBalancingMode, useSetLoadBalancingMode,
  useAccountThrottleConfig, useSetAccountThrottleConfig,
} from '@/hooks/use-credentials'
import { useUpdateCheck } from '@/hooks/use-update-check'
import { updateAdminKey, updateApiKey } from '@/api/credentials'
import { extractErrorMessage, generateApiKey } from '@/lib/utils'
import { ImageUpdateDialog } from '@/components/image-update-dialog'

/**
 * 顶栏右侧通用工具栏：负载均衡切换、刷新、在线更新、设置（Key 管理）。
 *
 * 与原 Dashboard 中的工具按钮等价，但全局 Tab 都可访问。刷新按钮会失效
 * 凭据/客户端 Key/统计三类查询，覆盖三个 Tab 的主要数据源。
 */
interface TopbarToolsProps {
  compact?: boolean
}

export function TopbarTools({ compact = false }: TopbarToolsProps) {
  const queryClient = useQueryClient()
  const { data: loadBalancingData, isLoading: isLoadingMode } = useLoadBalancingMode()
  const { mutate: setLoadBalancingMode, isPending: isSettingMode } = useSetLoadBalancingMode()
  const { data: throttleConfig, isLoading: isLoadingThrottle } = useAccountThrottleConfig()
  const { mutate: setThrottleConfig, isPending: isSettingThrottle } = useSetAccountThrottleConfig()
  const { data: updateCheck } = useUpdateCheck()

  const [imageUpdateOpen, setImageUpdateOpen] = useState(false)

  const [keyDialogOpen, setKeyDialogOpen] = useState(false)
  const [keyEditMode, setKeyEditMode] = useState<'admin' | 'api'>('admin')
  const [newKey, setNewKey] = useState('')
  const [showPlain, setShowPlain] = useState(false)
  const [updating, setUpdating] = useState(false)

  const handleRefresh = () => {
    queryClient.invalidateQueries({ queryKey: ['credentials'] })
    queryClient.invalidateQueries({ queryKey: ['client-keys'] })
    queryClient.invalidateQueries({ queryKey: ['stats'] })
    toast.success('已刷新')
  }

  const handleToggleLoadBalancing = () => {
    const cur = loadBalancingData?.mode || 'priority'
    const next = cur === 'priority' ? 'balanced' : 'priority'
    setLoadBalancingMode(next, {
      onSuccess: () => toast.success(`已切换到${next === 'priority' ? '优先级模式' : '均衡负载模式'}`),
      onError: (err) => toast.error(`切换失败: ${extractErrorMessage(err)}`),
    })
  }

  const handleToggleFailover = () => {
    const cur = throttleConfig?.failover ?? true
    const next = !cur
    setThrottleConfig({ failover: next }, {
      onSuccess: () => toast.success(next ? '已开启账号级风控故障转移' : '已关闭账号级风控故障转移'),
      onError: (err) => toast.error(`切换失败: ${extractErrorMessage(err)}`),
    })
  }

  const openKeyDialog = (mode: 'admin' | 'api') => {
    setKeyEditMode(mode)
    setNewKey('')
    setShowPlain(false)
    setKeyDialogOpen(true)
  }

  const handleUpdateKey = async (e: React.FormEvent) => {
    e.preventDefault()
    const key = newKey.trim()
    if (!key) {
      toast.error(keyEditMode === 'admin' ? '新登录API密钥不能为空' : '新管理员API密钥不能为空')
      return
    }
    setUpdating(true)
    try {
      if (keyEditMode === 'admin') {
        await updateAdminKey({ newKey: key })
        storage.setApiKey(key)
        toast.success('登录API密钥已更新，已自动切换到新 Key')
      } else {
        await updateApiKey({ newKey: key })
        toast.success('管理员API密钥已更新，所有使用 /v1 接口的客户端都需要切换')
      }
      setKeyDialogOpen(false)
      setNewKey('')
    } catch (err) {
      toast.error(`更新失败: ${extractErrorMessage(err)}`)
    } finally {
      setUpdating(false)
    }
  }

  const controls = {
    handleRefresh,
    handleToggleFailover,
    handleToggleLoadBalancing,
    isLoadingMode,
    isLoadingThrottle,
    isSettingMode,
    isSettingThrottle,
    loadBalancingMode: loadBalancingData?.mode,
    openImageUpdate: () => setImageUpdateOpen(true),
    openKeyDialog,
    throttleConfig,
    updateCheck,
    updateCooldown: (secs: number) =>
      setThrottleConfig({ cooldownSecs: secs }, {
        onSuccess: () =>
          toast.success(`冷却时长已设为 ${Math.round(secs / 60)} 分钟`),
        onError: (err) => toast.error(`保存失败: ${extractErrorMessage(err)}`),
      }),
  }

  return (
    <>
      {compact ? <CompactTools controls={controls} /> : <FullTools controls={controls} />}
      <ImageUpdateDialog open={imageUpdateOpen} onOpenChange={setImageUpdateOpen} />

      <Dialog
        open={keyDialogOpen}
        onOpenChange={(open) => { if (!updating) setKeyDialogOpen(open) }}
      >
        <DialogContent className="sm:max-w-sm">
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              <Key className="h-4 w-4" />
              {keyEditMode === 'admin' ? '修改登录API密钥' : '修改管理员API密钥'}
            </DialogTitle>
            <DialogDescription>
              {keyEditMode === 'admin'
                ? '用于登录此管理面板。修改后将自动更新本地存储的 Key，无需重新登录。'
                : '客户端调用 /v1/* 接口时携带的密钥。修改后所有第三方客户端（Cline、Cursor、SDK 等）都需要更新为新值。'}
            </DialogDescription>
          </DialogHeader>
          <form onSubmit={handleUpdateKey} className="space-y-4 py-2">
            <div className="relative">
              <Input
                type={showPlain ? 'text' : 'password'}
                placeholder={
                  keyEditMode === 'admin'
                    ? '输入或生成新的登录API密钥'
                    : '输入或生成新的管理员API密钥'
                }
                value={newKey}
                onChange={(e) => setNewKey(e.target.value)}
                disabled={updating}
                autoFocus
                className="pr-20 font-mono text-[13px]"
              />
              <div className="pointer-events-none absolute inset-y-0 right-0 flex items-center pr-1.5">
                <Button
                  type="button"
                  size="icon"
                  variant="ghost"
                  className="pointer-events-auto h-7 w-7"
                  onClick={() => setShowPlain((v) => !v)}
                  disabled={updating}
                  title={showPlain ? '隐藏' : '显示'}
                >
                  {showPlain ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
                </Button>
                <Button
                  type="button"
                  size="icon"
                  variant="ghost"
                  className="pointer-events-auto h-7 w-7"
                  onClick={async () => {
                    if (!newKey.trim()) {
                      toast.error('请先输入或生成 Key 再复制')
                      return
                    }
                    try {
                      await navigator.clipboard.writeText(newKey)
                      toast.success('已复制到剪贴板')
                    } catch {
                      toast.error('复制失败，请手动选择文本')
                    }
                  }}
                  disabled={updating}
                  title="复制"
                >
                  <Copy className="h-3.5 w-3.5" />
                </Button>
              </div>
            </div>
            <div className="flex items-center justify-between gap-2">
              <Button
                type="button"
                size="sm"
                variant="outline"
                onClick={() => {
                  const key = generateApiKey(
                    keyEditMode === 'admin' ? 'sk-admin-' : 'sk-kiro-',
                  )
                  setNewKey(key)
                  setShowPlain(true)
                }}
                disabled={updating}
              >
                <Wand2 className="h-3.5 w-3.5" />生成随机 Key
              </Button>
              <p className="text-[11px] text-muted-foreground">
                建议生成后立即复制保存，确认更新后即生效。
              </p>
            </div>
            <DialogFooter>
              <Button type="button" variant="outline" onClick={() => setKeyDialogOpen(false)} disabled={updating}>
                取消
              </Button>
              <Button type="submit" disabled={updating || !newKey.trim()}>
                {updating ? '更新中…' : '确认更新'}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>
    </>
  )
}

interface ToolControls {
  handleRefresh: () => void
  handleToggleFailover: () => void
  handleToggleLoadBalancing: () => void
  isLoadingMode: boolean
  isLoadingThrottle: boolean
  isSettingMode: boolean
  isSettingThrottle: boolean
  loadBalancingMode?: 'priority' | 'balanced'
  openImageUpdate: () => void
  openKeyDialog: (mode: 'admin' | 'api') => void
  throttleConfig?: { failover: boolean; cooldownSecs: number }
  updateCheck?: { hasUpdate: boolean; latestVersion: string; currentVersion: string }
  updateCooldown: (secs: number) => void
}

function FullTools({ controls }: { controls: ToolControls }) {
  return (
    <>
      <LoadBalancingButton controls={controls} />
      <ThrottleConfigButton
        config={controls.throttleConfig}
        loading={controls.isLoadingThrottle}
        saving={controls.isSettingThrottle}
        onToggleFailover={controls.handleToggleFailover}
        onChangeCooldown={controls.updateCooldown}
      />
      <RefreshButton onRefresh={controls.handleRefresh} />
      <ImageUpdateButton controls={controls} />
      <KeySettingsMenu onOpenKeyDialog={controls.openKeyDialog} />
    </>
  )
}

function CompactTools({ controls }: { controls: ToolControls }) {
  const throttleProps = {
    config: controls.throttleConfig,
    loading: controls.isLoadingThrottle,
    saving: controls.isSettingThrottle,
    onToggleFailover: controls.handleToggleFailover,
    onChangeCooldown: controls.updateCooldown,
  }

  return (
    <DropdownMenu modal={false}>
      <DropdownMenuTrigger asChild>
        <Button variant="outline" size="icon" title="更多操作">
          <MoreHorizontal className="h-4 w-4" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-64">
        <DropdownMenuLabel>系统操作</DropdownMenuLabel>
        <DropdownMenuItem
          disabled={controls.isLoadingMode || controls.isSettingMode}
          onSelect={controls.handleToggleLoadBalancing}
        >
          <Activity />
          {controls.isLoadingMode
            ? '负载均衡加载中'
            : controls.loadBalancingMode === 'priority'
              ? '切换到均衡负载'
              : '切换到优先级'}
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={controls.handleRefresh}>
          <RefreshCw />刷新数据
        </DropdownMenuItem>
        <DropdownMenuItem onSelect={controls.openImageUpdate}>
          <UploadCloud />镜像在线更新
        </DropdownMenuItem>
        <ThrottleCompactItems {...throttleProps} />
        <DropdownMenuLabel>密钥管理</DropdownMenuLabel>
        <KeyMenuItems onOpenKeyDialog={controls.openKeyDialog} />
      </DropdownMenuContent>
    </DropdownMenu>
  )
}

function LoadBalancingButton({ controls }: { controls: ToolControls }) {
  return (
    <Button
      variant="outline"
      size="sm"
      onClick={controls.handleToggleLoadBalancing}
      disabled={controls.isLoadingMode || controls.isSettingMode}
      title="切换负载均衡模式"
    >
      <Activity className="h-3.5 w-3.5" />
      <span className="hidden md:inline">
        {controls.isLoadingMode
          ? '加载中…'
          : controls.loadBalancingMode === 'priority'
            ? '优先级'
            : '均衡负载'}
      </span>
    </Button>
  )
}

function RefreshButton({ onRefresh }: { onRefresh: () => void }) {
  return (
    <Button variant="ghost" size="icon" onClick={onRefresh} title="刷新">
      <RefreshCw className="h-4 w-4" />
    </Button>
  )
}

function ImageUpdateButton({ controls }: { controls: ToolControls }) {
  return (
    <Button
      variant="ghost"
      size="icon"
      onClick={controls.openImageUpdate}
      title={imageUpdateTitle(controls.updateCheck)}
      className="relative"
    >
      <UploadCloud className="h-4 w-4" />
      {controls.updateCheck?.hasUpdate && <UpdateDot />}
    </Button>
  )
}

function imageUpdateTitle(updateCheck: ToolControls['updateCheck']) {
  if (!updateCheck?.hasUpdate) return '镜像在线更新'
  return `发现新版本 v${updateCheck.latestVersion}（当前 v${updateCheck.currentVersion}）`
}

function UpdateDot() {
  return (
    <span className="absolute right-1 top-1 inline-flex h-2 w-2 items-center justify-center">
      <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-red-400 opacity-75" />
      <span className="relative inline-flex h-2 w-2 rounded-full bg-red-500" />
    </span>
  )
}

function KeySettingsMenu({
  onOpenKeyDialog,
}: {
  onOpenKeyDialog: (mode: 'admin' | 'api') => void
}) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" size="icon" title="设置">
          <Settings className="h-4 w-4" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        <DropdownMenuLabel>密钥管理</DropdownMenuLabel>
        <KeyMenuItems onOpenKeyDialog={onOpenKeyDialog} />
      </DropdownMenuContent>
    </DropdownMenu>
  )
}

function KeyMenuItems({
  onOpenKeyDialog,
}: {
  onOpenKeyDialog: (mode: 'admin' | 'api') => void
}) {
  return (
    <>
      <DropdownMenuItem onSelect={() => onOpenKeyDialog('admin')}>
        <Key />修改登录API密钥（管理面板登录）
      </DropdownMenuItem>
      <DropdownMenuItem onSelect={() => onOpenKeyDialog('api')}>
        <Key />修改管理员API密钥（客户端 /v1 调用）
      </DropdownMenuItem>
    </>
  )
}

interface ThrottleConfigButtonProps {
  config?: { failover: boolean; cooldownSecs: number }
  loading: boolean
  saving: boolean
  onToggleFailover: () => void
  onChangeCooldown: (secs: number) => void
}

interface ThrottleState {
  cooldownMin: number
  cooldownSecs: number
  failover: boolean
}

interface CustomCooldownFormProps {
  cooldownMin: number
  customMin: string
  disabled: boolean
  onCustomMinChange: (value: string) => void
  onSubmit: (e: React.FormEvent) => void
}

interface ThrottleTriggerProps extends ComponentPropsWithoutRef<typeof Button> {
  loading: boolean
  saving: boolean
  state: ThrottleState
}

const COOLDOWN_PRESETS = [
  { label: '5 分钟', secs: 5 * 60 },
  { label: '15 分钟', secs: 15 * 60 },
  { label: '30 分钟', secs: 30 * 60 },
  { label: '1 小时', secs: 60 * 60 },
  { label: '2 小时', secs: 2 * 60 * 60 },
]

const DEFAULT_COOLDOWN_SECS = 30 * 60
const SECONDS_PER_MINUTE = 60
const MIN_CUSTOM_COOLDOWN_MINUTES = 1
const MAX_CUSTOM_COOLDOWN_MINUTES = 1440

/**
 * 故障转移开关 + 冷却时长设置（紧凑下拉）
 *
 * 主按钮文案显示当前状态；下拉里:
 * - 顶部一个 Switch 切换 failover
 * - 5 个预设时长 + 一个自定义输入（分钟）
 */
function ThrottleConfigButton({
  config, loading, saving, onToggleFailover, onChangeCooldown,
}: ThrottleConfigButtonProps) {
  const [open, setOpen] = useState(false)
  const [customMin, setCustomMin] = useState('')
  const state = readThrottleState(config)

  useEffect(() => {
    if (!open) setCustomMin('')
  }, [open])

  const submitCustom = (e: React.FormEvent) => {
    e.preventDefault()
    const min = parseInt(customMin, 10)
    if (invalidCooldownMinutes(min)) {
      toast.error('请输入 1-1440 之间的分钟数')
      return
    }
    onChangeCooldown(min * SECONDS_PER_MINUTE)
    setOpen(false)
  }

  return (
    <DropdownMenu open={open} onOpenChange={setOpen}>
      <DropdownMenuTrigger asChild>
        <ThrottleTrigger loading={loading} saving={saving} state={state} />
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-64">
        <ThrottleStatusPanel
          saving={saving}
          state={state}
          onToggleFailover={onToggleFailover}
        />
        <ThrottleCooldownPanel
          customMin={customMin}
          saving={saving}
          state={state}
          onChangeCooldown={onChangeCooldown}
          onCustomMinChange={setCustomMin}
          onDone={() => setOpen(false)}
          onSubmitCustom={submitCustom}
        />
      </DropdownMenuContent>
    </DropdownMenu>
  )
}

const ThrottleTrigger = forwardRef<HTMLButtonElement, ThrottleTriggerProps>(
  function ThrottleTrigger({ loading, saving, state, ...props }, ref) {
    return (
      <Button
        {...props}
        ref={ref}
        variant="outline"
        size="sm"
        disabled={loading || saving}
        title={throttleTitle(loading, state)}
      >
        {state.failover ? (
          <ShieldCheck className="h-3.5 w-3.5 text-emerald-600" />
        ) : (
          <ShieldAlert className="h-3.5 w-3.5 text-amber-500" />
        )}
        <span className="hidden md:inline">
          {throttleTriggerText(loading, state)}
        </span>
      </Button>
    )
  },
)

function ThrottleStatusPanel({
  saving, state, onToggleFailover,
}: {
  saving: boolean
  state: ThrottleState
  onToggleFailover: () => void
}) {
  return (
    <>
      <DropdownMenuLabel>账号级风控故障转移</DropdownMenuLabel>
      <div className="px-2 pb-2">
        <div className="flex items-center justify-between gap-2 rounded-md bg-secondary/40 px-2.5 py-2">
          <ThrottleStatusText failover={state.failover} />
          <Switch
            checked={state.failover}
            disabled={saving}
            onCheckedChange={() => onToggleFailover()}
          />
        </div>
      </div>
    </>
  )
}

function ThrottleStatusText({ failover }: { failover: boolean }) {
  return (
    <div className="text-xs">
      <div className="font-medium text-foreground">
        {failover ? '开启' : '关闭'}
      </div>
      <div className="text-muted-foreground leading-snug">
        {failover
          ? '上游对当前账号触发临时限速时，自动冷却该凭据并切换到下一个可用凭据'
          : '上游对当前账号触发临时限速时，仅按瞬态错误重试，不切换凭据'}
      </div>
    </div>
  )
}

function ThrottleCooldownPanel({
  customMin, saving, state, onChangeCooldown, onCustomMinChange, onDone, onSubmitCustom,
}: {
  customMin: string
  saving: boolean
  state: ThrottleState
  onChangeCooldown: (secs: number) => void
  onCustomMinChange: (value: string) => void
  onDone?: () => void
  onSubmitCustom: (e: React.FormEvent) => void
}) {
  const disabled = saving || !state.failover

  return (
    <>
      <DropdownMenuLabel className="pt-1">冷却时长</DropdownMenuLabel>
      <div className={cooldownPanelClassName(state.failover)}>
        <CooldownPresetButtons
          cooldownSecs={state.cooldownSecs}
          disabled={disabled}
          onChangeCooldown={onChangeCooldown}
          onDone={onDone}
        />
        <CustomCooldownForm
          cooldownMin={state.cooldownMin}
          customMin={customMin}
          disabled={disabled}
          onCustomMinChange={onCustomMinChange}
          onSubmit={onSubmitCustom}
        />
      </div>
    </>
  )
}

function CustomCooldownForm({
  cooldownMin, customMin, disabled, onCustomMinChange, onSubmit,
}: CustomCooldownFormProps) {
  return (
    <form onSubmit={onSubmit} className="mt-2 flex items-center gap-1.5">
      <Input
        type="number"
        min={MIN_CUSTOM_COOLDOWN_MINUTES}
        max={MAX_CUSTOM_COOLDOWN_MINUTES}
        placeholder={`自定义（当前 ${cooldownMin}）`}
        value={customMin}
        onChange={(e) => onCustomMinChange(e.target.value)}
        disabled={disabled}
        className="h-7 text-xs"
      />
      <span className="text-xs text-muted-foreground">分钟</span>
      <Button
        type="submit"
        size="sm"
        variant="outline"
        className="h-7 text-xs"
        disabled={disabled || !customMin.trim()}
      >
        保存
      </Button>
    </form>
  )
}

function ThrottleCompactItems(props: ThrottleConfigButtonProps) {
  const { loading, saving, onToggleFailover, onChangeCooldown } = props
  const [customMin, setCustomMin] = useState('')
  const state = readThrottleState(props.config)
  const busy = loading || saving

  const submitCustom = (e: React.FormEvent) => {
    e.preventDefault()
    const min = parseInt(customMin, 10)
    if (invalidCooldownMinutes(min)) {
      toast.error('请输入 1-1440 之间的分钟数')
      return
    }
    onChangeCooldown(min * SECONDS_PER_MINUTE)
    setCustomMin('')
  }

  return (
    <>
      <DropdownMenuLabel>故障转移</DropdownMenuLabel>
      <DropdownMenuItem
        disabled={busy}
        onSelect={onToggleFailover}
      >
        {state.failover ? <ShieldCheck /> : <ShieldAlert />}
        {compactThrottleText(loading, state)}
      </DropdownMenuItem>
      <ThrottleCooldownPanel
        customMin={customMin}
        saving={busy}
        state={state}
        onChangeCooldown={onChangeCooldown}
        onCustomMinChange={setCustomMin}
        onSubmitCustom={submitCustom}
      />
    </>
  )
}

function CooldownPresetButtons({
  cooldownSecs, disabled, onChangeCooldown, onDone,
}: {
  cooldownSecs: number
  disabled: boolean
  onChangeCooldown: (secs: number) => void
  onDone?: () => void
}) {
  return (
    <div className="grid grid-cols-3 gap-1">
      {COOLDOWN_PRESETS.map((preset) => (
        <CooldownPresetButton
          key={preset.secs}
          active={preset.secs === cooldownSecs}
          disabled={disabled}
          label={preset.label}
          secs={preset.secs}
          onChangeCooldown={onChangeCooldown}
          onDone={onDone}
        />
      ))}
    </div>
  )
}

function CooldownPresetButton({
  active, disabled, label, secs, onChangeCooldown, onDone,
}: {
  active: boolean
  disabled: boolean
  label: string
  secs: number
  onChangeCooldown: (secs: number) => void
  onDone?: () => void
}) {
  return (
    <Button
      type="button"
      size="sm"
      variant={active ? 'default' : 'outline'}
      className="h-7 text-xs"
      disabled={disabled}
      onClick={() => {
        if (!active) onChangeCooldown(secs)
        onDone?.()
      }}
    >
      {label}
    </Button>
  )
}

function secondsToMinutes(seconds: number) {
  return Math.round(seconds / SECONDS_PER_MINUTE)
}

function readThrottleState(
  config: ThrottleConfigButtonProps['config'],
): ThrottleState {
  const cooldownSecs = config?.cooldownSecs ?? DEFAULT_COOLDOWN_SECS
  return {
    cooldownMin: secondsToMinutes(cooldownSecs),
    cooldownSecs,
    failover: config?.failover ?? true,
  }
}

function throttleTitle(loading: boolean, state: ThrottleState) {
  if (loading) return '加载中…'
  if (!state.failover) return '账号级风控故障转移：关闭'
  return `账号级风控故障转移：开启（冷却 ${state.cooldownMin} 分钟）`
}

function throttleTriggerText(loading: boolean, state: ThrottleState) {
  if (loading) return '加载中…'
  if (!state.failover) return '不切换'
  return `故障转移 · ${state.cooldownMin}m`
}

function compactThrottleText(loading: boolean, state: ThrottleState) {
  if (loading) return '故障转移加载中'
  if (!state.failover) return '开启故障转移'
  return `关闭故障转移 · ${state.cooldownMin}m`
}

function invalidCooldownMinutes(minutes: number) {
  return (
    Number.isNaN(minutes) ||
    minutes < MIN_CUSTOM_COOLDOWN_MINUTES ||
    minutes > MAX_CUSTOM_COOLDOWN_MINUTES
  )
}

function cooldownPanelClassName(failover: boolean) {
  return `px-2 pb-2 ${failover ? '' : 'opacity-60'}`
}
