import { Check, Monitor, Moon, Palette, Sun } from 'lucide-react'
import { Button } from '@/components/ui/button'
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu'
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from '@/components/ui/tooltip'
import {
  THEME_METADATA,
  type ThemeId,
  type ThemeMode,
  type ThemeSelection,
} from '@/lib/theme'

const MODE_OPTIONS: readonly {
  id: ThemeMode
  label: string
  icon: typeof Monitor
}[] = [
  { id: 'system', label: '跟随系统', icon: Monitor },
  { id: 'light', label: '浅色', icon: Sun },
  { id: 'dark', label: '深色', icon: Moon },
]

interface ThemePickerProps {
  theme: ThemeSelection
  isDarkMode: boolean
  onSelectPalette: (palette: ThemeId) => void
  onSelectMode: (mode: ThemeMode) => void
}

export function ThemePicker({
  theme,
  isDarkMode,
  onSelectPalette,
  onSelectMode,
}: ThemePickerProps) {
  const palette = THEME_METADATA.find((item) => item.id === theme.palette) ?? THEME_METADATA[0]
  const modeLabel = MODE_OPTIONS.find((item) => item.id === theme.mode)?.label ?? '跟随系统'
  const title = `主题：${palette.name} · ${modeLabel}${theme.mode === 'system' ? `（当前${isDarkMode ? '深色' : '浅色'}）` : ''}`

  return (
    <TooltipProvider delayDuration={350}>
      <DropdownMenu modal={false}>
        <Tooltip>
          <TooltipTrigger asChild>
            <DropdownMenuTrigger asChild>
              <Button
                variant="ghost"
                size="icon"
                aria-label={title}
                className="theme-picker-trigger"
              >
                <Palette className="h-4 w-4" />
              </Button>
            </DropdownMenuTrigger>
          </TooltipTrigger>
          <TooltipContent>{title}</TooltipContent>
        </Tooltip>
        <DropdownMenuContent align="end" className="w-64">
          <DropdownMenuLabel>配色主题</DropdownMenuLabel>
          {THEME_METADATA.map((item) => (
            <DropdownMenuItem
              key={item.id}
              role="menuitemradio"
              aria-checked={theme.palette === item.id}
              onSelect={() => onSelectPalette(item.id)}
              className="gap-2.5"
            >
              <span
                aria-hidden="true"
                className="size-3 shrink-0 rounded-full ring-1 ring-black/10 dark:ring-white/20"
                style={{ backgroundColor: item.swatch }}
              />
              <span className="min-w-0 flex-1">
                <span className="block text-sm">{item.name}</span>
                <span className="block truncate text-[11px] text-muted-foreground">
                  {item.description}
                </span>
              </span>
              {theme.palette === item.id && <Check className="size-4 text-primary" aria-hidden="true" />}
            </DropdownMenuItem>
          ))}
          <DropdownMenuSeparator />
          <DropdownMenuLabel>明暗模式</DropdownMenuLabel>
          {MODE_OPTIONS.map(({ id, label, icon: Icon }) => (
            <DropdownMenuItem
              key={id}
              role="menuitemradio"
              aria-checked={theme.mode === id}
              onSelect={() => onSelectMode(id)}
            >
              <Icon aria-hidden="true" />
              <span className="flex-1">{label}</span>
              {theme.mode === id && <Check className="size-4 text-primary" aria-hidden="true" />}
            </DropdownMenuItem>
          ))}
        </DropdownMenuContent>
      </DropdownMenu>
    </TooltipProvider>
  )
}
