import { useEffect, useState } from 'react'
import {
  Plus,
  Save,
  Trash2,
  ChevronUp,
  ChevronDown,
  Palette,
  Sliders,
  Settings2,
} from 'lucide-react'
import { toast } from 'sonner'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Badge } from '@/components/ui/badge'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { SettingGroup } from '@/components/console/setting-row'
import { useQueryClient } from '@tanstack/react-query'
import {
  useCredentialMetadataSchema,
  useSetCredentialMetadataSchema,
} from '@/hooks/use-credentials'
import { extractErrorMessage } from '@/lib/utils'
import { validateMetadataCss, metadataCssToStyle } from '@/lib/credential-metadata-style'
import type {
  CredentialMetadataFieldSchema,
  CredentialMetadataSchema,
} from '@/types/api'

type ValueType = CredentialMetadataFieldSchema['type']

export interface OptionDraft {
  value: string
  label: string
}

export interface FieldDraft {
  locked: boolean
  key: string
  title: string
  description: string
  type: ValueType
  defaultValue: string
  options: OptionDraft[]
  css: string
  expanded?: boolean
}

// 快速 CSS 预设集
const CSS_PRESETS = [
  {
    name: '琥珀',
    css: 'color: #b45309; background-color: #fffbeb; border-color: #fde68a; font-weight: 600;',
  },
  {
    name: '翡翠',
    css: 'color: #047857; background-color: #ecfdf5; border-color: #a7f3d0; font-weight: 600;',
  },
  {
    name: '玫瑰',
    css: 'color: #b91c1c; background-color: #fef2f2; border-color: #fecaca; font-weight: 600;',
  },
  {
    name: '紫罗兰',
    css: 'color: #6d28d9; background-color: #f5f3ff; border-color: #ddd6fe; font-weight: 600;',
  },
  {
    name: '中性',
    css: 'color: #374151; background-color: #f3f4f6; border-color: #e5e7eb;',
  },
]

// 系统默认的基础字段模板 (type, saleStatus, salePrice)
const DEFAULT_BUILTIN_PROPERTIES: Record<string, CredentialMetadataFieldSchema> = {
  type: {
    title: '账号类型',
    description: '账号运营分类，仅用于标记，不参与调度。',
    type: 'string',
    default: 'normal',
    oneOf: [
      { const: 'normal', title: '正常号' },
      { const: 'boom', title: '炸弹号' },
    ],
    'x-css': 'color: #b45309; background-color: #fffbeb; border-color: #fde68a;',
  },
  saleStatus: {
    title: '在售状态',
    description: '账号运营销售状态，仅用于标记，不参与调度。',
    type: 'string',
    default: 'not_for_sale',
    oneOf: [
      { const: 'not_for_sale', title: '非卖品' },
      { const: 'for_sale', title: '在售' },
      { const: 'sold', title: '已售' },
    ],
    'x-css': 'color: #047857; background-color: #ecfdf5; border-color: #a7f3d0;',
  },
  salePrice: {
    title: '销售价格（CNY）',
    description: '账号销售价格，单位为人民币；未设置时不在卡片显示。',
    type: 'number',
    minimum: 0,
    'x-css': 'color: #0284c7; background-color: #f0f9ff; border-color: #bae6fd;',
  },
}

function schemaToDrafts(schema: CredentialMetadataSchema): FieldDraft[] {
  const mergedProperties = {
    ...DEFAULT_BUILTIN_PROPERTIES,
    ...(schema?.properties ?? {}),
  }
  return Object.entries(mergedProperties).map(([key, field]) => ({
    locked: ['type', 'saleStatus', 'salePrice'].includes(key),
    key,
    title: field.title,
    description: field.description ?? '',
    type: field.type,
    defaultValue: field.default == null ? '' : String(field.default),
    options:
      field.oneOf?.map((option) => ({
        value: String(option.const),
        label: option.title,
      })) ?? [],
    css: field['x-css'] ?? '',
    expanded: false,
  }))
}

function parseDefault(value: string, type: ValueType): unknown {
  if (value === '') return undefined
  if (type === 'boolean') return value === 'true'
  if (type === 'number' || type === 'integer') return Number(value)
  return value
}

function draftsToSchema(
  base: CredentialMetadataSchema,
  drafts: FieldDraft[],
): CredentialMetadataSchema {
  const properties: Record<string, CredentialMetadataFieldSchema> = {}
  for (const draft of drafts) {
    const field: CredentialMetadataFieldSchema = {
      title: draft.title.trim() || draft.key,
      type: draft.type,
    }
    if (draft.description.trim()) field.description = draft.description.trim()
    const defaultValue = parseDefault(draft.defaultValue.trim(), draft.type)
    if (defaultValue !== undefined) field.default = defaultValue

    const options = draft.options
      .filter((opt) => opt.value.trim() !== '')
      .map((opt) => {
        const raw = opt.value.trim()
        const parsed =
          draft.type === 'boolean'
            ? raw === 'true'
            : draft.type === 'number' || draft.type === 'integer'
            ? Number(raw)
            : raw
        return { const: parsed, title: opt.label.trim() || raw }
      })

    if (options.length > 0) field.oneOf = options
    if (draft.css.trim()) field['x-css'] = draft.css.trim()
    if (draft.key.trim() === 'salePrice') field.minimum = 0
    properties[draft.key.trim()] = field
  }

  const required = ['type', 'saleStatus'].filter((k) =>
    Object.prototype.hasOwnProperty.call(properties, k),
  )

  return {
    ...base,
    type: 'object',
    properties,
    required,
    additionalProperties: true,
  }
}

export function MetadataSection() {
  const queryClient = useQueryClient()
  const { data, isLoading } = useCredentialMetadataSchema()
  const { mutate, isPending } = useSetCredentialMetadataSchema()
  const [drafts, setDrafts] = useState<FieldDraft[]>([])

  useEffect(() => {
    if (!data?.schema) return

    const existingKeys = Object.keys(data.schema.properties ?? {})
    const hasAllBuiltins = ['type', 'saleStatus', 'salePrice'].every((key) =>
      existingKeys.includes(key),
    )

    const initialDrafts = schemaToDrafts(data.schema)
    setDrafts(initialDrafts)

    // 如果后端的 config.json 尚未持久化这些默认字段，自动触发一次 API 将完整 Payload 写入后端磁盘与内存
    if (!hasAllBuiltins && !isPending) {
      const fullSchema = draftsToSchema(data.schema, initialDrafts)
      mutate(
        { schema: fullSchema },
        {
          onSuccess: () => {
            queryClient.invalidateQueries({ queryKey: ['credentials'] })
            queryClient.invalidateQueries({ queryKey: ['credential-metadata-schema'] })
          },
        },
      )
    }
  }, [data])

  const update = (index: number, patch: Partial<FieldDraft>) => {
    setDrafts((current) =>
      current.map((field, i) => (i === index ? { ...field, ...patch } : field)),
    )
  }

  const toggleExpand = (index: number) => {
    setDrafts((current) =>
      current.map((field, i) =>
        i === index ? { ...field, expanded: !field.expanded } : field,
      ),
    )
  }

  const moveField = (index: number, direction: 'up' | 'down') => {
    setDrafts((current) => {
      const targetIndex = direction === 'up' ? index - 1 : index + 1
      if (targetIndex < 0 || targetIndex >= current.length) return current
      const next = [...current]
      const temp = next[index]
      next[index] = next[targetIndex]
      next[targetIndex] = temp
      return next
    })
  }

  const addField = () => {
    setDrafts((current) => [
      ...current,
      {
        locked: false,
        key: '',
        title: '',
        description: '',
        type: 'string',
        defaultValue: '',
        options: [],
        css: '',
        expanded: true,
      },
    ])
  }

  const addOption = (fieldIndex: number) => {
    const field = drafts[fieldIndex]
    const nextOptions = [...field.options, { value: '', label: '' }]
    update(fieldIndex, { options: nextOptions })
  }

  const updateOption = (
    fieldIndex: number,
    optionIndex: number,
    patch: Partial<OptionDraft>,
  ) => {
    const field = drafts[fieldIndex]
    const nextOptions = field.options.map((opt, i) =>
      i === optionIndex ? { ...opt, ...patch } : opt,
    )
    update(fieldIndex, { options: nextOptions })
  }

  const removeOption = (fieldIndex: number, optionIndex: number) => {
    const field = drafts[fieldIndex]
    const nextOptions = field.options.filter((_, i) => i !== optionIndex)
    update(fieldIndex, { options: nextOptions })
  }

  const save = () => {
    if (!data?.schema) return
    const keys = drafts.map((field) => field.key.trim())
    if (keys.some((key) => !key)) {
      toast.error('字段 key 不能为空')
      return
    }
    if (new Set(keys).size !== keys.length) {
      toast.error('字段 key 不能重复')
      return
    }
    for (const field of drafts) {
      const defaultValue = field.defaultValue.trim()
      if (
        defaultValue &&
        field.type === 'boolean' &&
        !['true', 'false'].includes(defaultValue)
      ) {
        toast.error(`${field.key} 的布尔默认值只能是 true 或 false`)
        return
      }
      if (
        defaultValue &&
        (field.type === 'number' || field.type === 'integer') &&
        (!Number.isFinite(Number(defaultValue)) ||
          (field.type === 'integer' && !Number.isInteger(Number(defaultValue))))
      ) {
        toast.error(`${field.key} 的默认值类型不正确`)
        return
      }
      const cssError = validateMetadataCss(field.css)
      if (cssError) {
        toast.error(`${field.key}: ${cssError}`)
        return
      }
    }
    mutate(
      { schema: draftsToSchema(data.schema, drafts) },
      {
        onSuccess: () => {
          queryClient.invalidateQueries({ queryKey: ['credentials'] })
          queryClient.invalidateQueries({ queryKey: ['credential-metadata-schema'] })
          toast.success('凭据 Metadata Schema 已保存（已被实时保存至配置文件）')
        },
        onError: (error) =>
          toast.error(`保存失败: ${extractErrorMessage(error)}`),
      },
    )
  }

  return (
    <SettingGroup
      title="凭据 Metadata Schema"
      description="自定义凭据属性结构。通过列表清晰管理字段属性、类型、胶囊渲染样式与展示顺序。"
    >
      <div className="space-y-3 py-2">
        {/* 表格容器 */}
        <div className="rounded-2xl border border-border/80 bg-card overflow-hidden shadow-apple-sm">
          {/* 表头 Header Row */}
          <div className="hidden md:grid grid-cols-[56px_minmax(0,1.3fr)_minmax(86px,100px)_minmax(0,1fr)_minmax(120px,1.2fr)_72px] items-center gap-2 border-b border-border/60 bg-muted/40 px-3 py-2 text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
            <div className="min-w-0 text-center">排序</div>
            <div className="min-w-0">字段 Key & 名称</div>
            <div className="min-w-0">数据类型</div>
            <div className="min-w-0">默认值 / 枚举</div>
            <div className="min-w-0">卡片渲染 Preview</div>
            <div className="min-w-0 text-right">操作</div>
          </div>

          {/* 表格数据行 Table Rows */}
          <div className="divide-y divide-border/40">
            {drafts.map((field, index) => {
              const builtIn = field.locked
              const isExpanded = field.expanded ?? false
              const parsedStyle = metadataCssToStyle(field.css)

              return (
                <div key={`field-${field.key || index}`} className="transition-colors">
                  {/* 桌面端数据行 */}
                  <div
                    onClick={() => toggleExpand(index)}
                    className={`hidden md:grid grid-cols-[56px_minmax(0,1.3fr)_minmax(86px,100px)_minmax(0,1fr)_minmax(120px,1.2fr)_72px] items-center gap-2 px-3 py-3 text-sm cursor-pointer transition-colors ${
                      isExpanded ? 'bg-accent/40' : 'hover:bg-accent/20'
                    }`}
                  >
                    {/* 列 1: 排序控制 */}
                    <div
                      className="flex min-w-0 items-center justify-center gap-0.5"
                      onClick={(e) => e.stopPropagation()}
                    >
                      <Button
                        type="button"
                        size="icon"
                        variant="ghost"
                        className="h-6 w-6 text-muted-foreground/70 hover:text-foreground"
                        disabled={index === 0 || isPending}
                        onClick={() => moveField(index, 'up')}
                        title="上移"
                      >
                        <ChevronUp className="h-3.5 w-3.5" />
                      </Button>
                      <Button
                        type="button"
                        size="icon"
                        variant="ghost"
                        className="h-6 w-6 text-muted-foreground/70 hover:text-foreground"
                        disabled={index === drafts.length - 1 || isPending}
                        onClick={() => moveField(index, 'down')}
                        title="下移"
                      >
                        <ChevronDown className="h-3.5 w-3.5" />
                      </Button>
                    </div>

                    {/* 列 2: Key & Title */}
                    <div className="flex min-w-0 items-center gap-2 overflow-hidden">
                      <span className="min-w-0 truncate font-mono text-sm font-bold text-foreground">
                        {field.key || <span className="text-muted-foreground italic">未命名 key</span>}
                      </span>
                      {field.title && (
                        <span className="min-w-0 truncate text-xs text-muted-foreground">
                          ({field.title})
                        </span>
                      )}
                      {builtIn && (
                        <Badge
                          variant="secondary"
                          className="bg-primary/10 text-primary border-primary/20 text-[9px] px-1 py-0 h-4"
                        >
                          默认
                        </Badge>
                      )}
                    </div>

                    {/* 列 3: 数据类型 */}
                    <div className="min-w-0">
                      <Badge variant="outline" className="text-[10px] font-mono uppercase">
                        {field.type}
                      </Badge>
                    </div>

                    {/* 列 4: 默认值 / 枚举数 */}
                    <div className="min-w-0 truncate text-xs text-muted-foreground">
                      {field.options.length > 0 ? (
                        <span className="font-medium text-foreground">
                          {field.options.length} 个枚举项
                        </span>
                      ) : field.defaultValue ? (
                        <span className="font-mono text-[11px] bg-muted/60 px-1.5 py-0.5 rounded">
                          默认: {field.defaultValue}
                        </span>
                      ) : (
                        <span className="text-muted-foreground/50 italic">-</span>
                      )}
                    </div>

                    {/* 列 5: 卡片渲染 Preview */}
                    <div className="flex min-w-0 items-center">
                      <span
                        className="inline-flex max-w-[140px] truncate items-center rounded border px-2 py-0.5 text-xs font-medium"
                        style={parsedStyle}
                      >
                        {field.title || field.key || '字段'}: 示例
                      </span>
                    </div>

                    {/* 列 6: 操作按钮 (设置 / 删除) */}
                    <div
                      className="flex min-w-0 items-center justify-end gap-1"
                      onClick={(e) => e.stopPropagation()}
                    >
                      <Button
                        type="button"
                        size="icon"
                        variant={isExpanded ? 'secondary' : 'ghost'}
                        className="h-7 w-7 text-muted-foreground hover:text-foreground"
                        onClick={() => toggleExpand(index)}
                        title={isExpanded ? '收起配置' : '展开配置'}
                      >
                        <Settings2 className="h-4 w-4" />
                      </Button>
                      <Button
                        type="button"
                        size="icon"
                        variant="ghost"
                        disabled={isPending}
                        onClick={() =>
                          setDrafts((current) => current.filter((_, i) => i !== index))
                        }
                        className="h-7 w-7 text-destructive/70 hover:text-destructive hover:bg-destructive/10"
                        title="删除字段"
                      >
                        <Trash2 className="h-4 w-4" />
                      </Button>
                    </div>
                  </div>

                  {/* 移动端数据行：用两行信息替代表格列，避免窄屏逐字换行 */}
                  <div
                    onClick={() => toggleExpand(index)}
                    className={`flex flex-col gap-2 px-3 py-3 text-sm cursor-pointer transition-colors md:hidden ${
                      isExpanded ? 'bg-accent/40' : 'hover:bg-accent/20'
                    }`}
                  >
                    <div className="flex min-w-0 items-start gap-2">
                      <div
                        className="flex shrink-0 items-center gap-0.5"
                        onClick={(e) => e.stopPropagation()}
                      >
                        <Button
                          type="button"
                          size="icon"
                          variant="ghost"
                          className="h-7 w-7 text-muted-foreground/70 hover:text-foreground"
                          disabled={index === 0 || isPending}
                          onClick={() => moveField(index, 'up')}
                          title="上移"
                        >
                          <ChevronUp className="h-3.5 w-3.5" />
                        </Button>
                        <Button
                          type="button"
                          size="icon"
                          variant="ghost"
                          className="h-7 w-7 text-muted-foreground/70 hover:text-foreground"
                          disabled={index === drafts.length - 1 || isPending}
                          onClick={() => moveField(index, 'down')}
                          title="下移"
                        >
                          <ChevronDown className="h-3.5 w-3.5" />
                        </Button>
                      </div>

                      <div className="min-w-0 flex-1">
                        <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
                          <span className="max-w-full break-all font-mono text-sm font-bold text-foreground">
                            {field.key || (
                              <span className="text-muted-foreground italic">未命名 key</span>
                            )}
                          </span>
                          {field.title && (
                            <span className="max-w-full truncate text-xs text-muted-foreground">
                              ({field.title})
                            </span>
                          )}
                          {builtIn && (
                            <Badge
                              variant="secondary"
                              className="h-4 border-primary/20 bg-primary/10 px-1 py-0 text-[9px] text-primary"
                            >
                              默认
                            </Badge>
                          )}
                        </div>

                        <div className="mt-2 flex min-w-0 flex-wrap items-center gap-2 text-xs">
                          <Badge variant="outline" className="text-[10px] font-mono uppercase">
                            {field.type}
                          </Badge>
                          {field.options.length > 0 ? (
                            <span className="truncate font-medium text-foreground">
                              {field.options.length} 个枚举项
                            </span>
                          ) : field.defaultValue ? (
                            <span className="max-w-full truncate rounded bg-muted/60 px-1.5 py-0.5 font-mono text-[11px]">
                              默认: {field.defaultValue}
                            </span>
                          ) : null}
                          <span
                            className="inline-flex min-w-0 max-w-full truncate items-center rounded border px-2 py-0.5 text-xs font-medium"
                            style={parsedStyle}
                          >
                            {field.title || field.key || '字段'}: 示例
                          </span>
                        </div>
                      </div>

                      <div
                        className="flex shrink-0 items-center gap-1"
                        onClick={(e) => e.stopPropagation()}
                      >
                        <Button
                          type="button"
                          size="icon"
                          variant={isExpanded ? 'secondary' : 'ghost'}
                          className="h-7 w-7 text-muted-foreground hover:text-foreground"
                          onClick={() => toggleExpand(index)}
                          title={isExpanded ? '收起配置' : '展开配置'}
                        >
                          <Settings2 className="h-4 w-4" />
                        </Button>
                        <Button
                          type="button"
                          size="icon"
                          variant="ghost"
                          disabled={isPending}
                          onClick={() =>
                            setDrafts((current) => current.filter((_, i) => i !== index))
                          }
                          className="h-7 w-7 text-destructive/70 hover:bg-destructive/10 hover:text-destructive"
                          title="删除字段"
                        >
                          <Trash2 className="h-4 w-4" />
                        </Button>
                      </div>
                    </div>
                  </div>

                  {/* 展开的平滑大方配置面板 */}
                  {isExpanded && (
                    <div className="space-y-4 border-t border-border/40 bg-muted/10 p-4 sm:p-5 text-sm">
                      {/* 第一行 (4列): Key, Title, Type, DefaultValue */}
                      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-4">
                        <div className="space-y-1">
                          <label className="text-xs font-semibold text-muted-foreground uppercase">
                            字段 Key
                          </label>
                          <Input
                            value={field.key}
                            onChange={(event) =>
                              update(index, { key: event.target.value })
                            }
                            placeholder="例如: salePrice"
                            disabled={isPending}
                            className="font-mono"
                          />
                        </div>

                        <div className="space-y-1">
                          <label className="text-xs font-semibold text-muted-foreground uppercase">
                            显示名称 (Title)
                          </label>
                          <Input
                            value={field.title}
                            onChange={(event) =>
                              update(index, { title: event.target.value })
                            }
                            placeholder="例如: 售卖价格"
                            disabled={isPending}
                          />
                        </div>

                        <div className="space-y-1">
                          <label className="text-xs font-semibold text-muted-foreground uppercase">
                            数据类型 (Type)
                          </label>
                          <Select
                            value={field.type}
                            onValueChange={(value) =>
                              update(index, { type: value as ValueType })
                            }
                            disabled={isPending}
                          >
                            <SelectTrigger>
                              <SelectValue />
                            </SelectTrigger>
                            <SelectContent>
                              <SelectItem value="string">字符串 (String)</SelectItem>
                              <SelectItem value="number">数字 (Number)</SelectItem>
                              <SelectItem value="integer">整数 (Integer)</SelectItem>
                              <SelectItem value="boolean">布尔值 (Boolean)</SelectItem>
                            </SelectContent>
                          </Select>
                        </div>

                        <div className="space-y-1">
                          <label className="text-xs font-semibold text-muted-foreground uppercase">
                            默认值
                          </label>
                          <Input
                            value={field.defaultValue}
                            onChange={(event) =>
                              update(index, { defaultValue: event.target.value })
                            }
                            placeholder={
                              field.type === 'boolean'
                                ? 'true 或 false'
                                : '初始默认值'
                            }
                            disabled={isPending}
                            className="font-mono"
                          />
                        </div>
                      </div>

                      {/* 第二行 (2列): 说明 & CSS 样式 */}
                      <div className="grid grid-cols-1 gap-3 lg:grid-cols-2">
                        <div className="space-y-1">
                          <label className="text-xs font-semibold text-muted-foreground uppercase">
                            字段说明文案
                          </label>
                          <Input
                            value={field.description}
                            onChange={(event) =>
                              update(index, { description: event.target.value })
                            }
                            placeholder="表单与提示框中展示的说明"
                            disabled={isPending}
                          />
                        </div>

                        <div className="space-y-1">
                          <div className="flex flex-col items-start gap-2 sm:flex-row sm:items-center sm:justify-between">
                            <label className="flex items-center gap-1.5 text-xs font-semibold uppercase text-muted-foreground">
                              <Palette className="h-3.5 w-3.5 text-primary" />
                              CSS 胶囊样式
                            </label>
                            <div className="flex flex-wrap items-center gap-1">
                              <span className="text-[10px] text-muted-foreground">
                                快捷配色:
                              </span>
                              {CSS_PRESETS.map((preset) => (
                                <button
                                  key={preset.name}
                                  type="button"
                                  onClick={() => update(index, { css: preset.css })}
                                  disabled={isPending}
                                  className="rounded px-1.5 py-0.5 text-[10px] border border-border/50 bg-muted/40 hover:bg-accent hover:text-primary transition-colors"
                                >
                                  {preset.name}
                                </button>
                              ))}
                            </div>
                          </div>
                          <Input
                            value={field.css}
                            onChange={(event) =>
                              update(index, { css: event.target.value })
                            }
                            placeholder="例如: color: #b45309; background-color: #fffbeb;"
                            disabled={isPending}
                            className="font-mono text-xs"
                          />
                        </div>
                      </div>

                      {/* 第三行: 枚举选项 (Options Visual Editor) */}
                      <div className="space-y-2 rounded-xl border border-border/50 bg-background/60 p-3">
                        <div className="flex items-center justify-between">
                          <span className="flex items-center gap-1.5 text-xs font-semibold text-muted-foreground uppercase">
                            <Sliders className="h-3.5 w-3.5 text-primary" />
                            枚举选项配置 (OneOf Options)
                          </span>
                          <Button
                            type="button"
                            size="sm"
                            variant="ghost"
                            className="h-7 text-xs text-primary"
                            onClick={() => addOption(index)}
                            disabled={isPending}
                          >
                            <Plus className="mr-1 h-3.5 w-3.5" />
                            添加选项
                          </Button>
                        </div>

                        {field.options.length > 0 ? (
                          <div className="grid gap-2 sm:grid-cols-2">
                            {field.options.map((opt, optIdx) => (
                              <div key={`opt-${optIdx}`} className="flex min-w-0 flex-wrap items-center gap-2">
                                <Input
                                  value={opt.value}
                                  onChange={(e) =>
                                    updateOption(index, optIdx, {
                                      value: e.target.value,
                                    })
                                  }
                                  placeholder="存储值 (const)"
                                  disabled={isPending}
                                  className="h-8 min-w-0 basis-28 flex-1 font-mono text-xs"
                                />
                                <span className="text-muted-foreground/60 text-xs">:</span>
                                <Input
                                  value={opt.label}
                                  onChange={(e) =>
                                    updateOption(index, optIdx, {
                                      label: e.target.value,
                                    })
                                  }
                                  placeholder="显示名称 (title)"
                                  disabled={isPending}
                                  className="h-8 min-w-0 basis-28 flex-1 text-xs"
                                />
                                <Button
                                  type="button"
                                  size="icon"
                                  variant="ghost"
                                  className="h-8 w-8 shrink-0 text-muted-foreground hover:text-destructive"
                                  disabled={isPending}
                                  onClick={() => removeOption(index, optIdx)}
                                  title="删除选项"
                                >
                                  <Trash2 className="h-3.5 w-3.5" />
                                </Button>
                              </div>
                            ))}
                          </div>
                        ) : (
                          <p className="text-xs text-muted-foreground/60 italic py-0.5">
                            未设定固定枚举限制（接受任意对应类型的值）。
                          </p>
                        )}
                      </div>
                    </div>
                  )}
                </div>
              )
            })}
          </div>
        </div>

        {/* 底部保存与添加控制 */}
        <div className="flex flex-wrap justify-between gap-3 pt-2">
          <Button
            type="button"
            variant="outline"
            onClick={addField}
            disabled={isLoading || isPending}
            className="h-9 px-4 text-xs font-medium"
          >
            <Plus className="mr-1.5 h-4 w-4" />
            新增字段
          </Button>

          <Button
            type="button"
            onClick={save}
            disabled={isLoading || isPending || drafts.length === 0}
            className="h-9 px-5 text-xs font-semibold shadow-apple-sm"
          >
            <Save className="mr-1.5 h-4 w-4" />
            {isPending ? '保存中…' : '保存 Schema 配置'}
          </Button>
        </div>
      </div>
    </SettingGroup>
  )
}
