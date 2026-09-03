import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  getCredentials,
  setCredentialDisabled,
  setCredentialPriority,
  resetCredentialFailure,
  forceRefreshToken,
  clearThrottle,
  getCredentialBalance,
  getCredentialModels,
  getCurrentCredentialModels,
  testModel,
  addCredential,
  deleteCredential,
  updateCredential,
  updateRefreshToken,
  getLoadBalancingMode,
  setLoadBalancingMode,
  getAccountThrottleConfig,
  setAccountThrottleConfig,
  getAccountRpmLimitConfig,
  setAccountRpmLimitConfig,
  getSelfHealConfig,
  setSelfHealConfig,
  getLogGovernanceConfig,
  setLogGovernanceConfig,
  getCacheMeteringConfig,
  setCacheMeteringConfig,
  getGlobalProxy,
  setGlobalProxy,
  getCustomModels,
  setCustomModels,
  getUpdateConfig,
  setUpdateConfig,
  resetSuccessCount,
  resetAllSuccessCount,
  getCredentialMetadataSchema,
  setCredentialMetadataSchema,
} from '@/api/credentials'
import type {
  AddCredentialRequest,
  CustomModelItem,
  SetGlobalProxyRequest,
  SetUpdateConfigRequest,
  UpdateCredentialRequest,
  UpdateRefreshTokenRequest,
  CredentialMetadataSchemaConfig,
} from '@/types/api'

// 查询凭据列表
export function useCredentials() {
  return useQuery({
    queryKey: ['credentials'],
    queryFn: getCredentials,
    refetchInterval: 30000, // 每 30 秒刷新一次
  })
}

export function useCredentialMetadataSchema() {
  return useQuery({
    queryKey: ['credential-metadata-schema'],
    queryFn: getCredentialMetadataSchema,
  })
}

export function useSetCredentialMetadataSchema() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (config: CredentialMetadataSchemaConfig) =>
      setCredentialMetadataSchema(config),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credential-metadata-schema'] })
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
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
    staleTime: 0, // 始终视为过期，每次打开对话框都实时查上游
    retry: false, // 失败不重试，避免对被封禁/异常账号反复请求
  })
}

// 使用账号池当前选中的可用凭据查询模型列表
export function useCurrentCredentialModels(enabled: boolean) {
  return useQuery({
    queryKey: ['current-credential-models'],
    queryFn: getCurrentCredentialModels,
    enabled,
    staleTime: 0, // 始终视为过期，每次打开对话框都实时查上游
    retry: false,
  })
}

// 对模型发送真实请求
export function useTestModel() {
  return useMutation({
    mutationFn: testModel,
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
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
      queryClient.invalidateQueries({ queryKey: ['current-credential-models'] })
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

// 获取单账号 RPM 限流配置
export function useAccountRpmLimitConfig() {
  return useQuery({
    queryKey: ['accountRpmLimitConfig'],
    queryFn: getAccountRpmLimitConfig,
  })
}

// 更新单账号 RPM 限流配置
export function useSetAccountRpmLimitConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setAccountRpmLimitConfig,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['accountRpmLimitConfig'] })
    },
  })
}

// 获取自愈治理配置（30s 刷新以便观测 consecutiveRounds/totalCount 变化）
export function useSelfHealConfig() {
  return useQuery({
    queryKey: ['selfHealConfig'],
    queryFn: getSelfHealConfig,
    refetchInterval: 30_000,
  })
}

// 更新自愈治理配置
export function useSetSelfHealConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setSelfHealConfig,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['selfHealConfig'] })
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

// 获取 prompt cache 计量模拟开关
export function useCacheMeteringConfig() {
  return useQuery({
    queryKey: ['cacheMeteringConfig'],
    queryFn: getCacheMeteringConfig,
  })
}

// 切换 prompt cache 计量模拟开关
export function useSetCacheMeteringConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setCacheMeteringConfig,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['cacheMeteringConfig'] })
    },
  })
}

// 全局出站代理。此前只在代理池弹窗里内联查询，设置页需要独立入口，
// 抽成 hook 后两处共用同一份缓存（queryKey 与弹窗保持一致：'global-proxy'）。
export function useGlobalProxy() {
  return useQuery({
    queryKey: ['global-proxy'],
    queryFn: getGlobalProxy,
  })
}

export function useSetGlobalProxy() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (req: SetGlobalProxyRequest) => setGlobalProxy(req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['global-proxy'] })
    },
  })
}

// 自定义模型配置
export function useCustomModels() {
  return useQuery({
    queryKey: ['custom-models'],
    queryFn: getCustomModels,
  })
}

export function useSetCustomModels() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (req: { models: CustomModelItem[] }) =>
      setCustomModels(req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['custom-models'] })
    },
  })
}

// 镜像在线更新配置（GitHub Token / 无人值守自动更新）
export function useUpdateConfig() {
  return useQuery({
    queryKey: ['update-config'],
    queryFn: getUpdateConfig,
  })
}

export function useSetUpdateConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (req: SetUpdateConfigRequest) => setUpdateConfig(req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['update-config'] })
    },
  })
}
