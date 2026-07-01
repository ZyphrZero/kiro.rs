//! Kiro API Provider
//!
//! 核心组件，负责与 Kiro API 通信
//! 支持流式和非流式请求
//! 支持多凭据故障转移和重试
//! 支持按凭据级 endpoint 切换不同 Kiro API 端点

use reqwest::Client;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::admin::trace_db::{TraceAttempt, TraceSink, outcome, truncate_snippet};
use crate::http_client::{ProxyConfig, build_client, build_streaming_client};
use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
use crate::kiro::endpoint::amazonq::AMAZONQ_ENDPOINT_NAME;
use crate::kiro::endpoint::cli::CLI_ENDPOINT_NAME;
use crate::kiro::endpoint::codewhisperer::CODEWHISPERER_ENDPOINT_NAME;
use crate::kiro::endpoint::ide::IDE_ENDPOINT_NAME;
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::{CallContext, MultiTokenManager};
use crate::model::config::TlsBackend;
use parking_lot::Mutex;

/// debug 日志下请求体最大打印字节数。超过则只打印头部 + 省略提示,
/// 避免 RUST_LOG=debug 时 200K 级请求体被完整格式化拖垮代理。
const LOG_BODY_MAX_BYTES: usize = 2048;

/// 为 debug 日志截断请求体（UTF-8 安全,只在字符边界切）。
pub(crate) fn truncate_for_log(body: &str) -> std::borrow::Cow<'_, str> {
    if body.len() <= LOG_BODY_MAX_BYTES {
        return std::borrow::Cow::Borrowed(body);
    }
    let mut end = LOG_BODY_MAX_BYTES;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    std::borrow::Cow::Owned(format!(
        "{}…[截断,完整 {} bytes]",
        &body[..end],
        body.len()
    ))
}

/// 每个凭据的最大重试次数
const MAX_RETRIES_PER_CREDENTIAL: usize = 3;

/// 总重试次数硬上限（避免无限重试）
///
/// 注：上游 429 多为账号级速率配额（SERVICE_REQUEST_RATE_EXCEEDED），高峰期
/// 多账号同时触顶时，过多重试会在账号间连环撞墙、放大限流。故上限取较小值，
/// 配合 429 专用长退避（见 retry_delay_throttle），被限时尽早返回而非耗尽配额。
const MAX_TOTAL_RETRIES: usize = 4;

/// A4a 大请求字节阈值（~512KB）。请求体超此值视为 Long 档（约对应 128K token 的
/// Anthropic JSON 体量），触发调度排序惩罚以分散大请求、避免扎堆同账号拖垮首字节。
const LARGE_REQUEST_BYTES: usize = 512 * 1024;

/// HTTP Client 缓存容量上限（不含常驻的全局代理 client）。
/// 代理池条目较多时，避免每个不同代理都常驻一个 reqwest::Client 导致内存无界增长。
const CLIENT_CACHE_CAP: usize = 64;

/// 带容量上限的 HTTP Client 缓存。
///
/// - key 为 effective proxy 配置（None = 直连/全局回退）
/// - 受保护 key（全局代理对应的 effective 配置）永不被淘汰
/// - 超出容量时按插入顺序淘汰最旧的「非受保护」条目
struct ClientCache {
    map: HashMap<Option<ProxyConfig>, Client>,
    /// 插入顺序（仅记录可淘汰的非受保护 key）
    order: std::collections::VecDeque<Option<ProxyConfig>>,
    /// 受保护、不参与淘汰的 key（全局代理）
    protected: Option<ProxyConfig>,
    cap: usize,
}

impl ClientCache {
    fn new(protected: Option<ProxyConfig>, initial: Client, cap: usize) -> Self {
        let mut map = HashMap::new();
        map.insert(protected.clone(), initial);
        Self {
            map,
            order: std::collections::VecDeque::new(),
            protected,
            cap,
        }
    }

    fn get(&self, key: &Option<ProxyConfig>) -> Option<Client> {
        self.map.get(key).cloned()
    }

    /// 插入新条目，必要时淘汰最旧的非受保护条目
    fn insert(&mut self, key: Option<ProxyConfig>, client: Client) {
        if key == self.protected || self.map.contains_key(&key) {
            self.map.insert(key, client);
            return;
        }
        while self.order.len() >= self.cap {
            if let Some(evict) = self.order.pop_front() {
                self.map.remove(&evict);
            } else {
                break;
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, client);
    }
}

/// API 调用结果，附带本次实际命中的上游凭据 ID（用于用量统计）
pub struct KiroCallResult {
    pub response: reqwest::Response,
    pub credential_id: u64,
    pub account_guard: CallContext,
}

/// Kiro API Provider
///
/// 核心组件，负责与 Kiro API 通信
/// 支持多凭据故障转移和重试机制
/// 按凭据 `endpoint` 字段选择 [`KiroEndpoint`] 实现
pub struct KiroProvider {
    token_manager: Arc<MultiTokenManager>,
    /// 全局代理配置（用于凭据无自定义代理时的回退）
    global_proxy: Option<ProxyConfig>,
    /// Client 缓存：key = effective proxy config, value = reqwest::Client
    /// 不同代理配置的凭据使用不同的 Client，共享相同代理的凭据复用 Client。
    /// 带容量上限淘汰（全局代理 client 常驻），避免代理数量增长导致内存无界增长。
    client_cache: Mutex<ClientCache>,
    /// 流式专用 Client 缓存（禁用空闲连接复用，见 [`build_streaming_client`]）。
    /// 与 `client_cache` 同构、同样按 effective proxy 缓存，但每条流用全新连接，
    /// 避免长流被上游中途掐断导致的"断流"。
    streaming_client_cache: Mutex<ClientCache>,
    /// TLS 后端配置
    tls_backend: TlsBackend,
    /// 端点实现注册表（key: endpoint 名称）
    endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
    /// 默认端点名称（凭据未指定 endpoint 时使用）
    default_endpoint: String,
    /// Preferred endpoint when route-level fallback is enabled.
    preferred_endpoint: Option<String>,
    /// Whether to try Kiro-Go-compatible fallback endpoints on the same credential.
    endpoint_fallback: bool,
    /// 已尝试过 profileArn 解析的凭据 ID（进程内）。
    ///
    /// 避免对「无 Enterprise profile」的账号（如纯 BuilderID）在每次请求都重复调用
    /// `ListAvailableProfiles`。命中真实 ARN 的账号会把 ARN 持久化进凭据，之后
    /// 通过 `streaming_profile_arn()` 直接命中，不再进入解析路径。
    profile_resolution_attempted: Mutex<HashSet<u64>>,
}

impl KiroProvider {
    /// 创建带代理配置和端点注册表的 KiroProvider 实例
    ///
    /// # Arguments
    /// * `token_manager` - 多凭据 Token 管理器
    /// * `proxy` - 全局代理配置
    /// * `endpoints` - 端点名 → 实现的注册表（至少包含 `default_endpoint` 对应条目）
    /// * `default_endpoint` - 凭据未显式指定 endpoint 时使用的名称
    pub fn with_proxy(
        token_manager: Arc<MultiTokenManager>,
        proxy: Option<ProxyConfig>,
        endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
        default_endpoint: String,
        preferred_endpoint: Option<String>,
        endpoint_fallback: bool,
    ) -> Self {
        assert!(
            endpoints.contains_key(&default_endpoint),
            "默认端点 {} 未在 endpoints 注册表中",
            default_endpoint
        );
        if let Some(preferred) = preferred_endpoint.as_deref() {
            assert!(
                endpoints.contains_key(preferred),
                "preferred endpoint {} 未在 endpoints 注册表中",
                preferred
            );
        }
        let tls_backend = token_manager.config().tls_backend;
        // 预热：构建全局代理对应的 Client（作为受保护的常驻条目）
        let initial_client =
            build_client(proxy.as_ref(), 720, tls_backend).expect("创建 HTTP 客户端失败");
        let client_cache = ClientCache::new(proxy.clone(), initial_client, CLIENT_CACHE_CAP);
        // 流式专用 Client（禁用空闲连接复用）同样预热全局代理条目。
        let initial_streaming_client = build_streaming_client(proxy.as_ref(), 720, tls_backend)
            .expect("创建流式 HTTP 客户端失败");
        let streaming_client_cache =
            ClientCache::new(proxy.clone(), initial_streaming_client, CLIENT_CACHE_CAP);

        Self {
            token_manager,
            global_proxy: proxy,
            client_cache: Mutex::new(client_cache),
            streaming_client_cache: Mutex::new(streaming_client_cache),
            tls_backend,
            endpoints,
            default_endpoint,
            preferred_endpoint,
            endpoint_fallback,
            profile_resolution_attempted: Mutex::new(HashSet::new()),
        }
    }

    /// 根据凭据的代理配置获取（或创建并缓存）对应的 reqwest::Client
    fn client_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Client> {
        let effective = credentials.effective_proxy(self.global_proxy.as_ref());
        let mut cache = self.client_cache.lock();
        if let Some(client) = cache.get(&effective) {
            return Ok(client);
        }
        let client = build_client(effective.as_ref(), 720, self.tls_backend)?;
        cache.insert(effective, client.clone());
        Ok(client)
    }

    /// 获取流式专用 Client（禁用空闲连接复用，见 [`build_streaming_client`]）。
    /// 与 [`Self::client_for`] 同构，但走独立的 `streaming_client_cache`。
    fn streaming_client_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Client> {
        let effective = credentials.effective_proxy(self.global_proxy.as_ref());
        let mut cache = self.streaming_client_cache.lock();
        if let Some(client) = cache.get(&effective) {
            return Ok(client);
        }
        let client = build_streaming_client(effective.as_ref(), 720, self.tls_backend)?;
        cache.insert(effective, client.clone());
        Ok(client)
    }

    /// 根据凭据选择 endpoint 实现
    fn endpoint_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Arc<dyn KiroEndpoint>> {
        let name = credentials
            .endpoint
            .as_deref()
            .unwrap_or(&self.default_endpoint);
        self.endpoints
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("未知端点: {}", name))
    }

    /// Build endpoint order for API calls. When endpoint fallback is disabled,
    /// returns the single selected endpoint; when enabled, primary first then the
    /// remaining Kiro-Go-compatible streaming endpoints.
    fn endpoint_order_for(
        &self,
        credentials: &KiroCredentials,
    ) -> anyhow::Result<Vec<Arc<dyn KiroEndpoint>>> {
        let primary = credentials
            .endpoint
            .as_deref()
            .or(self.preferred_endpoint.as_deref())
            .unwrap_or(&self.default_endpoint);
        if !self.endpoint_fallback || primary == CLI_ENDPOINT_NAME {
            return self.endpoints.get(primary).cloned()
                .map(|endpoint| vec![endpoint])
                .ok_or_else(|| anyhow::anyhow!("未知端点: {}", primary));
        }
        const FALLBACK_ORDER: [&str; 3] = [
            IDE_ENDPOINT_NAME,
            CODEWHISPERER_ENDPOINT_NAME,
            AMAZONQ_ENDPOINT_NAME,
        ];
        if !FALLBACK_ORDER.contains(&primary) {
            return self.endpoints.get(primary).cloned()
                .map(|endpoint| vec![endpoint])
                .ok_or_else(|| anyhow::anyhow!("未知端点: {}", primary));
        }
        let mut names = Vec::with_capacity(3);
        names.push(primary);
        for name in FALLBACK_ORDER {
            if name != primary {
                names.push(name);
            }
        }
        names.into_iter().map(|name| {
            self.endpoints.get(name).cloned()
                .ok_or_else(|| anyhow::anyhow!("未知端点: {}", name))
        }).collect()
    }

    fn is_fallbackable_status(status: reqwest::StatusCode) -> bool {
        // 对齐 Kiro-Go (proxy/kiro.go:401-419): 除账号级状态(401/402/403)外,
        // 其余非 200 一律 fallback 到下一个端点(含 524 网关超时、400 客户端校验错误)。
        // 401/403/402 由下方账号级状态处理分支负责,在同一账号上换端点无意义。
        !matches!(status.as_u16(), 401 | 402 | 403)
    }

    fn is_fallbackable_network_error() -> bool {
        true
    }

    /// 在发起请求前，确保 Enterprise / IdC 账号的真实 profileArn 已解析并写入 `ctx`。
    ///
    /// 流式端点强制要求 profileArn；Enterprise / IdC 账号必须先把 BuilderID
    /// 占位符解析为真实 ARN，纯 BuilderID 账号则回退占位符。
    /// 仅对「OAuth 凭据 + profileArn 缺失或为占位符」的账号触发一次上游
    /// `ListAvailableProfiles` 查询（进程内去重）：
    /// - 命中真实 ARN → 写回 `ctx.credentials.profile_arn` 并由 token_manager 持久化；
    ///   之后该凭据的 `streaming_profile_arn()` 直接命中，不再进入此路径。
    /// - 无 Enterprise profile（纯 BuilderID 等）→ 保持占位符回退逻辑，并标记已尝试，
    ///   避免每次请求重复查询。
    async fn ensure_profile_arn(&self, ctx: &mut crate::kiro::token_manager::CallContext) {
        use crate::kiro::model::credentials::is_placeholder_profile_arn;

        if ctx.credentials.is_api_key_credential() {
            return;
        }
        let needs = match ctx.credentials.profile_arn.as_deref() {
            None => true,
            Some(arn) => is_placeholder_profile_arn(arn),
        };
        if !needs {
            return;
        }
        // 进程内去重：仅在「拿到上游确定结果」后才标记已尝试，避免一次网络抖动
        // 把账号永久卡在占位符上（重启前不再重试）。
        if self.profile_resolution_attempted.lock().contains(&ctx.id) {
            return;
        }
        match self
            .token_manager
            .resolve_profile_arn_for(ctx.id, &ctx.token)
            .await
        {
            Ok(Some(arn)) => {
                ctx.credentials.profile_arn = Some(arn);
                self.profile_resolution_attempted.lock().insert(ctx.id);
            }
            Ok(None) => {
                // 上游确认该账号无 Enterprise profile（纯 BuilderID 等）：标记已尝试，
                // 后续请求回退到占位符逻辑，不再重复查询。
                self.profile_resolution_attempted.lock().insert(ctx.id);
            }
            Err(e) => {
                // 网络/瞬态错误：不标记，下次请求再试；本次按原 profileArn 继续
                tracing::warn!(
                    "凭据 #{} 解析真实 profileArn 失败（按原 profileArn 继续）: {}",
                    ctx.id,
                    e
                );
            }
        }
    }

    /// 发送非流式 API 请求
    ///
    /// 支持多凭据故障转移（见 [`Self::call_api_with_retry`]）。
    /// `sink` 可选，用于逐跳上报链路追踪。
    pub async fn call_api(
        &self,
        request_body: &str,
        sink: Option<&dyn TraceSink>,
        group: Option<&str>,
    ) -> anyhow::Result<KiroCallResult> {
        self.call_api_with_retry(request_body, false, sink, group)
            .await
    }

    /// 发送流式 API 请求
    pub async fn call_api_stream(
        &self,
        request_body: &str,
        sink: Option<&dyn TraceSink>,
        group: Option<&str>,
    ) -> anyhow::Result<KiroCallResult> {
        self.call_api_with_retry(request_body, true, sink, group)
            .await
    }

    /// 发送 MCP API 请求（WebSearch 等工具调用）
    pub async fn call_mcp(&self, request_body: &str) -> anyhow::Result<reqwest::Response> {
        self.call_mcp_with_retry(request_body).await
    }

    /// 内部方法：带重试逻辑的 MCP API 调用
    async fn call_mcp_with_retry(&self, request_body: &str) -> anyhow::Result<reqwest::Response> {
        let total_credentials = self.token_manager.total_count();
        let max_retries = (total_credentials * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();

        for attempt in 0..max_retries {
            // MCP 调用（WebSearch 等工具）不涉及模型选择，也不参与分组隔离；不启用快速模式
            let ctx = match self.token_manager.acquire_context(None, None).await {
                Ok(c) => c,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, config);

            let endpoint = match self.endpoint_for(&ctx.credentials) {
                Ok(e) => e,
                Err(e) => {
                    last_error = Some(e);
                    // endpoint 解析失败：记为失败，换下一张凭据
                    self.token_manager.report_failure(ctx.id);
                    continue;
                }
            };

            let rctx = RequestContext {
                credentials: &ctx.credentials,
                token: &ctx.token,
                machine_id: &machine_id,
                config,
            };

            let url = endpoint.mcp_url(&rctx);
            let body = endpoint.transform_mcp_body(request_body, &rctx);

            let base = self
                .client_for(&ctx.credentials)?
                .post(&url)
                .body(body)
                .header("content-type", endpoint.content_type());
            let request = endpoint.decorate_mcp(base, &rctx);

            let response = match request.send().await {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!(
                        "MCP 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        e
                    );
                    last_error = Some(e.into());
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }
            };

            let status = response.status();

            // 成功响应
            if status.is_success() {
                self.token_manager.report_success(ctx.id);
                return Ok(response);
            }

            // 失败响应
            let body = response.text().await.unwrap_or_default();

            // 402 额度用尽
            if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body) {
                let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 400 Bad Request
            if status.as_u16() == 400 {
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 401/403 凭据问题
            if matches!(status.as_u16(), 401 | 403) {
                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                if endpoint.is_bearer_token_invalid(&body) && !force_refreshed.contains(&ctx.id) {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    if self
                        .token_manager
                        .force_refresh_token_for(ctx.id)
                        .await
                        .is_ok()
                    {
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                }

                let has_available = self.token_manager.report_failure(ctx.id);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 瞬态错误
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                tracing::warn!(
                    "MCP 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                if attempt + 1 < max_retries {
                    // 429 限流用更长退避；408/5xx 仍用通用快速退避
                    let delay = if status.as_u16() == 429 {
                        Self::retry_delay_throttle(attempt)
                    } else {
                        Self::retry_delay(attempt)
                    };
                    sleep(delay).await;
                }
                continue;
            }

            // 其他 4xx
            if status.is_client_error() {
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 兜底
            last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!("MCP 请求失败：已达到最大重试次数（{}次）", max_retries)
        }))
    }

    /// 内部方法：带重试逻辑的 API 调用
    ///
    /// 重试策略：
    /// - 每个凭据最多重试 MAX_RETRIES_PER_CREDENTIAL 次
    /// - 总重试次数 = min(凭据数量 × 每凭据重试次数, MAX_TOTAL_RETRIES)
    /// - 硬上限 9 次，避免无限重试
    async fn call_api_with_retry(
        &self,
        request_body: &str,
        is_stream: bool,
        sink: Option<&dyn TraceSink>,
        group: Option<&str>,
    ) -> anyhow::Result<KiroCallResult> {
        // 重试预算按当前请求所属分组的账号数计算，避免小分组按全局账号数获得过多无效重试
        let total_credentials = self.token_manager.total_count_in_group(group).max(1);
        let max_retries = (total_credentials * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();
        let api_type = if is_stream { "流式" } else { "非流式" };

        // 尝试从请求体中提取模型信息
        let model = Self::extract_model_from_request(request_body);

        // A4a 大请求调度惩罚：请求体 ≥ LARGE_REQUEST_BYTES(~512KB,约对应 128K token 的
        // Anthropic JSON 体量)视为 Long 档,按命中账号在途数加排序惩罚,避免大请求扎堆同账号。
        // 普通请求 large_penalty=0,调度行为与历史完全一致。
        let large_penalty = if request_body.len() >= LARGE_REQUEST_BYTES {
            self.token_manager.config().large_request_rank_penalty
        } else {
            0
        };

        for attempt in 0..max_retries {
            let attempt_start = Instant::now();
            // 获取调用上下文（绑定 index、credentials、token）
            let mut ctx = match self
                .token_manager
                .acquire_context_sized(model.as_deref(), group, large_penalty)
                .await
            {
                Ok(c) => {
                    // 首次成功拿到凭据：标记"等账号槽"结束（幂等，sink 内部只记第一次）。
                    if let Some(s) = sink {
                        s.on_credential_acquired();
                    }
                    c
                }
                Err(e) => {
                    Self::emit_attempt(
                        sink,
                        attempt,
                        0,
                        "",
                        None,
                        outcome::UNKNOWN,
                        Some(&e.to_string()),
                        attempt_start,
                    );
                    last_error = Some(e);
                    // acquire 默认非阻塞返回"池忙"。attempt 之间加短退避(指数 200ms 起),
                    // 既防 busy-spin、又给账号释放并发槽的窗口。MAX_TOTAL_RETRIES=4 兜底,
                    // 全忙时总延迟 ~1.5s 内返回。account_acquire_blocking=true 时 acquire 自身
                    // 已阻塞等待,这里的退避不会过度叠加(下一轮 acquire 仍受 30s 上限约束)。
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }
            };

            // 确保 Enterprise / IdC 账号的真实 profileArn 已解析（流式端点强制要求）
            self.ensure_profile_arn(&mut ctx).await;

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, config);

            let endpoints = match self.endpoint_order_for(&ctx.credentials) {
                Ok(e) => e,
                Err(e) => {
                    Self::emit_attempt(sink, attempt, ctx.id, "", None, outcome::UNKNOWN, Some(&e.to_string()), attempt_start);
                    last_error = Some(e);
                    self.token_manager.report_failure(ctx.id);
                    continue;
                }
            };

            let stream_reuse = config.stream_conn_reuse_enabled;
            let http_client = match if is_stream && !stream_reuse {
                self.streaming_client_for(&ctx.credentials)
            } else {
                self.client_for(&ctx.credentials)
            } {
                Ok(c) => c,
                Err(e) => {
                    Self::emit_attempt(sink, attempt, ctx.id, "", None, outcome::NETWORK_ERROR, Some(&e.to_string()), attempt_start);
                    last_error = Some(e);
                    if attempt + 1 < max_retries { sleep(Self::retry_delay(attempt)).await; }
                    continue;
                }
            };

            let endpoint_count = endpoints.len();
            let mut selected_failure: Option<(Arc<dyn KiroEndpoint>, reqwest::StatusCode, String)> = None;

            for (endpoint_index, endpoint) in endpoints.into_iter().enumerate() {
                let endpoint_name = endpoint.name();
                let endpoint_attempt_start = if endpoint_index == 0 { attempt_start } else { Instant::now() };
                let rctx = RequestContext {
                    credentials: &ctx.credentials,
                    token: &ctx.token,
                    machine_id: &machine_id,
                    config,
                };
                let url = endpoint.api_url(&rctx);
                let body = endpoint.transform_api_body(request_body, &rctx);
                tracing::debug!("使用端点 [{}] POST {}", endpoint.name(), url);
                if tracing::enabled!(tracing::Level::DEBUG) {
                    tracing::debug!("实际发送请求体: {}", truncate_for_log(&body));
                }
                let base = http_client.post(&url).body(body).header("content-type", endpoint.content_type());
                let request = endpoint.decorate_api(base, &rctx);
                let request = request.build().map_err(|e| anyhow::anyhow!("构建请求失败: {}", e))?;
                if tracing::enabled!(tracing::Level::DEBUG) {
                    for (k, v) in request.headers() {
                        tracing::debug!("  header {}: {}", k, v.to_str().unwrap_or("<binary>"));
                    }
                }
                let response = match http_client.execute(request).await {
                    Ok(resp) => resp,
                    Err(e) => {
                        tracing::warn!("API 请求发送失败（端点 {}, 尝试 {}/{}）: {}", endpoint_name, attempt + 1, max_retries, e);
                        Self::emit_attempt(sink, attempt, ctx.id, endpoint_name, None, outcome::NETWORK_ERROR, Some(&e.to_string()), endpoint_attempt_start);
                        if Self::is_fallbackable_network_error() && endpoint_index + 1 < endpoint_count {
                            tracing::warn!("Endpoint {} network error; trying next fallback on #{}", endpoint_name, ctx.id);
                            continue;
                        }
                        last_error = Some(e.into());
                        if attempt + 1 < max_retries { sleep(Self::retry_delay(attempt)).await; }
                        break;
                    }
                };
                let status = response.status();
                if status.is_success() {
                    Self::emit_attempt(sink, attempt, ctx.id, endpoint_name, Some(status.as_u16()), outcome::SUCCESS, None, endpoint_attempt_start);
                    self.token_manager.report_success(ctx.id);
                    return Ok(KiroCallResult { response, credential_id: ctx.id, account_guard: ctx });
                }
                let body = response.text().await.unwrap_or_default();
                if Self::is_fallbackable_status(status) && endpoint_index + 1 < endpoint_count {
                    tracing::warn!("Endpoint {} returned {}; trying next fallback on #{}", endpoint_name, status, ctx.id);
                    Self::emit_attempt(sink, attempt, ctx.id, endpoint_name, Some(status.as_u16()), outcome::TRANSIENT, Some(&body), endpoint_attempt_start);
                    continue;
                }
                selected_failure = Some((endpoint, status, body));
                break;
            }

            let Some((endpoint, status, body)) = selected_failure else { continue; };
            let endpoint_name = endpoint.name();

            // 402 Payment Required 且额度用尽：禁用凭据并故障转移
            if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body) {
                tracing::warn!(
                    "API 请求失败（额度已用尽，禁用凭据并切换，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::QUOTA_EXHAUSTED,
                    Some(&body),
                    attempt_start,
                );

                let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                if !has_available {
                    anyhow::bail!(
                        "{} API 请求失败（所有凭据已用尽）: {} {}",
                        api_type,
                        status,
                        body
                    );
                }

                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                continue;
            }

            // 400 Bad Request - 请求问题，重试/切换凭据无意义
            if status.as_u16() == 400 {
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(400),
                    outcome::BAD_REQUEST,
                    Some(&body),
                    attempt_start,
                );
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 401/403 - 更可能是凭据/权限问题：计入失败并允许故障转移
            if matches!(status.as_u16(), 401 | 403) {
                // 403 账号级临时风控（"temporarily suspended / unusual user activity"）：
                // 这是会自动恢复的临时封锁，按长冷却处理、到期自动回池，
                // 不计入连续失败——否则会被自愈反复复活、持续捅一个正在被风控的账号。
                if status.as_u16() == 403
                    && self.token_manager.get_account_throttle_failover()
                    && endpoint.is_account_throttled(&body)
                {
                    let cooldown_secs = self
                        .token_manager
                        .get_account_throttle_cooldown_secs()
                        .max(1);
                    let cooldown = std::time::Duration::from_secs(cooldown_secs);
                    tracing::warn!(
                        "API 请求失败（账号级临时封锁 403，凭据 #{} 冷却 {}s 并切换，尝试 {}/{}）: {}",
                        ctx.id,
                        cooldown_secs,
                        attempt + 1,
                        max_retries,
                        body
                    );
                    let remaining = self
                        .token_manager
                        .report_account_throttled(ctx.id, cooldown);
                    Self::emit_attempt(
                        sink,
                        attempt,
                        ctx.id,
                        endpoint_name,
                        Some(403),
                        outcome::ACCOUNT_THROTTLED,
                        Some(&body),
                        attempt_start,
                    );
                    last_error = Some(anyhow::anyhow!(
                        "{} API 请求失败（账号级临时封锁，凭据 #{} 已冷却 {} 分钟）: {} {}",
                        api_type,
                        ctx.id,
                        cooldown_secs / 60,
                        status,
                        body
                    ));
                    if remaining == 0 {
                        anyhow::bail!(
                            "{} API 请求失败：所有凭据都处于账号风控冷却或已禁用状态。\
                             上游对凭据 #{} 的账号触发了临时封锁（temporarily suspended / unusual \
                             user activity）。建议：(1) 增加更多不同 AWS 账号的凭据；\
                             (2) 降低请求并发，避免触发上游反滥用；\
                             (3) 在管理面板降低冷却时长或手动解除冷却以重试。原始响应: {} {}",
                            api_type,
                            ctx.id,
                            status,
                            body
                        );
                    }
                    continue;
                }

                tracing::warn!(
                    "API 请求失败（可能为凭据错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::AUTH_FAILED,
                    Some(&body),
                    attempt_start,
                );

                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                if endpoint.is_bearer_token_invalid(&body) && !force_refreshed.contains(&ctx.id) {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    if self
                        .token_manager
                        .force_refresh_token_for(ctx.id)
                        .await
                        .is_ok()
                    {
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                }

                let has_available = self.token_manager.report_failure(ctx.id);
                if !has_available {
                    anyhow::bail!(
                        "{} API 请求失败（所有凭据已用尽）: {} {}",
                        api_type,
                        status,
                        body
                    );
                }

                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                continue;
            }

            // 429 + suspicious activity = 账号级临时风控
            // 仅当前凭据被针对，故障转移到其它凭据可立即恢复（受配置开关控制）。
            if status.as_u16() == 429
                && self.token_manager.get_account_throttle_failover()
                && endpoint.is_account_throttled(&body)
            {
                let cooldown_secs = self
                    .token_manager
                    .get_account_throttle_cooldown_secs()
                    .max(1);
                let cooldown = std::time::Duration::from_secs(cooldown_secs);
                tracing::warn!(
                    "API 请求失败（账号级风控，凭据 #{} 冷却 {}s 并切换，尝试 {}/{}）: {}",
                    ctx.id,
                    cooldown_secs,
                    attempt + 1,
                    max_retries,
                    body
                );

                let remaining = self
                    .token_manager
                    .report_account_throttled(ctx.id, cooldown);
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(429),
                    outcome::ACCOUNT_THROTTLED,
                    Some(&body),
                    attempt_start,
                );
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败（账号级风控，凭据 #{} 已冷却 {} 分钟）: {} {}",
                    api_type,
                    ctx.id,
                    cooldown_secs / 60,
                    status,
                    body
                ));

                if remaining == 0 {
                    anyhow::bail!(
                        "{} API 请求失败：所有凭据都处于账号风控冷却或已禁用状态。\
                         上游对凭据 #{} 的账号触发了 \"suspicious activity\" 临时限速，\
                         建议：(1) 增加更多不同 AWS 账号的凭据；\
                         (2) 在管理面板降低冷却时长或手动解除冷却以重试；\
                         (3) 提交 AWS Support 申诉解封该账号。原始响应: {} {}",
                        api_type,
                        ctx.id,
                        status,
                        body
                    );
                }
                continue;
            }

            // 客户端请求格式错误（messages 数组违反协议）：根因在调用方，重试无意义
            // 上游常以 5xx 返回，必须在下方"瞬态错误重试"分支之前拦截，否则会被当作
            // 上游故障重试 max_retries 次，把一个坏请求放大成多次 503（503 风暴）。
            // 直接终止：不重试、不切换凭据、不计入凭据失败。
            if endpoint.is_client_validation_error(&body) {
                tracing::warn!(
                    "API 请求失败（客户端请求格式错误，不重试）: {} {}",
                    status,
                    body
                );
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::BAD_REQUEST,
                    Some(&body),
                    attempt_start,
                );
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 524 / gateway timeout：上游边缘层超时，继续在本次请求内重试通常只会
            // 放大客户端等待时间和 Claude 端 Retrying 轮数；快速返回，让客户端下一次调用
            // 重新建连。
            if status.as_u16() == 524 || endpoint.is_gateway_timeout(&body) {
                tracing::warn!("API 请求失败（上游网关超时，不重试）: {} {}", status, body);
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::TRANSIENT,
                    Some(&body),
                    attempt_start,
                );
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 429 + USER_REQUEST_RATE_EXCEEDED = 单账号请求速率超限。
            // 必须在下方通用瞬态分支之前拦截：通用分支会 sleep 后重试，而重试时
            // acquire_context 按在途数排序、并不排除刚超限的账号，常把同一账号重复选中
            // （实测每账号 >6 req/min 时 429 显著上升、平均重试 4 次、白占槽 ~26s）。
            // 这里对该账号施加短冷却（不计失败统计），让本次重试 acquire_context 自然
            // 跳过它、切换到有余量的账号；速率窗口在冷却后恢复。
            if status.as_u16() == 429 && endpoint.is_user_rate_limited(&body) {
                let cooldown_secs = self.token_manager.config().rate_limit_cooldown_secs.max(1);
                let cooldown = std::time::Duration::from_secs(cooldown_secs);
                let remaining = self.token_manager.report_rate_limited(ctx.id, cooldown);
                tracing::warn!(
                    "API 请求失败（账号 #{} 请求速率超限，短冷却 {}s 并切换，尝试 {}/{}，剩余可用 {}）: {}",
                    ctx.id,
                    cooldown_secs,
                    attempt + 1,
                    max_retries,
                    remaining,
                    body
                );
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(429),
                    outcome::RATE_LIMITED,
                    Some(&body),
                    attempt_start,
                );
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败（账号 #{} 请求速率超限）: {} {}",
                    api_type,
                    ctx.id,
                    status,
                    body
                ));
                // 所有账号都在冷却时，短暂等待让最早的冷却到期，而非立即失败
                // （速率冷却很短，等待远比放弃划算）。
                if remaining == 0 && attempt + 1 < max_retries {
                    sleep(cooldown).await;
                }
                continue;
            }

            // 429/408/5xx - 瞬态上游错误：重试但不禁用或切换凭据
            // （避免 429 high traffic / 502 high load 等瞬态错误把所有凭据锁死）
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                tracing::warn!(
                    "API 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::TRANSIENT,
                    Some(&body),
                    attempt_start,
                );
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                if attempt + 1 < max_retries {
                    // 429 限流用更长退避给账号配额恢复时间；408/5xx 仍用通用快速退避
                    let mut delay = if status.as_u16() == 429 {
                        Self::retry_delay_throttle(attempt)
                    } else {
                        Self::retry_delay(attempt)
                    };
                    // 事故熔断（A4）：全池错误率飙高时判定上游全局事故，瞬态错误 3× 退避，
                    // 避免重试风暴把错误率进一步放大、把稀缺并发槽耗在注定失败的重试上。
                    let threshold = config.circuit_breaker_threshold;
                    if threshold < 1.0 && self.token_manager.pool_ewma_error() > threshold {
                        delay *= 3;
                        tracing::warn!(
                            "全池错误率超阈值（>{:.2}），判定上游事故，瞬态错误退避 3×（{}ms）",
                            threshold,
                            delay.as_millis()
                        );
                    }
                    sleep(delay).await;
                }
                continue;
            }

            // 其他 4xx - 通常为请求/配置问题：直接返回，不计入凭据失败
            if status.is_client_error() {
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::BAD_REQUEST,
                    Some(&body),
                    attempt_start,
                );
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 兜底：当作可重试的瞬态错误处理（不切换凭据）
            tracing::warn!(
                "API 请求失败（未知错误，尝试 {}/{}）: {} {}",
                attempt + 1,
                max_retries,
                status,
                body
            );
            Self::emit_attempt(
                sink,
                attempt,
                ctx.id,
                endpoint_name,
                Some(status.as_u16()),
                outcome::UNKNOWN,
                Some(&body),
                attempt_start,
            );
            last_error = Some(anyhow::anyhow!(
                "{} API 请求失败: {} {}",
                api_type,
                status,
                body
            ));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        // 所有重试都失败
        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!(
                "{} API 请求失败：已达到最大重试次数（{}次）",
                api_type,
                max_retries
            )
        }))
    }

    /// 向 trace sink 上报一跳结果（sink 为 None 时无开销）
    #[allow(clippy::too_many_arguments)]
    fn emit_attempt(
        sink: Option<&dyn TraceSink>,
        attempt: usize,
        credential_id: u64,
        endpoint: &str,
        http_status: Option<u16>,
        outcome: &str,
        error_body: Option<&str>,
        started: Instant,
    ) {
        let Some(sink) = sink else { return };
        sink.on_attempt(TraceAttempt {
            attempt: attempt as u32,
            credential_id,
            endpoint: endpoint.to_string(),
            http_status,
            outcome: outcome.to_string(),
            error_snippet: error_body.and_then(truncate_snippet),
            duration_ms: started.elapsed().as_millis() as u64,
        });
    }

    /// 从请求体中提取模型信息
    ///
    /// 尝试解析 JSON 请求体，提取 conversationState.currentMessage.userInputMessage.modelId
    fn extract_model_from_request(request_body: &str) -> Option<String> {
        use serde_json::Value;

        let json: Value = serde_json::from_str(request_body).ok()?;

        json.get("conversationState")?
            .get("currentMessage")?
            .get("userInputMessage")?
            .get("modelId")?
            .as_str()
            .map(|s| s.to_string())
    }

    fn retry_delay(attempt: usize) -> Duration {
        // 指数退避 + 少量抖动，避免上游抖动时放大故障
        const BASE_MS: u64 = 200;
        const MAX_MS: u64 = 2_000;
        let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(6) as u32));
        let backoff = exp.min(MAX_MS);
        let jitter_max = (backoff / 4).max(1);
        let jitter = fastrand::u64(0..=jitter_max);
        Duration::from_millis(backoff.saturating_add(jitter))
    }

    /// 429 限流专用退避：比通用退避更长。
    ///
    /// 上游 429（SERVICE_REQUEST_RATE_EXCEEDED）是账号级速率配额耗尽，需要更长
    /// 时间恢复；用通用的 ≤2s 快速退避只会让请求在配额恢复前反复撞墙、持续触顶。
    /// 这里 base 1s、封顶 8s，给账号配额留出恢复窗口。
    fn retry_delay_throttle(attempt: usize) -> Duration {
        const BASE_MS: u64 = 1_000;
        const MAX_MS: u64 = 8_000;
        let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(6) as u32));
        let backoff = exp.min(MAX_MS);
        let jitter_max = (backoff / 4).max(1);
        let jitter = fastrand::u64(0..=jitter_max);
        Duration::from_millis(backoff.saturating_add(jitter))
    }
}
