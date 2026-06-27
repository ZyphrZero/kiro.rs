import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  getCredentials,
  setCredentialDisabled,
  setCredentialPriority,
  setCredentialConcurrency,
  resetCredentialFailure,
  forceRefreshToken,
  clearThrottle,
  getCredentialBalance,
  getCredentialModels,
  addCredential,
  deleteCredential,
  updateCredential,
  updateRefreshToken,
  getLoadBalancingMode,
  setLoadBalancingMode,
  getAccountThrottleConfig,
  setAccountThrottleConfig,
  getLogGovernanceConfig,
  setLogGovernanceConfig,
  getRuntimeGovernanceConfig,
  setRuntimeGovernanceConfig,
  resetSuccessCount,
  resetAllSuccessCount,
} from '@/api/credentials'
import type { AddCredentialRequest, UpdateCredentialRequest, UpdateRefreshTokenRequest } from '@/types/api'

// 查询凭据列表
// refetchInterval 可覆盖默认 30s；监控视图下传更短间隔（如 3s）以近实时展示在途并发。
export function useCredentials(refetchInterval: number = 30000) {
  return useQuery({
    queryKey: ['credentials'],
    queryFn: getCredentials,
    refetchInterval,
  })
}

// 查询凭据余额
export function useCredentialBalance(id: number | null) {
  return useQuery({
    queryKey: ['credential-balance', id],
    queryFn: () => getCredentialBalance(id!),
    enabled: id !== null,
    retry: false, // 余额查询失败时不重试（避免重复请求被封禁的账号）
  })
}

// 查询凭据当前可用的模型列表（按需实时查询上游）
export function useCredentialModels(id: number | null) {
  return useQuery({
    queryKey: ['credential-models', id],
    queryFn: () => getCredentialModels(id!),
    enabled: id !== null,
    retry: false, // 失败不重试，避免对被封禁/异常账号反复请求
  })
}

// 设置禁用状态
export function useSetDisabled() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, disabled }: { id: number; disabled: boolean }) =>
      setCredentialDisabled(id, disabled),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 设置优先级
export function useSetPriority() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, priority }: { id: number; priority: number }) =>
      setCredentialPriority(id, priority),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 设置单账号并发覆盖
export function useSetConcurrency() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, maxConcurrency }: { id: number; maxConcurrency: number | null }) =>
      setCredentialConcurrency(id, maxConcurrency),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 重置失败计数
export function useResetFailure() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => resetCredentialFailure(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 强制刷新 Token
export function useForceRefreshToken() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => forceRefreshToken(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 解除账号级风控冷却
export function useClearThrottle() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => clearThrottle(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 添加新凭据
export function useAddCredential() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (req: AddCredentialRequest) => addCredential(req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 删除凭据
export function useDeleteCredential() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => deleteCredential(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 重置单个凭据的成功次数
export function useResetSuccessCount() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => resetSuccessCount(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 重置所有凭据的成功次数
export function useResetAllSuccessCount() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: () => resetAllSuccessCount(),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 更新已禁用凭据的 refreshToken
export function useUpdateRefreshToken() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, req }: { id: number; req: UpdateRefreshTokenRequest }) =>
      updateRefreshToken(id, req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 更新凭据可编辑字段
export function useUpdateCredential() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, req }: { id: number; req: UpdateCredentialRequest }) =>
      updateCredential(id, req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 获取负载均衡模式
export function useLoadBalancingMode() {
  return useQuery({
    queryKey: ['loadBalancingMode'],
    queryFn: getLoadBalancingMode,
  })
}

// 设置负载均衡模式
export function useSetLoadBalancingMode() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setLoadBalancingMode,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['loadBalancingMode'] })
    },
  })
}

// 获取账号级风控故障转移配置
export function useAccountThrottleConfig() {
  return useQuery({
    queryKey: ['accountThrottleConfig'],
    queryFn: getAccountThrottleConfig,
  })
}

// 更新账号级风控故障转移配置
export function useSetAccountThrottleConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setAccountThrottleConfig,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['accountThrottleConfig'] })
    },
  })
}

// 获取日志治理配置
export function useLogGovernanceConfig() {
  return useQuery({
    queryKey: ['logGovernanceConfig'],
    queryFn: getLogGovernanceConfig,
  })
}

// 更新日志治理配置
export function useSetLogGovernanceConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setLogGovernanceConfig,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['logGovernanceConfig'] })
    },
  })
}

// 获取运行时治理配置（配额阈值 + 全局响应缓存默认）
export function useRuntimeGovernanceConfig() {
  return useQuery({
    queryKey: ['runtimeGovernanceConfig'],
    queryFn: getRuntimeGovernanceConfig,
  })
}

// 更新运行时治理配置
export function useSetRuntimeGovernanceConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setRuntimeGovernanceConfig,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['runtimeGovernanceConfig'] })
    },
  })
}
