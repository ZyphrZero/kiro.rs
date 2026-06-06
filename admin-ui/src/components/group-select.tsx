import { useState } from 'react'
import { Input } from '@/components/ui/input'
import { Button } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'

const NEW_GROUP = '__new__'

const selectClass =
  'flex h-10 w-full rounded-xl border border-input bg-background/60 px-3.5 py-2 text-sm transition-all duration-150 ease-apple placeholder:text-muted-foreground/70 hover:border-border focus-visible:outline-none focus-visible:border-ring focus-visible:ring-2 focus-visible:ring-ring/30 disabled:cursor-not-allowed disabled:opacity-50'

/** 单选分组：下拉选现有分组 / 不绑定 / 新建分组。用于客户端 Key 绑定分组。 */
export function GroupSingleSelect({
  value,
  options,
  onChange,
  disabled,
  noneLabel = '（不绑定）',
}: {
  value: string
  options: string[]
  onChange: (v: string) => void
  disabled?: boolean
  noneLabel?: string
}) {
  // 当前值不在已知选项里且非空 → 视为"正在新建"
  const isKnown = value === '' || options.includes(value)
  const [creating, setCreating] = useState(!isKnown)

  const selectVal = creating ? NEW_GROUP : value

  return (
    <div className="space-y-2">
      <select
        className={selectClass}
        value={selectVal}
        disabled={disabled}
        onChange={(e) => {
          const v = e.target.value
          if (v === NEW_GROUP) {
            setCreating(true)
            onChange('')
          } else {
            setCreating(false)
            onChange(v)
          }
        }}
      >
        <option value="">{noneLabel}</option>
        {options.map((g) => (
          <option key={g} value={g}>
            {g}
          </option>
        ))}
        <option value={NEW_GROUP}>+ 新建分组…</option>
      </select>
      {creating && (
        <Input
          placeholder="输入新分组名"
          value={value}
          disabled={disabled}
          onChange={(e) => onChange(e.target.value)}
          autoFocus
        />
      )}
    </div>
  )
}

/** 多选分组：勾选现有分组 + 新建分组。用于账号(credential) groups 编辑。 */
export function GroupMultiSelect({
  value,
  options,
  onChange,
  disabled,
}: {
  value: string[]
  options: string[]
  onChange: (v: string[]) => void
  disabled?: boolean
}) {
  const [newGroup, setNewGroup] = useState('')
  // 选项 = 已知分组 ∪ 当前已选（含可能已不在 options 里的旧分组）
  const allOptions = Array.from(new Set([...options, ...value])).sort()

  const toggle = (g: string) => {
    if (value.includes(g)) onChange(value.filter((x) => x !== g))
    else onChange([...value, g])
  }

  const addNew = () => {
    const g = newGroup.trim()
    if (g && !value.includes(g)) onChange([...value, g])
    setNewGroup('')
  }

  return (
    <div className="space-y-2">
      {allOptions.length > 0 && (
        <div className="flex flex-wrap gap-x-4 gap-y-2 rounded-xl border border-input bg-background/60 p-3">
          {allOptions.map((g) => (
            <label
              key={g}
              className="flex cursor-pointer items-center gap-2 text-sm"
            >
              <Checkbox
                checked={value.includes(g)}
                disabled={disabled}
                onCheckedChange={() => toggle(g)}
              />
              <span>{g}</span>
            </label>
          ))}
        </div>
      )}
      <div className="flex gap-2">
        <Input
          placeholder="+ 新建分组名"
          value={newGroup}
          disabled={disabled}
          onChange={(e) => setNewGroup(e.target.value)}
          onBlur={() => addNew()}
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              e.preventDefault()
              addNew()
            }
          }}
        />
        <Button
          type="button"
          variant="outline"
          size="sm"
          disabled={disabled || !newGroup.trim()}
          onClick={addNew}
        >
          添加
        </Button>
      </div>
      {value.length > 0 && (
        <p className="text-xs text-muted-foreground">
          已选分组：{value.join('、')}
        </p>
      )}
    </div>
  )
}
