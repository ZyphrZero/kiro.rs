export const THEME_STORAGE_KEY = 'adminTheme'

export type ThemeId =
  | 'system'
  | 'ocean'
  | 'forest'
  | 'violet'
  | 'amber'
  | 'rose'

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
    name: '系统蓝',
    description: '清爽中性的系统蓝',
    swatch: 'hsl(211 100% 50%)',
  },
  {
    id: 'ocean',
    name: '海洋青',
    description: '冷静通透的海洋青',
    swatch: 'hsl(193 84% 42%)',
  },
  {
    id: 'forest',
    name: '森林绿',
    description: '稳定自然的森林绿',
    swatch: 'hsl(151 60% 38%)',
  },
  {
    id: 'violet',
    name: '紫罗兰',
    description: '沉静鲜明的紫罗兰',
    swatch: 'hsl(262 72% 56%)',
  },
  {
    id: 'amber',
    name: '琥珀',
    description: '温暖醒目的琥珀色',
    swatch: 'hsl(38 92% 50%)',
  },
  {
    id: 'rose',
    name: '玫瑰',
    description: '柔和有力的玫瑰色',
    swatch: 'hsl(347 75% 52%)',
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
