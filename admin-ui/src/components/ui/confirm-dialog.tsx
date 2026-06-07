import * as React from 'react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogFooter,
  DialogTitle,
  DialogDescription,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'

/** 单次确认的配置项 */
export interface ConfirmOptions {
  title?: string
  description: React.ReactNode
  /** 确认按钮文案，默认「确认」 */
  confirmText?: string
  /** 取消按钮文案，默认「取消」 */
  cancelText?: string
  /** 确认按钮是否使用危险（红色）样式，删除类操作应设为 true */
  destructive?: boolean
}

type ConfirmFn = (options: ConfirmOptions) => Promise<boolean>

const ConfirmContext = React.createContext<ConfirmFn | null>(null)

/** 命令式确认：返回 Promise<boolean>，与原生 confirm() 控制流一致。 */
export function useConfirm(): ConfirmFn {
  const ctx = React.useContext(ConfirmContext)
  if (!ctx) {
    throw new Error('useConfirm 必须在 <ConfirmProvider> 内使用')
  }
  return ctx
}

interface PendingState {
  options: ConfirmOptions
  resolve: (value: boolean) => void
}

/** 全局确认弹窗 Provider：挂在 App 根，子树内任意组件用 useConfirm() 调起。 */
export function ConfirmProvider({ children }: { children: React.ReactNode }) {
  const [pending, setPending] = React.useState<PendingState | null>(null)

  const confirm = React.useCallback<ConfirmFn>((options) => {
    return new Promise<boolean>((resolve) => {
      setPending({ options, resolve })
    })
  }, [])

  // 关闭时落定结果：确认 → true，其余（取消 / 点遮罩 / Esc）→ false。
  const settle = React.useCallback(
    (value: boolean) => {
      setPending((prev) => {
        prev?.resolve(value)
        return null
      })
    },
    []
  )

  const opts = pending?.options
  const destructive = opts?.destructive ?? false

  return (
    <ConfirmContext.Provider value={confirm}>
      {children}
      <Dialog
        open={pending !== null}
        onOpenChange={(open) => {
          if (!open) settle(false)
        }}
      >
        <DialogContent className="sm:max-w-sm">
          <DialogHeader>
            <DialogTitle>{opts?.title ?? '请确认'}</DialogTitle>
            <DialogDescription>{opts?.description}</DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => settle(false)}>
              {opts?.cancelText ?? '取消'}
            </Button>
            <Button
              variant={destructive ? 'destructive' : 'default'}
              onClick={() => settle(true)}
            >
              {opts?.confirmText ?? '确认'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </ConfirmContext.Provider>
  )
}

