import { useMemo, useState } from 'react'
import { toast } from 'sonner'
import {
  Plus, FolderTree, Trash2, Pencil, Users, KeyRound, RefreshCw, Loader2,
} from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Input } from '@/components/ui/input'
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter, DialogDescription,
} from '@/components/ui/dialog'
import {
  useGroups, useCreateGroup, useUpdateGroup, useDeleteGroup,
} from '@/hooks/use-groups'
import { useConfirm } from '@/components/ui/confirm-dialog'
import { extractErrorMessage } from '@/lib/utils'
import type { GroupItem } from '@/types/api'
import { ConsoleTable, type ConsoleColumn } from '@/components/console/data-table'
import { BulkBar } from '@/components/console/bulk-bar'

/**
 * 分组管理页：CRUD 已注册分组。
 *
 * 设计要点：
 * - 统一使用 ConsoleTable 密集型表格展示
 * - 支持批量选择与 BulkBar 批量删除（自动检测引用并支持级联清理）
 * - 改名走级联（后端自动同步所有引用）
 * - 单项删除与批量删除均做引用前置检查
 */
export function GroupsPage() {
  const { data, isLoading, isFetching, refetch } = useGroups()
  const createGroup = useCreateGroup()
  const updateGroup = useUpdateGroup()
  const deleteGroup = useDeleteGroup()
  const confirm = useConfirm()

  const [selectedNames, setSelectedNames] = useState<Set<number | string>>(new Set())
  const [createOpen, setCreateOpen] = useState(false)
  const [createName, setCreateName] = useState('')
  const [createDesc, setCreateDesc] = useState('')

  const [editOpen, setEditOpen] = useState(false)
  const [editTarget, setEditTarget] = useState<GroupItem | null>(null)
  const [editNewName, setEditNewName] = useState('')
  const [editDesc, setEditDesc] = useState('')
  const [batchDeleting, setBatchDeleting] = useState(false)
  const [deleteProgress, setDeleteProgress] = useState<{ current: number; total: number } | null>(null)

  const groups = useMemo(() => data?.groups ?? [], [data?.groups])

  const openCreate = () => {
    setCreateName('')
    setCreateDesc('')
    setCreateOpen(true)
  }

  const handleCreate = async () => {
    const name = createName.trim()
    if (!name) {
      toast.error('分组名不能为空')
      return
    }
    try {
      await createGroup.mutateAsync({
        name,
        description: createDesc.trim() || undefined,
      })
      toast.success(`已创建分组：${name}`)
      setCreateOpen(false)
    } catch (e) {
      toast.error(extractErrorMessage(e))
    }
  }

  const openEdit = (g: GroupItem) => {
    setEditTarget(g)
    setEditNewName(g.name)
    setEditDesc(g.description ?? '')
    setEditOpen(true)
  }

  const handleEdit = async () => {
    if (!editTarget) return
    const newName = editNewName.trim()
    if (!newName) {
      toast.error('分组名不能为空')
      return
    }
    try {
      await updateGroup.mutateAsync({
        name: editTarget.name,
        req: {
          newName: newName !== editTarget.name ? newName : undefined,
          description: editDesc, // 空字符串 → 后端清空
        },
      })
      const renamed = newName !== editTarget.name
      toast.success(renamed ? `已改名：${editTarget.name} → ${newName}` : '备注已更新')
      setEditOpen(false)
    } catch (e) {
      toast.error(extractErrorMessage(e))
    }
  }

  const handleDelete = async (g: GroupItem) => {
    const refs = g.credentialCount + g.clientKeyCount
    // 无引用：单层确认；有引用：二次确认 + force
    if (refs === 0) {
      const ok = await confirm({
        title: `删除分组 ${g.name}？`,
        description: '该分组当前无任何引用，可以安全删除。',
        confirmText: '删除',
        destructive: true,
      })
      if (!ok) return
      try {
        await deleteGroup.mutateAsync({ name: g.name })
        toast.success(`分组 ${g.name} 已删除`)
        setSelectedNames((prev) => {
          const next = new Set(prev)
          next.delete(g.name)
          return next
        })
      } catch (e) {
        toast.error(extractErrorMessage(e))
      }
    } else {
      const ok = await confirm({
        title: `强制删除分组 ${g.name}？`,
        description: `该分组当前被 ${g.credentialCount} 个凭据 + ${g.clientKeyCount} 把客户端 Key 引用。继续将级联清理这些引用（凭据从 groups 列表移除该分组；客户端 Key 解除绑定）。此操作不可撤销。`,
        confirmText: '强制删除',
        destructive: true,
      })
      if (!ok) return
      try {
        await deleteGroup.mutateAsync({ name: g.name, force: true })
        toast.success(`分组 ${g.name} 已删除，已清理 ${refs} 个引用`)
        setSelectedNames((prev) => {
          const next = new Set(prev)
          next.delete(g.name)
          return next
        })
      } catch (e) {
        toast.error(extractErrorMessage(e))
      }
    }
  }

  const handleBatchDelete = async () => {
    if (selectedNames.size === 0) return
    const names = Array.from(selectedNames) as string[]
    const selectedGroups = groups.filter((g) => selectedNames.has(g.name))
    const referencedGroups = selectedGroups.filter(
      (g) => g.credentialCount > 0 || g.clientKeyCount > 0,
    )
    const totalRefs = selectedGroups.reduce(
      (acc, g) => acc + g.credentialCount + g.clientKeyCount,
      0,
    )

    let force = false
    if (referencedGroups.length > 0) {
      const ok = await confirm({
        title: `批量强制删除 ${names.length} 个分组？`,
        description: `选中的分组中，有 ${referencedGroups.length} 个分组正被 ${totalRefs} 处凭据/Key 引用。继续将级联清理所有引用（凭据移除分组标签，Key 解绑分组）。此操作不可撤销。`,
        confirmText: '强制批量删除',
        destructive: true,
      })
      if (!ok) return
      force = true
    } else {
      const ok = await confirm({
        title: `批量删除 ${names.length} 个分组？`,
        description: `确定要删除选中的 ${names.length} 个分组吗？所选分组当前均无引用。此操作不可撤销。`,
        confirmText: '确认删除',
        destructive: true,
      })
      if (!ok) return
    }

    setBatchDeleting(true)
    setDeleteProgress({ current: 0, total: names.length })
    let successCount = 0
    let failCount = 0

    try {
      for (let i = 0; i < names.length; i++) {
        const name = names[i]
        try {
          await deleteGroup.mutateAsync({ name, force })
          successCount++
        } catch {
          failCount++
        }
        setDeleteProgress({ current: i + 1, total: names.length })
      }
      if (failCount === 0) {
        toast.success(`已批量删除 ${successCount} 个分组`)
      } else {
        toast.warning(`批量删除完成：成功 ${successCount} 个，失败 ${failCount} 个`)
      }
      setSelectedNames(new Set())
    } finally {
      setBatchDeleting(false)
      setDeleteProgress(null)
    }
  }

  const columns: ConsoleColumn<GroupItem>[] = useMemo(
    () => [
      {
        id: 'name',
        header: '分组名称',
        cell: (g) => (
          <div className="flex items-center gap-2">
            <span className="font-medium text-foreground">{g.name}</span>
          </div>
        ),
      },
      {
        id: 'description',
        header: '备注',
        cell: (g) => (
          <span className="text-muted-foreground truncate max-w-[320px] inline-block" title={g.description}>
            {g.description || '—'}
          </span>
        ),
      },
      {
        id: 'credentialCount',
        header: '关联凭据',
        cell: (g) => (
          <Badge variant="secondary" className="gap-1 font-mono text-[11px] tabular-nums">
            <Users className="h-3 w-3 text-muted-foreground" />
            {g.credentialCount}
          </Badge>
        ),
      },
      {
        id: 'clientKeyCount',
        header: '关联 Key',
        cell: (g) => (
          <Badge variant="secondary" className="gap-1 font-mono text-[11px] tabular-nums">
            <KeyRound className="h-3 w-3 text-muted-foreground" />
            {g.clientKeyCount}
          </Badge>
        ),
      },
      {
        id: 'createdAt',
        header: '创建时间',
        cell: (g) => (
          <span className="console-num text-[12px] text-muted-foreground">
            {g.createdAt ? new Date(g.createdAt).toLocaleString('zh-CN', { hour12: false }) : '—'}
          </span>
        ),
      },
    ],
    [],
  )

  const rowActions = (g: GroupItem) => (
    <div className="flex items-center justify-end gap-1">
      <Button
        size="icon"
        variant="ghost"
        className="h-7 w-7"
        onClick={(e) => {
          e.stopPropagation()
          openEdit(g)
        }}
        title="编辑"
      >
        <Pencil className="h-3.5 w-3.5" />
      </Button>
      <Button
        size="icon"
        variant="ghost"
        className="h-7 w-7 text-destructive hover:text-destructive"
        onClick={(e) => {
          e.stopPropagation()
          handleDelete(g)
        }}
        title="删除"
      >
        <Trash2 className="h-3.5 w-3.5" />
      </Button>
    </div>
  )

  return (
    <div className="console-scope space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <h2 className="text-lg font-semibold flex items-center gap-2">
            <FolderTree className="h-4 w-4" />
            分组管理
          </h2>
          <p className="text-sm text-muted-foreground mt-0.5">
            分组是凭据 / 客户端 Key 共享的独立实体；改名与删除会自动级联同步。
          </p>
        </div>
        <div className="flex items-center gap-2">
          <Button size="sm" variant="outline" onClick={() => refetch()} disabled={isFetching}>
            <RefreshCw className={`h-3.5 w-3.5 ${isFetching ? 'animate-spin' : ''}`} />
            刷新
          </Button>
          <Button size="sm" onClick={openCreate}>
            <Plus className="h-3.5 w-3.5" />
            新建分组
          </Button>
        </div>
      </div>

      <ConsoleTable
        rows={groups}
        columns={columns}
        rowKey={(g) => g.name}
        selectable
        selected={selectedNames}
        onSelectedChange={setSelectedNames}
        rowActions={rowActions}
        loading={isLoading}
        empty="暂无分组。点击右上角「新建分组」开始。"
      />

      {/* 吸底批量操作栏 */}
      <BulkBar
        count={selectedNames.size}
        onClear={() => setSelectedNames(new Set())}
        noun="个分组"
      >
        <Button
          onClick={handleBatchDelete}
          size="sm"
          variant="destructive"
          className="h-8 px-3 text-xs gap-1.5 rounded-full"
          disabled={batchDeleting}
        >
          {batchDeleting && deleteProgress ? (
            <>
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
              删除中 {deleteProgress.current}/{deleteProgress.total}
            </>
          ) : (
            <>
              <Trash2 className="h-3.5 w-3.5" />
              批量删除
            </>
          )}
        </Button>
      </BulkBar>

      {/* 新建分组弹框 */}
      {createOpen && (
        <Dialog open={createOpen} onOpenChange={setCreateOpen}>
          <DialogContent>
            <DialogHeader>
              <DialogTitle>新建分组</DialogTitle>
              <DialogDescription>
                注册后即可在凭据 / 客户端 Key 中选择该分组。
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-3">
              <div className="space-y-1">
                <label className="text-sm font-medium">分组名 *</label>
                <Input
                  placeholder="例如：客户A、生产、备用池"
                  value={createName}
                  onChange={(e) => setCreateName(e.target.value)}
                  disabled={createGroup.isPending}
                  autoFocus
                />
              </div>
              <div className="space-y-1">
                <label className="text-sm font-medium">备注（可选）</label>
                <Input
                  placeholder="用途说明，方便后续辨认"
                  value={createDesc}
                  onChange={(e) => setCreateDesc(e.target.value)}
                  disabled={createGroup.isPending}
                />
              </div>
            </div>
            <DialogFooter>
              <Button variant="outline" onClick={() => setCreateOpen(false)} disabled={createGroup.isPending}>
                取消
              </Button>
              <Button onClick={handleCreate} disabled={createGroup.isPending || !createName.trim()}>
                {createGroup.isPending ? '创建中…' : '创建'}
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
      )}

      {/* 编辑分组弹框 */}
      {editOpen && (
        <Dialog open={editOpen} onOpenChange={setEditOpen}>
          <DialogContent>
            <DialogHeader>
              <DialogTitle>编辑分组：{editTarget?.name}</DialogTitle>
              <DialogDescription>
                改名会级联同步所有引用此分组的凭据与客户端 Key。
              </DialogDescription>
            </DialogHeader>
            <div className="space-y-3">
              <div className="space-y-1">
                <label className="text-sm font-medium">分组名</label>
                <Input
                  value={editNewName}
                  onChange={(e) => setEditNewName(e.target.value)}
                  disabled={updateGroup.isPending}
                />
              </div>
              <div className="space-y-1">
                <label className="text-sm font-medium">备注</label>
                <Input
                  placeholder="（清空备注请留空）"
                  value={editDesc}
                  onChange={(e) => setEditDesc(e.target.value)}
                  disabled={updateGroup.isPending}
                />
              </div>
              {editTarget && (editTarget.credentialCount > 0 || editTarget.clientKeyCount > 0) && (
                <p className="text-xs text-amber-600">
                  当前被 {editTarget.credentialCount} 凭据 + {editTarget.clientKeyCount} 客户端 Key 引用，改名会自动同步。
                </p>
              )}
            </div>
            <DialogFooter>
              <Button variant="outline" onClick={() => setEditOpen(false)} disabled={updateGroup.isPending}>
                取消
              </Button>
              <Button onClick={handleEdit} disabled={updateGroup.isPending || !editNewName.trim()}>
                {updateGroup.isPending ? '保存中…' : '保存'}
              </Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
      )}
    </div>
  )
}

