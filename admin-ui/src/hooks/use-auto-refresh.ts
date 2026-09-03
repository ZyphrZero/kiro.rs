import { useCallback, useEffect, useRef, useState } from 'react'

export const AUTO_REFRESH_INTERVAL_OPTIONS = [5, 10, 15, 30] as const
export type AutoRefreshInterval = (typeof AUTO_REFRESH_INTERVAL_OPTIONS)[number]

interface UseAutoRefreshOptions {
  onRefresh: () => void | Promise<unknown>
  isRefreshing?: boolean
  enabled?: boolean
  defaultInterval?: AutoRefreshInterval
}

/**
 * 页面级自动刷新状态机。刷新回调和 loading 状态都通过 ref 读取，避免每次请求
 * 开始/结束都重建计时器；自动刷新不会与当前请求重叠。
 */
export function useAutoRefresh({
  onRefresh,
  isRefreshing = false,
  enabled = true,
  defaultInterval = 30,
}: UseAutoRefreshOptions) {
  const onRefreshRef = useRef(onRefresh)
  const isRefreshingRef = useRef(isRefreshing)
  const remainingRef = useRef(defaultInterval)
  const [isEnabled, setIsEnabled] = useState(enabled)
  const [interval, setIntervalValue] = useState<AutoRefreshInterval>(defaultInterval)
  const [secondsRemaining, setSecondsRemaining] = useState(defaultInterval)

  useEffect(() => {
    setIsEnabled(enabled)
  }, [enabled])

  useEffect(() => {
    onRefreshRef.current = onRefresh
  }, [onRefresh])

  useEffect(() => {
    isRefreshingRef.current = isRefreshing
  }, [isRefreshing])

  useEffect(() => {
    if (!isEnabled) {
      remainingRef.current = interval
      setSecondsRemaining(interval)
      return
    }

    remainingRef.current = interval
    setSecondsRemaining(interval)
    const timer = window.setInterval(() => {
      remainingRef.current -= 1
      if (remainingRef.current <= 0) {
        remainingRef.current = interval
        if (!isRefreshingRef.current) {
          void onRefreshRef.current()
        }
      }
      setSecondsRemaining(remainingRef.current)
    }, 1000)

    return () => window.clearInterval(timer)
  }, [interval, isEnabled])

  const toggle = useCallback(() => {
    setIsEnabled((current) => !current)
    remainingRef.current = interval
    setSecondsRemaining(interval)
  }, [interval])

  const reset = useCallback(() => {
    remainingRef.current = interval
    setSecondsRemaining(interval)
  }, [interval])

  const updateInterval = useCallback((next: AutoRefreshInterval) => {
    remainingRef.current = next
    setIntervalValue(next)
    setSecondsRemaining(next)
    setIsEnabled(true)
  }, [])

  return {
    isEnabled,
    interval,
    secondsRemaining,
    toggle,
    reset,
    updateInterval,
  }
}
