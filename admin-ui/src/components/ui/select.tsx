import * as React from 'react'
import * as DropdownMenuPrimitive from '@radix-ui/react-dropdown-menu'
import { Check, ChevronDown } from 'lucide-react'
import { cn } from '@/lib/utils'

/**
 * Select API 的单选下拉框，基于 DropdownMenu non-modal 分支实现。
 *
 * Radix Select 打开 Content 时会调用 hideOthers(content)，并把下拉层以外的
 * DOM 标记为 aria-hidden。Select 嵌在 Dialog 内时，Content 默认 Portal 到 body，
 * DialogContent 会被当作外部节点隐藏；此时焦点仍在 trigger 上，浏览器会打印
 * "Blocked aria-hidden on an element because its descendant retained focus"。
 *
 * DropdownMenu 在 modal={false} 下不执行这套 aria-hidden / body 锁逻辑，可以保持
 * Dialog 内下拉框的可访问性状态稳定，同时保留 value / onValueChange / disabled 这类
 * Select 组件 API。
 */

interface SelectContextValue {
  value?: string
  onValueChange?: (value: string) => void
  labels: Map<string, React.ReactNode>
  trigger: HTMLElement | null
  setTrigger: (node: HTMLElement | null) => void
}

interface SelectProps {
  value?: string
  onValueChange?: (value: string) => void
  disabled?: boolean
  children: React.ReactNode
}

interface SelectItemProps
  extends Omit<
    React.ComponentPropsWithoutRef<typeof DropdownMenuPrimitive.Item>,
    'onSelect'
  > {
  value: string
}

interface SelectContentProps
  extends React.ComponentPropsWithoutRef<typeof DropdownMenuPrimitive.Content> {
  container?: HTMLElement | null
}

const SelectContext = React.createContext<SelectContextValue | null>(null)
const SelectDisabledContext = React.createContext(false)
const SelectGroup = DropdownMenuPrimitive.Group

function useSelectContext() {
  const ctx = React.useContext(SelectContext)
  if (!ctx) throw new Error('Select 子组件必须在 <Select> 内使用')
  return ctx
}

function collectLabels(
  children: React.ReactNode,
  labels: Map<string, React.ReactNode>
) {
  React.Children.forEach(children, (child) => {
    if (!React.isValidElement(child)) return
    const type = child.type as { displayName?: string } | undefined
    if (type?.displayName === 'SelectItem') {
      const props = child.props as { value: string; children: React.ReactNode }
      labels.set(props.value, props.children)
      return
    }
    const props = child.props as { children?: React.ReactNode }
    if (props.children) collectLabels(props.children, labels)
  })
}

function Select({ value, onValueChange, disabled, children }: SelectProps) {
  const [trigger, setTrigger] = React.useState<HTMLElement | null>(null)
  const labels = React.useMemo(() => {
    const nextLabels = new Map<string, React.ReactNode>()
    collectLabels(children, nextLabels)
    return nextLabels
  }, [children])

  const contextValue = React.useMemo<SelectContextValue>(
    () => ({ value, onValueChange, labels, trigger, setTrigger }),
    [value, onValueChange, labels, trigger]
  )

  return (
    <SelectContext.Provider value={contextValue}>
      <DropdownMenuPrimitive.Root modal={false}>
        <SelectDisabledContext.Provider value={disabled ?? false}>
          {children}
        </SelectDisabledContext.Provider>
      </DropdownMenuPrimitive.Root>
    </SelectContext.Provider>
  )
}
Select.displayName = 'Select'

function SelectValue({ placeholder }: { placeholder?: React.ReactNode }) {
  const { value, labels } = useSelectContext()
  const label = value === undefined ? undefined : labels.get(value)
  if (label === undefined || label === null || label === '') {
    return <span className="text-muted-foreground">{placeholder}</span>
  }
  return <>{label}</>
}
SelectValue.displayName = 'SelectValue'

const SelectTrigger = React.forwardRef<
  React.ElementRef<typeof DropdownMenuPrimitive.Trigger>,
  React.ComponentPropsWithoutRef<typeof DropdownMenuPrimitive.Trigger>
>(({ className, children, ...props }, ref) => {
  const disabled = React.useContext(SelectDisabledContext)
  const { setTrigger } = useSelectContext()
  const triggerRef = React.useCallback(
    (node: React.ElementRef<typeof DropdownMenuPrimitive.Trigger> | null) => {
      setTrigger(node)
      if (typeof ref === 'function') ref(node)
      else if (ref) ref.current = node
    },
    [ref, setTrigger]
  )

  return (
    <DropdownMenuPrimitive.Trigger
      ref={triggerRef}
      disabled={disabled}
      className={cn(
        'flex h-8 w-full items-center justify-between gap-2 rounded-md border border-border/70 bg-background px-2.5 text-[13px]',
        'focus:outline-none focus:ring-2 focus:ring-ring',
        'disabled:cursor-not-allowed disabled:opacity-50 [&>span]:truncate',
        className
      )}
      {...props}
    >
      {children}
      <ChevronDown className="h-4 w-4 shrink-0 opacity-60" />
    </DropdownMenuPrimitive.Trigger>
  )
})
SelectTrigger.displayName = 'SelectTrigger'

const SelectContent = React.forwardRef<
  React.ElementRef<typeof DropdownMenuPrimitive.Content>,
  SelectContentProps
>(({ className, children, container, sideOffset = 6, ...props }, ref) => {
  const { trigger } = useSelectContext()
  const dialog = trigger?.closest('[role="dialog"]') as HTMLElement | null

  return (
    <DropdownMenuPrimitive.Portal container={container ?? dialog ?? undefined}>
      <DropdownMenuPrimitive.Content
        ref={ref}
        sideOffset={sideOffset}
        className={cn(
          'z-50 max-h-72 min-w-[var(--radix-dropdown-menu-trigger-width)] overflow-y-auto rounded-2xl border border-border/60 bg-popover/90 p-1.5 text-popover-foreground shadow-apple-lg backdrop-blur-2xl backdrop-saturate-150',
          'data-[state=open]:animate-in data-[state=closed]:animate-out data-[state=closed]:fade-out-0 data-[state=open]:fade-in-0 data-[state=closed]:zoom-out-95 data-[state=open]:zoom-in-95',
          className
        )}
        {...props}
      >
        {children}
      </DropdownMenuPrimitive.Content>
    </DropdownMenuPrimitive.Portal>
  )
})
SelectContent.displayName = 'SelectContent'

const SelectLabel = React.forwardRef<
  React.ElementRef<typeof DropdownMenuPrimitive.Label>,
  React.ComponentPropsWithoutRef<typeof DropdownMenuPrimitive.Label>
>(({ className, ...props }, ref) => (
  <DropdownMenuPrimitive.Label
    ref={ref}
    className={cn(
      'px-2.5 pb-1 pt-1.5 text-[11px] font-semibold uppercase tracking-wider text-muted-foreground',
      className
    )}
    {...props}
  />
))
SelectLabel.displayName = 'SelectLabel'

const SelectItem = React.forwardRef<
  React.ElementRef<typeof DropdownMenuPrimitive.Item>,
  SelectItemProps
>(({ className, children, value, ...props }, ref) => {
  const { value: selected, onValueChange } = useSelectContext()
  const isSelected = selected === value

  return (
    <DropdownMenuPrimitive.Item
      ref={ref}
      onSelect={() => onValueChange?.(value)}
      className={cn(
        'relative flex cursor-default select-none items-center rounded-lg py-1.5 pl-7 pr-2.5 text-sm outline-none transition-colors',
        'focus:bg-accent focus:text-accent-foreground data-[disabled]:pointer-events-none data-[disabled]:opacity-40',
        className
      )}
      {...props}
    >
      {isSelected && (
        <span className="absolute left-2 flex h-3.5 w-3.5 items-center justify-center">
          <Check className="h-4 w-4" />
        </span>
      )}
      {children}
    </DropdownMenuPrimitive.Item>
  )
})
SelectItem.displayName = 'SelectItem'

export {
  Select,
  SelectGroup,
  SelectValue,
  SelectTrigger,
  SelectContent,
  SelectItem,
  SelectLabel,
}
