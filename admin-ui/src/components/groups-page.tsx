import { useState } from 'react'
import { toast } from 'sonner'
import {
  Plus, FolderTree, Trash2, Pencil, Users, KeyRound, RefreshCw,
} from 'lucide-react'
import { Card, CardContent } from '@/components/ui/card'
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

/**
 * 分组管理页：CRUD 已注册分组。
 *
 * 设计要点：
 * - 分组是独立实体，凭据 / 客户端 Key 通过名字引用
 * - 改名走级联（后端自动同步所有引用）
 * - 删除默认拒绝有引用的，二次确认才允许 force 级联清理
 * - 列表展示每个分组当前被多少个凭据 / Key 引用，删除前清楚知道影响
 */
export function GroupsPage() {
  const { data, isLoading, isFetching, refetch } = useGroups()
  const createGroup = useCreateGroup()
  const updateGroup = useUpdateGroup()
  const deleteGroup = useDeleteGroup()
  const confirm = useConfirm()

  const [createOpen, setCreateOpen] = useState(false)
  const [createName, setCreateName] = useState('')
  const [createDesc, setCreateDesc] = useState('')

  const [editOpen, setEditOpen] = useState(false)
  const [editTarget, setEditTarget] = useState<GroupItem | null>(null)
  const [editNewName, setEditNewName] = useState('')
  const [editDesc, setEditDesc] = useState('')

  const groups = data?.groups ?? []

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
      } catch (e) {
        toast.error(extractErrorMessage(e))
      }
    }
  }

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <h2 className="text-lg font-semibold flex items-center gap-2">
            <FolderTree className="h-4 w-4" />
            分组管理
          </h2>
          <p className="text-sm text-muted-foreground mt-1">
            分组是凭据 / 客户端 Key 共享的独立实体；改名 / 删除会级联同步。
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

      {isLoading ? (
        <Card><CardContent className="py-8 text-sm text-center text-muted-foreground">加载中…</CardContent></Card>
      ) : groups.length === 0 ? (
        <Card>
          <CardContent className="py-12 text-sm text-center text-muted-foreground space-y-2">
            <FolderTree className="h-8 w-8 mx-auto opacity-40" />
            <p>暂无分组。点上方「新建分组」开始。</p>
          </CardContent>
        </Card>
      ) : (
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
          {groups.map((g) => (
            <Card key={g.name}>
              <CardContent className="p-4 space-y-3">
                <div className="flex items-start justify-between gap-2">
                  <div className="min-w-0">
                    <div className="font-medium truncate">{g.name}</div>
                    {g.description && (
                      <p className="text-xs text-muted-foreground mt-0.5 line-clamp-2">{g.description}</p>
                    )}
                  </div>
                  <div className="flex shrink-0 items-center gap-1">
                    <Button size="icon" variant="ghost" className="h-7 w-7" onClick={() => openEdit(g)} title="编辑">
                      <Pencil className="h-3.5 w-3.5" />
                    </Button>
                    <Button
                      size="icon"
                      variant="ghost"
                      className="h-7 w-7 text-destructive hover:text-destructive"
                      onClick={() => handleDelete(g)}
                      title="删除"
                    >
                      <Trash2 className="h-3.5 w-3.5" />
                    </Button>
                  </div>
                </div>

                <div className="flex flex-wrap items-center gap-2 text-xs">
                  <Badge variant="secondary" className="gap-1">
                    <Users className="h-3 w-3" />
                    {g.credentialCount} 凭据
                  </Badge>
                  <Badge variant="secondary" className="gap-1">
                    <KeyRound className="h-3 w-3" />
                    {g.clientKeyCount} Key
                  </Badge>
                </div>

                <p className="text-[11px] text-muted-foreground">
                  创建于 {new Date(g.createdAt).toLocaleString()}
                </p>
              </CardContent>
            </Card>
          ))}
        </div>
      )}

      {/* 新建分组弹框 */}
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

      {/* 编辑分组弹框 */}
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
    </div>
  )
}
