import { useState, useMemo } from 'react'
import { toast } from 'sonner'
import {
  Plus, KeyRound, Trash2, Copy, Eye, EyeOff, Power, RotateCcw, Pencil, RefreshCw, Loader2,
} from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Input } from '@/components/ui/input'
import {
  DropdownMenu, DropdownMenuTrigger, DropdownMenuContent, DropdownMenuItem,
} from '@/components/ui/dropdown-menu'
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter, DialogDescription,
} from '@/components/ui/dialog'
import {
  useClientKeys, useCreateClientKey, useDeleteClientKey,
  useSetClientKeyDisabled, useResetClientKeyStats, useUpdateClientKey,
  useRotateClientKey, useSetClientKeyMaxCredits,
} from '@/hooks/use-client-keys'
import { useGroupOptions } from '@/hooks/use-groups'
import { GroupSingleSelect } from '@/components/group-select'
import type { ClientKeyItem, CreateClientKeyResponse } from '@/types/api'
import { extractErrorMessage, formatCredits } from '@/lib/utils'
import { useConfirm } from '@/components/ui/confirm-dialog'
import { ConsoleTable, type ConsoleColumn } from '@/components/console/data-table'
import { BulkBar } from '@/components/console/bulk-bar'

function formatTokens(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(2) + 'M'
  if (n >= 1_000) return (n / 1_000).toFixed(1) + 'K'
  return n.toString()
}

/**
 * 解析积分上限输入框：
 * - 空字符串 → null（不限制）
 * - 合法非负数 → number
 * - 其它 → 'invalid'
 */
function parseMaxCreditsInput(raw: string): number | null | 'invalid' {
  const t = raw.trim()
  if (t === '') return null
  const n = Number(t)
  if (!Number.isFinite(n) || n < 0) return 'invalid'
  return n
}

/** 渲染「已用积分 / 上限」列。无上限时仅显示已用量。 */
function CreditsUsage({ used, max }: { used: number; max?: number }) {
  if (max == null) {
    return (
      <span className="text-[12px] tabular-nums text-muted-foreground">
        {formatCredits(used)} <span className="text-[11px]">/ 无限制</span>
      </span>
    )
  }
  const ratio = max > 0 ? used / max : 1
  const over = used >= max
  const color = over ? 'text-destructive' : ratio >= 0.8 ? 'text-amber-500' : 'text-foreground'
  return (
    <span className={`text-[12px] tabular-nums ${color}`} title={`已用 ${used} / 上限 ${max}`}>
      {formatCredits(used)} <span className="text-[11px] text-muted-foreground">/ {formatCredits(max)}</span>
    </span>
  )
}

function formatRelative(ts?: string): string {
  if (!ts) return '从未使用'
  const t = new Date(ts).getTime()
  const diff = Date.now() - t
  if (diff < 60_000) return '刚刚'
  if (diff < 3600_000) return `${Math.floor(diff / 60_000)} 分钟前`
  if (diff < 86400_000) return `${Math.floor(diff / 3600_000)} 小时前`
  return `${Math.floor(diff / 86400_000)} 天前`
}

export function ClientKeysPage() {
  const { data, isLoading } = useClientKeys()
  // 已注册分组列表（来自 groups.json 注册表，与凭据的 groups 字段解耦）
  const groupOptions = useGroupOptions()
  const createKey = useCreateClientKey()
  const deleteKey = useDeleteClientKey()
  const setDisabled = useSetClientKeyDisabled()
  const resetStats = useResetClientKeyStats()
  const updateKey = useUpdateClientKey()
  const rotateKey = useRotateClientKey()
  const setMaxCredits = useSetClientKeyMaxCredits()
  const confirm = useConfirm()

  const [createOpen, setCreateOpen] = useState(false)
  const [createName, setCreateName] = useState('')
  const [createDesc, setCreateDesc] = useState('')
  const [createGroup, setCreateGroup] = useState('')
  const [createMaxCredits, setCreateMaxCredits] = useState('')
  const [createdKey, setCreatedKey] = useState<CreateClientKeyResponse | null>(null)
  const [showCreatedPlain, setShowCreatedPlain] = useState(true)

  const [editOpen, setEditOpen] = useState(false)
  const [editTarget, setEditTarget] = useState<ClientKeyItem | null>(null)
  const [editName, setEditName] = useState('')
  const [editDesc, setEditDesc] = useState('')
  const [editGroup, setEditGroup] = useState('')
  const [editMaxCredits, setEditMaxCredits] = useState('')

  const handleCreate = async (e: React.FormEvent) => {
    e.preventDefault()
    const name = createName.trim()
    if (!name) {
      toast.error('请填写名称')
      return
    }
    const maxCredits = parseMaxCreditsInput(createMaxCredits)
    if (maxCredits === 'invalid') {
      toast.error('积分上限必须是非负数')
      return
    }
    try {
      const res = await createKey.mutateAsync({
        name,
        description: createDesc.trim() || undefined,
        group: createGroup.trim() || undefined,
        maxCredits: maxCredits ?? undefined,
      })
      setCreatedKey(res)
      setCreateOpen(false)
      setCreateName('')
      setCreateDesc('')
      setCreateGroup('')
      setCreateMaxCredits('')
      setShowCreatedPlain(true)
    } catch (err) {
      toast.error('创建失败：' + extractErrorMessage(err))
    }
  }

  const handleDelete = async (item: ClientKeyItem) => {
    if (
      !(await confirm({
        title: '确认删除 Key',
        description: `确认删除 Key "${item.name}"？此操作无法撤销。`,
        confirmText: '确认删除',
        destructive: true,
      }))
    )
      return
    try {
      await deleteKey.mutateAsync(item.id)
      toast.success(`已删除 Key #${item.id}`)
    } catch (err) {
      toast.error('删除失败：' + extractErrorMessage(err))
    }
  }

  const handleToggleDisabled = async (item: ClientKeyItem) => {
    try {
      await setDisabled.mutateAsync({ id: item.id, disabled: !item.disabled })
      toast.success(item.disabled ? '已启用' : '已禁用')
    } catch (err) {
      toast.error('操作失败：' + extractErrorMessage(err))
    }
  }

  const handleReset = async (item: ClientKeyItem) => {
    if (
      !(await confirm({
        title: '重置统计',
        description: `重置 Key "${item.name}" 的累计统计？`,
        confirmText: '重置',
      }))
    )
      return
    try {
      await resetStats.mutateAsync(item.id)
      toast.success('统计已重置')
    } catch (err) {
      toast.error('重置失败：' + extractErrorMessage(err))
    }
  }

  const handleRotate = async (item: ClientKeyItem) => {
    const systemHint = item.isSystem
      ? '这是系统密钥（config.json apiKey），重新生成后会同步更新 config.json 的 apiKey。'
      : ''
    if (
      !(await confirm({
        title: '重新生成 Key',
        description: `重新生成 Key "${item.name}"？旧明文将立即失效，使用旧明文的下游需要换上新明文才能继续调用。Key 的名称、描述、绑定分组与累计统计保留不变。${systemHint ? ' ' + systemHint : ''}`,
        confirmText: '重新生成',
        destructive: true,
      }))
    )
      return
    try {
      const res = await rotateKey.mutateAsync(item.id)
      setCreatedKey(res)
      setShowCreatedPlain(true)
      // 系统密钥轮换后本地存储的 apiKey 已失效，提示用户用新明文重新登录
      if (item.isSystem) {
        toast.info('系统密钥已更新，若你正用该密钥登录管理面板，请用新明文重新登录')
      }
    } catch (err) {
      toast.error('重新生成失败：' + extractErrorMessage(err))
    }
  }

  const startEdit = (item: ClientKeyItem) => {
    setEditTarget(item)
    setEditName(item.name)
    setEditDesc(item.description ?? '')
    setEditGroup(item.group ?? '')
    setEditMaxCredits(item.maxCredits != null ? String(item.maxCredits) : '')
    setEditOpen(true)
  }

  const handleEditSave = async (e: React.FormEvent) => {
    e.preventDefault()
    if (!editTarget) return
    const maxCredits = parseMaxCreditsInput(editMaxCredits)
    if (maxCredits === 'invalid') {
      toast.error('积分上限必须是非负数')
      return
    }
    try {
      await updateKey.mutateAsync({
        id: editTarget.id,
        req: { name: editName.trim(), description: editDesc.trim(), group: editGroup.trim() },
      })
      // 仅在上限发生变化时才调用额度接口，避免无谓写入
      const prev = editTarget.maxCredits ?? null
      if (maxCredits !== prev) {
        await setMaxCredits.mutateAsync({ id: editTarget.id, maxCredits })
      }
      toast.success('已更新')
      setEditOpen(false)
    } catch (err) {
      toast.error('更新失败：' + extractErrorMessage(err))
    }
  }

  const copyText = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text)
      toast.success('已复制')
    } catch {
      toast.error('复制失败')
    }
  }

  const [selectedIds, setSelectedIds] = useState<Set<number | string>>(new Set())
  const [batchActionPending, setBatchActionPending] = useState(false)
  const [batchProgress, setBatchProgress] = useState<{ current: number; total: number; action: 'delete' | 'toggle' } | null>(null)

  const keys: ClientKeyItem[] = useMemo(() => data?.keys ?? [], [data?.keys])

  const handleBatchDelete = async () => {
    if (selectedIds.size === 0) return
    const selectedItems = keys.filter((k) => selectedIds.has(k.id))
    const deletableItems = selectedItems.filter((k) => !k.isSystem)
    const systemCount = selectedItems.length - deletableItems.length

    if (deletableItems.length === 0) {
      toast.error('所选项均为系统密钥，不可删除')
      return
    }

    const systemHint = systemCount > 0 ? `（已自动跳过 ${systemCount} 个系统密钥）` : ''
    const ok = await confirm({
      title: `批量删除 ${deletableItems.length} 把 Key？`,
      description: `确定要删除选中的 ${deletableItems.length} 把客户端 Key 吗？${systemHint}此操作无法撤销。`,
      confirmText: '确认删除',
      destructive: true,
    })
    if (!ok) return

    setBatchActionPending(true)
    setBatchProgress({ current: 0, total: deletableItems.length, action: 'delete' })
    let s = 0
    let f = 0
    try {
      for (let i = 0; i < deletableItems.length; i++) {
        const item = deletableItems[i]
        try {
          await deleteKey.mutateAsync(item.id)
          s++
        } catch {
          f++
        }
        setBatchProgress({ current: i + 1, total: deletableItems.length, action: 'delete' })
      }
      if (f === 0) {
        toast.success(`已批量删除 ${s} 把 Key`)
      } else {
        toast.warning(`批量删除完成：成功 ${s} 个，失败 ${f} 个`)
      }
      setSelectedIds(new Set())
    } finally {
      setBatchActionPending(false)
      setBatchProgress(null)
    }
  }

  const handleBatchSetDisabled = async (disabled: boolean) => {
    if (selectedIds.size === 0) return
    const ids = Array.from(selectedIds).map(Number)
    setBatchActionPending(true)
    setBatchProgress({ current: 0, total: ids.length, action: 'toggle' })
    let s = 0
    let f = 0
    try {
      for (let i = 0; i < ids.length; i++) {
        const id = ids[i]
        try {
          await setDisabled.mutateAsync({ id, disabled })
          s++
        } catch {
          f++
        }
        setBatchProgress({ current: i + 1, total: ids.length, action: 'toggle' })
      }
      toast.success(`已批量${disabled ? '禁用' : '启用'} ${s} 把 Key${f > 0 ? `，失败 ${f} 个` : ''}`)
      setSelectedIds(new Set())
    } finally {
      setBatchActionPending(false)
      setBatchProgress(null)
    }
  }

  const columns: ConsoleColumn<ClientKeyItem>[] = useMemo(
    () => [
      {
        id: 'id',
        header: 'ID',
        cell: (k) => (
          <span className="console-num text-[12px] text-muted-foreground">
            #{k.id}
          </span>
        ),
      },
      {
        id: 'name',
        header: '名称',
        cell: (k) => (
          <div className="min-w-0 max-w-[240px]">
            <div className="flex items-center gap-1.5">
              <span className="truncate font-medium text-foreground">{k.name}</span>
              {k.isSystem && (
                <Badge variant="secondary" title="由 config.json apiKey 同步，不可删除、可轮换">
                  系统
                </Badge>
              )}
            </div>
            {k.description && (
              <div className="truncate text-[11px] text-muted-foreground">
                {k.description}
              </div>
            )}
          </div>
        ),
      },
      {
        id: 'key',
        header: 'Key',
        cell: (k) => (
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <button
                type="button"
                className="rounded px-1 py-0.5 font-mono text-[12px] text-muted-foreground hover:bg-accent/60 focus:outline-none focus-visible:ring-1 focus-visible:ring-ring"
                title="点击展开 Key 操作"
              >
                {k.maskedKey}
              </button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="start">
              <DropdownMenuItem onSelect={() => handleRotate(k)}>
                <RefreshCw className="h-3.5 w-3.5" />
                重新生成 Key（旧 Key 立即失效）
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        ),
      },
      {
        id: 'group',
        header: '分组',
        cell: (k) =>
          k.group ? (
            <Badge variant="outline">{k.group}</Badge>
          ) : (
            <span className="text-[12px] text-muted-foreground">全部账号</span>
          ),
      },
      {
        id: 'status',
        header: '状态',
        cell: (k) =>
          k.disabled ? (
            <Badge variant="destructive">已禁用</Badge>
          ) : (
            <Badge variant="success">启用</Badge>
          ),
      },
      {
        id: 'totalCalls',
        header: '总调用',
        align: 'right',
        cell: (k) => <span className="console-num">{k.totalCalls}</span>,
      },
      {
        id: 'inputTokens',
        header: '输入',
        align: 'right',
        cell: (k) => <span className="console-num">{formatTokens(k.totalInputTokens)}</span>,
      },
      {
        id: 'outputTokens',
        header: '输出',
        align: 'right',
        cell: (k) => <span className="console-num">{formatTokens(k.totalOutputTokens)}</span>,
      },
      {
        id: 'credits',
        header: '积分 / 上限',
        align: 'right',
        cell: (k) => <CreditsUsage used={k.totalCredits} max={k.maxCredits} />,
      },
      {
        id: 'lastUsed',
        header: '最后使用',
        cell: (k) => (
          <span className="text-[12px] text-muted-foreground">
            {formatRelative(k.lastUsedAt)}
          </span>
        ),
      },
    ],
    [],
  )

  const rowActions = (k: ClientKeyItem) => (
    <div className="flex items-center justify-end gap-1">
      <Button
        size="icon"
        variant="ghost"
        className="h-7 w-7"
        onClick={(e) => {
          e.stopPropagation()
          startEdit(k)
        }}
        title="编辑"
      >
        <Pencil className="h-3.5 w-3.5" />
      </Button>
      <Button
        size="icon"
        variant="ghost"
        className="h-7 w-7"
        onClick={(e) => {
          e.stopPropagation()
          handleToggleDisabled(k)
        }}
        title={k.disabled ? '启用' : '禁用'}
      >
        <Power className={`h-3.5 w-3.5 ${k.disabled ? 'text-emerald-500' : 'text-amber-500'}`} />
      </Button>
      <Button
        size="icon"
        variant="ghost"
        className="h-7 w-7"
        onClick={(e) => {
          e.stopPropagation()
          handleReset(k)
        }}
        title="重置统计"
      >
        <RotateCcw className="h-3.5 w-3.5" />
      </Button>
      {!k.isSystem && (
        <Button
          size="icon"
          variant="ghost"
          className="h-7 w-7 text-destructive hover:text-destructive"
          onClick={(e) => {
            e.stopPropagation()
            handleDelete(k)
          }}
          title="删除"
        >
          <Trash2 className="h-3.5 w-3.5" />
        </Button>
      )}
    </div>
  )

  return (
    <div className="console-scope space-y-4">
      <div className="flex items-end justify-between gap-4">
        <div>
          <h1 className="text-[28px] font-semibold tracking-tight leading-tight">客户端 Key</h1>
          <p className="mt-1 text-sm text-muted-foreground">
            分发给下游用户/项目的访问密钥。每把 Key 独立计数与禁用，泄露后只需替换一把。
          </p>
        </div>
        <Button onClick={() => setCreateOpen(true)} size="sm">
          <Plus className="h-3.5 w-3.5" />新建 Key
        </Button>
      </div>

      <ConsoleTable
        rows={keys}
        columns={columns}
        rowKey={(k) => k.id}
        selectable
        selected={selectedIds}
        onSelectedChange={setSelectedIds}
        rowActions={rowActions}
        loading={isLoading}
        empty="还没有客户端 Key，点击右上角「新建 Key」开始。"
      />

      {/* 吸底批量操作栏 */}
      <BulkBar
        count={selectedIds.size}
        onClear={() => setSelectedIds(new Set())}
        noun="把 Key"
      >
        <Button
          onClick={() => handleBatchSetDisabled(false)}
          size="sm"
          variant="ghost"
          className="h-8 px-3 text-xs gap-1.5 rounded-full hover:bg-accent"
          disabled={batchActionPending}
        >
          <Power className="h-3.5 w-3.5 text-emerald-500" />
          批量启用
        </Button>
        <Button
          onClick={() => handleBatchSetDisabled(true)}
          size="sm"
          variant="ghost"
          className="h-8 px-3 text-xs gap-1.5 rounded-full hover:bg-accent"
          disabled={batchActionPending}
        >
          {batchActionPending && batchProgress?.action === 'toggle' ? (
            <>
              <Loader2 className="h-3.5 w-3.5 animate-spin text-amber-500" />
              处理中 {batchProgress.current}/{batchProgress.total}
            </>
          ) : (
            <>
              <Power className="h-3.5 w-3.5 text-amber-500" />
              批量禁用
            </>
          )}
        </Button>
        <Button
          onClick={handleBatchDelete}
          size="sm"
          variant="destructive"
          className="h-8 px-3 text-xs gap-1.5 rounded-full"
          disabled={batchActionPending}
        >
          {batchActionPending && batchProgress?.action === 'delete' ? (
            <>
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
              删除中 {batchProgress.current}/{batchProgress.total}
            </>
          ) : (
            <>
              <Trash2 className="h-3.5 w-3.5" />
              批量删除
            </>
          )}
        </Button>
      </BulkBar>

      {/* 新建对话框 */}
      <Dialog open={createOpen} onOpenChange={(o) => !createKey.isPending && setCreateOpen(o)}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>新建客户端 Key</DialogTitle>
            <DialogDescription>
              创建后明文 Key 仅显示一次，请立即复制保存到安全位置。
            </DialogDescription>
          </DialogHeader>
          <form onSubmit={handleCreate} className="space-y-3 py-2">
            <div>
              <label className="text-[12px] text-muted-foreground">名称 *</label>
              <Input
                placeholder="VS Code 本机 / 团队 A 等"
                value={createName}
                onChange={(e) => setCreateName(e.target.value)}
                disabled={createKey.isPending}
                autoFocus
              />
            </div>
            <div>
              <label className="text-[12px] text-muted-foreground">描述（可选）</label>
              <Input
                placeholder="可选备注，如绑定的项目、负责人等"
                value={createDesc}
                onChange={(e) => setCreateDesc(e.target.value)}
                disabled={createKey.isPending}
              />
            </div>
            <div>
              <label className="text-[12px] text-muted-foreground">绑定分组（可选）</label>
              <GroupSingleSelect
                value={createGroup}
                options={groupOptions}
                onChange={setCreateGroup}
                disabled={createKey.isPending}
                noneLabel="（不绑定，可用全部账号）"
              />
              <p className="mt-1 text-[11px] text-muted-foreground">
                绑定后该 Key 仅会使用含此分组的账号（严格隔离，分组内无可用账号时请求会失败）。
              </p>
            </div>
            <div>
              <label className="text-[12px] text-muted-foreground">积分上限（可选）</label>
              <Input
                type="number"
                min="0"
                step="any"
                inputMode="decimal"
                placeholder="留空表示不限制"
                value={createMaxCredits}
                onChange={(e) => setCreateMaxCredits(e.target.value)}
                disabled={createKey.isPending}
              />
              <p className="mt-1 text-[11px] text-muted-foreground">
                累计使用的 credit 达到上限后，该 Key 的请求会被拒绝（HTTP 429）。重置统计后重新计费。
              </p>
            </div>
            <DialogFooter>
              <Button type="button" variant="outline" onClick={() => setCreateOpen(false)} disabled={createKey.isPending}>
                取消
              </Button>
              <Button type="submit" disabled={createKey.isPending || !createName.trim()}>
                {createKey.isPending ? '创建中…' : '创建'}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>

      {/* 创建后明文展示对话框 */}
      <Dialog open={!!createdKey} onOpenChange={(o) => { if (!o) setCreatedKey(null) }}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              <KeyRound className="h-4 w-4 text-emerald-500" />
              Key 已生成
            </DialogTitle>
            <DialogDescription>
              这是 Key "{createdKey?.name}" 的明文。<strong>关闭对话框后将无法再查看</strong>，请立即复制。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-3">
            <div className="relative">
              <Input
                readOnly
                type={showCreatedPlain ? 'text' : 'password'}
                value={createdKey?.key ?? ''}
                className="pr-20 font-mono text-[13px]"
              />
              <div className="absolute inset-y-0 right-0 flex items-center pr-1">
                <Button
                  type="button"
                  size="icon"
                  variant="ghost"
                  className="h-7 w-7"
                  onClick={() => setShowCreatedPlain((v) => !v)}
                  title={showCreatedPlain ? '隐藏' : '显示'}
                >
                  {showCreatedPlain ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
                </Button>
                <Button
                  type="button"
                  size="icon"
                  variant="ghost"
                  className="h-7 w-7"
                  onClick={() => createdKey && copyText(createdKey.key)}
                  title="复制"
                >
                  <Copy className="h-3.5 w-3.5" />
                </Button>
              </div>
            </div>
            <p className="text-[11px] text-muted-foreground">
              客户端调用 <code>/v1/messages</code> 时，把它放在 <code>x-api-key</code> 或 <code>Authorization: Bearer</code> 头中。
            </p>
          </div>
          <DialogFooter>
            <Button onClick={() => setCreatedKey(null)}>我已保存好</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* 编辑对话框 */}
      <Dialog open={editOpen} onOpenChange={(o) => !updateKey.isPending && setEditOpen(o)}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>编辑 Key</DialogTitle>
            <DialogDescription>修改名称与描述（不影响 Key 值与统计）</DialogDescription>
          </DialogHeader>
          <form onSubmit={handleEditSave} className="space-y-3 py-2">
            <div>
              <label className="text-[12px] text-muted-foreground">名称</label>
              <Input value={editName} onChange={(e) => setEditName(e.target.value)} />
            </div>
            <div>
              <label className="text-[12px] text-muted-foreground">描述</label>
              <Input value={editDesc} onChange={(e) => setEditDesc(e.target.value)} />
            </div>
            <div>
              <label className="text-[12px] text-muted-foreground">绑定分组</label>
              <GroupSingleSelect
                value={editGroup}
                options={groupOptions}
                onChange={setEditGroup}
                disabled={updateKey.isPending}
                noneLabel="（不绑定，可用全部账号）"
              />
              <p className="mt-1 text-[11px] text-muted-foreground">
                绑定后仅调度该分组内账号（严格隔离）。选「不绑定」表示解除绑定。
              </p>
            </div>
            <div>
              <label className="text-[12px] text-muted-foreground">积分上限</label>
              <Input
                type="number"
                min="0"
                step="any"
                inputMode="decimal"
                placeholder="留空表示不限制"
                value={editMaxCredits}
                onChange={(e) => setEditMaxCredits(e.target.value)}
                disabled={updateKey.isPending || setMaxCredits.isPending}
              />
              <p className="mt-1 text-[11px] text-muted-foreground">
                累计 credit 达到上限后该 Key 请求会被拒绝（HTTP 429）。清空则取消限制；重置统计可清零已用量。
              </p>
            </div>
            <DialogFooter>
              <Button type="button" variant="outline" onClick={() => setEditOpen(false)}>取消</Button>
              <Button type="submit" disabled={updateKey.isPending || setMaxCredits.isPending}>
                {updateKey.isPending || setMaxCredits.isPending ? '保存中…' : '保存'}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>
    </div>
  )
}
