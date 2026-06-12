import { Checkbox } from '@/components/ui/checkbox'
import { Button } from '@/components/ui/button'
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuTrigger,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
} from '@/components/ui/dropdown-menu'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { ChevronDown, Check } from 'lucide-react'

// 哨兵值：shadcn Select 不允许 SelectItem 用空字符串作 value，
// 所以"不绑定"用一个 sentinel 占位，进出 onChange 时双向转换。
const NONE_VALUE = '__none__'

const NO_GROUPS_HINT_CLS =
  'text-xs text-muted-foreground italic px-1 py-2 leading-relaxed'

/** 提示用户去分组管理页注册新分组（取代旧版的"+ 新建分组"内联输入）。 */
function ManageGroupsHint() {
  return (
    <p className={NO_GROUPS_HINT_CLS}>
      还没有分组？前往
      <a href="#/groups" className="text-primary underline mx-1">
        分组管理
      </a>
      创建。分组名统一注册后才能在此选择，避免拼写漂移。
    </p>
  )
}

/** 单选分组：下拉选现有分组 / 不绑定。用于客户端 Key 绑定分组。
 *
 *  与改造前的差异：去掉"+ 新建分组"option（避免 typo 漂移）。
 *  新建分组请去 #/groups 管理页。
 */
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
  // 当前值不在已知选项里且非空 → "已删除分组的遗留引用"
  const isOrphan = value !== '' && !options.includes(value)
  // 与全站统一的 shadcn Select 用 NONE_VALUE 哨兵代替空字符串
  const selectValue = value === '' ? NONE_VALUE : value

  return (
    <div className="space-y-2">
      <Select
        value={selectValue}
        disabled={disabled}
        onValueChange={(v) => onChange(v === NONE_VALUE ? '' : v)}
      >
        <SelectTrigger className="h-10 rounded-xl px-3.5">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value={NONE_VALUE}>{noneLabel}</SelectItem>
          {options.map((g) => (
            <SelectItem key={g} value={g}>
              {g}
            </SelectItem>
          ))}
          {isOrphan && (
            <SelectItem value={value}>{value}（已注销）</SelectItem>
          )}
        </SelectContent>
      </Select>
      {options.length === 0 && <ManageGroupsHint />}
      {isOrphan && (
        <p className="text-xs text-amber-600">
          当前绑定的分组 &quot;{value}&quot; 已不在注册表，请重新选择或前往
          <a href="#/groups" className="text-primary underline mx-1">
            分组管理
          </a>
          重建同名分组。
        </p>
      )}
    </div>
  )
}

/** 多选分组：下拉菜单形式（点击展开 + 多选 checkbox）。用于账号(credential) groups 编辑。
 *
 *  与改造前的差异：
 *  - 收起时只显示一个按钮，节省空间
 *  - 多选能力保留（一个凭据可以同时属于多个分组）
 *  - 去掉"+ 新建分组"输入框，新建分组请去 #/groups 管理页
 */
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
  // 选项 = 已注册 ∪ 当前已选（含可能已注销的旧分组，便于用户取消）
  const allOptions = Array.from(new Set([...options, ...value])).sort()
  const orphans = value.filter((g) => !options.includes(g))

  const toggle = (g: string) => {
    if (value.includes(g)) onChange(value.filter((x) => x !== g))
    else onChange([...value, g])
  }

  // 触发器按钮的展示文案：未选 / 已选 N 个 / 单个分组直接显示名字
  const triggerLabel = (() => {
    if (value.length === 0) return '选择分组'
    if (value.length === 1) return value[0]
    return `已选 ${value.length} 个分组`
  })()

  return (
    <div className="space-y-2">
      {allOptions.length === 0 ? (
        <ManageGroupsHint />
      ) : (
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button
              type="button"
              variant="outline"
              disabled={disabled}
              className="w-full justify-between font-normal"
            >
              <span className={value.length === 0 ? 'text-muted-foreground' : ''}>
                {triggerLabel}
              </span>
              <ChevronDown className="h-4 w-4 opacity-50" />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent
            align="start"
            // 与触发器按钮等宽，避免菜单过窄或溢出
            style={{ width: 'var(--radix-dropdown-menu-trigger-width)' }}
            className="max-h-72 overflow-y-auto"
          >
            <DropdownMenuLabel className="text-xs text-muted-foreground">
              选择分组（可多选）
            </DropdownMenuLabel>
            <DropdownMenuSeparator />
            {allOptions.map((g) => {
              const orphan = !options.includes(g)
              const checked = value.includes(g)
              return (
                <DropdownMenuItem
                  key={g}
                  // 阻止默认 close-on-select：分组管理是多选场景，选完一个还要继续选
                  onSelect={(e) => {
                    e.preventDefault()
                    toggle(g)
                  }}
                  className="cursor-pointer gap-2"
                >
                  <Checkbox checked={checked} className="pointer-events-none" />
                  <span className={`flex-1 ${orphan ? 'italic text-amber-600' : ''}`}>
                    {g}
                    {orphan && '（已注销）'}
                  </span>
                  {checked && <Check className="h-3.5 w-3.5 text-primary" />}
                </DropdownMenuItem>
              )
            })}
            <DropdownMenuSeparator />
            <DropdownMenuItem
              className="cursor-pointer text-xs text-muted-foreground"
              onSelect={(e) => {
                e.preventDefault()
                window.location.hash = '#/groups'
              }}
            >
              管理分组…
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      )}
      {value.length > 0 && (
        <p className="text-xs text-muted-foreground">
          已选：{value.join('、')}
        </p>
      )}
      {orphans.length > 0 && (
        <p className="text-xs text-amber-600">
          有 {orphans.length} 个分组已不在注册表，建议取消或前往
          <a href="#/groups" className="text-primary underline mx-1">
            分组管理
          </a>
          重建。
        </p>
      )}
    </div>
  )
}
