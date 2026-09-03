import { useCallback, useEffect, useRef, useState } from 'react'
import { Check, Loader2 } from 'lucide-react'
import { Switch } from '@/components/ui/switch'
import { Input } from '@/components/ui/input'
import { Button } from '@/components/ui/button'
import { cn } from '@/lib/utils'

/**
 * 设置行原语 —— 设置页所有配置项的唯一排版与保存范式。
 *
 * 现状的问题不是"设置藏得深"，而是**保存语义不一致**：顶栏下拉里，开关是点了
 * 立即生效、预设按钮是点了立即生效、数字输入却要再点一个「保存」；而反馈全靠
 * toast 转述，行内看不出这条改没改成。同一个面板里三种保存方式，手感自然是断的。
 *
 * 这里统一成一条规则：**所有配置即时保存，状态显示在它自己那一行**。
 * - 开关 / 分段选择：点击即提交
 * - 数字 / 文本：失焦或 Enter 提交，Esc 撤回；不再有独立保存按钮
 * - 提交中行尾转圈，成功后打勾停留 1.5s
 *
 * 于是 toast 只用于报错。成功不需要弹窗告知 —— 行内那个勾已经说完了。
 */
export function SettingRow({
  label,
  hint,
  children,
  pending,
  saved,
  disabled,
  className,
}: {
  label: string
  hint?: React.ReactNode
  children: React.ReactNode
  pending?: boolean
  saved?: boolean
  disabled?: boolean
  className?: string
}) {
  return (
    <div
      className={cn(
        'flex flex-col items-stretch gap-2 border-b border-border/50 py-3 last:border-b-0 sm:flex-row sm:flex-wrap sm:items-start sm:justify-between sm:gap-x-4 sm:gap-y-2',
        disabled && 'opacity-55',
        className,
      )}
    >
      <div className="min-w-0 sm:flex-1 sm:basis-56">
        <div className="flex items-center gap-1.5 text-[13.5px] font-medium">
          {label}
          {pending && (
            <Loader2 className="h-3 w-3 animate-spin text-muted-foreground" />
          )}
          {!pending && saved && (
            <Check className="h-3.5 w-3.5 text-emerald-500" />
          )}
        </div>
        {hint && (
          <p className="mt-0.5 text-xs leading-snug text-muted-foreground">
            {hint}
          </p>
        )}
      </div>
      <div className="flex min-w-0 w-full flex-wrap items-center gap-2 sm:w-auto sm:shrink-0 sm:justify-end">
        {children}
      </div>
    </div>
  )
}

/**
 * 按字段追踪保存态。
 *
 * 一个分区通常只有一个 mutation（比如自愈的 4 个字段都走 `useSetSelfHealConfig`），
 * 它的 `isPending` 是**分区级**的。直接把它铺给每一行会造成两个假象：改一个开关时
 * 同分区其余行也跟着转圈、跟着打勾 —— 5 行都宣称自己保存了，实际只有 1 行。
 *
 * 这里用 `Set` 而不是单个"当前字段"变量：同分区连着改两个字段时（先切开关、紧接着
 * 改数字），单变量会被后者覆盖，等前者结算时会把仍在飞行中的后者的转圈错误地清掉。
 *
 * 打勾只挂在 `onSuccess`：失败时不该出现成功标记，报错交给 toast。
 */
export function useFieldSaver<TVars>(
  mutate: (
    vars: TVars,
    opts: { onSuccess?: () => void; onError?: (err: unknown) => void },
  ) => void,
  onError: (err: unknown) => void,
) {
  const [saving, setSaving] = useState<Set<string>>(new Set())
  const [saved, setSaved] = useState<Set<string>>(new Set())
  // 卸载后不再 setState，也避免残留的定时器
  const timers = useRef<number[]>([])
  useEffect(
    () => () => {
      timers.current.forEach((t) => window.clearTimeout(t))
    },
    [],
  )

  const mark = (set: Set<string>, field: string, on: boolean) => {
    const next = new Set(set)
    if (on) next.add(field)
    else next.delete(field)
    return next
  }

  const save = useCallback(
    (field: string, vars: TVars) => {
      setSaving((s) => mark(s, field, true))
      mutate(vars, {
        onSuccess: () => {
          setSaving((s) => mark(s, field, false))
          setSaved((s) => mark(s, field, true))
          const t = window.setTimeout(() => {
            setSaved((s) => mark(s, field, false))
          }, 1500)
          timers.current.push(t)
        },
        onError: (err) => {
          setSaving((s) => mark(s, field, false))
          onError(err)
        },
      })
    },
    [mutate, onError],
  )

  return {
    save,
    isSaving: (field: string) => saving.has(field),
    isSaved: (field: string) => saved.has(field),
    /** 分区内是否有任何字段在保存中（用于整区禁用） */
    busy: saving.size > 0,
  }
}

/** 开关型设置：点击即提交 */
export function SettingSwitch({
  label,
  hint,
  checked,
  onChange,
  pending,
  saved,
  disabled,
}: {
  label: string
  hint?: React.ReactNode
  checked: boolean
  onChange: (next: boolean) => void
  pending?: boolean
  /** 该字段刚保存成功（由 useFieldSaver 按字段给出），不是分区级状态 */
  saved?: boolean
  disabled?: boolean
}) {
  return (
    <SettingRow label={label} hint={hint} pending={pending} saved={saved}>
      <Switch
        checked={checked}
        disabled={disabled || pending}
        onCheckedChange={onChange}
        aria-label={label}
      />
    </SettingRow>
  )
}

/**
 * 数值型设置：失焦 / Enter 提交，Esc 撤回。
 *
 * 不设保存按钮是有意的 —— 一个数字输入配一个按钮，等于把"我改完了"这件
 * 本来就明确的事再确认一遍。越界时不提交并回弹到上次有效值。
 */
export function SettingNumber({
  label,
  hint,
  value,
  onCommit,
  min,
  max,
  unit,
  pending,
  saved,
  disabled,
  presets,
  /** 展示值转换（如秒 → 分钟） */
  toDisplay = (v) => v,
  fromDisplay = (v) => v,
}: {
  label: string
  hint?: React.ReactNode
  value: number
  onCommit: (next: number) => void
  min: number
  max: number
  unit?: string
  pending?: boolean
  /** 该字段刚保存成功（由 useFieldSaver 按字段给出） */
  saved?: boolean
  disabled?: boolean
  /** 常用值快捷按钮，单位与展示值一致 */
  presets?: number[]
  toDisplay?: (raw: number) => number
  fromDisplay?: (display: number) => number
}) {
  const display = toDisplay(value)
  const [draft, setDraft] = useState(String(display))
  const [invalid, setInvalid] = useState(false)

  // 外部值变化（他处修改 / 刷新）时同步草稿
  useEffect(() => {
    setDraft(String(display))
    setInvalid(false)
  }, [display])

  const commit = () => {
    const n = Number(draft)
    if (!Number.isFinite(n) || n < min || n > max) {
      setInvalid(true)
      setDraft(String(display))
      window.setTimeout(() => setInvalid(false), 1200)
      return
    }
    if (n === display) return
    onCommit(fromDisplay(n))
  }

  return (
    <SettingRow label={label} hint={hint} pending={pending} saved={saved}>
      {presets && presets.length > 0 && (
        <div className="flex flex-wrap items-center gap-1">
          {presets.map((p) => (
            <Button
              key={p}
              size="sm"
              variant={display === p ? 'default' : 'outline'}
              className="h-7 px-2 text-xs"
              disabled={disabled || pending}
              onClick={() => {
                if (display !== p) onCommit(fromDisplay(p))
              }}
            >
              {p}
            </Button>
          ))}
        </div>
      )}
      <div className="flex items-center gap-1.5">
        <Input
          type="number"
          min={min}
          max={max}
          value={draft}
          disabled={disabled || pending}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={commit}
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              e.currentTarget.blur()
            } else if (e.key === 'Escape') {
              setDraft(String(display))
              e.currentTarget.blur()
            }
          }}
          className={cn(
            'console-num h-8 w-20 text-right text-[13px]',
            invalid && 'border-destructive focus-visible:border-destructive',
          )}
          title={`${min} ~ ${max}`}
        />
        {unit && (
          <span className="text-xs text-muted-foreground">{unit}</span>
        )}
      </div>
    </SettingRow>
  )
}

/** 分段选择：点击即提交，适合 2~4 个互斥选项 */
export function SettingSegments<T extends string>({
  label,
  hint,
  value,
  options,
  onChange,
  pending,
  saved,
  disabled,
}: {
  label: string
  hint?: React.ReactNode
  value: T
  options: { value: T; label: string; hint?: string }[]
  onChange: (next: T) => void
  pending?: boolean
  /** 该字段刚保存成功（由 useFieldSaver 按字段给出） */
  saved?: boolean
  disabled?: boolean
}) {
  return (
    <SettingRow label={label} hint={hint} pending={pending} saved={saved}>
      <div className="inline-flex h-8 items-center rounded-full border border-border bg-card/60 p-0.5">
        {options.map((o) => (
          <button
            key={o.value}
            type="button"
            title={o.hint}
            disabled={disabled || pending}
            aria-pressed={value === o.value}
            onClick={() => {
              if (value !== o.value) onChange(o.value)
            }}
            className={cn(
              'inline-flex h-7 items-center rounded-full px-3 text-[12.5px] transition-colors disabled:opacity-50',
              value === o.value
                ? 'bg-background text-foreground shadow-apple-sm'
                : 'text-muted-foreground hover:text-foreground',
            )}
          >
            {o.label}
          </button>
        ))}
      </div>
    </SettingRow>
  )
}

/** 只读观测行：展示运行时状态，不可编辑 */
export function SettingReadout({
  label,
  hint,
  children,
}: {
  label: string
  hint?: React.ReactNode
  children: React.ReactNode
}) {
  return (
    <SettingRow label={label} hint={hint}>
      <span className="console-num text-[13px] text-muted-foreground">
        {children}
      </span>
    </SettingRow>
  )
}

/** 设置分区容器 */
export function SettingGroup({
  title,
  description,
  children,
}: {
  title: string
  description?: string
  children: React.ReactNode
}) {
  return (
    <section className="console-scope">
      <div className="mb-3">
        <h2 className="text-base font-semibold tracking-tight">{title}</h2>
        {description && (
          <p className="mt-1 text-xs leading-relaxed text-muted-foreground">
            {description}
          </p>
        )}
      </div>
      <div>{children}</div>
    </section>
  )
}
