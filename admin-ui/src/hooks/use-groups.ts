import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  listGroups,
  createGroup,
  deleteGroup,
  updateGroup,
} from '@/api/groups'
import type { CreateGroupRequest, UpdateGroupRequest } from '@/types/api'

export function useGroups() {
  return useQuery({
    queryKey: ['groups'],
    queryFn: listGroups,
    // 分组变更频率低（人工操作），15s 自动刷新足够
    refetchInterval: 15000,
    staleTime: 5000,
  })
}

/**
 * 给所有 GroupSelect 用的"已注册分组名"字符串数组。
 * 内部复用 useGroups 缓存，不会重复打接口。
 */
export function useGroupOptions(): string[] {
  const { data } = useGroups()
  return (data?.groups ?? []).map((g) => g.name)
}

export function useCreateGroup() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (req: CreateGroupRequest) => createGroup(req),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['groups'] }),
  })
}

export function useUpdateGroup() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: ({ name, req }: { name: string; req: UpdateGroupRequest }) =>
      updateGroup(name, req),
    onSuccess: () => {
      // 改名 / 改备注会影响凭据 / Key 的展示，三处缓存全部失效
      qc.invalidateQueries({ queryKey: ['groups'] })
      qc.invalidateQueries({ queryKey: ['credentials'] })
      qc.invalidateQueries({ queryKey: ['client-keys'] })
    },
  })
}

export function useDeleteGroup() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: ({ name, force }: { name: string; force?: boolean }) =>
      deleteGroup(name, !!force),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: ['groups'] })
      qc.invalidateQueries({ queryKey: ['credentials'] })
      qc.invalidateQueries({ queryKey: ['client-keys'] })
    },
  })
}
