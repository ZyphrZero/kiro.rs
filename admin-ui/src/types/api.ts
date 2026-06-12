// 凭据状态响应
export interface CredentialsStatusResponse {
  total: number
  available: number
  currentId: number
  credentials: CredentialStatusItem[]
}

// 单个凭据状态
export interface CredentialStatusItem {
  id: number
  priority: number
  disabled: boolean
  failureCount: number
  /** 累计失败次数（所有失败类型，只增不减，仅手动重置归零） */
  totalFailureCount: number
  isCurrent: boolean
  expiresAt: string | null
  authMethod: string | null
  provider?: string | null
  hasProfileArn: boolean
  email?: string
  refreshTokenHash?: string
  apiKeyHash?: string
  maskedApiKey?: string
  successCount: number
  lastUsedAt: string | null
  hasProxy: boolean
  proxyUrl?: string
  refreshFailureCount: number
  disabledReason?: string
  /** 账号级风控冷却剩余秒数（>0 表示冷却中） */
  throttledRemainingSecs?: number
  endpoint: string
  /** 账号所属分组（可属于多个分组） */
  groups?: string[]
  /** 账号来源渠道（纯备注） */
  sourceChannel?: string
  /** 后端缓存的最近一次余额（5 分钟内） */
  balance?: BalanceResponse
  /** 余额缓存的更新时间（Unix 秒） */
  balanceUpdatedAt?: number
}

// 余额响应
export interface BalanceResponse {
  id: number
  subscriptionTitle: string | null
  currentUsage: number
  usageLimit: number
  remaining: number
  usagePercentage: number
  nextResetAt: number | null
  /** 用户是否当前开启了超额 */
  overageEnabled?: boolean
  /** 账号订阅是否可以开启超额 */
  overageCapable?: boolean
  /** 上游 overageCapability 原始字符串，用于排查"未知"状态 */
  overageCapabilityRaw?: string
}

// 某凭据当前可用的模型列表响应
export interface AvailableModelsResponse {
  id: number
  models: AvailableModelItem[]
}

// 单个可用模型
export interface AvailableModelItem {
  modelId: string
  modelName?: string
  description?: string
  maxInputTokens?: number
}

// 成功响应
export interface SuccessResponse {
  success: boolean
  message: string
}

// 错误响应
export interface AdminErrorResponse {
  error: {
    type: string
    message: string
  }
}

// 请求类型
export interface SetDisabledRequest {
  disabled: boolean
}

export interface SetPriorityRequest {
  priority: number
}

// 添加凭据请求
export interface AddCredentialRequest {
  refreshToken?: string
  accessToken?: string
  profileArn?: string
  expiresAt?: string
  authMethod?: 'social' | 'idc' | 'api_key'
  provider?: string
  clientId?: string
  clientSecret?: string
  startUrl?: string
  priority?: number
  authRegion?: string
  apiRegion?: string
  machineId?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  kiroApiKey?: string
  endpoint?: string
  email?: string
  groups?: string[]
  sourceChannel?: string
}

// 添加凭据响应
export interface AddCredentialResponse {
  success: boolean
  message: string
  credentialId: number
  email?: string
}

// 更新凭据请求（字段为 undefined 表示不修改，空字符串表示清除）
export interface UpdateCredentialRequest {
  email?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  /** 账号所属分组（undefined 表示不修改，数组表示整体替换） */
  groups?: string[]
  /** 账号来源渠道（undefined 表示不修改，空串表示清除） */
  sourceChannel?: string
}

// 更新 refreshToken 请求
export interface UpdateRefreshTokenRequest {
  refreshToken: string
  accessToken?: string
  expiresAt?: string
}

// 代理健康状态
export type ProxyHealth = 'unknown' | 'healthy' | 'unhealthy'

// 代理池条目
export interface ProxyPoolEntry {
  id: number
  url: string
  label?: string
  enabled: boolean
  credentialCount: number
  health: ProxyHealth
  latencyMs?: number
  lastCheckedAt?: string
  consecutiveFailures: number
  autoDisabled: boolean
}

// 代理池列表响应
export interface ProxyPoolResponse {
  total: number
  proxies: ProxyPoolEntry[]
}

// 添加代理请求
export interface AddProxyRequest {
  url: string
  label?: string
}

// 批量添加代理请求
export interface BatchAddProxyRequest {
  urls: string[]
}

// 分配代理给凭据请求
export interface AssignProxyRequest {
  proxyId?: number | null
}

// 批量添加代理响应
export interface BatchAddProxyResponse {
  added: number
  errors: number
  proxies: ProxyPoolEntry[]
  errorMessages: string[]
}

// 单个代理健康检查响应
export interface ProxyCheckResponse {
  id: number
  health: ProxyHealth
  latencyMs?: number
  lastCheckedAt?: string
  enabled: boolean
  autoDisabled: boolean
}

// 全量健康检查响应
export interface ProxyCheckAllResponse {
  healthy: number
  unhealthy: number
  autoDisabled: number
}

// 轮询批量分配请求
export interface AssignRoundRobinRequest {
  credentialIds?: number[] | null
}

// 轮询批量分配响应
export interface AssignRoundRobinResponse {
  assigned: number
  proxyCount: number
}

// 全局代理配置
export interface GlobalProxyResponse {
  proxyUrl: string | null
}

export interface SetGlobalProxyRequest {
  proxyUrl: string | null
}

// 在线更新配置
export interface UpdateConfigResponse {
  /** 上一次更新前正在运行的版本号（带 v 前缀）；存在时可调用回退接口 */
  previousVersion?: string
  /** 上一次成功完成在线更新的时间（RFC3339） */
  lastAppliedAt?: string
  /** 是否已配置 GitHub Token（仅返回布尔，不回明文） */
  githubTokenSet: boolean
  /** 是否开启无人值守自动更新 */
  autoApply: boolean
  /** 自动更新触发时间（本地时区，HH:MM 24 小时制） */
  autoApplyTime: string
}

export interface SetUpdateConfigRequest {
  /** GitHub Personal Access Token；空字符串表示清除 */
  githubToken?: string
  autoApply?: boolean
  autoApplyTime?: string
}

/** GitHub API 限流状态（含 token 验证结果） */
export interface GitHubRateLimitInfo {
  /** 提供的 token 是否有效（无 token 时为 false 但仍能查到匿名限额） */
  valid: boolean
  /** 是否带 token 调用（false = 匿名查询） */
  authenticated: boolean
  /** 限流上限（匿名 60，认证 5000） */
  limit: number
  /** 剩余可用次数 */
  remaining: number
  /** 已用次数 */
  used: number
  /** 限流窗口重置时间（Unix 秒） */
  reset: number
  /** token 对应的用户名（可能为空） */
  login?: string
  /** 失败时的提示信息 */
  warning?: string
}

export interface ImageUpdateResponse {
  success: boolean
  message: string
  output?: string
  applied: boolean
  needRestart: boolean
}

export interface UpdateCheckInfo {
  currentVersion: string
  latestVersion: string
  hasUpdate: boolean
  buildType: string
  releaseName?: string
  releaseNotes?: string
  releaseUrl?: string
  publishedAt?: string
  checkedAt: string
  cached: boolean
  warning?: string
}

// Admin Key 修改
export interface UpdateAdminKeyRequest {
  newKey: string
}

// IdC 设备授权登录
export interface StartIdcLoginRequest {
  region: string
  startUrl?: string
  priority?: number
  email?: string
  proxyUrl?: string
}

export interface StartIdcLoginResponse {
  sessionId: string
  userCode: string
  verificationUri: string
  verificationUriComplete?: string
  expiresAt: string
  pollInterval: number
}

export type PollIdcLoginResponse =
  | { status: 'pending' }
  | { status: 'success'; credentialId: number }
  | { status: 'expired' }

// Social 登录（Portal PKCE OAuth）
export interface StartSocialLoginRequest {
  priority?: number
  email?: string
  proxyUrl?: string
  authEndpoint?: string
}

/** 远程访问时手动完成 Social 登录：从浏览器地址栏粘贴的回调 URL 中提取参数 */
export interface CompleteSocialLoginRequest {
  code: string
  state: string
  loginOption?: string
  path?: string
}

export interface StartSocialLoginResponse {
  sessionId: string
  portalUrl: string
  expiresAt: string
}

export type PollSocialLoginResponse = PollIdcLoginResponse

// ============ 客户端 API Key 分发 ============

export interface ClientKeyItem {
  id: number
  /** 脱敏后的 Key（仅展示） */
  maskedKey: string
  name: string
  description?: string
  disabled: boolean
  createdAt: string
  lastUsedAt?: string
  totalCalls: number
  totalInputTokens: number
  totalOutputTokens: number
  totalCacheCreationTokens: number
  totalCacheReadTokens: number
  /** 绑定的账号分组（未绑定时为 undefined） */
  group?: string
}

export interface ClientKeysResponse {
  total: number
  keys: ClientKeyItem[]
}

export interface CreateClientKeyRequest {
  name: string
  description?: string
  group?: string
}

/** 创建响应：明文 Key 仅在此处返回一次 */
export interface CreateClientKeyResponse {
  id: number
  key: string
  name: string
  createdAt: string
}

export interface UpdateClientKeyRequest {
  name?: string
  description?: string
  group?: string
}

// ============ 用量统计 ============

export type StatsRange = '24h' | '7d' | '30d'
export type StatsGranularity = 'hour' | 'day'

export interface StatsTimeFilter {
  range?: StatsRange
  startDate?: string
  endDate?: string
  granularity: StatsGranularity
}

export interface StatsFilter {
  /** 不传 = 全部；0 = 管理员API密钥；其它值 = 创建的客户端 Key id */
  keyId?: number
  /** 按账号分组筛选（仅影响 timeseries / by-credential，by-model 不支持） */
  group?: string
}

export interface OverviewStats {
  todayCalls: number
  todayInputTokens: number
  todayOutputTokens: number
  todayErrors: number
  todayCredits: number
  weekCalls: number
  weekInputTokens: number
  weekOutputTokens: number
  weekCredits: number
  activeClientKeys: number
  activeCredentials: number
}

export interface TimeSeriesPoint {
  ts: string
  inputTokens: number
  outputTokens: number
  cacheCreationTokens: number
  cacheReadTokens: number
  calls: number
  errors: number
  credits: number
}

export interface ModelDistribution {
  model: string
  calls: number
  inputTokens: number
  outputTokens: number
}

export interface CredentialDistribution {
  credentialId: number
  email?: string
  calls: number
  inputTokens: number
  outputTokens: number
  errors: number
}

// ============ 请求链路追踪 ============

/** 单次上游尝试 */
export interface TraceAttempt {
  attempt: number
  credentialId: number
  email?: string | null
  endpoint: string
  /** 上游 HTTP 状态码；null = 网络层失败 */
  httpStatus: number | null
  /** success / quota_exhausted / account_throttled / auth_failed / transient / network_error / bad_request / unknown */
  outcome: string
  /** 上游错误体片段（已截断） */
  errorSnippet: string | null
  durationMs: number
}

/** 一个外部请求的完整链路 */
export interface TraceRecord {
  traceId: string
  ts: string
  keyId: number
  /** masterApiKey = 管理员API密钥；clientKey = 创建的客户端 Key */
  keySource: 'masterApiKey' | 'clientKey'
  /** 发起请求的客户端 Key 名称（master 表示主 apiKey；管理员业务 Key 可为 null） */
  keyName?: string | null
  model: string
  isStream: boolean
  /** success / error / interrupted */
  finalStatus: string
  finalCredentialId: number
  finalEmail?: string | null
  errorType: string | null
  errorMessage: string | null
  totalAttempts: number
  durationMs: number
  /** 流式中断时已发送字节数 */
  interruptedAfterBytes: number | null
  /** 输入 token */
  inputTokens?: number
  /** 输出 token */
  outputTokens?: number
  /** 缓存创建 token */
  cacheCreationTokens?: number
  /** 缓存读取 token */
  cacheReadTokens?: number
  /** 总 token = input + output + cache_creation + cache_read */
  totalTokens?: number
  /** 费用（credits） */
  credits?: number
  /** 首 Token 延迟（毫秒，仅流式有值） */
  firstTokenMs?: number | null
  attempts: TraceAttempt[]
}

/** 链路查询参数 */
export interface TraceQuery {
  status?: string
  errorType?: string
  credentialId?: number
  /** 按发起请求的客户端 Key 筛选（0 = master apiKey） */
  keyId?: number
  /** 该凭据在某一跳失败过（即便 trace 最终成功）——用于凭据失败详情 */
  failedAttemptCredentialId?: number
  model?: string
  onlyFailed?: boolean
  limit?: number
  offset?: number
}

/** 分页响应 */
export interface TracePage {
  records: TraceRecord[]
  total: number
}

/** 单凭据失败分类计数（鉴权 / 账号风控 / 其他） */
export interface FailureStats {
  auth: number
  throttle: number
  other: number
}

/** credentialId(字符串) → 失败分类计数 */
export type FailureStatsMap = Record<string, FailureStats>

// ============ 账号分组（独立实体）============

export interface GroupItem {
  name: string
  description?: string
  createdAt: string
  /** 引用计数：有多少个凭据带这个分组 */
  credentialCount: number
  /** 引用计数：有多少把客户端 Key 绑定这个分组 */
  clientKeyCount: number
}

export interface GroupsResponse {
  total: number
  groups: GroupItem[]
}

export interface CreateGroupRequest {
  name: string
  description?: string
}

export interface UpdateGroupRequest {
  /** 新名字；不传或与原名一致则不改名 */
  newName?: string
  /** 新备注；空字符串清除；undefined 保留原值 */
  description?: string
}
