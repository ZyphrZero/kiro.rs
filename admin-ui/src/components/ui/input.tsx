import * as React from 'react'
import { cn } from '@/lib/utils'

export interface InputProps extends React.InputHTMLAttributes<HTMLInputElement> {}

const Input = React.forwardRef<HTMLInputElement, InputProps>(
  ({ className, type, ...props }, ref) => {
    return (
      <input
        type={type}
        className={cn(
          'flex h-8 w-full rounded-md border border-input bg-background px-3 py-1.5 text-xs ring-offset-background transition-colors duration-100 ease-out',
          'file:border-0 file:bg-transparent file:text-xs file:font-medium file:text-foreground',
          'placeholder:text-muted-foreground/60',
          'hover:border-border/80 focus-visible:outline-none focus-visible:border-primary focus-visible:ring-1 focus-visible:ring-ring focus-visible:bg-background',
          'disabled:cursor-not-allowed disabled:opacity-50',
          className
        )}
        ref={ref}
        {...props}
      />
    )
  }
)
Input.displayName = 'Input'

export { Input }
