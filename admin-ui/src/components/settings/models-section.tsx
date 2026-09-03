import { useEffect, useMemo, useState } from 'react'
import { toast } from 'sonner'
import { Cpu, Bot, Sparkles, Wrench, Plus, Trash2, Save, Loader2, AlertCircle } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { SettingGroup, SettingSwitch, SettingNumber, useFieldSaver } from '@/components/console/setting-row'
import {
  useCustomModels,
  useSetCustomModels,
  useCurrentCredentialModels,
  useCacheMeteringConfig,
  useSetCacheMeteringConfig,
} from '@/hooks/use-credentials'
import { reportSaveError } from '@/components/settings/report-error'
import { extractErrorMessage, cn, parseError } from '@/lib/utils'
import type { CustomModelItem, AvailableModelItem } from '@/types/api'

// ──── 厂商定义 ────────────────────────────────────────────────────────────

type VendorKey = 'anthropic' | 'openai' | 'kiro' | 'custom'

const VENDORS: { key: VendorKey; label: string; icon: React.ReactNode }[] = [
  { key: 'anthropic', label: 'Anthropic', icon: <Bot className="h-3.5 w-3.5" /> },
  { key: 'openai', label: 'OpenAI', icon: <Sparkles className="h-3.5 w-3.5" /> },
  { key: 'kiro', label: 'Kiro', icon: <Cpu className="h-3.5 w-3.5" /> },
  { key: 'custom', label: '自定义', icon: <Wrench className="h-3.5 w-3.5" /> },
]

/** 客户端推断模型厂商（与后端 infer_model_owner 保持一致） */
function vendorFor(modelId: string): VendorKey {
  const id = modelId.toLowerCase()
  if (id.startsWith('claude-')) return 'anthropic'
  if (id.startsWith('gpt-') || id.startsWith('chatgpt-') || id.startsWith('o1-') || id.startsWith('o3-') || id.startsWith('o4-')) return 'openai'
  return 'kiro'
}

/** 空自定义模型模板 */
function emptyModel(): CustomModelItem {
  return {
    id: '',
    backendId: '',
    displayName: undefined,
    contextWindow: undefined,
    maxTokens: undefined,
    supportsReasoning: false,
    ownedBy: undefined,
  }
}

// ──── 模型显示行（上游只读） ──────────────────────────────────────────────

function UpstreamModelRow({ model }: { model: AvailableModelItem }) {
  return (
    <div className="rounded-md border border-border/60 px-3 py-2.5">
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0 flex-1">
          <div className="text-sm font-medium">
            {model.modelName || model.modelId}
          </div>
          {model.modelName && model.modelName !== model.modelId && (
            <div className="mt-0.5 break-all font-mono text-[11px] text-muted-foreground">
              {model.modelId}
            </div>
          )}
          {model.description && (
            <div className="mt-1.5 text-xs leading-relaxed text-muted-foreground">
              {model.description}
            </div>
          )}
        </div>
        {(model.maxInputTokens != null || model.maxOutputTokens != null) && (
          <div className="flex shrink-0 flex-wrap gap-1 self-start text-[11px] tabular-nums text-muted-foreground">
            {model.maxInputTokens != null && <span>输入 {model.maxInputTokens.toLocaleString()}</span>}
            {model.maxOutputTokens != null && <span>输出 {model.maxOutputTokens.toLocaleString()}</span>}
          </div>
        )}
      </div>
    </div>
  )
}

// ──── 主组件 ──────────────────────────────────────────────────────────────

export function ModelsSection() {
  const [vendor, setVendor] = useState<VendorKey>('anthropic')

  // Prompt Cache 计量模拟开关
  const { data: cacheMetering, isLoading: cacheLoading } = useCacheMeteringConfig()
  const { mutate: mutateCacheMetering } = useSetCacheMeteringConfig()
  const cacheSaver = useFieldSaver(mutateCacheMetering, reportSaveError)
  const cacheMeteringEnabled = cacheMetering?.enabled ?? true
  const cacheDiscountRatio = cacheMetering?.effectiveDiscountRatio ?? 0.6

  // 上游模型
  const upstreamQuery = useCurrentCredentialModels(vendor !== 'custom')

  // 自定义模型
  const { data: customData, isLoading: customLoading } = useCustomModels()
  const { mutate: saveCustom, isPending: customSaving } = useSetCustomModels()

  const [drafts, setDrafts] = useState<CustomModelItem[]>([])
  const [dirty, setDirty] = useState(false)

  // 同步自定义模型草稿
  useEffect(() => {
    if (!customData?.models) return
    setDrafts(customData.models.map((m) => ({ ...m })))
    setDirty(false)
  }, [customData?.models])

  // 按厂商分组上游模型
  const vendorGroups = useMemo(() => {
    const groups: Record<Exclude<VendorKey, 'custom'>, AvailableModelItem[]> = {
      anthropic: [],
      openai: [],
      kiro: [],
    }
    for (const m of upstreamQuery.data?.models ?? []) {
      const v = vendorFor(m.modelId)
      if (v !== 'custom') groups[v].push(m)
    }
    return groups
  }, [upstreamQuery.data])

  const upstreamModels = vendor !== 'custom' ? vendorGroups[vendor] ?? [] : []
  const upstreamError = upstreamQuery.error

  // ── 自定义模型编辑 ────────────────────────────────────────────────────

  const markDirty = () => setDirty(true)

  const updateField = <K extends keyof CustomModelItem>(
    index: number,
    field: K,
    value: CustomModelItem[K],
  ) => {
    setDrafts((prev) => {
      const next = [...prev]
      next[index] = { ...next[index], [field]: value }
      return next
    })
    markDirty()
  }

  const addRow = () => {
    setDrafts((prev) => [...prev, emptyModel()])
    markDirty()
  }

  const removeRow = (index: number) => {
    setDrafts((prev) => prev.filter((_, i) => i !== index))
    markDirty()
  }

  const apply = () => {
    for (let i = 0; i < drafts.length; i++) {
      const m = drafts[i]
      if (!m.id.trim()) {
        toast.error(`第 ${i + 1} 条模型的 id 不能为空`)
        return
      }
      if (!m.backendId.trim()) {
        toast.error(`第 ${i + 1} 条模型（${m.id || '未命名'}）的 backendId 不能为空`)
        return
      }
    }
    const clean = drafts.map((m) => ({
      ...m,
      displayName: m.displayName?.trim() || undefined,
      ownedBy: m.ownedBy?.trim() || undefined,
      contextWindow: m.contextWindow ?? undefined,
      maxTokens: m.maxTokens ?? undefined,
    }))
    saveCustom(
      { models: clean },
      {
        onSuccess: () => {
          setDirty(false)
          toast.success(`已保存 ${clean.length} 条自定义模型`)
        },
        onError: (err) => {
          toast.error('保存失败：' + extractErrorMessage(err))
        },
      },
    )
  }

  const customCount = drafts.length

  return (
    <div className="space-y-6">
      <SettingGroup
        title="Prompt Cache 提示词缓存"
        description="控制向客户端返回的用量中是否包含 Anthropic 格式的缓存计量与下游计费对齐"
      >
        <SettingSwitch
          label="模拟 prompt cache 计量"
          hint={
            cacheMeteringEnabled
              ? '按客户端声明的 cache_control 断点估算缓存读取与创建；终端客户端（如 Claude Code、Cursor）可正常识别缓存命中'
              : '输入 Token 全额计入 input tokens，缓存两项恒为 0；彻底关闭缓存计量模拟'
          }
          checked={cacheMeteringEnabled}
          onChange={(next) => cacheSaver.save('cacheMetering', { enabled: next })}
          pending={cacheSaver.isSaving('cacheMetering')}
          saved={cacheSaver.isSaved('cacheMetering')}
          disabled={cacheLoading}
        />
        {cacheMeteringEnabled && (
          <SettingNumber
            label="实际成本折算率（对齐下游计费）"
            hint="下游中转站（如 NewAPI）见到缓存读取会自动强制按 1 折（0.1x）扣费。由于上游长对话平均成本约为 6 折（0.6x），本程序会自动将多出的缓存读等价转换为普通输入，使得下游按 0.1x 计算后的总扣费精确等于设定成本，防止站长倒贴亏损。"
            value={cacheDiscountRatio}
            min={10}
            max={100}
            unit="%"
            toDisplay={(v) => Math.round(v * 100)}
            fromDisplay={(v) => Number((v / 100).toFixed(2))}
            presets={[10, 60, 100]}
            onCommit={(next) =>
              cacheSaver.save('discountRatio', { effectiveDiscountRatio: next })
            }
            pending={cacheSaver.isSaving('discountRatio')}
            saved={cacheSaver.isSaved('discountRatio')}
            disabled={cacheLoading}
          />
        )}
      </SettingGroup>

      <SettingGroup
        title="可用模型"
        description="按厂商查看上游可用模型，或自定义模型别名与元数据。"
      >
        {/* 厂商 Tab */}
        <nav className="mb-3 flex flex-wrap gap-0.5" aria-label="模型厂商">
          {VENDORS.map((v) => (
            <button
              key={v.key}
              type="button"
              onClick={() => setVendor(v.key)}
              className={cn(
                'inline-flex items-center gap-1.5 rounded-md px-3 py-1.5 text-[13px] transition-colors',
                'focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/40',
                vendor === v.key
                  ? 'bg-primary/12 font-medium text-foreground'
                  : 'text-muted-foreground hover:bg-accent/60 hover:text-foreground',
              )}
            >
              {v.icon}
              {v.label}
              {v.key !== 'custom' && vendorGroups[v.key] && (
                <span className="ml-0.5 text-[11px] text-muted-foreground">
                  {vendorGroups[v.key].length}
                </span>
              )}
            </button>
          ))}
        </nav>

        {/* ── 上游模型 Tab ────────────────────────────────────────────── */}
        {vendor !== 'custom' && (
          <div className="min-h-[120px]">
            {upstreamQuery.isLoading && !upstreamQuery.data && (
              <div className="flex items-center justify-center py-10 text-muted-foreground">
                <Loader2 className="h-6 w-6 animate-spin" />
                <span className="ml-2 text-sm">正在查询上游模型…</span>
              </div>
            )}

            {upstreamError && <UpstreamError error={upstreamError} />}

            {!upstreamQuery.isLoading && upstreamModels.length === 0 && (
              <p className="py-10 text-center text-sm text-muted-foreground">
                {vendor === 'anthropic' ? '暂无 Anthropic 模型'
                  : vendor === 'openai' ? '暂无 OpenAI 模型'
                  : '暂无 Kiro 模型'}
              </p>
            )}

            {upstreamModels.length > 0 && (
              <div className="space-y-2">
                {upstreamModels.map((m) => (
                  <UpstreamModelRow key={m.modelId} model={m} />
                ))}
              </div>
            )}
          </div>
        )}

        {/* ── 自定义模型 Tab ───────────────────────────────────────────── */}
        {vendor === 'custom' && (
          <>
            <div className="mb-3 flex flex-wrap items-center justify-between gap-2">
              <span className="text-xs text-muted-foreground">
                {customCount > 0 ? `${customCount} 条自定义模型` : '暂无自定义模型'}
              </span>
              <div className="flex items-center gap-2">
                <Button
                  size="sm"
                  variant="outline"
                  onClick={addRow}
                  disabled={customSaving}
                >
                  <Plus className="mr-1 h-3.5 w-3.5" />
                  添加模型
                </Button>
                <Button
                  size="sm"
                  onClick={apply}
                  disabled={customLoading || customSaving || !dirty}
                >
                  <Save className="mr-1 h-3.5 w-3.5" />
                  {customSaving ? '保存中…' : '应用'}
                </Button>
              </div>
            </div>

            {customCount === 0 && !customLoading && (
              <p className="py-6 text-center text-[13px] text-muted-foreground">
                点击「添加模型」创建第一条映射
              </p>
            )}

            {customCount > 0 && (
              <div className="hidden overflow-x-auto md:block">
                <div className="mb-1 grid min-w-[52rem] grid-cols-[1fr_1fr_80px_70px_90px_1fr_100px_36px] gap-1.5 px-1 text-[11px] font-medium text-muted-foreground">
                  <span>id</span>
                  <span>backendId</span>
                  <span>窗口</span>
                  <span>tokens</span>
                  <span>Reasoning</span>
                  <span>displayName</span>
                  <span>ownedBy</span>
                  <span />
                </div>

                <div className="space-y-1">
                  {drafts.map((m, i) => (
                    <div
                      key={i}
                      className="grid min-w-[52rem] grid-cols-[1fr_1fr_80px_70px_90px_1fr_100px_36px] items-center gap-1.5 rounded-md border border-border/60 px-2 py-1.5"
                    >
                      <Input
                        value={m.id}
                        onChange={(e) => updateField(i, 'id', e.target.value)}
                        placeholder="my-gpt"
                        disabled={customSaving}
                        spellCheck={false}
                        className="h-7 text-[12.5px]"
                      />
                      <Input
                        value={m.backendId}
                        onChange={(e) => updateField(i, 'backendId', e.target.value)}
                        placeholder="gpt-5.7"
                        disabled={customSaving}
                        spellCheck={false}
                        className="h-7 text-[12.5px]"
                      />
                      <Input
                        type="number"
                        value={m.contextWindow ?? ''}
                        onChange={(e) => {
                          const v = e.target.value
                          updateField(i, 'contextWindow', v === '' ? undefined : Number(v))
                        }}
                        placeholder="200000"
                        disabled={customSaving}
                        className="h-7 text-[12.5px] [appearance:textfield] [&::-webkit-inner-spin-button]:appearance-none [&::-webkit-outer-spin-button]:appearance-none"
                      />
                      <Input
                        type="number"
                        value={m.maxTokens ?? ''}
                        onChange={(e) => {
                          const v = e.target.value
                          updateField(i, 'maxTokens', v === '' ? undefined : Number(v))
                        }}
                        placeholder="64000"
                        disabled={customSaving}
                        className="h-7 text-[12.5px] [appearance:textfield] [&::-webkit-inner-spin-button]:appearance-none [&::-webkit-outer-spin-button]:appearance-none"
                      />
                      <div className="flex justify-center">
                        <Switch
                          checked={m.supportsReasoning ?? false}
                          onCheckedChange={(checked) =>
                            updateField(i, 'supportsReasoning', checked)
                          }
                          disabled={customSaving}
                          aria-label={`第 ${i + 1} 条模型 reasoning`}
                        />
                      </div>
                      <Input
                        value={m.displayName ?? ''}
                        onChange={(e) =>
                          updateField(i, 'displayName', e.target.value || undefined)
                        }
                        placeholder="同 id"
                        disabled={customSaving}
                        spellCheck={false}
                        className="h-7 text-[12.5px]"
                      />
                      <Input
                        value={m.ownedBy ?? ''}
                        onChange={(e) =>
                          updateField(i, 'ownedBy', e.target.value || undefined)
                        }
                        placeholder="custom"
                        disabled={customSaving}
                        spellCheck={false}
                        className="h-7 text-[12.5px]"
                      />
                      <Button
                        size="icon"
                        variant="ghost"
                        onClick={() => removeRow(i)}
                        disabled={customSaving}
                        title="删除此行"
                        className="h-7 w-7 text-muted-foreground hover:text-destructive"
                      >
                        <Trash2 className="h-3.5 w-3.5" />
                      </Button>
                    </div>
                  ))}
                </div>
              </div>
            )}

            {customCount > 0 && (
              <div className="space-y-2 md:hidden">
                {drafts.map((m, i) => (
                  <div
                    key={`mobile-${i}`}
                    className="space-y-3 rounded-md border border-border/60 p-3"
                  >
                    <div className="flex items-center justify-between gap-2">
                      <span className="text-xs font-semibold text-muted-foreground">
                        模型 {i + 1}
                      </span>
                      <Button
                        size="icon"
                        variant="ghost"
                        onClick={() => removeRow(i)}
                        disabled={customSaving}
                        title="删除此行"
                        className="h-7 w-7 text-muted-foreground hover:text-destructive"
                      >
                        <Trash2 className="h-3.5 w-3.5" />
                      </Button>
                    </div>

                    <div className="grid grid-cols-1 gap-2 sm:grid-cols-2">
                      <label className="min-w-0 space-y-1">
                        <span className="text-[11px] font-medium text-muted-foreground">id</span>
                        <Input
                          value={m.id}
                          onChange={(e) => updateField(i, 'id', e.target.value)}
                          placeholder="my-gpt"
                          disabled={customSaving}
                          spellCheck={false}
                          className="h-8 text-[12.5px]"
                        />
                      </label>
                      <label className="min-w-0 space-y-1">
                        <span className="text-[11px] font-medium text-muted-foreground">backendId</span>
                        <Input
                          value={m.backendId}
                          onChange={(e) => updateField(i, 'backendId', e.target.value)}
                          placeholder="gpt-5.7"
                          disabled={customSaving}
                          spellCheck={false}
                          className="h-8 text-[12.5px]"
                        />
                      </label>
                      <label className="min-w-0 space-y-1">
                        <span className="text-[11px] font-medium text-muted-foreground">上下文窗口</span>
                        <Input
                          type="number"
                          value={m.contextWindow ?? ''}
                          onChange={(e) => {
                            const v = e.target.value
                            updateField(i, 'contextWindow', v === '' ? undefined : Number(v))
                          }}
                          placeholder="200000"
                          disabled={customSaving}
                          className="h-8 text-[12.5px] [appearance:textfield] [&::-webkit-inner-spin-button]:appearance-none [&::-webkit-outer-spin-button]:appearance-none"
                        />
                      </label>
                      <label className="min-w-0 space-y-1">
                        <span className="text-[11px] font-medium text-muted-foreground">最大 tokens</span>
                        <Input
                          type="number"
                          value={m.maxTokens ?? ''}
                          onChange={(e) => {
                            const v = e.target.value
                            updateField(i, 'maxTokens', v === '' ? undefined : Number(v))
                          }}
                          placeholder="64000"
                          disabled={customSaving}
                          className="h-8 text-[12.5px] [appearance:textfield] [&::-webkit-inner-spin-button]:appearance-none [&::-webkit-outer-spin-button]:appearance-none"
                        />
                      </label>
                      <label className="min-w-0 space-y-1">
                        <span className="text-[11px] font-medium text-muted-foreground">displayName</span>
                        <Input
                          value={m.displayName ?? ''}
                          onChange={(e) =>
                            updateField(i, 'displayName', e.target.value || undefined)
                          }
                          placeholder="同 id"
                          disabled={customSaving}
                          spellCheck={false}
                          className="h-8 text-[12.5px]"
                        />
                      </label>
                      <label className="min-w-0 space-y-1">
                        <span className="text-[11px] font-medium text-muted-foreground">ownedBy</span>
                        <Input
                          value={m.ownedBy ?? ''}
                          onChange={(e) =>
                            updateField(i, 'ownedBy', e.target.value || undefined)
                          }
                          placeholder="custom"
                          disabled={customSaving}
                          spellCheck={false}
                          className="h-8 text-[12.5px]"
                        />
                      </label>
                    </div>

                    <div className="flex items-center justify-between rounded-md bg-muted/40 px-2.5 py-2">
                      <span className="text-xs text-muted-foreground">支持 Reasoning</span>
                      <Switch
                        checked={m.supportsReasoning ?? false}
                        onCheckedChange={(checked) =>
                          updateField(i, 'supportsReasoning', checked)
                        }
                        disabled={customSaving}
                        aria-label={`第 ${i + 1} 条模型 reasoning`}
                      />
                    </div>
                  </div>
                ))}
              </div>
            )}
          </>
        )}
      </SettingGroup>
    </div>
  )
}

// ──── 错误展示 ──────────────────────────────────────────────────────────────

function UpstreamError({ error }: { error: unknown }) {
  const parsed = parseError(error)
  return (
    <div className="space-y-2 py-8 text-center">
      <div className="flex items-center justify-center gap-2 font-medium text-destructive">
        <AlertCircle className="h-5 w-5" />
        <span>{parsed.title}</span>
      </div>
      {parsed.detail && (
        <div className="px-4 text-sm text-muted-foreground">{parsed.detail}</div>
      )}
    </div>
  )
}
