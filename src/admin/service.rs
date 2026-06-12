//! Admin API 业务逻辑服务

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Duration, Timelike, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::http_client::ProxyConfig;
use crate::kiro::auth::idc::{self, BUILDER_ID_START_URL};
use crate::kiro::auth::social;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::Config;

use super::error::AdminServiceError;
use super::proxy_pool::{GetUrlResult, ProxyPoolManager};
use super::types::{
    AccountThrottleConfigResponse, AddCredentialRequest, AddCredentialResponse,
    AssignProxyRequest, AssignRoundRobinResponse, AvailableModelItem, AvailableModelsResponse,
    BalanceResponse, BatchAddProxyRequest,
    CheckRateLimitRequest, CredentialStatusItem, CredentialsStatusResponse, EnableOverageAllResult,
    GitHubRateLimitInfo, ImageUpdateResponse, ExportedAccount, ExportedCredentials,
    CredentialsExportResponse,
    LoadBalancingModeResponse, LogGovernanceConfigResponse, PollIdcLoginResponse,
    ProxyCheckAllResponse, ProxyCheckResponse, ProxyPoolEntry, ProxyPoolResponse,
    QuotaExceededResult, SetAccountThrottleConfigRequest, SetLoadBalancingModeRequest,
    SetLogGovernanceConfigRequest, SetUpdateConfigRequest, StartIdcLoginRequest,
    StartIdcLoginResponse, StartSocialLoginRequest, StartSocialLoginResponse, UpdateCheckInfo,
    UpdateConfigResponse, UpdateCredentialRequest, UpdateRefreshTokenRequest,
};

/// 余额缓存过期时间（秒），5 分钟
const BALANCE_CACHE_TTL_SECS: i64 = 300;

/// 在线检查更新结果缓存时间（秒），30 分钟。
/// 在线检查更新结果缓存时间（秒），30 分钟。
/// Docker Hub 的 tags 接口对匿名访问有 IP 维度的限流，30 分钟 TTL 既能让用户
/// 看到红点提醒，又能避免短时间内重复请求被限流。
const UPDATE_CHECK_TTL_SECS: i64 = 1800;

/// 缓存的余额条目（含时间戳）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedBalance {
    /// 缓存时间（Unix 秒）
    cached_at: f64,
    /// 缓存的余额数据
    data: BalanceResponse,
}

/// 缓存的"检查更新"结果
#[derive(Debug, Clone)]
struct CachedUpdateCheck {
    /// 缓存时间
    cached_at: DateTime<Utc>,
    /// 拉取到的更新信息
    info: UpdateCheckInfo,
}

#[derive(Debug, Clone)]
struct RuntimeUpdateConfig {
    previous_version: Option<String>,
    last_applied_at: Option<String>,
    github_token: Option<String>,
    auto_apply: bool,
    auto_apply_time: String,
}

impl RuntimeUpdateConfig {
    fn from_config(config: &Config) -> Self {
        Self {
            previous_version: config.update_previous_version.clone(),
            last_applied_at: config.update_last_applied_at.clone(),
            github_token: config.github_token.clone(),
            auto_apply: config.update_auto_apply,
            auto_apply_time: config.update_auto_apply_time.clone(),
        }
    }

    fn response(&self) -> UpdateConfigResponse {
        UpdateConfigResponse {
            previous_version: self.previous_version.clone(),
            last_applied_at: self.last_applied_at.clone(),
            github_token_set: self
                .github_token
                .as_deref()
                .map(|t| !t.trim().is_empty())
                .unwrap_or(false),
            auto_apply: self.auto_apply,
            auto_apply_time: self.auto_apply_time.clone(),
        }
    }
}

/// Admin 服务
///
/// 封装所有 Admin API 的业务逻辑
pub struct AdminService {
    token_manager: Arc<MultiTokenManager>,
    balance_cache: Mutex<HashMap<u64, CachedBalance>>,
    cache_path: Option<PathBuf>,
    /// 已注册的端点名称集合（用于 add_credential 校验）
    known_endpoints: HashSet<String>,
    /// 代理 IP 池管理器
    proxy_pool: ProxyPoolManager,
    /// 在线镜像更新运行时配置
    update_config: Mutex<RuntimeUpdateConfig>,
    /// 最近一次"检查更新"结果（带 TTL，用于减少 GitHub API 调用）
    update_check_cache: Mutex<Option<CachedUpdateCheck>>,
    /// 进行中的 IdC 设备授权会话
    idc_sessions: Arc<Mutex<HashMap<String, IdcAuthSession>>>,
    /// 进行中的 Social 登录会话
    social_sessions: Arc<Mutex<HashMap<String, SocialAuthSession>>>,
    /// 请求链路追踪存储（用于日志治理：开关 + 保留天数运行时可改）
    trace_store: Option<crate::admin::trace_db::SharedTraceStore>,
    /// 用量日志记录器（用于日志治理：保留天数运行时可改）
    usage_recorder: Option<crate::admin::usage_stats::SharedRecorder>,
}

/// Social 登录会话状态
struct SocialAuthSession {
    auth_endpoint: String,
    /// 发起时生成的 state，用于 CSRF 验证
    state: String,
    code_verifier: String,
    redirect_uri: String,
    expires_at: DateTime<Utc>,
    /// 收到 OAuth 回调时的数据（code + login_option + path）
    callback_rx: tokio::sync::Mutex<tokio::sync::oneshot::Receiver<social::OAuthCallbackData>>,
    cred_template: KiroCredentials,
    proxy: Option<ProxyConfig>,
    /// Drop 时自动关闭回调服务器并释放端口
    _server_handle: social::ServerHandle,
    /// 重新登录时更新此凭据的 Token（非 None 时更新已有凭据而非创建新凭据）
    relogin_target_id: Option<u64>,
}

/// IdC 设备授权会话状态
struct IdcAuthSession {
    region: String,
    client_id: String,
    client_secret: String,
    device_code: String,
    expires_at: DateTime<Utc>,
    poll_interval: i64,
    /// 登录成功后写入的凭据配置
    cred_template: KiroCredentials,
    /// 用于发起 token 请求的代理
    proxy: Option<ProxyConfig>,
    /// 重新登录时更新此凭据的 Token（非 None 时更新已有凭据而非创建新凭据）
    relogin_target_id: Option<u64>,
}

/// 解析自动更新触发时间（`HH:MM`，本地 24 小时制）。允许 `H:M` 简写，
/// 例如 `3:0`；解析失败时返回原字符串，便于错误信息提示。
fn parse_auto_apply_time(value: &str) -> Result<(u32, u32), AdminServiceError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AdminServiceError::InvalidCredential(
            "自动更新时间不能为空".to_string(),
        ));
    }
    let mut parts = trimmed.splitn(2, ':');
    let hour_str = parts.next().unwrap_or("");
    let minute_str = parts.next().unwrap_or("");
    let hour: u32 = hour_str.parse().map_err(|_| {
        AdminServiceError::InvalidCredential(format!(
            "自动更新时间格式无效：{}（应为 HH:MM）",
            value
        ))
    })?;
    let minute: u32 = minute_str.parse().map_err(|_| {
        AdminServiceError::InvalidCredential(format!(
            "自动更新时间格式无效：{}（应为 HH:MM）",
            value
        ))
    })?;
    if hour > 23 || minute > 59 {
        return Err(AdminServiceError::InvalidCredential(format!(
            "自动更新时间超出范围：{}（HH 0-23，MM 0-59）",
            value
        )));
    }
    Ok((hour, minute))
}

/// 把 HH:MM 规范化成 `HH:MM`（两位补零），方便存储和比较。
fn normalize_auto_apply_time(value: &str) -> Result<String, AdminServiceError> {
    let (h, m) = parse_auto_apply_time(value)?;
    Ok(format!("{:02}:{:02}", h, m))
}

/// GitHub `repos/{owner}/{repo}/releases/tags/{tag}` 返回 JSON 中我们关心
/// 的字段，用于在「检查更新」结果里附带本次发布的 changelog。
#[derive(Debug, Deserialize)]
struct GitHubRelease {
    #[serde(default)]
    name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    published_at: String,
    #[serde(default)]
    tag_name: String,
}

/// 比较两个 semver 字符串。仅按 `MAJOR.MINOR.PATCH` 三段数字比较，忽略
/// 预发布后缀；解析失败的段当作 0 处理（最坏情况下"无更新"）。
fn compare_semver(current: &str, latest: &str) -> std::cmp::Ordering {
    parse_semver_core(current).cmp(&parse_semver_core(latest))
}

/// 解析 semver 三段数字，解析失败的段作 0；用于 latest tag 的稳定排序。
fn parse_semver_core(value: &str) -> [u32; 3] {
    let core = value
        .trim_start_matches('v')
        .split(|c: char| c == '-' || c == '+')
        .next()
        .unwrap_or("");
    let mut out = [0u32; 3];
    for (i, part) in core.splitn(3, '.').enumerate() {
        if i >= 3 {
            break;
        }
        out[i] = part.parse::<u32>().unwrap_or(0);
    }
    out
}

/// 当前构建类型。在线更新走"下载 GitHub Releases 二进制 + 进程退出由
/// docker restart policy 接管重启"的方案。
const BUILD_TYPE: &str = "binary";

/// 暂存路径：下载到 `<exe>.staged`，原子替换前再 mv 到 `<exe>`。
/// 暂存路径：下载到 `<exe>.staged-<version>`，原子替换前再 mv 到 `<exe>`。
/// 文件名中带版本号，便于 apply 复用 pull 已下载的二进制（命中时跳过重新下载）。
fn staged_binary_path(exe: &std::path::Path, version: &str) -> std::path::PathBuf {
    let mut s = exe.as_os_str().to_os_string();
    s.push(format!(".staged-{}", version.trim().trim_start_matches('v')));
    std::path::PathBuf::from(s)
}

/// 清理目标版本之外的所有 staged 文件，避免之前下载的旧版本残留干扰。
fn cleanup_other_staged(exe: &std::path::Path, keep_version: &str) {
    let dir = match exe.parent() {
        Some(d) => d,
        None => return,
    };
    let exe_name = match exe.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return,
    };
    let keep = format!(
        "{}.staged-{}",
        exe_name,
        keep_version.trim().trim_start_matches('v')
    );
    let prefix = format!("{}.staged-", exe_name);
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if name.starts_with(&prefix) && name != keep {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 将单个凭据映射为嵌套 `Account` 结构
///
/// API Key 凭据无 refreshToken，导出格式无对应字段，跳过。
/// 空字符串字段会被过滤，保持导出 JSON 整洁。
fn credential_to_export_account(cred: KiroCredentials) -> Option<ExportedAccount> {
    let refresh_token = cred
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)?;

    fn non_empty(value: Option<String>) -> Option<String> {
        value
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    // authMethod 规范化："idc" → "IdC"，其余按 social 处理
    let auth_method = non_empty(cred.auth_method.clone()).map(|m| {
        if m.eq_ignore_ascii_case("idc")
            || m.eq_ignore_ascii_case("builder-id")
            || m.eq_ignore_ascii_case("iam")
        {
            "IdC".to_string()
        } else {
            "social".to_string()
        }
    });
    let is_idc = auth_method.as_deref() == Some("IdC");

    let provider = non_empty(cred.provider.clone());
    // idp 与 provider 同义；缺失时按认证方式回退到合法的身份提供商
    let idp = provider
        .clone()
        .unwrap_or_else(|| if is_idc { "BuilderId" } else { "Google" }.to_string());

    let status = if cred.disabled {
        "unknown".to_string()
    } else {
        "active".to_string()
    };

    // expiresAt → 毫秒时间戳（解析失败或缺失时为 0）
    let expires_at_ms = cred
        .expires_at
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0);

    // 订阅：最小可用结构（type + 原始 title）
    let subscription = serde_json::json!({
        "type": subscription_type_from_title(cred.subscription_title.as_deref()),
        "title": cred.subscription_title,
    });
    let now_ms = Utc::now().timestamp_millis();
    let usage = serde_json::json!({
        "current": 0,
        "limit": 0,
        "percentUsed": 0,
        "lastUpdated": now_ms,
    });

    // 仅导出真实 profileArn，跳过 BuilderID 占位符
    let profile_arn = cred.effective_profile_arn().map(str::to_string);

    let credentials = ExportedCredentials {
        access_token: non_empty(cred.access_token).unwrap_or_default(),
        csrf_token: String::new(),
        refresh_token: Some(refresh_token),
        client_id: non_empty(cred.client_id),
        client_secret: non_empty(cred.client_secret),
        region: non_empty(cred.region.clone())
            .or_else(|| non_empty(cred.auth_region.clone()))
            .or_else(|| non_empty(cred.api_region.clone())),
        start_url: non_empty(cred.start_url.clone()),
        expires_at: expires_at_ms,
        auth_method,
        provider: provider.clone(),
    };

    Some(ExportedAccount {
        id: uuid::Uuid::new_v4().to_string(),
        email: non_empty(cred.email).unwrap_or_default(),
        nickname: None,
        idp,
        user_id: None,
        profile_arn,
        machine_id: non_empty(cred.machine_id),
        credentials,
        subscription,
        usage,
        tags: Vec::new(),
        status,
        created_at: now_ms,
        last_used_at: now_ms,
    })
}

/// 由订阅标题推断 `SubscriptionType`（粗粒度，导入方刷新后会自行校正）
fn subscription_type_from_title(title: Option<&str>) -> &'static str {
    let Some(title) = title else { return "Free" };
    let u = title.to_uppercase();
    if u.contains("FREE") {
        "Free"
    } else if u.contains("PRO+") || u.contains("PRO PLUS") || u.contains("PRO_PLUS") {
        "Pro_Plus"
    } else if u.contains("POWER") || u.contains("ENTERPRISE") || u.contains("TEAM") {
        "Enterprise"
    } else if u.contains("PRO") {
        "Pro"
    } else {
        "Free"
    }
}

/// GitHub Release 仓库名（owner/repo）。
/// 在线更新所需的版本号、changelog、二进制资产都从这里取。
const GITHUB_RELEASES_REPO: &str = "ZyphrZero/kiro.rs";

impl AdminService {
    pub fn new(
        token_manager: Arc<MultiTokenManager>,
        known_endpoints: impl IntoIterator<Item = String>,
    ) -> Self {
        let cache_path = token_manager
            .cache_dir()
            .map(|d| d.join("kiro_balance_cache.json"));

        let proxy_pool_path = token_manager.cache_dir().map(|d| d.join("proxy_pool.json"));
        let token_manager_tls_backend = token_manager.config().tls_backend;

        let balance_cache = Self::load_balance_cache_from(&cache_path);
        let update_config = RuntimeUpdateConfig::from_config(token_manager.config());

        let svc = Self {
            token_manager,
            balance_cache: Mutex::new(balance_cache),
            cache_path,
            known_endpoints: known_endpoints.into_iter().collect(),
            proxy_pool: ProxyPoolManager::new(proxy_pool_path, token_manager_tls_backend),
            update_config: Mutex::new(update_config),
            update_check_cache: Mutex::new(None),
            idc_sessions: Arc::new(Mutex::new(HashMap::new())),
            social_sessions: Arc::new(Mutex::new(HashMap::new())),
            trace_store: None,
            usage_recorder: None,
        };

        // 后台任务：每 5 分钟清理过期的登录会话，防止内存泄漏
        {
            let idc = Arc::clone(&svc.idc_sessions);
            let social = Arc::clone(&svc.social_sessions);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
                loop {
                    interval.tick().await;
                    let now = Utc::now();
                    idc.lock().retain(|_, s| now < s.expires_at);
                    social.lock().retain(|_, s| now < s.expires_at);
                }
            });
        }

        svc
    }

    /// 暴露 TokenManager 给 handlers（分组管理需要 count / rename / remove 凭据 groups 字段）
    pub fn token_manager(&self) -> &Arc<MultiTokenManager> {
        &self.token_manager
    }

    /// 注入日志治理句柄（trace 存储 + 用量记录器），用于运行时改保留期/开关。
    pub fn with_log_governance(
        mut self,
        trace_store: Option<crate::admin::trace_db::SharedTraceStore>,
        usage_recorder: Option<crate::admin::usage_stats::SharedRecorder>,
    ) -> Self {
        self.trace_store = trace_store;
        self.usage_recorder = usage_recorder;
        self
    }

    /// 获取所有凭据状态
    pub fn get_all_credentials(&self) -> CredentialsStatusResponse {
        let snapshot = self.token_manager.snapshot();
        let default_endpoint = self.token_manager.config().default_endpoint.clone();

        // 一次性快照余额缓存，避免 N 次加锁
        let balance_snapshot: HashMap<u64, CachedBalance> = {
            let cache = self.balance_cache.lock();
            cache.clone()
        };
        let now_ts = Utc::now().timestamp() as f64;

        let mut credentials: Vec<CredentialStatusItem> = snapshot
            .entries
            .into_iter()
            .map(|entry| {
                let (balance, balance_updated_at) = balance_snapshot
                    .get(&entry.id)
                    .filter(|c| (now_ts - c.cached_at) < BALANCE_CACHE_TTL_SECS as f64)
                    .map(|c| (Some(c.data.clone()), Some(c.cached_at)))
                    .unwrap_or((None, None));

                CredentialStatusItem {
                    id: entry.id,
                    priority: entry.priority,
                    disabled: entry.disabled,
                    failure_count: entry.failure_count,
                    total_failure_count: entry.total_failure_count,
                    is_current: entry.id == snapshot.current_id,
                    expires_at: entry.expires_at,
                    auth_method: entry.auth_method,
                    provider: entry.provider,
                    has_profile_arn: entry.has_profile_arn,
                    refresh_token_hash: entry.refresh_token_hash,
                    api_key_hash: entry.api_key_hash,
                    masked_api_key: entry.masked_api_key,
                    email: entry.email,
                    success_count: entry.success_count,
                    last_used_at: entry.last_used_at.clone(),
                    has_proxy: entry.has_proxy,
                    proxy_url: entry.proxy_url,
                    refresh_failure_count: entry.refresh_failure_count,
                    disabled_reason: entry.disabled_reason,
                    endpoint: entry.endpoint.unwrap_or_else(|| default_endpoint.clone()),
                    groups: entry.groups,
                    source_channel: entry.source_channel,
                    balance,
                    balance_updated_at,
                }
            })
            .collect();

        // 按优先级排序（数字越小优先级越高）
        credentials.sort_by_key(|c| c.priority);

        CredentialsStatusResponse {
            total: snapshot.total,
            available: snapshot.available,
            current_id: snapshot.current_id,
            credentials,
        }
    }

    /// 导出凭据为兼容 JSON（嵌套 `Account` 格式）
    ///
    /// 返回的结构体含 refreshToken、accessToken、clientSecret 等敏感字段，
    /// 调用方需自行保证传输与存储安全；按 priority 升序排序，与 UI 列表一致。
    /// `id_filter` 为 None 时导出全部凭据；为 Some 时仅导出集合内的 ID。
    pub fn export_credentials(
        &self,
        id_filter: Option<&HashSet<u64>>,
    ) -> CredentialsExportResponse {
        let mut credentials = self.token_manager.clone_all_credentials();
        if let Some(filter) = id_filter {
            credentials.retain(|c| c.id.map(|id| filter.contains(&id)).unwrap_or(false));
        }
        credentials.sort_by_key(|c| c.priority);

        let accounts = credentials
            .into_iter()
            .filter_map(credential_to_export_account)
            .collect();

        CredentialsExportResponse {
            version: "1.8.3".to_string(),
            exported_at: Utc::now().timestamp_millis(),
            accounts,
            groups: Vec::new(),
            tags: Vec::new(),
        }
    }

    /// 一键禁用所有"已超额"的凭据（remaining ≤ 0 或 usage_percentage ≥ 100）
    ///
    /// 数据来源是 `balance_cache`，所以前端在调用前最好先触发一次"查询信息"
    /// 或等待后台调度器完成首次刷新。返回 (禁用数量, 跳过数量, 已超额未禁用名单)。
    pub fn disable_quota_exceeded(&self) -> QuotaExceededResult {
        let snapshot = self.token_manager.snapshot();
        let current_id = snapshot.current_id;

        let cache_snapshot: HashMap<u64, CachedBalance> = {
            let cache = self.balance_cache.lock();
            cache.clone()
        };
        let now_ts = Utc::now().timestamp() as f64;

        let mut disabled_ids: Vec<u64> = Vec::new();
        let mut skipped_ids: Vec<u64> = Vec::new();
        let mut switched_current = false;

        for entry in snapshot.entries.iter() {
            if entry.disabled {
                continue;
            }
            let cached = match cache_snapshot.get(&entry.id) {
                Some(c) if (now_ts - c.cached_at) < BALANCE_CACHE_TTL_SECS as f64 => c,
                _ => continue,
            };
            let exceeded = cached.data.remaining <= 0.0 || cached.data.usage_percentage >= 100.0;
            if !exceeded {
                continue;
            }
            match self.token_manager.disable_quota_exceeded(entry.id) {
                Ok(()) => {
                    disabled_ids.push(entry.id);
                    if entry.id == current_id {
                        switched_current = true;
                    }
                }
                Err(e) => {
                    tracing::warn!("一键超额：禁用凭据 #{} 失败: {}", entry.id, e);
                    skipped_ids.push(entry.id);
                }
            }
        }

        if switched_current {
            let _ = self.token_manager.switch_to_next();
        }

        QuotaExceededResult {
            disabled_ids,
            skipped_ids,
        }
    }

    /// 设置凭据禁用状态
    pub fn set_disabled(&self, id: u64, disabled: bool) -> Result<(), AdminServiceError> {
        // 先获取当前凭据 ID，用于判断是否需要切换
        let snapshot = self.token_manager.snapshot();
        let current_id = snapshot.current_id;

        self.token_manager
            .set_disabled(id, disabled)
            .map_err(|e| self.classify_error(e, id))?;

        // 只有禁用的是当前凭据时才尝试切换到下一个
        if disabled && id == current_id {
            let _ = self.token_manager.switch_to_next();
        }
        Ok(())
    }

    /// 设置凭据优先级
    pub fn set_priority(&self, id: u64, priority: u32) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_priority(id, priority)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 重置失败计数并重新启用
    pub fn reset_and_enable(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .reset_and_enable(id)
            .map_err(|e| self.classify_error(e, id))
    }

    pub fn clear_throttle(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .clear_throttle(id)
            .map_err(|e| self.classify_error(e, id))
    }

    pub fn reset_success_count(&self, id: Option<u64>) -> Result<u32, AdminServiceError> {
        self.token_manager
            .reset_success_count(id)
            .map_err(|e| self.classify_error(e, id.unwrap_or(0)))
    }

    /// 获取凭据余额（带缓存）
    pub async fn get_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        // 先查缓存
        {
            let cache = self.balance_cache.lock();
            if let Some(cached) = cache.get(&id) {
                let now = Utc::now().timestamp() as f64;
                if (now - cached.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    tracing::debug!("凭据 #{} 余额命中缓存", id);
                    return Ok(cached.data.clone());
                }
            }
        }

        // 缓存未命中或已过期，从上游获取
        let balance = self.fetch_balance(id).await?;

        // 更新缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.insert(
                id,
                CachedBalance {
                    cached_at: Utc::now().timestamp() as f64,
                    data: balance.clone(),
                },
            );
        }
        self.save_balance_cache();

        Ok(balance)
    }

    /// 从上游获取余额（无缓存）
    async fn fetch_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        let usage = self
            .token_manager
            .get_usage_limits_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        let current_usage = usage.current_usage();
        let usage_limit = usage.usage_limit();
        // 允许 remaining 显示为负值：开启超额后实际使用可能超过限额，
        // 直接保留差值便于在 UI 中体现"已欠多少"。
        let remaining = usage_limit - current_usage;
        // usage_percentage 同理保留真实值，超额时 > 100%。
        let usage_percentage = if usage_limit > 0.0 {
            current_usage / usage_limit * 100.0
        } else {
            0.0
        };

        Ok(BalanceResponse {
            id,
            subscription_title: usage.subscription_title().map(|s| s.to_string()),
            current_usage,
            usage_limit,
            remaining,
            usage_percentage,
            next_reset_at: usage.next_date_reset,
            overage_enabled: usage.overage_enabled(),
            overage_capable: usage.overage_capable(),
            overage_capability_raw: usage
                .subscription_info
                .as_ref()
                .and_then(|s| s.overage_capability.clone()),
        })
    }

    /// 获取指定凭据当前可用的模型列表（按需实时查询上游，不缓存）
    pub async fn get_available_models(
        &self,
        id: u64,
    ) -> Result<AvailableModelsResponse, AdminServiceError> {
        let resp = self
            .token_manager
            .get_available_models_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        let models = resp
            .models
            .into_iter()
            .map(|m| AvailableModelItem {
                model_id: m.model_id,
                model_name: m.model_name,
                description: m.description,
                max_input_tokens: m.token_limits.and_then(|t| t.max_input_tokens),
            })
            .collect();

        Ok(AvailableModelsResponse { id, models })
    }

    /// 批量刷新所有非禁用凭据的余额（用于后台调度）
    ///
    /// 串行执行以避免对上游产生瞬时高并发，每次成功的查询都会更新内存缓存
    /// 与磁盘缓存。失败的条目不会清空旧缓存，调用方可在下次轮询时重试。
    pub async fn refresh_all_balances(&self) -> (usize, usize) {
        let snapshot = self.token_manager.snapshot();
        let mut success = 0_usize;
        let mut failure = 0_usize;

        for entry in snapshot.entries.into_iter() {
            if entry.disabled {
                continue;
            }
            match self.fetch_balance(entry.id).await {
                Ok(balance) => {
                    {
                        let mut cache = self.balance_cache.lock();
                        cache.insert(
                            entry.id,
                            CachedBalance {
                                cached_at: Utc::now().timestamp() as f64,
                                data: balance,
                            },
                        );
                    }
                    success += 1;
                }
                Err(e) => {
                    tracing::warn!("后台刷新凭据 #{} 余额失败: {}", entry.id, e);
                    failure += 1;
                }
            }
            // 节流，避免上游限流
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }

        if success > 0 {
            self.save_balance_cache();
        }
        (success, failure)
    }

    /// 启动余额后台刷新调度器
    ///
    /// - 启动后立刻执行一次刷新
    /// - 之后按 `interval` 周期循环刷新
    /// - 调用方持有 `Arc<Self>` 即可，任务在后台 tokio runtime 上运行
    pub fn start_balance_refresher(self: &Arc<Self>, interval: std::time::Duration) {
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            // 启动后稍等片刻，让上游/Token Manager 准备就绪
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            loop {
                let started = std::time::Instant::now();
                let (ok, err) = svc.refresh_all_balances().await;
                tracing::info!(
                    "余额后台刷新完成：成功 {}，失败 {}，耗时 {:.1}s",
                    ok,
                    err,
                    started.elapsed().as_secs_f32()
                );
                tokio::time::sleep(interval).await;
            }
        });
    }

    /// 启动代理池后台健康检查调度器
    ///
    /// - 启动后稍等片刻再执行首次探测
    /// - 之后按 `interval` 周期循环，对所有已启用代理并发探测
    /// - 连续探测失败达阈值的代理由 `check_all` 内部自动禁用
    pub fn start_proxy_health_checker(self: &Arc<Self>, interval: std::time::Duration) {
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            // 启动后稍等片刻，让网络/代理就绪
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            loop {
                let started = std::time::Instant::now();
                let summary = svc.proxy_pool.check_all().await;
                tracing::info!(
                    "代理池健康检查完成：健康 {}，异常 {}，本轮自动禁用 {}，耗时 {:.1}s",
                    summary.healthy,
                    summary.unhealthy,
                    summary.auto_disabled,
                    started.elapsed().as_secs_f32()
                );
                tokio::time::sleep(interval).await;
            }
        });
    }

    /// 启动无人值守自动更新调度器。
    ///
    /// 任务始终运行，每分钟唤醒一次：
    /// - `update_auto_apply` 关闭时只是记录"未到点"，不做任何远端调用。
    /// - 开启时，比较当前本地时间与 `update_auto_apply_time`，命中目标分钟
    ///   就触发一次 `apply_image_update`。同一目标版本只会被自动应用一次。
    pub fn start_auto_update_scheduler(self: &Arc<Self>) {
        let svc = Arc::clone(self);
        tokio::spawn(async move {
            // 给 Docker socket / compose 元数据探测留点准备时间
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;

            // 同一分钟避免重复触发；记录最近一次应用过的"日期 + 版本"
            let mut last_run_marker: Option<String> = None;
            let mut last_applied_version: Option<String> = None;

            loop {
                let runtime = svc.update_config.lock().clone();
                if runtime.auto_apply {
                    let target = parse_auto_apply_time(&runtime.auto_apply_time).ok();
                    if let Some((target_hour, target_minute)) = target {
                        let now = chrono::Local::now();
                        let date_minute_marker = format!(
                            "{}-{:02}:{:02}",
                            now.format("%Y-%m-%d"),
                            now.hour(),
                            now.minute()
                        );

                        let hit = now.hour() == target_hour && now.minute() == target_minute;
                        let already_ran_this_minute = last_run_marker.as_deref()
                            == Some(date_minute_marker.as_str());

                        if hit && !already_ran_this_minute {
                            last_run_marker = Some(date_minute_marker);
                            let info = svc.check_update(true).await;
                            if info.has_update
                                && !info.latest_version.is_empty()
                                && last_applied_version.as_deref()
                                    != Some(info.latest_version.as_str())
                            {
                                tracing::info!(
                                    "自动更新：到达计划时间 {}，发现新版本 {}（当前 {}），开始应用",
                                    runtime.auto_apply_time,
                                    info.latest_version,
                                    info.current_version
                                );
                            match svc.apply_image_update().await {
                                    Ok(res) => {
                                        tracing::info!("自动更新完成：{}", res.message);
                                        last_applied_version = Some(info.latest_version);
                                    }
                                    Err(e) => {
                                        tracing::warn!("自动更新失败：{}", e);
                                    }
                                }
                            } else {
                                tracing::info!(
                                    "自动更新：到达计划时间 {}，但当前已是最新版本（{}）",
                                    runtime.auto_apply_time,
                                    info.current_version
                                );
                            }
                        }
                    } else {
                        tracing::warn!(
                            "自动更新时间配置无效：{}，跳过本轮检查",
                            runtime.auto_apply_time
                        );
                    }
                }

                // 30 秒粒度足以可靠命中目标分钟，又不会在系统时间漂移下错过
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        });
    }

    /// 添加新凭据
    pub async fn add_credential(
        &self,
        req: AddCredentialRequest,
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        // 校验端点名：未指定则默认合法，指定则必须已注册
        if let Some(ref name) = req.endpoint {
            if !self.known_endpoints.contains(name) {
                let mut known: Vec<&str> =
                    self.known_endpoints.iter().map(|s| s.as_str()).collect();
                known.sort();
                return Err(AdminServiceError::InvalidCredential(format!(
                    "未知端点 \"{}\"，已注册端点: {:?}",
                    name, known
                )));
            }
        }

        // 构建凭据对象
        let email = req.email.clone();
        let new_cred = KiroCredentials {
            id: None,
            access_token: req.access_token,
            refresh_token: req.refresh_token,
            profile_arn: req.profile_arn,
            expires_at: req.expires_at,
            auth_method: Some(req.auth_method),
            provider: req.provider,
            client_id: req.client_id,
            client_secret: req.client_secret,
            start_url: req.start_url,
            priority: req.priority,
            region: req.region,
            auth_region: req.auth_region,
            api_region: req.api_region,
            machine_id: req.machine_id,
            email: req.email,
            subscription_title: None, // 将在首次获取使用额度时自动更新
            proxy_url: req.proxy_url,
            proxy_username: req.proxy_username,
            proxy_password: req.proxy_password,
            disabled: false, // 新添加的凭据默认启用
            kiro_api_key: req.kiro_api_key,
            endpoint: req.endpoint,
            groups: req.groups,
            source_channel: req.source_channel,
        };

        // 调用 token_manager 添加凭据
        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| self.classify_add_error(e))?;

        // 主动获取余额（含订阅等级 / 邮箱）并写入缓存，添加后立即可见，
        // 同时避免首次请求时 Free 账号绕过 Opus 模型过滤
        if let Err(e) = self.get_balance(credential_id).await {
            tracing::warn!("添加凭据后刷新余额失败（不影响凭据添加）: {}", e);
        }

        Ok(AddCredentialResponse {
            success: true,
            message: format!("凭据添加成功，ID: {}", credential_id),
            credential_id,
            email,
        })
    }

    /// 更新凭据的可编辑字段（email、proxy 等）
    pub fn update_credential(
        &self,
        id: u64,
        req: UpdateCredentialRequest,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .update_credential(
                id,
                req.email.map(|v| if v.is_empty() { None } else { Some(v) }),
                req.proxy_url
                    .map(|v| if v.is_empty() { None } else { Some(v) }),
                req.proxy_username
                    .map(|v| if v.is_empty() { None } else { Some(v) }),
                req.proxy_password
                    .map(|v| if v.is_empty() { None } else { Some(v) }),
                req.groups,
                req.source_channel
                    .map(|v| if v.is_empty() { None } else { Some(v) }),
            )
            .map_err(|e| self.classify_error(e, id))
    }

    /// 删除凭据
    pub fn delete_credential(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .delete_credential(id)
            .map_err(|e| self.classify_delete_error(e, id))?;

        // 清理已删除凭据的余额缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.remove(&id);
        }
        self.save_balance_cache();

        Ok(())
    }

    /// 从磁盘加载最新配置并应用更新，再写回磁盘。
    ///
    /// 每次读最新文件再写，避免多次调用之间字段互相覆盖。
    fn update_config_file(&self, updater: impl FnOnce(&mut Config)) {
        let base = self.token_manager.config();
        let Some(path) = base.config_path() else {
            return;
        };
        match Config::load(path) {
            Ok(mut fresh) => {
                updater(&mut fresh);
                if let Err(e) = fresh.save() {
                    tracing::warn!("保存配置文件失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("读取配置文件失败（跳过持久化）: {}", e),
        }
    }

    /// 获取全局代理 URL
    pub fn get_global_proxy(&self) -> Option<String> {
        self.token_manager.proxy().map(|p| p.url.clone())
    }

    /// 设置全局代理 URL（None 表示清除）并持久化到配置文件
    pub fn set_global_proxy(&self, url: Option<String>) -> Result<(), AdminServiceError> {
        if let Some(ref u) = url {
            let valid_prefix = u.starts_with("http://")
                || u.starts_with("https://")
                || u.starts_with("socks5://")
                || u.starts_with("socks4://");
            if !valid_prefix {
                return Err(AdminServiceError::InvalidCredential(
                    "代理 URL 格式无效，需以 http://、https://、socks5:// 或 socks4:// 开头"
                        .to_string(),
                ));
            }
        }

        let proxy = url.as_deref().map(ProxyConfig::new);
        self.token_manager.set_global_proxy(proxy);

        // 从磁盘加载最新 config 再写，避免覆盖其他字段的并发修改
        let url_for_save = url;
        self.update_config_file(move |c| c.proxy_url = url_for_save);
        Ok(())
    }

    /// 持久化新的登录API密钥到配置文件（内存中的 key 由 handler 层负责更新）
    pub fn persist_admin_key(&self, new_key: &str) {
        let key = new_key.to_string();
        self.update_config_file(move |c| c.admin_api_key = Some(key));
    }

    /// 持久化新的管理员API密钥到配置文件
    pub fn persist_api_key(&self, new_key: &str) {
        let key = new_key.to_string();
        self.update_config_file(move |c| c.api_key = Some(key));
    }

    /// 获取在线更新配置（GitHub Token 只返回是否已配置）
    pub fn get_update_config(&self) -> UpdateConfigResponse {
        self.update_config.lock().response()
    }

    /// 更新在线更新配置。
    pub fn set_update_config(
        &self,
        req: SetUpdateConfigRequest,
    ) -> Result<UpdateConfigResponse, AdminServiceError> {
        // 在写入运行时之前先校验时间格式，并规范化成两位补零的 HH:MM
        let normalized_time = match req.auto_apply_time.as_deref() {
            Some(value) => Some(normalize_auto_apply_time(value)?),
            None => None,
        };

        // GitHub Token：空字符串表示清除，None 表示保持原值
        let token_update: Option<Option<String>> = req.github_token.as_ref().map(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });

        {
            let mut runtime = self.update_config.lock();
            if let Some(auto_apply) = req.auto_apply {
                runtime.auto_apply = auto_apply;
            }
            if let Some(time) = &normalized_time {
                runtime.auto_apply_time = time.clone();
            }
            if let Some(token) = &token_update {
                runtime.github_token = token.clone();
            }
        }

        self.update_config_file(move |c| {
            if let Some(auto_apply) = req.auto_apply {
                c.update_auto_apply = auto_apply;
            }
            if let Some(time) = normalized_time {
                c.update_auto_apply_time = time;
            }
            if let Some(token) = token_update {
                c.github_token = token;
            }
        });

        Ok(self.get_update_config())
    }

    /// 下载新版二进制并通过校验和验证（对应前端「拉取镜像」按钮）。
    /// 不替换当前可执行文件，便于用户在正式应用前先确认下载成功。
    /// 下载产物保存到 `<exe>.staged-<version>`，下次 apply 命中同版本时复用。
    pub async fn pull_update_image(&self) -> Result<ImageUpdateResponse, AdminServiceError> {
        let (proxy, token) = {
            let runtime = self.update_config.lock();
            (
                self.token_manager.proxy().map(|p| p.url.clone()),
                runtime.github_token.clone(),
            )
        };
        let exe = super::binary_update::current_executable()?;

        let version = self.resolve_target_version(false).await?;
        let staged = staged_binary_path(&exe, &version);

        // 已经下载过同版本时直接复用，避免重复网络请求
        let reused = staged.exists();
        if !reused {
            super::binary_update::download_release_binary(
                &version,
                proxy.as_deref(),
                token.as_deref(),
                &staged,
            )
            .await?;
        }
        // 清理其它版本的旧 staged 文件，避免占用磁盘
        cleanup_other_staged(&exe, &version);

        Ok(ImageUpdateResponse {
            success: true,
            message: if reused {
                format!("v{} 已下载并校验，可直接执行「更新并重启」", version)
            } else {
                format!(
                    "已下载并校验 v{} 二进制，可直接执行「更新并重启」",
                    version
                )
            },
            output: Some(format!(
                "{}: v{}\nstaged: {}",
                if reused { "reused" } else { "downloaded" },
                version,
                staged.display()
            )),
            applied: false,
            need_restart: false,
        })
    }

    /// 下载新版二进制并替换当前可执行文件，随后让进程退出由
    /// `restart: unless-stopped` 接管重启（对应前端「更新并重启」按钮）。
    /// 若 pull 已经把目标版本下载到 `<exe>.staged-<version>`，跳过重复下载。
    pub async fn apply_image_update(&self) -> Result<ImageUpdateResponse, AdminServiceError> {
        let (proxy, token) = {
            let runtime = self.update_config.lock();
            (
                self.token_manager.proxy().map(|p| p.url.clone()),
                runtime.github_token.clone(),
            )
        };
        let exe = super::binary_update::current_executable()?;

        let version = self.resolve_target_version(true).await?;
        let staged = staged_binary_path(&exe, &version);

        let reused = staged.exists();
        if !reused {
            super::binary_update::download_release_binary(
                &version,
                proxy.as_deref(),
                token.as_deref(),
                &staged,
            )
            .await?;
        }
        cleanup_other_staged(&exe, &version);

        // 记录当前版本作为「上一版本」，供前端展示「回退」按钮
        let previous_version = env!("CARGO_PKG_VERSION").to_string();
        super::binary_update::install_binary(&exe, &staged)?;

        let prev_label = format!("v{}", previous_version);
        let applied_at = chrono::Utc::now().to_rfc3339();
        {
            let mut runtime = self.update_config.lock();
            runtime.previous_version = Some(prev_label.clone());
            runtime.last_applied_at = Some(applied_at.clone());
        }
        let prev_to_persist = prev_label.clone();
        let applied_at_to_persist = applied_at.clone();
        self.update_config_file(move |c| {
            c.update_previous_version = Some(prev_to_persist);
            c.update_last_applied_at = Some(applied_at_to_persist);
        });

        super::binary_update::schedule_self_exit(std::time::Duration::from_secs(2));

        Ok(ImageUpdateResponse {
            success: true,
            message: format!(
                "已替换为 v{}，进程将在 2 秒后退出，由容器重启策略接管",
                version
            ),
            output: Some(format!(
                "previous: v{}\n{}: v{}",
                previous_version,
                if reused { "reused-staged" } else { "installed" },
                version
            )),
            applied: true,
            need_restart: true,
        })
    }

    /// 把可执行文件回退到 `<exe>.backup`，再重启进程。
    pub async fn rollback_image_update(&self) -> Result<ImageUpdateResponse, AdminServiceError> {
        let previous_label = self
            .update_config
            .lock()
            .previous_version
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                AdminServiceError::InvalidCredential(
                    "尚未记录可回退的版本，请先执行一次在线更新".to_string(),
                )
            })?
            .to_string();

        let exe = super::binary_update::current_executable()?;
        super::binary_update::restore_backup(&exe)?;
        // 回退后清掉所有 staged：用户已表态"上一次更新是错的"，残留只会误导
        cleanup_other_staged(&exe, "");

        // 回退视为撤销最近一次更新：清空 previous_version 和 last_applied_at
        {
            let mut runtime = self.update_config.lock();
            runtime.previous_version = None;
            runtime.last_applied_at = None;
        }
        self.update_config_file(|c| {
            c.update_previous_version = None;
            c.update_last_applied_at = None;
        });

        super::binary_update::schedule_self_exit(std::time::Duration::from_secs(2));

        Ok(ImageUpdateResponse {
            success: true,
            message: format!(
                "已回退到 {}，进程将在 2 秒后退出，由容器重启策略接管",
                previous_label
            ),
            output: Some(format!("rolled back to: {}", previous_label)),
            applied: true,
            need_restart: true,
        })
    }

    /// 返回 GitHub Releases 上的最新可用版本号（无 `v` 前缀）。
    /// 失败时返回 `InternalError`，调用方应直接返回给前端。
    /// 返回 GitHub Releases 上的最新可用版本号（无 `v` 前缀）。
    /// 失败时返回 `InternalError`，调用方应直接返回给前端。
    ///
    /// `require_update` 为 true 时，若当前版本已经是最新（无更新可用），
    /// 直接返回错误而不是返回相同版本号——避免 apply 流程下载并替换同一版本。
    async fn resolve_target_version(
        &self,
        require_update: bool,
    ) -> Result<String, AdminServiceError> {
        let info = self.check_update(true).await;
        if let Some(warn) = info.warning {
            return Err(AdminServiceError::InternalError(warn));
        }
        if info.latest_version.is_empty() {
            return Err(AdminServiceError::InternalError(
                "无法解析最新版本号（GitHub Releases 返回空）".to_string(),
            ));
        }
        if require_update && !info.has_update {
            return Err(AdminServiceError::InvalidCredential(format!(
                "当前已是最新版本 v{}，无需更新",
                info.current_version
            )));
        }
        Ok(info.latest_version)
    }

    /// 检查 GitHub Releases 上是否存在新版本。
    ///
    /// `force=false` 时优先返回 30 分钟内的缓存结果；`force=true` 时强制查询
    /// 远端。查询失败但有旧缓存时，返回旧缓存并附带 warning。
    pub async fn check_update(&self, force: bool) -> UpdateCheckInfo {
        if !force {
            if let Some(cached) = self.update_check_cache.lock().clone() {
                let age = Utc::now()
                    .signed_duration_since(cached.cached_at)
                    .num_seconds();
                if age < UPDATE_CHECK_TTL_SECS {
                    let mut info = cached.info.clone();
                    info.cached = true;
                    return info;
                }
            }
        }

        match self.fetch_latest_release().await {
            Ok(info) => {
                self.update_check_cache.lock().replace(CachedUpdateCheck {
                    cached_at: Utc::now(),
                    info: info.clone(),
                });
                info
            }
            Err(err) => {
                let warning = format!("检查更新失败：{}", err);
                if let Some(cached) = self.update_check_cache.lock().clone() {
                    let mut info = cached.info.clone();
                    info.cached = true;
                    info.warning = Some(warning);
                    return info;
                }
                UpdateCheckInfo {
                    current_version: env!("CARGO_PKG_VERSION").to_string(),
                    latest_version: String::new(),
                    has_update: false,
                    build_type: BUILD_TYPE.to_string(),
                    release_name: None,
                    release_notes: None,
                    release_url: None,
                    published_at: None,
                    checked_at: Utc::now().to_rfc3339(),
                    cached: false,
                    warning: Some(warning),
                }
            }
        }
    }

    async fn fetch_latest_release(&self) -> Result<UpdateCheckInfo, AdminServiceError> {
        let url = format!(
            "https://api.github.com/repos/{}/releases/latest",
            GITHUB_RELEASES_REPO
        );
        let token = self.update_config.lock().github_token.clone();
        let mut req = reqwest::Client::new()
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "kiro-rs-update-checker")
            .timeout(std::time::Duration::from_secs(15));
        if let Some(t) = token.as_deref() {
            let trimmed = t.trim();
            if !trimmed.is_empty() {
                req = req.header("Authorization", format!("Bearer {}", trimmed));
            }
        }
        let resp = req.send().await.map_err(|e| {
            AdminServiceError::InternalError(format!("请求 GitHub API 失败: {}", e))
        })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AdminServiceError::InternalError(format!(
                "GitHub API 返回 {}: {}",
                status,
                body.chars().take(200).collect::<String>()
            )));
        }

        let release: GitHubRelease = resp.json().await.map_err(|e| {
            AdminServiceError::InternalError(format!("解析 GitHub release 失败: {}", e))
        })?;

        let current = env!("CARGO_PKG_VERSION").to_string();
        let latest_version = release.tag_name.trim().trim_start_matches('v').to_string();
        let has_update =
            !latest_version.is_empty() && compare_semver(&current, &latest_version).is_lt();

        Ok(UpdateCheckInfo {
            current_version: current,
            latest_version,
            has_update,
            build_type: BUILD_TYPE.to_string(),
            release_name: Some(release.name).filter(|v| !v.is_empty()),
            release_notes: Some(release.body).filter(|v| !v.is_empty()),
            release_url: Some(release.html_url).filter(|v| !v.is_empty()),
            published_at: Some(release.published_at).filter(|v| !v.is_empty()),
            checked_at: Utc::now().to_rfc3339(),
            cached: false,
            warning: None,
        })
    }

    /// 查询 GitHub API 当前限流配额。
    ///
    /// `req.github_token` 不为空时使用该 token 验证（用于"保存前先试一下"），
    /// 否则使用配置中已保存的 `config.github_token`，再缺则匿名查询。
    /// `/rate_limit` 端点本身不消耗任何配额。
    pub async fn check_rate_limit(
        &self,
        req: CheckRateLimitRequest,
    ) -> GitHubRateLimitInfo {
        // 优先用入参 token；空字符串视作"尝试匿名"；缺省回退到已保存 token
        let token = req
            .github_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| {
                self.update_config
                    .lock()
                    .github_token
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
            });
        let authenticated = token.is_some();

        let proxy = self.token_manager.proxy().map(|p| p.url.clone());
        let client = match super::binary_update::build_http_client(proxy.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                return GitHubRateLimitInfo {
                    valid: false,
                    authenticated,
                    limit: 0,
                    remaining: 0,
                    used: 0,
                    reset: 0,
                    login: None,
                    warning: Some(format!("构造 HTTP 客户端失败: {}", e)),
                };
            }
        };

        let mut req_builder = client
            .get("https://api.github.com/rate_limit")
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "kiro-rs-update-checker")
            .timeout(std::time::Duration::from_secs(10));
        if let Some(t) = token.as_deref() {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", t));
        }

        let resp = match req_builder.send().await {
            Ok(r) => r,
            Err(e) => {
                return GitHubRateLimitInfo {
                    valid: false,
                    authenticated,
                    limit: 0,
                    remaining: 0,
                    used: 0,
                    reset: 0,
                    login: None,
                    warning: Some(format!("请求 GitHub API 失败: {}", e)),
                };
            }
        };

        let status = resp.status();

        if status == reqwest::StatusCode::UNAUTHORIZED {
            return GitHubRateLimitInfo {
                valid: false,
                authenticated,
                limit: 0,
                remaining: 0,
                used: 0,
                reset: 0,
                login: None,
                warning: Some("GitHub Token 无效或已过期".to_string()),
            };
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return GitHubRateLimitInfo {
                valid: false,
                authenticated,
                limit: 0,
                remaining: 0,
                used: 0,
                reset: 0,
                login: None,
                warning: Some(format!(
                    "GitHub API 返回 {}: {}",
                    status,
                    body.chars().take(200).collect::<String>()
                )),
            };
        }

        let payload: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return GitHubRateLimitInfo {
                    valid: false,
                    authenticated,
                    limit: 0,
                    remaining: 0,
                    used: 0,
                    reset: 0,
                    login: None,
                    warning: Some(format!("解析 GitHub 响应失败: {}", e)),
                };
            }
        };

        // /rate_limit 返回结构：{ resources: { core: { limit, remaining, used, reset } }, rate: {...} }
        // 其中 `core` 是 REST API 整体配额，最贴合在线更新的实际消耗
        let core = payload
            .get("resources")
            .and_then(|r| r.get("core"))
            .or_else(|| payload.get("rate"));
        let limit = core.and_then(|c| c.get("limit")).and_then(|v| v.as_u64()).unwrap_or(0);
        let remaining = core
            .and_then(|c| c.get("remaining"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let used = core.and_then(|c| c.get("used")).and_then(|v| v.as_u64()).unwrap_or(0);
        let reset = core.and_then(|c| c.get("reset")).and_then(|v| v.as_u64()).unwrap_or(0);

        // 同时尝试拿 token 对应的用户名；失败不影响主结果
        let login = if authenticated {
            self.fetch_github_login(&client, token.as_deref()).await
        } else {
            None
        };

        GitHubRateLimitInfo {
            valid: true,
            authenticated,
            limit,
            remaining,
            used,
            reset,
            login,
            warning: None,
        }
    }

    async fn fetch_github_login(
        &self,
        client: &reqwest::Client,
        token: Option<&str>,
    ) -> Option<String> {
        let mut req = client
            .get("https://api.github.com/user")
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "kiro-rs-update-checker")
            .timeout(std::time::Duration::from_secs(10));
        if let Some(t) = token {
            req = req.header("Authorization", format!("Bearer {}", t));
        }
        let resp = req.send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let payload: serde_json::Value = resp.json().await.ok()?;
        payload
            .get("login")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    /// 获取负载均衡模式
    pub fn get_load_balancing_mode(&self) -> LoadBalancingModeResponse {
        LoadBalancingModeResponse {
            mode: self.token_manager.get_load_balancing_mode(),
        }
    }

    /// 设置负载均衡模式
    pub fn set_load_balancing_mode(
        &self,
        req: SetLoadBalancingModeRequest,
    ) -> Result<LoadBalancingModeResponse, AdminServiceError> {
        // 验证模式值
        if req.mode != "priority" && req.mode != "balanced" {
            return Err(AdminServiceError::InvalidCredential(
                "mode 必须是 'priority' 或 'balanced'".to_string(),
            ));
        }

        self.token_manager
            .set_load_balancing_mode(req.mode.clone())
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        Ok(LoadBalancingModeResponse { mode: req.mode })
    }

    /// 获取账号级风控故障转移配置
    pub fn get_account_throttle_config(&self) -> AccountThrottleConfigResponse {
        AccountThrottleConfigResponse {
            failover: self.token_manager.get_account_throttle_failover(),
            cooldown_secs: self.token_manager.get_account_throttle_cooldown_secs(),
        }
    }

    /// 更新账号级风控故障转移配置
    pub fn set_account_throttle_config(
        &self,
        req: SetAccountThrottleConfigRequest,
    ) -> Result<AccountThrottleConfigResponse, AdminServiceError> {
        if req.failover.is_none() && req.cooldown_secs.is_none() {
            return Err(AdminServiceError::InvalidCredential(
                "至少提供 failover 或 cooldownSecs 一个字段".to_string(),
            ));
        }

        self.token_manager
            .set_account_throttle_config(req.failover, req.cooldown_secs)
            .map_err(|e| AdminServiceError::InvalidCredential(e.to_string()))?;

        Ok(self.get_account_throttle_config())
    }

    /// 读取日志治理配置（trace 开关 / trace 保留天数 / usage 保留天数）
    pub fn get_log_governance_config(&self) -> LogGovernanceConfigResponse {
        let cfg = self.token_manager.config();
        LogGovernanceConfigResponse {
            trace_enabled: self
                .trace_store
                .as_ref()
                .map(|s| s.is_enabled())
                .unwrap_or(cfg.trace_enabled),
            trace_retention_days: self
                .trace_store
                .as_ref()
                .map(|s| s.retention_days() as u32)
                .unwrap_or(cfg.trace_retention_days),
            usage_log_retention_days: self
                .usage_recorder
                .as_ref()
                .map(|r| r.retention_days() as u32)
                .unwrap_or(cfg.usage_log_retention_days),
        }
    }

    /// 更新日志治理配置：改运行时原子值 + 持久化到 config.json。
    /// 任一字段缺省表示不修改。
    pub fn set_log_governance_config(
        &self,
        req: SetLogGovernanceConfigRequest,
    ) -> Result<LogGovernanceConfigResponse, AdminServiceError> {
        if req.trace_enabled.is_none()
            && req.trace_retention_days.is_none()
            && req.usage_log_retention_days.is_none()
        {
            return Err(AdminServiceError::InvalidCredential(
                "至少提供 traceEnabled / traceRetentionDays / usageLogRetentionDays 一个字段"
                    .to_string(),
            ));
        }
        // 校验范围：保留天数 1..=365
        for (name, v) in [
            ("traceRetentionDays", req.trace_retention_days),
            ("usageLogRetentionDays", req.usage_log_retention_days),
        ] {
            if let Some(d) = v {
                if !(1..=365).contains(&d) {
                    return Err(AdminServiceError::InvalidCredential(format!(
                        "{} 必须在 1..=365 内: {}",
                        name, d
                    )));
                }
            }
        }

        // 先改运行时原子值
        if let Some(enabled) = req.trace_enabled {
            if let Some(s) = &self.trace_store {
                s.set_enabled(enabled);
            }
        }
        if let Some(days) = req.trace_retention_days {
            if let Some(s) = &self.trace_store {
                s.set_retention_days(days);
            }
        }
        if let Some(days) = req.usage_log_retention_days {
            if let Some(r) = &self.usage_recorder {
                r.set_retention_days(days as i64);
            }
        }

        // 持久化到 config.json
        if let Err(e) = self.persist_log_governance_config(&req) {
            tracing::warn!("持久化日志治理配置失败（运行时已生效）: {}", e);
        }

        Ok(self.get_log_governance_config())
    }

    fn persist_log_governance_config(
        &self,
        req: &SetLogGovernanceConfigRequest,
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        let config_path = match self.token_manager.config().config_path() {
            Some(p) => p.to_path_buf(),
            None => {
                tracing::warn!("配置文件路径未知，日志治理配置仅在当前进程生效");
                return Ok(());
            }
        };
        let mut config = crate::model::config::Config::load(&config_path)
            .with_context(|| format!("重新加载配置失败: {}", config_path.display()))?;
        if let Some(v) = req.trace_enabled {
            config.trace_enabled = v;
        }
        if let Some(v) = req.trace_retention_days {
            config.trace_retention_days = v;
        }
        if let Some(v) = req.usage_log_retention_days {
            config.usage_log_retention_days = v;
        }
        config
            .save()
            .with_context(|| format!("持久化日志治理配置失败: {}", config_path.display()))?;
        Ok(())
    }

    /// 更新指定凭据的 refreshToken（仅限已禁用凭据）
    pub fn update_refresh_token(
        &self,
        id: u64,
        req: UpdateRefreshTokenRequest,
    ) -> Result<(), AdminServiceError> {
        self.token_manager
            .update_refresh_token(id, req.refresh_token, req.access_token, req.expires_at)
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("不存在") {
                    AdminServiceError::NotFound { id }
                } else if msg.contains("只能为已禁用")
                    || msg.contains("refreshToken 重复")
                    || msg.contains("已被截断")
                    || msg.contains("refreshToken 为空")
                    || msg.contains("缺少 refreshToken")
                {
                    AdminServiceError::InvalidCredential(msg)
                } else {
                    AdminServiceError::InternalError(msg)
                }
            })
    }

    /// 一键开启所有"可开启超额且当前未开启"凭据的超额
    /// 数据来源是 balance_cache（5 分钟有效）；若缓存缺失或 capable 状态未知则乐观尝试，
    /// 由上游 setUserPreference 接口本身决定是否成功（不支持的订阅会返回 4xx 失败）。
    pub async fn enable_overage_for_all_capable(&self) -> EnableOverageAllResult {
        let snapshot = self.token_manager.snapshot();
        let cache_snapshot: HashMap<u64, CachedBalance> = {
            let cache = self.balance_cache.lock();
            cache.clone()
        };
        let now_ts = Utc::now().timestamp() as f64;

        // 选出需要操作的 ID 列表
        let mut targets: Vec<u64> = Vec::new();
        let mut skipped: Vec<u64> = Vec::new();
        for entry in snapshot.entries.iter() {
            if entry.disabled {
                skipped.push(entry.id);
                continue;
            }
            let cached = cache_snapshot.get(&entry.id).filter(|c| {
                (now_ts - c.cached_at) < BALANCE_CACHE_TTL_SECS as f64
            });

            match cached {
                // 缓存命中：明确不可开启，跳过
                Some(c) if c.data.overage_capable == Some(false) => {
                    skipped.push(entry.id);
                    continue;
                }
                // 缓存命中：明确已开启，跳过
                Some(c) if c.data.overage_enabled == Some(true) => {
                    skipped.push(entry.id);
                    continue;
                }
                // 其它（缓存缺失 / 状态未知 / 明确可开启未开启）— 乐观尝试
                _ => targets.push(entry.id),
            }
        }

        let mut enabled_ids: Vec<u64> = Vec::new();
        let mut failed_ids: Vec<u64> = Vec::new();
        let mut failure_messages: Vec<String> = Vec::new();

        for id in targets {
            match self.token_manager.set_user_preference_for(id, "ENABLED").await {
                Ok(()) => {
                    enabled_ids.push(id);
                    // 失效本地缓存
                    let mut cache = self.balance_cache.lock();
                    cache.remove(&id);
                }
                Err(e) => {
                    tracing::warn!("一键开启超额：凭据 #{} 失败: {}", id, e);
                    failed_ids.push(id);
                    failure_messages.push(e.to_string());
                }
            }
            // 节流
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }

        if !enabled_ids.is_empty() {
            self.save_balance_cache();
        }

        EnableOverageAllResult {
            enabled_ids,
            skipped_ids: skipped,
            failed_ids,
            failure_messages,
        }
    }

    /// 强制刷新指定凭据的 Token
    pub async fn force_refresh_token(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .force_refresh_token_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))
    }

    /// 设置凭据的"超额"开关（ENABLED / DISABLED）
    /// 成功后会主动失效本地余额缓存，让下次列表刷新展示最新 overage 状态
    pub async fn set_overage(&self, id: u64, enabled: bool) -> Result<(), AdminServiceError> {
        let status = if enabled { "ENABLED" } else { "DISABLED" };
        self.token_manager
            .set_user_preference_for(id, status)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        // 让本地缓存的 overage 状态失效（下次刷新时重新拉）
        {
            let mut cache = self.balance_cache.lock();
            cache.remove(&id);
        }
        self.save_balance_cache();

        // 异步触发一次新的余额查询（不阻塞响应）
        let svc_handle = self.token_manager.clone();
        tokio::spawn(async move {
            if let Err(e) = svc_handle.get_usage_limits_for(id).await {
                tracing::warn!("超额状态变更后预热余额失败 #{}: {}", id, e);
            }
        });

        Ok(())
    }

    // ============ 余额缓存持久化 ============

    fn load_balance_cache_from(cache_path: &Option<PathBuf>) -> HashMap<u64, CachedBalance> {
        let path = match cache_path {
            Some(p) => p,
            None => return HashMap::new(),
        };

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return HashMap::new(),
        };

        // 文件中使用字符串 key 以兼容 JSON 格式
        let map: HashMap<String, CachedBalance> = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("解析余额缓存失败，将忽略: {}", e);
                return HashMap::new();
            }
        };

        let now = Utc::now().timestamp() as f64;
        map.into_iter()
            .filter_map(|(k, v)| {
                let id = k.parse::<u64>().ok()?;
                // 丢弃超过 TTL 的条目
                if (now - v.cached_at) < BALANCE_CACHE_TTL_SECS as f64 {
                    Some((id, v))
                } else {
                    None
                }
            })
            .collect()
    }

    fn save_balance_cache(&self) {
        let path = match &self.cache_path {
            Some(p) => p,
            None => return,
        };

        // 持有锁期间完成序列化和写入，防止并发损坏
        let cache = self.balance_cache.lock();
        let map: HashMap<String, &CachedBalance> =
            cache.iter().map(|(k, v)| (k.to_string(), v)).collect();

        match serde_json::to_string_pretty(&map) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    tracing::warn!("保存余额缓存失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("序列化余额缓存失败: {}", e),
        }
    }

    // ============ 代理池管理 ============

    /// 获取代理池列表（含凭据引用计数）
    pub fn get_proxy_pool(&self) -> ProxyPoolResponse {
        let proxies = self.proxy_pool.list();
        let credentials = {
            let snapshot = self.token_manager.snapshot();
            snapshot.entries
        };

        let pool: Vec<ProxyPoolEntry> = proxies
            .into_iter()
            .map(|p| {
                let count = credentials
                    .iter()
                    .filter(|c| c.proxy_url.as_deref().map(|u| u == p.url).unwrap_or(false))
                    .count() as u32;
                ProxyPoolEntry {
                    id: p.id,
                    url: p.url,
                    label: p.label,
                    enabled: p.enabled,
                    credential_count: count,
                    health: p.health,
                    latency_ms: p.latency_ms,
                    last_checked_at: p.last_checked_at,
                    consecutive_failures: p.consecutive_failures,
                    auto_disabled: p.auto_disabled,
                }
            })
            .collect();

        ProxyPoolResponse {
            total: pool.len(),
            proxies: pool,
        }
    }

    /// 添加代理到池中
    pub fn add_proxy(
        &self,
        url: String,
        label: Option<String>,
    ) -> Result<ProxyPoolEntry, AdminServiceError> {
        let entry = self
            .proxy_pool
            .add(url, label)
            .map_err(|e| AdminServiceError::InvalidCredential(e.to_string()))?;
        Ok(ProxyPoolEntry {
            id: entry.id,
            url: entry.url,
            label: entry.label,
            enabled: entry.enabled,
            credential_count: 0,
            health: entry.health,
            latency_ms: entry.latency_ms,
            last_checked_at: entry.last_checked_at,
            consecutive_failures: entry.consecutive_failures,
            auto_disabled: entry.auto_disabled,
        })
    }

    /// 批量添加代理
    pub fn batch_add_proxies(
        &self,
        req: BatchAddProxyRequest,
    ) -> (Vec<ProxyPoolEntry>, Vec<String>) {
        let (added, errors) = self.proxy_pool.batch_add(req.urls);
        let result = added
            .into_iter()
            .map(|e| ProxyPoolEntry {
                id: e.id,
                url: e.url,
                label: e.label,
                enabled: e.enabled,
                credential_count: 0,
                health: e.health,
                latency_ms: e.latency_ms,
                last_checked_at: e.last_checked_at,
                consecutive_failures: e.consecutive_failures,
                auto_disabled: e.auto_disabled,
            })
            .collect();
        (result, errors)
    }

    /// 删除代理池中的代理
    pub fn delete_proxy(&self, id: u64) -> Result<(), AdminServiceError> {
        self.proxy_pool.delete(id).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("不存在") {
                AdminServiceError::NotFound { id }
            } else {
                AdminServiceError::InternalError(msg)
            }
        })
    }

    /// 设置代理启用/禁用状态
    pub fn set_proxy_enabled(&self, id: u64, enabled: bool) -> Result<(), AdminServiceError> {
        self.proxy_pool
            .set_enabled(id, enabled)
            .map_err(|_| AdminServiceError::NotFound { id })
    }

    /// 将代理池中的代理分配给指定凭据
    pub fn assign_proxy_to_credential(
        &self,
        credential_id: u64,
        req: AssignProxyRequest,
    ) -> Result<(), AdminServiceError> {
        let proxy_url = match req.proxy_id {
            Some(proxy_id) => {
                let url = match self.proxy_pool.get_url(proxy_id) {
                    GetUrlResult::Ok(url) => url,
                    GetUrlResult::NotFound => {
                        return Err(AdminServiceError::NotFound { id: proxy_id });
                    }
                    GetUrlResult::Disabled => {
                        return Err(AdminServiceError::InvalidCredential(format!(
                            "代理 #{} 已被禁用，请先启用后再分配",
                            proxy_id
                        )));
                    }
                };
                Some(url)
            }
            None => None, // 清除代理
        };

        self.token_manager
            .update_credential(
                credential_id,
                None,            // email 不修改
                Some(proxy_url), // 设置或清除 proxy_url（Some(None) = 清除，Some(Some(url)) = 设置）
                None,            // proxy_username 不修改
                None,            // proxy_password 不修改
                None,            // groups 不修改
                None,            // source_channel 不修改
            )
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("不存在") {
                    AdminServiceError::NotFound { id: credential_id }
                } else {
                    AdminServiceError::InternalError(msg)
                }
            })
    }

    /// 即时探测单个代理的连通性（供 UI「测试」按钮调用）
    pub async fn check_proxy(&self, id: u64) -> Result<ProxyCheckResponse, AdminServiceError> {
        let entry = self
            .proxy_pool
            .check_one(id)
            .await
            .map_err(|_| AdminServiceError::NotFound { id })?;
        Ok(ProxyCheckResponse {
            id: entry.id,
            health: entry.health,
            latency_ms: entry.latency_ms,
            last_checked_at: entry.last_checked_at,
            enabled: entry.enabled,
            auto_disabled: entry.auto_disabled,
        })
    }

    /// 触发全部代理的健康检查
    pub async fn check_all_proxies(&self) -> ProxyCheckAllResponse {
        let summary = self.proxy_pool.check_all().await;
        ProxyCheckAllResponse {
            healthy: summary.healthy,
            unhealthy: summary.unhealthy,
            auto_disabled: summary.auto_disabled,
        }
    }

    /// 将可用代理（已启用且非 Unhealthy）按轮询方式批量分配给凭据
    ///
    /// - `credential_ids` 为 None 时对全部凭据分配
    /// - 无可用代理时返回错误
    pub fn assign_proxies_round_robin(
        &self,
        credential_ids: Option<Vec<u64>>,
    ) -> Result<AssignRoundRobinResponse, AdminServiceError> {
        let urls = self.proxy_pool.assignable_urls();
        if urls.is_empty() {
            return Err(AdminServiceError::InvalidCredential(
                "没有可用代理（需已启用且健康检查未失败）".to_string(),
            ));
        }

        let target_ids: Vec<u64> = match credential_ids {
            Some(ids) if !ids.is_empty() => ids,
            _ => self
                .token_manager
                .snapshot()
                .entries
                .iter()
                .map(|c| c.id)
                .collect(),
        };

        let mut assigned = 0;
        for (i, cred_id) in target_ids.iter().enumerate() {
            let url = urls[i % urls.len()].clone();
            if self
                .token_manager
                .update_credential(*cred_id, None, Some(Some(url)), None, None, None, None)
                .is_ok()
            {
                assigned += 1;
            }
        }

        Ok(AssignRoundRobinResponse {
            assigned,
            proxy_count: urls.len(),
        })
    }

    // ============ 错误分类 ============

    /// 分类简单操作错误（set_disabled, set_priority, reset_and_enable）
    fn classify_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类余额查询错误（可能涉及上游 API 调用）
    fn classify_balance_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();

        // 1. 凭据不存在
        if msg.contains("不存在") {
            return AdminServiceError::NotFound { id };
        }

        // 2. API Key 凭据不支持刷新：客户端请求错误，映射为 400
        if msg.contains("API Key 凭据不支持刷新") {
            return AdminServiceError::InvalidCredential(msg);
        }

        // 3. 上游明确指出凭据缺少或携带了错误的 Profile ARN，属于导入凭据不完整/无效。
        if msg.contains("Invalid profileArn") {
            return AdminServiceError::InvalidCredential(
                "凭据缺少或包含无效 profileArn，无法查询余额；请重新登录获取 profileArn，或导入包含 profileArn 的完整凭据"
                    .to_string(),
            );
        }

        // 3. 上游服务错误特征：HTTP 响应错误或网络错误
        let is_upstream_error = msg.contains("获取使用额度失败") ||
            // HTTP 响应错误（来自 refresh_*_token 的错误消息）
            msg.contains("凭证已过期或无效") ||
            msg.contains("权限不足") ||
            msg.contains("已被限流") ||
            msg.contains("服务器错误") ||
            msg.contains("Token 刷新失败") ||
            msg.contains("暂时不可用") ||
            // 网络错误（reqwest 错误格式）
            msg.contains("error sending request") ||
            msg.contains("error trying to connect") ||
            msg.contains("connection") ||
            msg.contains("timeout") ||
            msg.contains("timed out") ||
            msg.contains("proxy") ||
            msg.contains("SOCKS") ||
            msg.contains("dns") ||
            msg.contains("DNS");

        if is_upstream_error {
            AdminServiceError::UpstreamError(msg)
        } else {
            // 4. 默认归类为内部错误（本地验证失败、配置错误等）
            // 包括：缺少 refreshToken、refreshToken 已被截断、无法生成 machineId 等
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类添加凭据错误
    fn classify_add_error(&self, e: anyhow::Error) -> AdminServiceError {
        let msg = e.to_string();

        // 凭据验证失败（refreshToken 无效、格式错误等）
        let is_invalid_credential = msg.contains("缺少 refreshToken")
            || msg.contains("refreshToken 为空")
            || msg.contains("refreshToken 已被截断")
            || msg.contains("凭据已存在")
            || msg.contains("refreshToken 重复")
            || msg.contains("kiroApiKey 重复")
            || msg.contains("缺少 kiroApiKey")
            || msg.contains("kiroApiKey 为空")
            || msg.contains("凭证已过期或无效")
            || msg.contains("权限不足")
            || msg.contains("已被限流");

        if is_invalid_credential {
            AdminServiceError::InvalidCredential(msg)
        } else if msg.contains("error trying to connect")
            || msg.contains("connection")
            || msg.contains("timeout")
        {
            AdminServiceError::UpstreamError(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    // ── Social 登录（Portal PKCE OAuth）────────────────────────────────────────

    /// 发起 Social 登录，返回 portal URL 供用户在浏览器打开
    ///
    /// 模式选择：
    /// - `callback_base_url` 为 Some → 远程模式：redirect_uri 使用服务端公网地址，不启动本地端口
    /// - `callback_base_url` 为 None  → 本地模式：启动本地 TCP 回调服务器（浏览器与服务端须同机）
    pub async fn start_social_login(
        &self,
        req: StartSocialLoginRequest,
    ) -> Result<StartSocialLoginResponse, AdminServiceError> {
        let global_proxy = self.token_manager.proxy();
        let proxy = req
            .proxy_url
            .as_deref()
            .map(ProxyConfig::new)
            .or(global_proxy);

        let auth_endpoint = req
            .auth_endpoint
            .unwrap_or_else(|| social::KIRO_AUTH_ENDPOINT.to_string());

        let (code_verifier, code_challenge) = social::generate_pkce();
        let state = uuid::Uuid::new_v4().to_string();

        let (tx, rx) = tokio::sync::oneshot::channel::<social::OAuthCallbackData>();

        // 启动本地 TCP 回调服务器（本地模式）
        // 远程访问时用户须从浏览器地址栏复制回调 URL，通过 complete_social_login 接口手动完成
        let (port, server_handle) = social::start_callback_server(tx)
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        let redirect_uri = format!("http://127.0.0.1:{}", port);
        let portal_url = social::build_portal_url(&state, &code_challenge, &redirect_uri);

        let expires_at = Utc::now() + Duration::minutes(10);
        let session_id = uuid::Uuid::new_v4().to_string();

        let cred_template = KiroCredentials {
            auth_method: Some("social".to_string()),
            priority: req.priority,
            email: req.email,
            proxy_url: req.proxy_url,
            ..Default::default()
        };

        let session = SocialAuthSession {
            auth_endpoint,
            state,
            code_verifier,
            redirect_uri,
            expires_at,
            callback_rx: tokio::sync::Mutex::new(rx),
            cred_template,
            proxy,
            _server_handle: server_handle,
            relogin_target_id: None,
        };

        self.social_sessions
            .lock()
            .insert(session_id.clone(), session);

        Ok(StartSocialLoginResponse {
            session_id,
            portal_url,
            expires_at: expires_at.to_rfc3339(),
        })
    }

    /// 轮询一次 Social 登录状态
    pub async fn poll_social_login(
        &self,
        session_id: &str,
    ) -> Result<PollIdcLoginResponse, AdminServiceError> {
        use tokio::sync::oneshot::error::TryRecvError;

        // 一次加锁同时完成：过期检查 + 非阻塞回调接收，消除 TOCTOU
        enum PollOutcome {
            Expired,
            Closed,
            Pending,
            Received(social::OAuthCallbackData),
        }

        let outcome = {
            let sessions = self.social_sessions.lock();
            let Some(session) = sessions.get(session_id) else {
                return Err(AdminServiceError::NotFound { id: 0 });
            };

            if Utc::now() >= session.expires_at {
                PollOutcome::Expired
            } else {
                match session.callback_rx.try_lock() {
                    Ok(mut rx) => match rx.try_recv() {
                        Ok(data) => PollOutcome::Received(data),
                        Err(TryRecvError::Empty) => PollOutcome::Pending,
                        Err(TryRecvError::Closed) => PollOutcome::Closed,
                    },
                    Err(_) => PollOutcome::Pending,
                }
            }
        };

        match outcome {
            PollOutcome::Pending => return Ok(PollIdcLoginResponse::Pending),
            PollOutcome::Expired => {
                self.social_sessions.lock().remove(session_id);
                return Ok(PollIdcLoginResponse::Expired);
            }
            PollOutcome::Closed => {
                self.social_sessions.lock().remove(session_id);
                return Err(AdminServiceError::InternalError(
                    "Social 登录回调服务器已关闭，请重新发起登录".to_string(),
                ));
            }
            PollOutcome::Received(callback) => {
                self.do_complete_social_login(session_id, callback).await
            }
        }
    }

    /// 内部：完成 Social 登录的 token 兑换和凭据创建（供轮询回调和手动完成共用）
    ///
    /// 调用前须确认 session 存在且未过期。会在内部做 state CSRF 校验。
    async fn do_complete_social_login(
        &self,
        session_id: &str,
        callback: social::OAuthCallbackData,
    ) -> Result<PollIdcLoginResponse, AdminServiceError> {
        // 先做 CSRF 校验（不移除 session，校验失败时保持 session 可继续轮询）
        {
            let sessions = self.social_sessions.lock();
            let s = sessions
                .get(session_id)
                .ok_or(AdminServiceError::NotFound { id: 0 })?;
            if callback.state != s.state {
                tracing::warn!(
                    "Social 登录 state 不匹配（期望 {}, 收到 {}），已拒绝",
                    s.state,
                    callback.state
                );
                return Err(AdminServiceError::InternalError(
                    "OAuth state 不匹配，请重新发起登录".to_string(),
                ));
            }
        }

        // 移除 session（含 code_verifier 等敏感数据）
        let session = self
            .social_sessions
            .lock()
            .remove(session_id)
            .ok_or(AdminServiceError::NotFound { id: 0 })?;

        let config = self.token_manager.config();

        // 构建完整的 redirect_uri（与 IDE 行为一致）
        let full_redirect_uri = if callback.login_option.is_empty() {
            format!("{}{}", session.redirect_uri, callback.path)
        } else {
            format!(
                "{}{}?login_option={}",
                session.redirect_uri,
                callback.path,
                urlencoding::encode(&callback.login_option),
            )
        };

        let token = social::exchange_code_for_token(
            &session.auth_endpoint,
            &callback.code,
            &session.code_verifier,
            &full_redirect_uri,
            config,
            session.proxy.as_ref(),
        )
        .await
        .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        // 重新登录模式：更新已有凭据而非创建新凭据
        if let Some(target_id) = session.relogin_target_id {
            let refresh_token = token.refresh_token.ok_or_else(|| {
                AdminServiceError::InternalError(
                    "Social 登录未返回 refreshToken，无法更新凭据".to_string(),
                )
            })?;
            self.do_relogin_update(target_id, refresh_token)
                .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
            tracing::info!("Social 重新登录成功，凭据 #{} Token 已更新", target_id);
            return Ok(PollIdcLoginResponse::Success {
                credential_id: target_id,
            });
        }

        let mut new_cred = session.cred_template;
        new_cred.access_token = Some(token.access_token);
        new_cred.refresh_token = token.refresh_token;
        new_cred.expires_at = token.expires_at.or_else(|| {
            token
                .expires_in
                .map(|secs| (Utc::now() + Duration::seconds(secs)).to_rfc3339())
        });
        if let Some(arn) = token.profile_arn {
            new_cred.profile_arn = Some(arn);
        }

        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        // 主动刷新余额（含订阅等级 / 邮箱）并写入缓存，登录后立即可见
        if let Err(e) = self.get_balance(credential_id).await {
            tracing::warn!("Social 登录后刷新余额失败（不影响登录）: {}", e);
        }

        tracing::info!("Social 登录成功，已添加凭据 #{}", credential_id);
        Ok(PollIdcLoginResponse::Success { credential_id })
    }

    /// 手动完成 Social 登录：远程访问时从浏览器地址栏粘贴的回调 URL 中提取参数，直接完成 token 兑换
    pub async fn complete_social_login(
        &self,
        session_id: &str,
        code: String,
        state: String,
        login_option: String,
        path: String,
    ) -> Result<PollIdcLoginResponse, AdminServiceError> {
        // 过期检查
        {
            let sessions = self.social_sessions.lock();
            let s = sessions
                .get(session_id)
                .ok_or(AdminServiceError::NotFound { id: 0 })?;
            if Utc::now() >= s.expires_at {
                return Ok(PollIdcLoginResponse::Expired);
            }
        }

        let callback = social::OAuthCallbackData {
            code,
            login_option,
            path,
            state,
        };
        self.do_complete_social_login(session_id, callback).await
    }

    /// 分类删除凭据错误
    fn classify_delete_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else if msg.contains("只能删除已禁用的凭据") || msg.contains("请先禁用凭据")
        {
            AdminServiceError::InvalidCredential(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    // ── IdC 设备授权登录 ──────────────────────────────────────────────────────

    /// 发起 IdC 设备授权，返回验证码和 URL
    pub async fn start_idc_login(
        &self,
        req: StartIdcLoginRequest,
    ) -> Result<StartIdcLoginResponse, AdminServiceError> {
        let config = self.token_manager.config();
        let global_proxy = self.token_manager.proxy();

        // 代理：优先用请求级，否则回退全局
        let proxy = req
            .proxy_url
            .as_deref()
            .map(ProxyConfig::new)
            .or(global_proxy);

        let start_url = req.start_url.as_deref().unwrap_or(BUILDER_ID_START_URL);

        // 1. 注册 OIDC 客户端
        let reg = idc::register_client(&req.region, start_url, config, proxy.as_ref())
            .await
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        // 2. 发起设备授权
        let device = idc::start_device_authorization(
            &req.region,
            start_url,
            &reg.client_id,
            &reg.client_secret,
            config,
            proxy.as_ref(),
        )
        .await
        .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        let expires_at = Utc::now() + Duration::seconds(device.expires_in);
        let session_id = Uuid::new_v4().to_string();

        // 身份提供商：默认 Start URL 为 AWS Builder ID，自定义 Start URL 为企业 IAM Identity Center
        let provider = if start_url == BUILDER_ID_START_URL {
            "BuilderId"
        } else {
            "Enterprise"
        };

        // 构建登录成功后写入的凭据模板
        let cred_template = KiroCredentials {
            auth_method: Some("idc".to_string()),
            provider: Some(provider.to_string()),
            client_id: Some(reg.client_id.clone()),
            client_secret: Some(reg.client_secret.clone()),
            start_url: Some(start_url.to_string()),
            region: Some(req.region.clone()),
            priority: req.priority,
            email: req.email,
            proxy_url: req.proxy_url,
            ..Default::default()
        };

        let session = IdcAuthSession {
            region: req.region,
            client_id: reg.client_id,
            client_secret: reg.client_secret,
            device_code: device.device_code,
            expires_at,
            poll_interval: device.interval.max(5),
            cred_template,
            proxy,
            relogin_target_id: None,
        };

        let poll_interval = session.poll_interval;
        self.idc_sessions.lock().insert(session_id.clone(), session);

        Ok(StartIdcLoginResponse {
            session_id,
            user_code: device.user_code,
            verification_uri: device.verification_uri,
            verification_uri_complete: device.verification_uri_complete,
            expires_at: expires_at.to_rfc3339(),
            poll_interval,
        })
    }

    /// 轮询一次 IdC 登录状态
    pub async fn poll_idc_login(
        &self,
        session_id: &str,
    ) -> Result<PollIdcLoginResponse, AdminServiceError> {
        let (
            region,
            client_id,
            client_secret,
            device_code,
            _expires_at,
            proxy,
            cred_template,
            relogin_target_id,
        ) = {
            let sessions = self.idc_sessions.lock();
            let s = sessions
                .get(session_id)
                .ok_or_else(|| AdminServiceError::NotFound { id: 0 })?;

            if Utc::now() >= s.expires_at {
                return Ok(PollIdcLoginResponse::Expired);
            }

            (
                s.region.clone(),
                s.client_id.clone(),
                s.client_secret.clone(),
                s.device_code.clone(),
                s.expires_at,
                s.proxy.clone(),
                s.cred_template.clone(),
                s.relogin_target_id,
            )
        };

        let config = self.token_manager.config();

        match idc::poll_token(
            &region,
            &client_id,
            &client_secret,
            &device_code,
            config,
            proxy.as_ref(),
        )
        .await
        {
            idc::PollResult::Pending => Ok(PollIdcLoginResponse::Pending),
            idc::PollResult::Expired => {
                self.idc_sessions.lock().remove(session_id);
                Ok(PollIdcLoginResponse::Expired)
            }
            idc::PollResult::Error(e) => Err(AdminServiceError::InternalError(e.to_string())),
            idc::PollResult::Success(token) => {
                self.idc_sessions.lock().remove(session_id);

                // 重新登录模式：更新已有凭据而非创建新凭据
                if let Some(target_id) = relogin_target_id {
                    if let Some(refresh_token) = token.refresh_token {
                        self.do_relogin_update(target_id, refresh_token)
                            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;
                    }
                    tracing::info!("IdC 重新登录成功，凭据 #{} Token 已更新", target_id);
                    return Ok(PollIdcLoginResponse::Success {
                        credential_id: target_id,
                    });
                }

                // 写入凭据
                let mut new_cred = cred_template;
                new_cred.access_token = Some(token.access_token);
                new_cred.refresh_token = token.refresh_token;
                if let Some(secs) = token.expires_in {
                    new_cred.expires_at = Some((Utc::now() + Duration::seconds(secs)).to_rfc3339());
                }

                let credential_id = self
                    .token_manager
                    .add_credential(new_cred)
                    .await
                    .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

                // 主动刷新余额（含订阅等级 / 邮箱）并写入缓存，登录后立即可见
                if let Err(e) = self.get_balance(credential_id).await {
                    tracing::warn!("IdC 登录后刷新余额失败（不影响登录）: {}", e);
                }

                tracing::info!("IdC 设备授权登录成功，已添加凭据 #{}", credential_id);
                Ok(PollIdcLoginResponse::Success { credential_id })
            }
        }
    }

    /// 内部：重新登录完成后更新已有凭据的 Token（禁用→更新→重置→启用）
    fn do_relogin_update(&self, target_id: u64, refresh_token: String) -> anyhow::Result<()> {
        // 先禁用（update_refresh_token 要求凭据处于禁用状态）
        self.token_manager.set_disabled(target_id, true)?;
        // 更新 refreshToken（同时清空 accessToken 和 expiresAt，系统会在下次使用时自动刷新）
        self.token_manager
            .update_refresh_token(target_id, refresh_token, None, None)?;
        // 重置失败计数并重新启用
        self.token_manager.reset_and_enable(target_id)?;
        Ok(())
    }

    /// 发起 Social 重新登录（更新已有凭据的 Token 而非创建新凭据）
    pub async fn start_social_relogin(
        &self,
        target_id: u64,
        req: StartSocialLoginRequest,
    ) -> Result<StartSocialLoginResponse, AdminServiceError> {
        // 验证目标凭据存在
        {
            let snapshot = self.token_manager.snapshot();
            if !snapshot.entries.iter().any(|e| e.id == target_id) {
                return Err(AdminServiceError::NotFound { id: target_id });
            }
        }

        let global_proxy = self.token_manager.proxy();
        let proxy = req
            .proxy_url
            .as_deref()
            .map(ProxyConfig::new)
            .or(global_proxy);

        let auth_endpoint = req
            .auth_endpoint
            .unwrap_or_else(|| social::KIRO_AUTH_ENDPOINT.to_string());

        let (code_verifier, code_challenge) = social::generate_pkce();
        let state = uuid::Uuid::new_v4().to_string();

        let (tx, rx) = tokio::sync::oneshot::channel::<social::OAuthCallbackData>();

        let (port, server_handle) = social::start_callback_server(tx)
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        let redirect_uri = format!("http://127.0.0.1:{}", port);
        let portal_url = social::build_portal_url(&state, &code_challenge, &redirect_uri);

        let expires_at = Utc::now() + Duration::minutes(10);
        let session_id = uuid::Uuid::new_v4().to_string();

        let session = SocialAuthSession {
            auth_endpoint,
            state,
            code_verifier,
            redirect_uri,
            expires_at,
            callback_rx: tokio::sync::Mutex::new(rx),
            cred_template: KiroCredentials::default(),
            proxy,
            _server_handle: server_handle,
            relogin_target_id: Some(target_id),
        };

        self.social_sessions
            .lock()
            .insert(session_id.clone(), session);

        Ok(StartSocialLoginResponse {
            session_id,
            portal_url,
            expires_at: expires_at.to_rfc3339(),
        })
    }

    /// 发起 IdC 重新登录（更新已有凭据的 Token 而非创建新凭据）
    pub async fn start_idc_relogin(
        &self,
        target_id: u64,
        req: StartIdcLoginRequest,
    ) -> Result<StartIdcLoginResponse, AdminServiceError> {
        // 验证目标凭据存在
        {
            let snapshot = self.token_manager.snapshot();
            if !snapshot.entries.iter().any(|e| e.id == target_id) {
                return Err(AdminServiceError::NotFound { id: target_id });
            }
        }

        let config = self.token_manager.config();
        let global_proxy = self.token_manager.proxy();

        let proxy = req
            .proxy_url
            .as_deref()
            .map(ProxyConfig::new)
            .or(global_proxy);

        let start_url = req.start_url.as_deref().unwrap_or(BUILDER_ID_START_URL);

        let reg = idc::register_client(&req.region, start_url, config, proxy.as_ref())
            .await
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        let device = idc::start_device_authorization(
            &req.region,
            start_url,
            &reg.client_id,
            &reg.client_secret,
            config,
            proxy.as_ref(),
        )
        .await
        .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        let expires_at = Utc::now() + Duration::seconds(device.expires_in);
        let session_id = Uuid::new_v4().to_string();

        let session = IdcAuthSession {
            region: req.region,
            client_id: reg.client_id,
            client_secret: reg.client_secret,
            device_code: device.device_code,
            expires_at,
            poll_interval: device.interval.max(5),
            cred_template: KiroCredentials::default(),
            proxy,
            relogin_target_id: Some(target_id),
        };

        let poll_interval = session.poll_interval;
        self.idc_sessions.lock().insert(session_id.clone(), session);

        Ok(StartIdcLoginResponse {
            session_id,
            user_code: device.user_code,
            verification_uri: device.verification_uri,
            verification_uri_complete: device.verification_uri_complete,
            expires_at: expires_at.to_rfc3339(),
            poll_interval,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_compares_correctly() {
        use std::cmp::Ordering;
        assert_eq!(compare_semver("0.3.0", "0.3.1"), Ordering::Less);
        assert_eq!(compare_semver("v0.3.1", "0.3.1"), Ordering::Equal);
        assert_eq!(compare_semver("1.0.0", "0.99.99"), Ordering::Greater);
        assert_eq!(compare_semver("0.3.1-rc.1", "0.3.1"), Ordering::Equal);
    }

    #[test]
    fn export_uses_nested_account_format() {
        let mut cred = KiroCredentials::default();
        cred.refresh_token = Some("rt-123".to_string());
        cred.client_id = Some("cid".to_string());
        cred.client_secret = Some("csec".to_string());
        cred.auth_method = Some("idc".to_string());
        cred.provider = Some("Enterprise".to_string());
        cred.region = Some("us-east-1".to_string());
        cred.email = Some("e@example.com".to_string());
        cred.expires_at = Some("2026-06-06T00:00:00Z".to_string());
        // 占位符 profileArn 应在导出时被剥离
        cred.profile_arn = Some(
            crate::kiro::model::credentials::BUILDER_ID_PROFILE_ARN.to_string(),
        );

        let acc = credential_to_export_account(cred).expect("应生成账号");

        // 嵌套 credentials 结构
        assert_eq!(acc.credentials.refresh_token.as_deref(), Some("rt-123"));
        assert_eq!(acc.credentials.client_id.as_deref(), Some("cid"));
        // authMethod 规范化为 "IdC"
        assert_eq!(acc.credentials.auth_method.as_deref(), Some("IdC"));
        // expiresAt 解析为毫秒时间戳
        assert!(acc.credentials.expires_at > 0);
        // idp 取 provider
        assert_eq!(acc.idp, "Enterprise");
        // 占位符 profileArn 被跳过
        assert_eq!(acc.profile_arn, None);
        // 必填的 csrfToken 输出空串
        assert_eq!(acc.credentials.csrf_token, "");
    }

    #[test]
    fn export_skips_api_key_credentials() {
        let mut cred = KiroCredentials::default();
        cred.kiro_api_key = Some("ksk_abc".to_string());
        cred.auth_method = Some("api_key".to_string());
        // 无 refreshToken → 跳过
        assert!(credential_to_export_account(cred).is_none());
    }

    #[test]
    fn subscription_type_mapping() {
        assert_eq!(subscription_type_from_title(Some("KIRO FREE")), "Free");
        assert_eq!(subscription_type_from_title(Some("KIRO PRO+")), "Pro_Plus");
        assert_eq!(subscription_type_from_title(Some("KIRO PRO")), "Pro");
        assert_eq!(subscription_type_from_title(Some("KIRO POWER")), "Enterprise");
        assert_eq!(subscription_type_from_title(None), "Free");
    }
}
