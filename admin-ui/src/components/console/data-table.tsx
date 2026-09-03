import React, {
  useCallback,
  useMemo,
  useState,
  type ReactNode,
} from 'react'
import { Columns3, Check, ChevronRight } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuItem,
} from '@/components/ui/dropdown-menu'
import { cn } from '@/lib/utils'
import { railClass, type RailTone } from './rail'

/**
 * 运维密集表格 —— 凭据 / 日志共用。
 *
 * 与「卡片」形态的分工：卡片是看的，表格是做的。所以这里的每个取舍都偏向
 * 「一屏能扫多少行」和「操作是否紧凑」：
 *
 * - 行高 34px、字号 12.5px（`.console-table`，见 index.css）
 * - sticky 表头，长列表滚动时列名不丢
 * - 左侧 3px 状态色轨代替整行染色：既标状态，又不牺牲文字对比度
 * - 行内操作 hover 才显形（`.console-row-actions`），静默时不干扰扫读
 * - 可选列进列控制菜单并记住选择，避免 12 列硬挤出横向滚动
 */
export interface ConsoleColumn<T> {
  id: string
  header: string
  cell: (row: T) => ReactNode
  /** 默认隐藏，需要时从列控制菜单里打开 */
  optional?: boolean
  /** 数值列右对齐，配合 .console-num 做纵向对齐 */
  align?: 'right'
  /** 表头 title 提示 */
  hint?: string
}

export interface ConsoleTableProps<T> {
  rows: T[]
  columns: ConsoleColumn<T>[]
  rowKey: (row: T) => number | string
  /** 行状态 → 左侧色轨；不给则无轨 */
  tone?: (row: T) => RailTone
  /** 开启行选中（复选框列） */
  selectable?: boolean
  selected?: Set<number | string>
  onSelectedChange?: (next: Set<number | string>) => void
  /** 点击行触发 */
  onRowActivate?: (row: T) => void
  /** 展开行渲染函数；传入即开启可折叠行模式 */
  renderExpandedRow?: (row: T) => ReactNode
  /** 受控的已展开行 key 集合 */
  expandedKeys?: Set<number | string>
  /** 展开行改变回调 */
  onExpandedKeysChange?: (next: Set<number | string>) => void
  /** 点击整行是否切换展开，默认 true */
  expandOnRowClick?: boolean
  /** 行右侧的处置动作，hover / focus 行才显形 */
  rowActions?: (row: T) => ReactNode
  /** 列可见性持久化 key；不给则不显示列控制菜单 */
  columnsStorageKey?: string
  loading?: boolean
  empty?: ReactNode
  /** 表格上方右侧的额外控件（与列控制菜单同一行） */
  toolbar?: ReactNode
}

/** 列可见性：默认隐藏 optional 列，选择记到 localStorage */
function useColumnVisibility<T>(
  columns: ConsoleColumn<T>[],
  storageKey?: string,
) {
  const defaults = useMemo(
    () => columns.filter((c) => !c.optional).map((c) => c.id),
    [columns],
  )

  const [visible, setVisible] = useState<string[]>(() => {
    if (!storageKey) return defaults
    try {
      const raw = localStorage.getItem(storageKey)
      if (!raw) return defaults
      const parsed = JSON.parse(raw) as string[]
      // 与当前列定义求交集：列改名 / 删除后不会残留脏 id
      const known = new Set(columns.map((c) => c.id))
      const kept = parsed.filter((id) => known.has(id))
      return kept.length > 0 ? kept : defaults
    } catch {
      return defaults
    }
  })

  const toggle = useCallback(
    (id: string) => {
      setVisible((prev) => {
        const next = prev.includes(id)
          ? prev.filter((x) => x !== id)
          : [...prev, id]
        // 至少留一列，否则表格退化成空壳
        if (next.length === 0) return prev
        if (storageKey) {
          try {
            localStorage.setItem(storageKey, JSON.stringify(next))
          } catch {
            /* 隐私模式下 localStorage 可能不可写，忽略 */
          }
        }
        return next
      })
    },
    [storageKey],
  )

  // 按列定义原顺序输出，不受用户勾选顺序影响
  const ordered = useMemo(
    () => columns.filter((c) => visible.includes(c.id)),
    [columns, visible],
  )

  return { ordered, visible, toggle }
}

export function ConsoleTable<T>({
  rows,
  columns,
  rowKey,
  tone,
  selectable = false,
  selected,
  onSelectedChange,
  onRowActivate,
  renderExpandedRow,
  expandedKeys,
  onExpandedKeysChange,
  expandOnRowClick = true,
  rowActions,
  columnsStorageKey,
  loading = false,
  empty,
  toolbar,
}: ConsoleTableProps<T>) {
  const { ordered, visible, toggle } = useColumnVisibility(
    columns,
    columnsStorageKey,
  )

  const [internalExpandedKeys, setInternalExpandedKeys] = useState<Set<number | string>>(new Set())
  const effectiveExpandedKeys = expandedKeys ?? internalExpandedKeys
  const setEffectiveExpandedKeys = onExpandedKeysChange ?? setInternalExpandedKeys

  const toggleExpand = useCallback((key: number | string) => {
    const next = new Set(effectiveExpandedKeys)
    if (next.has(key)) {
      next.delete(key)
    } else {
      next.add(key)
    }
    setEffectiveExpandedKeys(next)
  }, [effectiveExpandedKeys, setEffectiveExpandedKeys])

  const allSelected =
    rows.length > 0 && rows.every((r) => selected?.has(rowKey(r)))

  const toggleAll = () => {
    if (!onSelectedChange) return
    const next = new Set(selected ?? [])
    if (allSelected) rows.forEach((r) => next.delete(rowKey(r)))
    else rows.forEach((r) => next.add(rowKey(r)))
    onSelectedChange(next)
  }

  const toggleOne = (key: number | string) => {
    if (!onSelectedChange) return
    const next = new Set(selected ?? [])
    if (next.has(key)) next.delete(key)
    else next.add(key)
    onSelectedChange(next)
  }

  const hasHiddenOption = columns.some((c) => c.optional)
  const colSpanTotal =
    ordered.length +
    (selectable ? 1 : 0) +
    (renderExpandedRow ? 1 : 0) +
    (rowActions ? 1 : 0)

  return (
    <div className="console-scope space-y-2">
      {(toolbar || (columnsStorageKey && hasHiddenOption)) && (
        <div className="flex items-center justify-end gap-2">
          {toolbar}
          {columnsStorageKey && hasHiddenOption && (
            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <Button size="sm" variant="outline" title="选择显示的列">
                  <Columns3 className="h-3.5 w-3.5" />
                  <span className="hidden sm:inline">列</span>
                </Button>
              </DropdownMenuTrigger>
              <DropdownMenuContent align="end" className="w-48">
                <DropdownMenuLabel>显示的列</DropdownMenuLabel>
                {columns.map((c) => (
                  <DropdownMenuItem
                    key={c.id}
                    onSelect={(e) => {
                      e.preventDefault()
                      toggle(c.id)
                    }}
                    className="gap-2"
                  >
                    <span className="flex h-4 w-4 items-center justify-center">
                      {visible.includes(c.id) && <Check className="h-3.5 w-3.5" />}
                    </span>
                    <span>{c.header}</span>
                  </DropdownMenuItem>
                ))}
              </DropdownMenuContent>
            </DropdownMenu>
          )}
        </div>
      )}

      <div className="overflow-x-auto rounded-xl border border-border/60 bg-card [transform:translateZ(0)]">
        <table className="console-table">
          <thead>
            <tr>
              {renderExpandedRow && <th className="w-8 px-1 text-center" />}
              {selectable && (
                <th className="w-9 pl-3">
                  <Checkbox
                    checked={allSelected}
                    onCheckedChange={toggleAll}
                    aria-label="全选当前页"
                  />
                </th>
              )}
              {ordered.map((c) => (
                <th
                  key={c.id}
                  title={c.hint}
                  className={c.align === 'right' ? 'text-right' : undefined}
                >
                  {c.header}
                </th>
              ))}
              {rowActions && <th className="w-px" />}
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => {
              const key = rowKey(row)
              const isSelected = selected?.has(key) ?? false
              const isExpanded = effectiveExpandedKeys.has(key)
              const canClickRow = Boolean(onRowActivate || (renderExpandedRow && expandOnRowClick))
              return (
                <React.Fragment key={key}>
                  <tr
                    data-selected={isSelected || undefined}
                    data-expanded={isExpanded || undefined}
                    className={cn(canClickRow && 'cursor-pointer')}
                    onClick={() => {
                      onRowActivate?.(row)
                      if (renderExpandedRow && expandOnRowClick) {
                        toggleExpand(key)
                      }
                    }}
                  >
                    {renderExpandedRow && (
                      <td
                        className={cn(
                          'w-8 px-1.5 text-center',
                          !selectable && tone && railClass(tone(row)),
                        )}
                        onClick={(e) => {
                          e.stopPropagation()
                          toggleExpand(key)
                        }}
                      >
                        <button
                          type="button"
                          className="inline-flex h-6 w-6 items-center justify-center rounded text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
                          title={isExpanded ? '收起详情' : '展开详情'}
                          aria-label={isExpanded ? '收起详情' : '展开详情'}
                        >
                          <ChevronRight
                            className={cn(
                              'h-3.5 w-3.5 transition-transform duration-200',
                              isExpanded && 'rotate-90 text-foreground',
                            )}
                          />
                        </button>
                      </td>
                    )}
                    {selectable && (
                      <td
                        className={cn(
                          'pl-3',
                          !renderExpandedRow && tone && railClass(tone(row)),
                        )}
                        onClick={(e) => e.stopPropagation()}
                      >
                        <Checkbox
                          checked={isSelected}
                          onCheckedChange={() => toggleOne(key)}
                          aria-label="选中此行"
                        />
                      </td>
                    )}
                    {ordered.map((c, ci) => (
                      <td
                        key={c.id}
                        className={cn(
                          c.align === 'right' && 'text-right',
                          // 无展开列与复选框列时，色轨落在第一个数据列上
                          !renderExpandedRow && !selectable && ci === 0 && tone && railClass(tone(row)),
                        )}
                      >
                        {c.cell(row)}
                      </td>
                    ))}
                    {rowActions && (
                      <td
                        className="pr-3 text-right"
                        onClick={(e) => e.stopPropagation()}
                      >
                        <div className="console-row-actions flex items-center justify-end gap-1">
                          {rowActions(row)}
                        </div>
                      </td>
                    )}
                  </tr>
                  {renderExpandedRow && isExpanded && (
                    <tr
                      key={`${key}-expanded`}
                      className="border-b border-border/60 bg-muted/15"
                    >
                      <td colSpan={colSpanTotal} className="p-0">
                        <div className="border-l-2 border-primary/40 bg-card p-4">
                          {renderExpandedRow(row)}
                        </div>
                      </td>
                    </tr>
                  )}
                </React.Fragment>
              )
            })}
          </tbody>
        </table>

        {loading && rows.length === 0 && (
          <div className="p-6 text-center text-[13px] text-muted-foreground">
            加载中…
          </div>
        )}
        {!loading && rows.length === 0 && (
          <div className="p-8 text-center text-[13px] text-muted-foreground">
            {empty ?? '没有匹配的记录'}
          </div>
        )}
      </div>
    </div>
  )
}
