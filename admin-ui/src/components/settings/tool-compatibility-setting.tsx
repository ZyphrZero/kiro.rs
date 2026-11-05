import { Loader2 } from 'lucide-react'
import { toast } from 'sonner'
import type { ToolCompatibilityMode } from '@/api/credentials'
import { SettingGroup } from '@/components/console/setting-row'
import { Button } from '@/components/ui/button'
import { useSetToolCompatibilityMode, useToolCompatibilityMode } from '@/hooks/use-credentials'
import { extractErrorMessage } from '@/lib/utils'

export function ToolCompatibilitySetting() {
  const query = useToolCompatibilityMode()
  const mutation = useSetToolCompatibilityMode()
  const mode = query.data?.mode
  const disabled = mutation.isPending || query.isError || !mode

  const changeMode = (next: ToolCompatibilityMode) => {
    if (disabled || next === mode) return
    mutation.mutate(next, {
      onSuccess: () => toast.success(`工具兼容模式已切换为 ${next}，对新请求生效`),
    })
  }

  return (
    <SettingGroup
      title="工具兼容模式"
      description="控制是否将客户端工具名称、参数和 schema 改写为 Kiro 内置格式。保存后对新请求立即生效，已在途请求保持原模式，无需重启。"
    >
      <div className="space-y-3">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <p className="text-sm" role="status">
            {query.isError ? '当前模式读取失败' : mode ? <>当前生效：<code>{mode}</code></> : '正在读取当前模式…'}
          </p>
          <div className="flex flex-wrap gap-2" role="group" aria-label="选择工具兼容模式">
            <Button
              type="button"
              size="sm"
              variant={mode === 'raw' ? 'default' : 'outline'}
              aria-pressed={mode === 'raw'}
              disabled={disabled}
              onClick={() => changeMode('raw')}
            >
              raw（推荐）
            </Button>
            <Button
              type="button"
              size="sm"
              variant={mode === 'claude-code' ? 'default' : 'outline'}
              aria-pressed={mode === 'claude-code'}
              disabled={disabled}
              onClick={() => changeMode('claude-code')}
            >
              claude-code（旧版兼容）
            </Button>
          </div>
        </div>
        <div className="space-y-1 text-xs leading-relaxed text-muted-foreground">
          <p><code>raw</code>：保留客户端工具名、描述、schema 和参数；仍执行上游要求的长度限制与 schema 规范化，不是字节级透传。</p>
          <p><code>claude-code</code>：改写 Write / Edit / Bash / Read 等内置工具名与参数，可能丢弃自定义字段；仅在确实需要旧映射时使用。</p>
        </div>
        {mutation.isPending && (
          <p className="flex items-center gap-2 text-xs text-muted-foreground" role="status">
            <Loader2 className="h-3.5 w-3.5 animate-spin" />正在保存…
          </p>
        )}
        {query.isError && (
          <div className="flex flex-wrap items-center gap-2 text-sm text-destructive" role="alert">
            <span>读取失败：{extractErrorMessage(query.error)}</span>
            <Button type="button" size="sm" variant="outline" disabled={query.isFetching} onClick={() => void query.refetch()}>重试</Button>
          </div>
        )}
        {mutation.isError && (
          <p className="text-sm text-destructive" role="alert">保存失败：{extractErrorMessage(mutation.error)}。请重试；如连接中断，请刷新确认服务器当前模式。</p>
        )}
      </div>
    </SettingGroup>
  )
}
