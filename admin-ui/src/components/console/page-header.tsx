import type { ReactNode } from 'react'
import { ChevronRight } from 'lucide-react'
import { cn } from '@/lib/utils'

export interface BreadcrumbItem {
  label: string
  onClick?: () => void
  active?: boolean
}

export interface PageHeaderProps {
  /** 面包屑列表（若不提供，则根据 title 默认生成两级面包屑：控制台 / title） */
  breadcrumbs?: BreadcrumbItem[]
  /** 业务图标 */
  icon?: ReactNode
  /** 模块主标题 */
  title: ReactNode
  /** 紧跟标题的状态 Badge 或计数 */
  badge?: ReactNode
  /** 辅助说明文案 */
  description?: ReactNode
  /** 兼容旧版 meta 插槽 */
  meta?: ReactNode
  /** 右侧操作动作插槽 */
  actions?: ReactNode
  className?: string
}

export function PageHeader({
  actions,
  badge,
  breadcrumbs,
  className,
  description,
  icon,
  meta,
  title,
}: PageHeaderProps) {
  // 默认面包屑结构
  const effectiveCrumbs: BreadcrumbItem[] =
    breadcrumbs && breadcrumbs.length > 0
      ? breadcrumbs
      : [
          { label: '控制台' },
          { label: typeof title === 'string' ? title : '', active: true },
        ]

  return (
    <div
      className={cn(
        'flex flex-col gap-2 pb-3.5 mb-4 border-b border-border/50',
        className,
      )}
    >
      <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-3 min-w-0">
        {/* 左侧：图标底座 + 一体化层级面包屑标题 + 状态徽标 */}
        <div className="flex items-center gap-2.5 min-w-0 flex-wrap">
          {icon && (
            <span
              className="flex size-7 shrink-0 items-center justify-center rounded-md bg-primary/10 text-primary border border-primary/20 shadow-2xs [&>svg]:size-3.5"
              aria-hidden="true"
            >
              {icon}
            </span>
          )}

          {/* 面包屑导航与主标题一体化结构 */}
          <nav
            aria-label="Breadcrumb"
            className="flex items-center gap-1.5 text-sm font-semibold tracking-tight text-foreground min-w-0"
          >
            {effectiveCrumbs.map((crumb, idx) => {
              const isLast = idx === effectiveCrumbs.length - 1
              return (
                <div key={idx} className="flex items-center gap-1.5 min-w-0">
                  {idx > 0 && (
                    <ChevronRight className="size-3 text-muted-foreground/50 shrink-0 -translate-y-[0.5px]" />
                  )}
                  {crumb.onClick && !isLast ? (
                    <button
                      type="button"
                      onClick={crumb.onClick}
                      className="text-muted-foreground hover:text-foreground font-normal transition-colors truncate focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring rounded-xs active:scale-[0.96] transition-transform duration-75"
                    >
                      {crumb.label}
                    </button>
                  ) : (
                    <span
                      className={cn(
                        'truncate text-balance',
                        isLast ? 'text-foreground font-semibold' : 'text-muted-foreground font-normal',
                      )}
                    >
                      {crumb.label}
                    </span>
                  )}
                </div>
              )
            })}
          </nav>

          {badge && (
            <div className="shrink-0 font-mono text-[11px] tabular-nums">
              {badge}
            </div>
          )}
          {meta}
        </div>

        {/* 右侧统一操作动作区 */}
        {actions && (
          <div className="flex shrink-0 items-center gap-2 flex-wrap sm:self-center">
            {actions}
          </div>
        )}
      </div>

      {/* 辅助说明（单行紧凑呈现） */}
      {description && (
        <p className="text-xs text-muted-foreground leading-normal max-w-4xl text-pretty">
          {description}
        </p>
      )}
    </div>
  )
}
