import * as React from 'react'
import { cva, type VariantProps } from 'class-variance-authority'
import { cn } from '@/lib/utils'

const badgeVariants = cva(
  'inline-flex items-center rounded border px-1.5 py-0.5 text-[11px] font-medium tracking-tight transition-colors focus:outline-none focus:ring-1 focus:ring-ring',
  {
    variants: {
      variant: {
        default:
          'border-primary/20 bg-primary/10 text-primary',
        secondary:
          'border-border bg-secondary text-secondary-foreground',
        destructive:
          'border-destructive/30 bg-destructive/10 text-destructive',
        outline:
          'border-border bg-transparent text-foreground/85',
        success:
          'border-emerald-500/20 bg-emerald-500/10 text-emerald-600 dark:text-emerald-400',
        warning:
          'border-amber-500/20 bg-amber-500/10 text-amber-600 dark:text-amber-400',
      },
    },
    defaultVariants: {
      variant: 'default',
    },
  }
)

export interface BadgeProps
  extends React.HTMLAttributes<HTMLDivElement>,
    VariantProps<typeof badgeVariants> {}

function Badge({ className, variant, ...props }: BadgeProps) {
  return <div className={cn(badgeVariants({ variant }), className)} {...props} />
}

export { Badge, badgeVariants }
