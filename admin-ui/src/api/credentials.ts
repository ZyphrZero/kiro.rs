import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  CredentialsStatusResponse,
  BalanceResponse,
  AvailableModelsResponse,
  SuccessResponse,
  SetDisabledRequest,
  SetPriorityRequest,
  AddCredentialRequest,
  AddCredentialResponse,
  UpdateCredentialRequest,
  UpdateRefreshTokenRequest,
  ProxyPoolEntry,
  ProxyPoolResponse,
  AddProxyRequest,
  BatchAddProxyRequest,
  BatchAddProxyResponse,
  AssignProxyRequest,
  ProxyCheckResponse,
  ProxyCheckAllResponse,
  AssignRoundRobinResponse,
  StartIdcLoginRequest,
  StartIdcLoginResponse,
  PollIdcLoginResponse,
  StartSocialLoginRequest,
  StartSocialLoginResponse,
  PollSocialLoginResponse,
  CompleteSocialLoginRequest,
  GlobalProxyResponse,
  SetGlobalProxyRequest,
  UpdateConfigResponse,
  SetUpdateConfigRequest,
  ImageUpdateResponse,
  UpdateCheckInfo,
  GitHubRateLimitInfo,
  UpdateAdminKeyRequest,
} from '@/types/api'

// 创建 axios 实例
const api = axios.create({
  baseURL: '/api/admin',
  timeout: 15000,
  headers: {
    'Content-Type': 'application/json',
  },
})

// 请求拦截器添加 API Key
api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) {
    config.headers['x-api-key'] = apiKey
  }
  return config
})

// 获取所有凭据状态
export async function getCredentials(): Promise<CredentialsStatusResponse> {
  const { data } = await api.get<CredentialsStatusResponse>('/credentials')
  return data
}

// ============ KAM 导出 ============

/** KAM 导出账号（KAM 1.8.3+ 平铺格式） */
export interface KamExportAccount {
  email?: string
  nickname?: string
  idp?: string
  provider?: string
  status?: string
  authMethod?: string
  region?: string
  startUrl?: string
  clientId?: string
  clientSecret?: string
  refreshToken?: string
  accessToken?: string
  profileArn?: string
  expiresAt?: string
  machineId?: string
}

export interface KamExportResponse {
  version: string
  exportedAt: string
  accounts: KamExportAccount[]
}

/** 导出凭据为 KAM 兼容 JSON（含 refreshToken 等敏感字段）。
 *  传入 `ids` 时仅导出这些凭据；省略则导出全部。 */
export async function exportKamCredentials(
  ids?: number[]
): Promise<KamExportResponse> {
  const params = ids && ids.length > 0 ? { ids: ids.join(',') } : undefined
  const { data } = await api.get<KamExportResponse>('/credentials/export', { params })
  return data
}

// 设置凭据禁用状态
export async function setCredentialDisabled(
  id: number,
  disabled: boolean
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/disabled`,
    { disabled } as SetDisabledRequest
  )
  return data
}

// 设置凭据优先级
export async function setCredentialPriority(
  id: number,
  priority: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/priority`,
    { priority } as SetPriorityRequest
  )
  return data
}

// 重置失败计数
export async function resetCredentialFailure(
  id: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/reset`)
  return data
}

// 强制刷新 Token
export async function forceRefreshToken(
  id: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/refresh`)
  return data
}

// 解除凭据的账号级风控冷却
export async function clearThrottle(id: number): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/clear-throttle`)
  return data
}

// 获取凭据余额
export async function getCredentialBalance(id: number): Promise<BalanceResponse> {
  const { data } = await api.get<BalanceResponse>(`/credentials/${id}/balance`)
  return data
}

// 获取凭据当前可用的模型列表（按需实时查询上游）
export async function getCredentialModels(id: number): Promise<AvailableModelsResponse> {
  const { data } = await api.get<AvailableModelsResponse>(`/credentials/${id}/models`)
  return data
}

// 添加新凭据
export async function addCredential(
  req: AddCredentialRequest
): Promise<AddCredentialResponse> {
  const { data } = await api.post<AddCredentialResponse>('/credentials', req)
  return data
}

// 删除凭据
export async function deleteCredential(id: number): Promise<SuccessResponse> {
  const { data } = await api.delete<SuccessResponse>(`/credentials/${id}`)
  return data
}

// 重置单个凭据的成功次数
export async function resetSuccessCount(id: number): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/reset-stats`)
  return data
}

// 重置所有凭据的成功次数
export async function resetAllSuccessCount(): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>('/credentials/reset-stats')
  return data
}

// 一键禁用所有"已超额"凭据
export interface QuotaExceededResult {
  disabledIds: number[]
  skippedIds: number[]
}
export async function disableQuotaExceeded(): Promise<QuotaExceededResult> {
  const { data } = await api.post<QuotaExceededResult>('/credentials/disable-quota-exceeded')
  return data
}

// 设置单个凭据的超额开关
export async function setCredentialOverage(id: number, enabled: boolean): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/overage`, { enabled })
  return data
}

// 一键开启所有可开启超额的凭据
export interface EnableOverageAllResult {
  enabledIds: number[]
  skippedIds: number[]
  failedIds: number[]
  failureMessages: string[]
}
export async function enableOverageForAllCapable(): Promise<EnableOverageAllResult> {
  const { data } = await api.post<EnableOverageAllResult>('/credentials/overage/enable-all')
  return data
}

// 更新已禁用凭据的 refreshToken
export async function updateRefreshToken(
  id: number,
  req: UpdateRefreshTokenRequest
): Promise<SuccessResponse> {
  const { data } = await api.put<SuccessResponse>(`/credentials/${id}/refresh-token`, req)
  return data
}

// 更新凭据可编辑字段
export async function updateCredential(
  id: number,
  req: UpdateCredentialRequest
): Promise<SuccessResponse> {
  const { data } = await api.put<SuccessResponse>(`/credentials/${id}`, req)
  return data
}

// ============ 代理池 ============

// 获取代理池列表
export async function getProxyPool(): Promise<ProxyPoolResponse> {
  const { data } = await api.get<ProxyPoolResponse>('/proxy-pool')
  return data
}

// 添加代理
export async function addProxy(req: AddProxyRequest): Promise<ProxyPoolEntry> {
  const { data } = await api.post<ProxyPoolEntry>('/proxy-pool', req)
  return data
}

// 批量添加代理
export async function batchAddProxies(req: BatchAddProxyRequest): Promise<BatchAddProxyResponse> {
  const { data } = await api.post<BatchAddProxyResponse>('/proxy-pool/batch', req)
  return data
}

// 删除代理
export async function deleteProxy(id: number): Promise<SuccessResponse> {
  const { data } = await api.delete<SuccessResponse>(`/proxy-pool/${id}`)
  return data
}

// 设置代理启用/禁用
export async function setProxyEnabled(id: number, enabled: boolean): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/proxy-pool/${id}/enabled`, { enabled })
  return data
}

// 分配代理给凭据
export async function assignProxyToCredential(
  credentialId: number,
  req: AssignProxyRequest
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${credentialId}/proxy`, req)
  return data
}

// 即时探测单个代理连通性
export async function checkProxy(id: number): Promise<ProxyCheckResponse> {
  const { data } = await api.post<ProxyCheckResponse>(`/proxy-pool/${id}/check`)
  return data
}

// 触发全部代理健康检查
export async function checkAllProxies(): Promise<ProxyCheckAllResponse> {
  const { data } = await api.post<ProxyCheckAllResponse>('/proxy-pool/check-all')
  return data
}

// 轮询批量分配可用代理给凭据
export async function assignProxiesRoundRobin(
  credentialIds?: number[] | null
): Promise<AssignRoundRobinResponse> {
  const { data } = await api.post<AssignRoundRobinResponse>('/proxy-pool/assign-round-robin', {
    credentialIds: credentialIds ?? null,
  })
  return data
}

// 获取负载均衡模式
export async function getLoadBalancingMode(): Promise<{ mode: 'priority' | 'balanced' }> {
  const { data } = await api.get<{ mode: 'priority' | 'balanced' }>('/config/load-balancing')
  return data
}

// 设置负载均衡模式
export async function setLoadBalancingMode(mode: 'priority' | 'balanced'): Promise<{ mode: 'priority' | 'balanced' }> {
  const { data } = await api.put<{ mode: 'priority' | 'balanced' }>('/config/load-balancing', { mode })
  return data
}

export interface AccountThrottleConfig {
  failover: boolean
  cooldownSecs: number
}

// 获取账号级风控故障转移配置
export async function getAccountThrottleConfig(): Promise<AccountThrottleConfig> {
  const { data } = await api.get<AccountThrottleConfig>('/config/account-throttle')
  return data
}

// 更新账号级风控故障转移配置
export async function setAccountThrottleConfig(
  patch: Partial<AccountThrottleConfig>,
): Promise<AccountThrottleConfig> {
  const { data } = await api.put<AccountThrottleConfig>('/config/account-throttle', patch)
  return data
}

export interface LogGovernanceConfig {
  traceEnabled: boolean
  traceRetentionDays: number
  usageLogRetentionDays: number
}

// 获取日志治理配置
export async function getLogGovernanceConfig(): Promise<LogGovernanceConfig> {
  const { data } = await api.get<LogGovernanceConfig>('/config/log-governance')
  return data
}

// 更新日志治理配置
export async function setLogGovernanceConfig(
  patch: Partial<LogGovernanceConfig>,
): Promise<LogGovernanceConfig> {
  const { data } = await api.put<LogGovernanceConfig>('/config/log-governance', patch)
  return data
}

// 发起 IdC 设备授权登录
export async function startIdcLogin(
  req: StartIdcLoginRequest
): Promise<StartIdcLoginResponse> {
  const { data } = await api.post<StartIdcLoginResponse>('/auth/idc/start', req)
  return data
}

// 轮询 IdC 登录状态
export async function pollIdcLogin(sessionId: string): Promise<PollIdcLoginResponse> {
  const { data } = await api.post<PollIdcLoginResponse>(`/auth/idc/poll/${sessionId}`)
  return data
}

// 获取全局代理配置
export async function getGlobalProxy(): Promise<GlobalProxyResponse> {
  const { data } = await api.get<GlobalProxyResponse>('/config/global-proxy')
  return data
}

// 设置全局代理配置
export async function setGlobalProxy(req: SetGlobalProxyRequest): Promise<SuccessResponse> {
  const { data } = await api.put<SuccessResponse>('/config/global-proxy', req)
  return data
}

// 获取镜像在线更新配置
export async function getUpdateConfig(): Promise<UpdateConfigResponse> {
  const { data } = await api.get<UpdateConfigResponse>('/config/update')
  return data
}

// 设置镜像在线更新配置
export async function setUpdateConfig(req: SetUpdateConfigRequest): Promise<UpdateConfigResponse> {
  const { data } = await api.put<UpdateConfigResponse>('/config/update', req)
  return data
}

// 拉取配置的 GHCR 镜像
export async function pullUpdateImage(): Promise<ImageUpdateResponse> {
  const { data } = await api.post<ImageUpdateResponse>('/system/update/pull')
  return data
}

// 拉取镜像并通过 Docker Compose 应用更新
export async function applyImageUpdate(): Promise<ImageUpdateResponse> {
  const { data } = await api.post<ImageUpdateResponse>('/system/update/apply')
  return data
}

// 通过本地备份 tag 回退到上一次更新前的镜像版本
export async function rollbackImageUpdate(): Promise<ImageUpdateResponse> {
  const { data } = await api.post<ImageUpdateResponse>('/system/update/rollback')
  return data
}

// 检查 GitHub Releases 是否有新版本（带后端 30 分钟缓存；force=true 强制刷新）
export async function checkSystemUpdate(force = false): Promise<UpdateCheckInfo> {
  const { data } = await api.get<UpdateCheckInfo>('/system/update/check', {
    params: force ? { force: 'true' } : undefined,
  })
  return data
}

// 查询 GitHub API 当前限流状态（可附带 token 用于"保存前先验证"）
export async function checkGitHubRateLimit(
  githubToken?: string,
): Promise<GitHubRateLimitInfo> {
  const body = githubToken ? { githubToken } : {}
  const { data } = await api.post<GitHubRateLimitInfo>(
    '/system/update/rate-limit',
    body,
  )
  return data
}

// 修改登录API密钥
export async function updateAdminKey(req: UpdateAdminKeyRequest): Promise<SuccessResponse> {
  const { data } = await api.put<SuccessResponse>('/config/admin-key', req)
  return data
}

// 修改管理员API密钥
export async function updateApiKey(req: UpdateAdminKeyRequest): Promise<SuccessResponse> {
  const { data } = await api.put<SuccessResponse>('/config/api-key', req)
  return data
}

// 发起 Social 登录
export async function startSocialLogin(
  req: StartSocialLoginRequest
): Promise<StartSocialLoginResponse> {
  const { data } = await api.post<StartSocialLoginResponse>('/auth/social/start', req)
  return data
}

// 轮询 Social 登录状态
export async function pollSocialLogin(sessionId: string): Promise<PollSocialLoginResponse> {
  const { data } = await api.post<PollSocialLoginResponse>(`/auth/social/poll/${sessionId}`)
  return data
}

// 手动完成 Social 登录（远程访问时粘贴回调 URL）
export async function completeSocialLogin(
  sessionId: string,
  req: CompleteSocialLoginRequest
): Promise<PollSocialLoginResponse> {
  const { data } = await api.post<PollSocialLoginResponse>(`/auth/social/complete/${sessionId}`, req)
  return data
}

// ============ 重新登录（更新已有凭据 Token） ============

// 发起 Social 重新登录
export async function startSocialRelogin(
  credentialId: number,
  req: StartSocialLoginRequest
): Promise<StartSocialLoginResponse> {
  const { data } = await api.post<StartSocialLoginResponse>(
    `/credentials/${credentialId}/relogin/social/start`,
    req
  )
  return data
}

// 轮询 Social 重新登录状态
export async function pollSocialRelogin(
  credentialId: number,
  sessionId: string
): Promise<PollSocialLoginResponse> {
  const { data } = await api.post<PollSocialLoginResponse>(
    `/credentials/${credentialId}/relogin/social/poll/${sessionId}`
  )
  return data
}

// 手动完成 Social 重新登录（远程访问时粘贴回调 URL）
export async function completeSocialRelogin(
  credentialId: number,
  sessionId: string,
  req: CompleteSocialLoginRequest
): Promise<PollSocialLoginResponse> {
  const { data } = await api.post<PollSocialLoginResponse>(
    `/credentials/${credentialId}/relogin/social/complete/${sessionId}`,
    req
  )
  return data
}

// 发起 IdC 重新登录
export async function startIdcRelogin(
  credentialId: number,
  req: StartIdcLoginRequest
): Promise<StartIdcLoginResponse> {
  const { data } = await api.post<StartIdcLoginResponse>(
    `/credentials/${credentialId}/relogin/idc/start`,
    req
  )
  return data
}

// 轮询 IdC 重新登录状态
export async function pollIdcRelogin(
  credentialId: number,
  sessionId: string
): Promise<PollIdcLoginResponse> {
  const { data } = await api.post<PollIdcLoginResponse>(
    `/credentials/${credentialId}/relogin/idc/poll/${sessionId}`
  )
  return data
}
