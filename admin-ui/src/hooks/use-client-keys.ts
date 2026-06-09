import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  listClientKeys,
  createClientKey,
  deleteClientKey,
  updateClientKey,
  setClientKeyDisabled,
  resetClientKeyStats,
  rotateClientKey,
} from '@/api/client-keys'
import type { CreateClientKeyRequest, UpdateClientKeyRequest } from '@/types/api'

export function useClientKeys() {
  return useQuery({
    queryKey: ['client-keys'],
    queryFn: listClientKeys,
    refetchInterval: 30000,
  })
}

export function useCreateClientKey() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (req: CreateClientKeyRequest) => createClientKey(req),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['client-keys'] }),
  })
}

export function useDeleteClientKey() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => deleteClientKey(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['client-keys'] }),
  })
}

export function useUpdateClientKey() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: ({ id, req }: { id: number; req: UpdateClientKeyRequest }) =>
      updateClientKey(id, req),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['client-keys'] }),
  })
}

export function useSetClientKeyDisabled() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: ({ id, disabled }: { id: number; disabled: boolean }) =>
      setClientKeyDisabled(id, disabled),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['client-keys'] }),
  })
}

export function useResetClientKeyStats() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => resetClientKeyStats(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['client-keys'] }),
  })
}

export function useRotateClientKey() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => rotateClientKey(id),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['client-keys'] }),
  })
}
