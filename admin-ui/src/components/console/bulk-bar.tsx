import { X } from 'lucide-react'
import { Button } from '@/components/ui/button'

/**
 * 吸底批量操作栏。
 */
export function BulkBar({
  count,
  onClear,
  children,
  noun = '项',
}: {
  count: number
  onClear: () => void
  /** 批量动作按钮 */
  children: React.ReactNode
  /** 计数单位，如「个凭据」 */
  noun?: string
}) {
  if (count === 0) return null

  return (
    <div className="pointer-events-none sticky bottom-6 z-40 flex justify-center px-4 mt-8 sm:mt-10 mb-2">
      <div className="console-bulkbar console-scope pointer-events-auto grid w-full max-w-full grid-cols-[minmax(0,1fr)_auto] items-center gap-x-2 gap-y-1.5 rounded-full border border-border/80 bg-card/95 px-3.5 py-2 shadow-apple-xl backdrop-blur-2xl sm:flex sm:w-auto sm:flex-wrap sm:gap-3 sm:px-4 sm:py-2.5">
        <div className="order-1 col-span-2 flex min-w-0 items-center justify-center gap-6 sm:contents">
          <span className="flex h-8 min-w-0 items-center pl-1 text-xs font-medium whitespace-nowrap text-foreground/90 sm:order-none">
            已选 <span className="console-num font-bold text-primary">{count}</span> {noun}
          </span>
          <Button
            size="icon"
            variant="ghost"
            onClick={onClear}
            title="取消选择（Esc）"
            className="h-8 w-8 shrink-0 rounded-full text-muted-foreground hover:bg-accent hover:text-foreground sm:order-4"
          >
            <X className="h-4 w-4" />
          </Button>
        </div>
        <span className="mx-1 hidden h-4 w-px shrink-0 bg-border/70 sm:order-1 sm:block" />
        <div className="order-2 col-span-2 grid min-w-0 w-full grid-cols-2 items-center gap-1 min-[400px]:grid-cols-3 sm:order-2 sm:flex sm:w-auto sm:flex-none sm:gap-2.5">
          {children}
        </div>
        <span className="mx-1 hidden h-4 w-px shrink-0 bg-border/70 sm:order-3 sm:block" />
      </div>
    </div>
  )
}
