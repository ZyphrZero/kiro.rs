import { useState, useEffect } from 'react'
import { toast } from 'sonner'
import { useQuery } from '@tanstack/react-query'
import { Tags, Settings2 } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import {
  Select,
  SelectGroup,
  SelectLabel,
  SelectTrigger,
  SelectValue,
  SelectContent,
  SelectItem,
} from '@/components/ui/select'
import { Input } from '@/components/ui/input'
import { useUpdateCredential } from '@/hooks/use-credentials'
import { useGroupOptions } from '@/hooks/use-groups'
import { getProxyPool } from '@/api/credentials'
import { extractErrorMessage, maskProxyUrl, cn } from '@/lib/utils'
import { GroupMultiSelect } from '@/components/group-select'
import { CredentialMetadataEditor, metadataDefaults } from '@/components/credential-metadata-field'
import type { CredentialMetadata, CredentialMetadataSchema, CredentialStatusItem } from '@/types/api'

type TabKey = 'general' | 'metadata'

const TABS: { key: TabKey; label: string; icon: React.ReactNode }[] = [
  { key: 'general', label: '基本', icon: <Settings2 className="h-3.5 w-3.5" /> },
  { key: 'metadata', label: 'Metadata', icon: <Tags className="h-3.5 w-3.5" /> },
]

interface EditCredentialDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  credential: CredentialStatusItem
  metadataSchema?: CredentialMetadataSchema
}

/** 状态接口的 metadata 带展示信息；编辑时只回填其中的实际值。 */
function metadataValues(metadata: CredentialStatusItem['metadata']): Partial<CredentialMetadata> {
  return Object.fromEntries(
    Object.entries(metadata ?? {}).map(([key, detail]) => [key, detail.value]),
  ) as Partial<CredentialMetadata>
}

export function EditCredentialDialog({
  open,
  onOpenChange,
  credential,
  metadataSchema,
}: EditCredentialDialogProps) {
  const [activeTab, setActiveTab] = useState<TabKey>('general')
  const [email, setEmail] = useState(credential.email ?? '')
  const [proxyUrl, setProxyUrl] = useState(credential.proxyUrl ?? '')
  const [proxyUsername, setProxyUsername] = useState('')
  const [proxyPassword, setProxyPassword] = useState('')
  const [groups, setGroups] = useState<string[]>(credential.groups ?? [])
  const [sourceChannel, setSourceChannel] = useState(credential.sourceChannel ?? '')
  const [loadFactor, setLoadFactor] = useState<number>(credential.loadFactor ?? 10)
  const [metadata, setMetadata] = useState<CredentialMetadata>(
    { ...metadataDefaults(metadataSchema), ...metadataValues(credential.metadata) },
  )
  const [manualMode, setManualMode] = useState(false)

  const groupOptions = useGroupOptions()

  const { data: proxyPool } = useQuery({
    queryKey: ['proxy-pool'],
    queryFn: getProxyPool,
    enabled: open,
  })

  // 每次打开时重置表单为当前凭据值
  useEffect(() => {
    if (open) {
      setActiveTab('general')
      setEmail(credential.email ?? '')
      setProxyUrl(credential.proxyUrl ?? '')
      setProxyUsername('')
      setProxyPassword('')
      setGroups(credential.groups ?? [])
      setSourceChannel(credential.sourceChannel ?? '')
      setLoadFactor(credential.loadFactor ?? 10)
      setMetadata({
        ...metadataDefaults(metadataSchema),
        ...metadataValues(credential.metadata),
      })
      setManualMode(false)
    }
  }, [open, credential, metadataSchema])

  const { mutate, isPending } = useUpdateCredential()

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault()

    mutate(
      {
        id: credential.id,
        req: {
          email: email,
          proxyUrl: proxyUrl,
          proxyUsername: proxyUsername || undefined,
          proxyPassword: proxyPassword || undefined,
          groups: groups,
          sourceChannel: sourceChannel,
          loadFactor:
            Number.isFinite(loadFactor) && loadFactor >= 1 ? loadFactor : 10,
          metadata,
        },
      },
      {
        onSuccess: (data) => {
          toast.success(data.message)
          onOpenChange(false)
        },
        onError: (error: unknown) => {
          toast.error(`更新失败: ${extractErrorMessage(error)}`)
        },
      }
    )
  }

  const enabledProxies = proxyPool?.proxies.filter(p => p.enabled) ?? []

  // 当前 proxyUrl 是否是自定义值（不匹配任何标准选项）
  const isCustomUrl = proxyUrl !== '' && proxyUrl !== 'direct' &&
    !enabledProxies.some(p => p.url === proxyUrl)

  // 显示手动输入框：明确进入手动模式，或当前值就是自定义值
  const showManualInput = manualMode || isCustomUrl

  const selectValue = showManualInput ? '__custom__' : proxyUrl

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>
            编辑凭据 #{credential.id}
          </DialogTitle>
          <DialogDescription>
            修改凭据标识、分组、Metadata 与代理配置。
          </DialogDescription>
        </DialogHeader>

        {/* 标签导航 */}
        <nav className="-mx-1 flex gap-0.5" aria-label="编辑分区">
          {TABS.map((tab) => (
            <button
              key={tab.key}
              type="button"
              onClick={() => setActiveTab(tab.key)}
              className={cn(
                'inline-flex items-center gap-1.5 rounded-md px-3 py-1.5 text-[13px] transition-colors',
                'focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40',
                activeTab === tab.key
                  ? 'bg-primary/12 font-medium text-foreground'
                  : 'text-muted-foreground hover:bg-accent/60 hover:text-foreground',
              )}
            >
              {tab.icon}
              {tab.label}
            </button>
          ))}
        </nav>

        <form onSubmit={handleSubmit}>
          <div className="min-h-[280px] space-y-4 py-2">
            {activeTab === 'general' && (
              <>
                {/* 邮箱 */}
                <div className="space-y-2">
                  <label htmlFor="email" className="text-sm font-medium">
                    邮箱（用于显示标识）
                  </label>
                  <Input
                    id="email"
                    type="email"
                    placeholder="例: user@example.com"
                    value={email}
                    onChange={(e) => setEmail(e.target.value)}
                    disabled={isPending}
                  />
                  <p className="text-xs text-muted-foreground">
                    留空则显示凭据 ID，清除请提交空值
                  </p>
                </div>

                {/* 账号分组 */}
                <div className="space-y-2">
                  <label className="text-sm font-medium">账号分组</label>
                  <GroupMultiSelect
                    value={groups}
                    options={groupOptions}
                    onChange={setGroups}
                    disabled={isPending}
                  />
                  <p className="text-xs text-muted-foreground">
                    绑定了某分组的客户端 Key 只会调度到含该分组的账号。不选表示不属于任何分组。
                  </p>
                </div>

                {/* 账号来源渠道 */}
                <div className="space-y-2">
                  <label htmlFor="sourceChannel" className="text-sm font-medium">
                    账号来源渠道（备注）
                  </label>
                  <Input
                    id="sourceChannel"
                    placeholder="例: 官方, 转售商A, 采购平台X"
                    value={sourceChannel}
                    onChange={(e) => setSourceChannel(e.target.value)}
                    disabled={isPending}
                  />
                  <p className="text-xs text-muted-foreground">
                    纯备注，标记此账号的购买来源/渠道，便于追踪。留空表示清除。
                  </p>
                </div>

                {/* 负载因子（均衡模式下的调度权重） */}
                <div className="space-y-2">
                  <label htmlFor="loadFactor" className="text-sm font-medium">
                    负载因子（调度权重）
                  </label>
                  <Input
                    id="loadFactor"
                    type="number"
                    min="1"
                    placeholder="默认 10"
                    value={loadFactor}
                    onChange={(e) =>
                      setLoadFactor(Math.max(1, parseInt(e.target.value, 10) || 10))
                    }
                    disabled={isPending}
                  />
                  <p className="text-xs text-muted-foreground">
                    均衡负载模式下按该权重平滑轮询，数值越大被调度频次越高，默认为 10。
                  </p>
                </div>

                {/* 代理配置 */}
                <div className="space-y-2">
                  <label className="text-sm font-medium">代理配置</label>

                  {/* 下拉选择代理 */}
                  <Select
                    value={selectValue === '' ? '__global__' : selectValue}
                    onValueChange={(val) => {
                      if (val === '__custom__') {
                        setManualMode(true)
                      } else {
                        setManualMode(false)
                        setProxyUrl(val === '__global__' ? '' : val)
                      }
                    }}
                    disabled={isPending}
                  >
                    <SelectTrigger className="h-10 rounded-xl px-3.5">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="__global__">使用全局代理配置</SelectItem>
                      <SelectItem value="direct">直连（不使用代理）</SelectItem>
                      {enabledProxies.length > 0 && (
                        <SelectGroup>
                          <SelectLabel>代理池</SelectLabel>
                          {enabledProxies.map((p) => (
                            <SelectItem key={p.id} value={p.url}>
                              {p.label ? `${p.label} | ${maskProxyUrl(p.url)}` : maskProxyUrl(p.url)}
                            </SelectItem>
                          ))}
                        </SelectGroup>
                      )}
                      <SelectItem value="__custom__">手动输入...</SelectItem>
                    </SelectContent>
                  </Select>

                  {/* 自定义 URL 手动输入框 */}
                  {showManualInput && (
                    <Input
                      placeholder="自定义代理 URL（如 socks5://user:pass@host:port）"
                      value={proxyUrl}
                      onChange={(e) => setProxyUrl(e.target.value)}
                      disabled={isPending}
                      className="font-mono text-sm"
                    />
                  )}

                  {/* 代理认证（仅在需要时显示） */}
                  <div className="grid grid-cols-2 gap-2">
                    <Input
                      id="proxyUsername"
                      placeholder="代理用户名（留空不修改）"
                      value={proxyUsername}
                      onChange={(e) => setProxyUsername(e.target.value)}
                      disabled={isPending}
                    />
                    <Input
                      id="proxyPassword"
                      type="password"
                      placeholder="代理密码（留空不修改）"
                      value={proxyPassword}
                      onChange={(e) => setProxyPassword(e.target.value)}
                      disabled={isPending}
                    />
                  </div>
                  <p className="text-xs text-muted-foreground">
                    用户名/密码留空表示不修改；代理 URL 已包含凭据时无需填写
                  </p>
                </div>
              </>
            )}

            {activeTab === 'metadata' && (
              <CredentialMetadataEditor
                schema={metadataSchema}
                value={metadata}
                onChange={setMetadata}
                disabled={isPending}
              />
            )}
          </div>

          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => onOpenChange(false)}
              disabled={isPending}
            >
              取消
            </Button>
            <Button type="submit" disabled={isPending}>
              {isPending ? '保存中...' : '保存'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
