export const THEME_STORAGE_KEY = 'adminTheme'

export type ThemeId =
  | 'system'
  | 'graphite'
  | 'ocean'
  | 'forest'
  | 'amber'

export type ThemeMode = 'light' | 'dark' | 'system'

export interface ThemeSelection {
  palette: ThemeId
  mode: ThemeMode
}

export interface ThemeMetadata {
  id: ThemeId
  name: string
  description: string
  swatch: string
}

export const DEFAULT_THEME_SELECTION: ThemeSelection = {
  palette: 'system',
  mode: 'system',
}

export const THEME_METADATA: readonly ThemeMetadata[] = [
  {
    id: 'system',
    name: 'sub2api 青',
    description: '官方网关标志性翡翠冷青',
    swatch: 'hsl(175 84% 32%)',
  },
  {
    id: 'graphite',
    name: '石墨工业',
    description: '极简克制的纯粹黑白中性',
    swatch: 'hsl(215 25% 27%)',
  },
  {
    id: 'ocean',
    name: '深海冷蓝',
    description: '深邃沉稳的工程冷蓝',
    swatch: 'hsl(221 83% 53%)',
  },
  {
    id: 'forest',
    name: '终端绿',
    description: '硬核复古终端翡翠绿',
    swatch: 'hsl(142 71% 45%)',
  },
  {
    id: 'amber',
    name: '工控琥珀',
    description: '清晰醒目的工业琥珀金',
    swatch: 'hsl(38 92% 50%)',
  },
] as const

const THEME_IDS = new Set<ThemeId>(THEME_METADATA.map(({ id }) => id))
const THEME_MODES = new Set<ThemeMode>(['light', 'dark', 'system'])
let transitionTimer: number | undefined

export function isThemeId(value: unknown): value is ThemeId {
  return typeof value === 'string' && THEME_IDS.has(value as ThemeId)
}

export function isThemeMode(value: unknown): value is ThemeMode {
  return typeof value === 'string' && THEME_MODES.has(value as ThemeMode)
}

export function parseThemeSelection(value: unknown): ThemeSelection {
  if (!value || typeof value !== 'object') return { ...DEFAULT_THEME_SELECTION }

  const candidate = value as { palette?: unknown; mode?: unknown }
  if (!isThemeId(candidate.palette) || !isThemeMode(candidate.mode)) {
    return { ...DEFAULT_THEME_SELECTION }
  }

  return { palette: candidate.palette, mode: candidate.mode }
}

export function resolveSystemDarkMode(): boolean {
  if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') {
    return false
  }
  return window.matchMedia('(prefers-color-scheme: dark)').matches
}

export function resolveDarkMode(selection: ThemeSelection): boolean {
  return selection.mode === 'dark' || (selection.mode === 'system' && resolveSystemDarkMode())
}

export function applyTheme(selection: ThemeSelection, isDark = resolveDarkMode(selection)): boolean {
  if (typeof document === 'undefined') return isDark

  const root = document.documentElement
  root.dataset.theme = selection.palette
  root.classList.toggle('dark', isDark)
  return isDark
}

export function applyThemeWithTransition(
  selection: ThemeSelection,
  isDark = resolveDarkMode(selection),
): boolean {
  if (typeof document === 'undefined') return isDark

  const root = document.documentElement
  root.classList.add('theme-transition')
  // Ensure the transition rule is committed before changing the custom properties.
  void root.offsetWidth
  const resolved = applyTheme(selection, isDark)
  if (typeof window !== 'undefined') {
    if (transitionTimer !== undefined) window.clearTimeout(transitionTimer)
    transitionTimer = window.setTimeout(() => {
      root.classList.remove('theme-transition')
      transitionTimer = undefined
    }, 260)
  } else {
    root.classList.remove('theme-transition')
  }
  return resolved
}
