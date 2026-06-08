import * as React from 'react'
import { Slot } from '@radix-ui/react-slot'
import { cva, type VariantProps } from 'class-variance-authority'
import { cn } from '@/lib/utils'

const buttonVariants = cva(
  'inline-flex items-center justify-center gap-1.5 whitespace-nowrap rounded-full text-sm font-medium transition-all duration-150 ease-apple focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring/45 disabled:pointer-events-none disabled:opacity-40 active:scale-[0.97] [&_svg]:pointer-events-none [&_svg]:size-4 [&_svg]:shrink-0',
  {
    variants: {
      variant: {
        default:
          'bg-primary text-primary-foreground shadow-apple-sm hover:bg-primary/90',
        destructive:
          'bg-destructive text-destructive-foreground shadow-apple-sm hover:bg-destructive/90',
        outline:
          'border border-border bg-card/60 backdrop-blur text-foreground hover:bg-accent hover:text-accent-foreground hover:border-accent',
        secondary:
          'bg-secondary text-secondary-foreground hover:bg-secondary/70',
        ghost:
          'hover:bg-secondary hover:text-foreground text-muted-foreground',
        link: 'text-primary underline-offset-4 hover:underline rounded-none active:scale-100',
      },
      size: {
        default: 'h-9 px-4 py-2',
        sm: 'h-8 px-3 text-[13px]',
        lg: 'h-11 px-6 text-base',
        icon: 'h-9 w-9 rounded-full',
      },
    },
    defaultVariants: {
      variant: 'default',
      size: 'default',
    },
  }
)

export interface ButtonProps
  extends React.ButtonHTMLAttributes<HTMLButtonElement>,
    VariantProps<typeof buttonVariants> {
  asChild?: boolean
}

const Button = React.forwardRef<HTMLButtonElement, ButtonProps>(
  ({ className, variant, size, asChild = false, ...props }, ref) => {
    const Comp = asChild ? Slot : 'button'
    return (
      <Comp className={cn(buttonVariants({ variant, size, className }))} ref={ref} {...props} />
    )
  }
)
Button.displayName = 'Button'

export { Button, buttonVariants }
