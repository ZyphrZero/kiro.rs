import { useEffect, useRef } from 'react'

/** 框选起点超过该像素阈值才进入"拖拽框选"模式，避免误把普通点击识别成框选。 */
const DRAG_THRESHOLD = 5

export interface UseRectSelectOptions {
  containerRef: React.RefObject<HTMLElement | null>
  /** 命中元素的 CSS 选择器；要求该元素带有 data-{idAttribute}，存放 number id */
  itemSelector: string
  /** 数据属性名（不带 `data-` 前缀），从命中元素读取 id */
  idAttribute: string
  /** 框选完成时的回调，参数是命中元素的 id 集合 + 是否按住了 Ctrl/Meta */
  onSelectionChange: (ids: Set<number>, additive: boolean) => void
  enabled?: boolean
}

/**
 * 在指定容器内启用鼠标左键拖拽框选。
 *
 * 性能设计：
 * - 拖拽过程中的虚线矩形通过独立的 DOM Overlay 节点直接更新样式，
 *   彻底避免在 mousemove 时调用 React setState 导致父组件 60fps 全量重渲染。
 * - 仅在按下的不是按钮 / 输入框 / 下拉等交互元素时才启动。
 * - 按住 Ctrl/Meta 框选时附加到既有选区，否则替换。
 */
export function useRectSelect(options: UseRectSelectOptions): void {
  const { containerRef, itemSelector, idAttribute, onSelectionChange, enabled = true } = options
  const onSelectionChangeRef = useRef(onSelectionChange)
  onSelectionChangeRef.current = onSelectionChange

  const startRef = useRef<{ x: number; y: number; additive: boolean } | null>(null)
  const activeRef = useRef(false)
  const overlayRef = useRef<HTMLDivElement | null>(null)

  useEffect(() => {
    if (!enabled) return
    const el = containerRef.current
    if (!el) return

    const isInteractive = (target: EventTarget | null): boolean => {
      if (!(target instanceof HTMLElement)) return false
      return Boolean(
        target.closest(
          'button, a, input, select, textarea, label, [role="checkbox"], [role="menuitem"], [data-no-rect-select]'
        )
      )
    }

    const dataAttr = `data-${idAttribute}`

    const getOrCreateOverlay = (): HTMLDivElement => {
      if (!overlayRef.current) {
        const overlay = document.createElement('div')
        overlay.className =
          'pointer-events-none fixed z-50 rounded-sm border border-primary/70 bg-primary/15'
        overlay.style.display = 'none'
        document.body.appendChild(overlay)
        overlayRef.current = overlay
      }
      return overlayRef.current
    }

    const onMouseDown = (e: MouseEvent) => {
      if (e.button !== 0) return
      if (isInteractive(e.target)) return
      startRef.current = { x: e.clientX, y: e.clientY, additive: e.ctrlKey || e.metaKey }
      activeRef.current = false
    }

    const onMouseMove = (e: MouseEvent) => {
      const start = startRef.current
      if (!start) return
      const dx = e.clientX - start.x
      const dy = e.clientY - start.y

      if (!activeRef.current) {
        if (Math.abs(dx) < DRAG_THRESHOLD && Math.abs(dy) < DRAG_THRESHOLD) return
        activeRef.current = true
        document.body.style.userSelect = 'none'
      }

      const left = Math.min(e.clientX, start.x)
      const top = Math.min(e.clientY, start.y)
      const width = Math.abs(dx)
      const height = Math.abs(dy)

      const overlay = getOrCreateOverlay()
      overlay.style.display = 'block'
      overlay.style.left = `${left}px`
      overlay.style.top = `${top}px`
      overlay.style.width = `${width}px`
      overlay.style.height = `${height}px`
    }

    const onMouseUp = (e: MouseEvent) => {
      const start = startRef.current
      startRef.current = null
      if (overlayRef.current) {
        overlayRef.current.style.display = 'none'
      }
      if (!start) return
      if (!activeRef.current) return // 普通点击不改变选区
      activeRef.current = false
      document.body.style.userSelect = ''

      const left = Math.min(e.clientX, start.x)
      const top = Math.min(e.clientY, start.y)
      const right = Math.max(e.clientX, start.x)
      const bottom = Math.max(e.clientY, start.y)

      const items = el.querySelectorAll<HTMLElement>(itemSelector)
      const hits = new Set<number>()
      items.forEach((item) => {
        const r = item.getBoundingClientRect()
        const overlap = r.left < right && r.right > left && r.top < bottom && r.bottom > top
        if (!overlap) return
        const raw = item.getAttribute(dataAttr)
        const id = raw ? Number(raw) : NaN
        if (Number.isFinite(id)) hits.add(id)
      })

      onSelectionChangeRef.current(hits, start.additive)
    }

    el.addEventListener('mousedown', onMouseDown)
    window.addEventListener('mousemove', onMouseMove)
    window.addEventListener('mouseup', onMouseUp)

    return () => {
      el.removeEventListener('mousedown', onMouseDown)
      window.removeEventListener('mousemove', onMouseMove)
      window.removeEventListener('mouseup', onMouseUp)
      document.body.style.userSelect = ''
      if (overlayRef.current) {
        overlayRef.current.remove()
        overlayRef.current = null
      }
    }
  }, [containerRef, itemSelector, idAttribute, enabled])
}

