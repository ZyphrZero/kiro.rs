import { useEffect, useMemo, useState } from 'react'
import { toast } from 'sonner'
import {
  Send,
  Plus,
  Trash2,
  RotateCcw,
  Save,
  Loader2,
  Info,
} from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { Badge } from '@/components/ui/badge'
import {
  SettingGroup,
  SettingRow,
  useFieldSaver,
} from '@/components/console/setting-row'
import {
  useGlobalProxy,
  useSetGlobalProxy,
  useCustomHeaders,
  useSetCustomHeaders,
} from '@/hooks/use-credentials'
import { maskProxyUrl, extractErrorMessage } from '@/lib/utils'
import { reportSaveError } from '@/components/settings/report-error'
import type { CustomHeaderItem } from '@/types/api'

/**
 * 全局出站代理协议
 */
const PROXY_SCHEMES = [
  'http://',
  'https://',
  'socks4://',
  'socks4a://',
  'socks5://',
  'socks5h://',
]

/**
 * 自定义响应头支持的运行时插值变量
 */
const AVAILABLE_VARIABLES: { tag: string; desc: string }[] = [
  { tag: '{trace_id}', desc: '链路追踪 ID' },
  { tag: '{request_id}', desc: '同 trace_id' },
  { tag: '{model}', desc: '请求模型名' },
  { tag: '{credential_id}', desc: '命中凭据 ID' },
  { tag: '{key_id}', desc: '客户端 Key ID' },
  { tag: '{group}', desc: '分组名' },
  { tag: '{client_ip}', desc: '客户端真实 IP' },
]

/**
 * 默认推荐的自定义响应头（开箱即用支持 NewAPI / OneAPI）
 */
const DEFAULT_HEADERS: CustomHeaderItem[] = [
  { key: 'X-Oneapi-Request-Id', value: '{trace_id}', enabled: true },
  { key: 'X-Request-Id', value: '{trace_id}', enabled: true },
]

function GlobalProxySetting() {
  const { data, isLoading } = useGlobalProxy()
  const { mutate } = useSetGlobalProxy()
  const saver = useFieldSaver(mutate, reportSaveError)
  const isPending = saver.isSaving('proxy')
  const saved = saver.isSaved('proxy')

  const currentUrl = data?.proxyUrl ?? null
  const currentUsername = data?.proxyUsername ?? null
  const currentPasswordSet = data?.proxyPasswordSet ?? false

  const [draftUrl, setDraftUrl] = useState('')
  const [draftUsername, setDraftUsername] = useState('')
  const [draftPassword, setDraftPassword] = useState('')

  useEffect(() => {
    setDraftUrl(currentUrl ?? '')
    setDraftUsername(currentUsername ?? '')
    setDraftPassword('')
  }, [currentUrl, currentUsername, currentPasswordSet])

  const buildPayload = () => {
    const url = draftUrl.trim() || null
    if (!url) return null
    return {
      proxyUrl: url,
      proxyUsername: draftUsername.trim() || null,
      ...(draftPassword ? { proxyPassword: draftPassword } : {}),
    }
  }

  const apply = () => {
    const url = draftUrl.trim()
    if (!url) {
      toast.error('代理地址不能为空。要停用请点「清除」。')
      return
    }
    if (!PROXY_SCHEMES.some((s) => url.toLowerCase().startsWith(s))) {
      toast.error(`代理地址需以 ${PROXY_SCHEMES.join(' / ')} 开头`)
      return
    }
    const payload = buildPayload()
    if (!payload) return
    saver.save('proxy', payload)
  }

  const clear = () => {
    setDraftUrl('')
    setDraftUsername('')
    setDraftPassword('')
    saver.save('proxy', { proxyUrl: null })
  }

  const hint = currentUrl
    ? `当前生效：${maskProxyUrl(currentUrl)}${currentUsername ? ` (${currentUsername})` : ''}`
    : '未配置，直连上游。支持 HTTP / HTTPS / SOCKS，可填写认证凭据'

  return (
    <SettingGroup
      title="全局出站代理"
      description="所有上游请求默认走这个代理；未绑定专属代理的凭据都受它影响"
    >
      <SettingRow label="代理地址" hint={hint} pending={isPending} saved={saved}>
        <div className="flex flex-wrap items-center gap-1.5">
          <Input
            value={draftUrl}
            onChange={(e) => setDraftUrl(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter') apply()
              if (e.key === 'Escape') {
                setDraftUrl(currentUrl ?? '')
                setDraftUsername(currentUsername ?? '')
                setDraftPassword('')
              }
            }}
            placeholder="socks5://host:1080"
            disabled={isLoading || isPending}
            spellCheck={false}
            autoComplete="off"
            className="console-num h-8 w-[min(20rem,60vw)] text-[12.5px]"
          />
          <Button
            size="sm"
            variant="outline"
            onClick={apply}
            disabled={isLoading || isPending || !draftUrl.trim()}
          >
            应用
          </Button>
          {currentUrl && (
            <Button
              size="sm"
              variant="ghost"
              onClick={clear}
              disabled={isPending}
              title="停用全局代理，恢复直连"
            >
              清除
            </Button>
          )}
        </div>
      </SettingRow>

      {(currentUrl || draftUrl.trim()) && (
        <>
          <SettingRow
            label="认证用户名"
            hint="选填，代理要求 Basic Auth 时填写"
            pending={isPending}
            saved={saved}
          >
            <Input
              value={draftUsername}
              onChange={(e) => setDraftUsername(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Escape') setDraftUsername(currentUsername ?? '')
              }}
              placeholder="留空则不认证"
              disabled={isLoading || isPending}
              spellCheck={false}
              autoComplete="off"
              className="console-num h-8 w-[min(16rem,50vw)] text-[12.5px]"
            />
          </SettingRow>
          <SettingRow
            label="认证密码"
            hint={currentPasswordSet ? '已配置密码；留空保存时保留现有密码' : '选填，与用户名配合使用'}
            pending={isPending}
            saved={saved}
          >
            <Input
              type="password"
              value={draftPassword}
              onChange={(e) => setDraftPassword(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Escape') setDraftPassword('')
              }}
              placeholder={currentPasswordSet ? '已配置，留空则保留' : '留空则不认证'}
              disabled={isLoading || isPending}
              spellCheck={false}
              autoComplete="new-password"
              className="console-num h-8 w-[min(16rem,50vw)] text-[12.5px]"
            />
            {currentPasswordSet && (
              <Button
                size="sm"
                variant="ghost"
                onClick={() => {
                  setDraftPassword('')
                  saver.save('proxy', {
                    proxyUrl: currentUrl,
                    proxyUsername: currentUsername,
                    proxyPassword: null,
                  })
                }}
                disabled={isLoading || isPending}
                title="清除已保存的代理密码"
              >
                清除密码
              </Button>
            )}
          </SettingRow>
        </>
      )}
    </SettingGroup>
  )
}

function CustomHeadersSetting() {
  const { data, isLoading } = useCustomHeaders()
  const { mutateAsync: saveHeaders, isPending: isSaving } = useSetCustomHeaders()

  const [drafts, setDrafts] = useState<CustomHeaderItem[]>([])
  const [activeInputIndex, setActiveInputIndex] = useState<number | null>(null)

  useEffect(() => {
    if (data?.headers) {
      setDrafts(data.headers.map((h) => ({ ...h })))
    }
  }, [data])

  const isDirty = useMemo(() => {
    if (!data?.headers) return drafts.length > 0
    if (drafts.length !== data.headers.length) return true
    return drafts.some((d, i) => {
      const orig = data.headers[i]
      return (
        d.key !== orig.key ||
        d.value !== orig.value ||
        d.enabled !== orig.enabled
      )
    })
  }, [drafts, data])

  const addRow = () => {
    if (drafts.length >= 50) {
      toast.error('最多支持配置 50 条自定义响应头')
      return
    }
    setDrafts((prev) => [
      ...prev,
      { key: '', value: '{trace_id}', enabled: true },
    ])
    setActiveInputIndex(drafts.length)
  }

  const removeRow = (index: number) => {
    setDrafts((prev) => prev.filter((_, i) => i !== index))
  }

  const updateRow = <K extends keyof CustomHeaderItem>(
    index: number,
    field: K,
    value: CustomHeaderItem[K],
  ) => {
    setDrafts((prev) => {
      const next = [...prev]
      next[index] = { ...next[index], [field]: value }
      return next
    })
  }

  const insertVariable = (tag: string) => {
    if (drafts.length === 0) {
      setDrafts([{ key: 'X-Oneapi-Request-Id', value: tag, enabled: true }])
      return
    }
    const targetIdx =
      activeInputIndex !== null && activeInputIndex < drafts.length
        ? activeInputIndex
        : drafts.length - 1
    const currentVal = drafts[targetIdx].value
    updateRow(targetIdx, 'value', currentVal ? `${currentVal}${tag}` : tag)
    toast.success(`已向第 ${targetIdx + 1} 行追加变量 ${tag}`)
  }

  const resetToDefault = () => {
    setDrafts(DEFAULT_HEADERS.map((h) => ({ ...h })))
    toast.info('已载入推荐默认响应头，点击「保存更改」生效')
  }

  const handleSave = async () => {
    const seen = new Set<string>()
    for (let i = 0; i < drafts.length; i++) {
      const row = drafts[i]
      const key = row.key.trim()
      if (!key) {
        toast.error(`第 ${i + 1} 条响应头的 Header Name 不能为空`)
        return
      }
      // HTTP Header Name 标准校验
      if (!/^[a-zA-Z0-9!#$%&'*+-.^_`|~]+$/.test(key)) {
        toast.error(`第 ${i + 1} 条响应头名称 "${key}" 格式不合法（不能含空格或特殊符号）`)
        return
      }
      if (row.value.includes('\r') || row.value.includes('\n')) {
        toast.error(`第 ${i + 1} 条响应头的值不能包含换行符`)
        return
      }
      const lower = key.toLowerCase()
      if (seen.has(lower)) {
        toast.error(`响应头名称 "${key}" 存在重复（不区分大小写）`)
        return
      }
      seen.add(lower)
    }

    try {
      await saveHeaders({
        headers: drafts.map((d) => ({
          key: d.key.trim(),
          value: d.value.trim(),
          enabled: d.enabled,
        })),
      })
      toast.success('自定义响应头配置已保存并即时生效')
    } catch (err) {
      toast.error(`保存失败: ${extractErrorMessage(err)}`)
    }
  }

  const enabledCount = drafts.filter((d) => d.enabled).length

  return (
    <SettingGroup
      title="自定义 HTTP 响应头"
      description="向所有下游 HTTP 响应（包含流式 SSE、非流式 JSON 及异常错误）注入自定义 Header。支持多响应头、启用停用与动态变量插值，修改后即时生效。"
    >
      <div className="space-y-4 py-1">
        {/* 概览与快捷操作 */}
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div className="flex items-center gap-2 text-[12px] text-muted-foreground">
            <Send className="h-4 w-4 text-primary" />
            <Badge variant="outline" className="console-num text-[11px]">
              {enabledCount} / {drafts.length} 启用
            </Badge>
          </div>
          <Button
            size="sm"
            variant="outline"
            onClick={resetToDefault}
            disabled={isLoading || isSaving}
            title="恢复推荐的 NewAPI / OneAPI 默认响应头配置"
            className="h-8 text-xs"
          >
            <RotateCcw className="mr-1.5 h-3.5 w-3.5" />
            恢复默认
          </Button>
        </div>

        {/* 表格视图（桌面端） */}
        {drafts.length > 0 ? (
          <div className="rounded-lg border border-border/70 overflow-hidden bg-card/50">
            <div className="hidden md:grid grid-cols-[60px_1fr_1.5fr_44px] items-center gap-3 border-b border-border/60 bg-muted/40 px-4 py-2 text-[11.5px] font-semibold text-muted-foreground uppercase tracking-wider">
              <div>启用</div>
              <div>Header Name（响应头键名）</div>
              <div>Header Value（取值 / 支持变量插值）</div>
              <div className="text-right">操作</div>
            </div>

            <div className="divide-y divide-border/40">
              {drafts.map((row, i) => (
                <div
                  key={i}
                  className="flex flex-col gap-2 p-3 md:grid md:grid-cols-[60px_1fr_1.5fr_44px] md:items-center md:gap-3 md:px-4 md:py-2.5 hover:bg-muted/20 transition-colors"
                >
                  <div className="flex items-center justify-between md:justify-start">
                    <span className="text-xs text-muted-foreground md:hidden font-medium">启用状态</span>
                    <Switch
                      checked={row.enabled}
                      onCheckedChange={(val) => updateRow(i, 'enabled', val)}
                      disabled={isSaving}
                      aria-label={`启用第 ${i + 1} 条响应头`}
                    />
                  </div>

                  <div className="space-y-1 md:space-y-0">
                    <span className="text-[11px] text-muted-foreground md:hidden font-medium">Header Name</span>
                    <Input
                      value={row.key}
                      onChange={(e) => updateRow(i, 'key', e.target.value)}
                      placeholder="如 X-Oneapi-Request-Id"
                      disabled={isSaving}
                      spellCheck={false}
                      className="console-num h-8 text-[12.5px]"
                    />
                  </div>

                  <div className="space-y-1 md:space-y-0">
                    <span className="text-[11px] text-muted-foreground md:hidden font-medium">Header Value</span>
                    <Input
                      value={row.value}
                      onChange={(e) => updateRow(i, 'value', e.target.value)}
                      onFocus={() => setActiveInputIndex(i)}
                      placeholder="如 {trace_id}"
                      disabled={isSaving}
                      spellCheck={false}
                      className="console-num h-8 text-[12.5px]"
                    />
                  </div>

                  <div className="flex justify-end">
                    <Button
                      size="icon"
                      variant="ghost"
                      onClick={() => removeRow(i)}
                      disabled={isSaving}
                      title="删除此响应头"
                      className="h-8 w-8 text-muted-foreground hover:text-destructive"
                    >
                      <Trash2 className="h-4 w-4" />
                    </Button>
                  </div>
                </div>
              ))}
            </div>
          </div>
        ) : (
          <div className="rounded-lg border border-dashed border-border/80 p-8 text-center">
            <p className="text-sm text-muted-foreground">当前暂无自定义响应头规则</p>
            <p className="mt-1 text-xs text-muted-foreground/80">
              默认会下发 <code className="rounded bg-muted px-1 py-0.5 font-mono text-[11px]">X-Oneapi-Request-Id</code> 与 <code className="rounded bg-muted px-1 py-0.5 font-mono text-[11px]">X-Request-Id</code>。点击下方按钮添加。
            </p>
          </div>
        )}

        {/* 变量插值快捷选择 */}
        <div className="rounded-lg border border-border/50 bg-muted/20 p-3 space-y-2">
          <div className="text-[12px] font-medium text-foreground flex items-center gap-1.5">
            <span>常用变量插值（点击追加到正在编辑的行）：</span>
          </div>
          <div className="flex flex-wrap gap-1.5">
            {AVAILABLE_VARIABLES.map((item) => (
              <button
                key={item.tag}
                type="button"
                onClick={() => insertVariable(item.tag)}
                title={`点击插入 ${item.tag} (${item.desc})`}
                className="inline-flex items-center gap-1 rounded border border-border/70 bg-card px-2 py-1 font-mono text-[11.5px] transition-colors hover:border-primary/50 hover:bg-primary/5 cursor-pointer"
              >
                <span className="font-semibold text-primary">{item.tag}</span>
                <span className="text-[11px] text-muted-foreground">· {item.desc}</span>
              </button>
            ))}
          </div>
        </div>

        {/* NewAPI / OneAPI 规范提示 */}
        <div className="rounded-lg border border-primary/20 bg-primary/5 p-3 text-[12px] leading-relaxed text-muted-foreground">
          <div className="flex items-center gap-1.5 font-medium text-foreground">
            <Info className="h-4 w-4 text-primary shrink-0" />
            <span>NewAPI / OneAPI 链路关联机制</span>
          </div>
          <div className="mt-1">
            NewAPI 与 OneAPI 会自动在上游响应头中提取 <code className="rounded bg-muted px-1 py-0.5 font-mono text-[11px] text-foreground">X-Oneapi-Request-Id</code> 作为其内部的渠道请求 ID（upstream_request_id）。保留该响应头并将值设为 <code className="rounded bg-muted px-1 py-0.5 font-mono text-[11px] text-foreground">&#123;trace_id&#125;</code>，可在 NewAPI 的消费日志中精准关联与追踪 kiro-rs 侧的详细诊断日志。
          </div>
        </div>

        {/* 底部操作栏 */}
        <div className="flex flex-wrap items-center justify-between gap-2 pt-2 border-t border-border/40">
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={addRow}
            disabled={isSaving || drafts.length >= 50}
            className="h-8 text-xs"
          >
            <Plus className="mr-1.5 h-3.5 w-3.5" />
            添加响应头
          </Button>

          <div className="flex items-center gap-2">
            {isDirty && (
              <span className="text-xs text-amber-500 font-medium">
                有未保存的更改
              </span>
            )}
            <Button
              type="button"
              size="sm"
              onClick={handleSave}
              disabled={isSaving || !isDirty}
              className="h-8 text-xs min-w-[80px]"
            >
              {isSaving ? (
                <>
                  <Loader2 className="mr-1.5 h-3.5 w-3.5 animate-spin" />
                  保存中
                </>
              ) : (
                <>
                  <Save className="mr-1.5 h-3.5 w-3.5" />
                  保存更改
                </>
              )}
            </Button>
          </div>
        </div>
      </div>
    </SettingGroup>
  )
}

export function NetworkSection() {
  return (
    <div className="space-y-6">
      <GlobalProxySetting />
      <CustomHeadersSetting />
    </div>
  )
}
