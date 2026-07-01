use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TlsBackend {
    Rustls,
    NativeTls,
}

impl Default for TlsBackend {
    fn default() -> Self {
        Self::Rustls
    }
}

/// KNA 应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,

    #[serde(default = "default_port")]
    pub port: u16,

    /// OAuth 回调公网地址（远程部署时配置）。
    ///
    /// 留空：Social 登录在服务端本机启动临时回调端口（`http://127.0.0.1:{port}`），
    /// 仅本机浏览器可达。
    /// 配置后（如 `https://example.com/api/admin/auth/callback`）：OAuth `redirect_uri`
    /// 改用此地址，浏览器授权后落到 `{callbackBaseUrl}/oauth/callback`，
    /// 由本服务的公网回调路由接收 `code` 并自动完成登录，适配 Docker / VPS / Render 等远程部署。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_base_url: Option<String>,

    #[serde(default = "default_region")]
    pub region: String,

    /// Auth Region（用于 Token 刷新），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,

    /// API Region（用于 API 请求），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,

    #[serde(default = "default_kiro_version")]
    pub kiro_version: String,

    #[serde(default)]
    pub machine_id: Option<String>,

    #[serde(default)]
    pub api_key: Option<String>,

    #[serde(default = "default_system_version")]
    pub system_version: String,

    #[serde(default = "default_node_version")]
    pub node_version: String,

    #[serde(default = "default_tls_backend")]
    pub tls_backend: TlsBackend,

    /// 外部 count_tokens API 地址（可选）
    #[serde(default)]
    pub count_tokens_api_url: Option<String>,

    /// count_tokens API 密钥（可选）
    #[serde(default)]
    pub count_tokens_api_key: Option<String>,

    /// count_tokens API 认证类型（可选，"x-api-key" 或 "bearer"，默认 "x-api-key"）
    #[serde(default = "default_count_tokens_auth_type")]
    pub count_tokens_auth_type: String,

    /// HTTP 代理地址（可选）
    /// 支持格式: http://host:port, https://host:port, socks5://host:port
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// 代理认证用户名（可选）
    #[serde(default)]
    pub proxy_username: Option<String>,

    /// 代理认证密码（可选）
    #[serde(default)]
    pub proxy_password: Option<String>,

    /// Admin API 密钥（可选，启用 Admin API 功能）
    #[serde(default)]
    pub admin_api_key: Option<String>,

    /// 上一次成功更新前正在运行的版本号，用于在前端展示「回退到 vX.Y.Z」按钮。
    /// 实际回退动作通过 `<exe>.backup` 文件完成，无需访问网络。
    #[serde(default)]
    pub update_previous_version: Option<String>,

    /// GitHub Personal Access Token（可选）。设置后 GitHub Releases 接口会带上
    /// `Authorization: Bearer <token>`，把限流从匿名 60/h 提到认证 5000/h。
    /// 仅需 `public_repo` 读取权限即可。
    #[serde(default)]
    pub github_token: Option<String>,

    /// 上一次成功完成在线更新的时间（RFC3339）。前端用于显示「上次更新于 …」。
    #[serde(default)]
    pub update_last_applied_at: Option<String>,

    /// 是否启用无人值守自动更新。开启后服务会在每天的 `update_auto_apply_time`
    /// 时刻检查 GitHub Releases，发现新版本即自动下载二进制并替换重启。
    #[serde(default)]
    pub update_auto_apply: bool,

    /// 自动更新的每日触发时间（本地时区，`HH:MM` 24 小时制）。
    /// 默认 03:00 凌晨执行，对在线服务影响最小。
    #[serde(default = "default_update_auto_apply_time")]
    pub update_auto_apply_time: String,

    /// 负载均衡模式（"priority" 或 "balanced"）
    #[serde(default = "default_load_balancing_mode")]
    pub load_balancing_mode: String,

    /// 账号级 429 风控触发时是否对当前凭据进入冷却并故障转移（默认 true）。
    ///
    /// 关闭后：429 + suspicious activity 仍按普通瞬态错误重试，不切换凭据。
    /// 开启后：识别到 suspicious activity 字符串时，把当前凭据冷却 `account_throttle_cooldown_secs` 秒，
    /// 立即切换到下一个可用凭据。
    #[serde(default = "default_account_throttle_failover")]
    pub account_throttle_failover: bool,

    /// 账号级风控冷却时长（秒，默认 1800 = 30 分钟）。
    #[serde(default = "default_account_throttle_cooldown_secs")]
    pub account_throttle_cooldown_secs: u64,

    /// 单账号请求速率超限（429 USER_REQUEST_RATE_EXCEEDED）后的短冷却时长（秒，默认 5）。
    ///
    /// 命中 per-user 速率限制时对该账号施加此短冷却，使本次重试自然切换到其它账号，
    /// 避免反复命中同一速率超限账号、白白浪费重试预算与并发槽。冷却很短，速率窗口
    /// 恢复后该账号即重新参与调度；不计入失败统计、不会推动禁用。
    #[serde(default = "default_rate_limit_cooldown_secs")]
    pub rate_limit_cooldown_secs: u64,

    /// 单账号最大并发请求数（默认 2）。
    ///
    /// 用于避免同一账号被无限并发打爆；不是串行，多个账号的整体并发约为
    /// `账号数 * account_max_concurrency`。
    #[serde(default = "default_account_max_concurrency")]
    pub account_max_concurrency: usize,

    /// 等待账号并发槽位的最长时间（秒，默认 30）。
    ///
    /// 所有匹配账号都满载时，请求会等待槽位释放；超时后返回错误，避免无限挂起。
    #[serde(default = "default_account_acquire_timeout_secs")]
    pub account_acquire_timeout_secs: u64,

    /// 是否开启非流式响应的 thinking 块提取（默认 true）
    ///
    /// 启用后，非流式响应中的 `<thinking>...</thinking>` 标签会被解析为
    /// 独立的 `{"type": "thinking", ...}` 内容块,与流式响应行为一致。
    #[serde(default = "default_extract_thinking")]
    pub extract_thinking: bool,

    /// 默认端点名称（凭据未显式指定 endpoint 时使用，默认 "ide"）
    #[serde(default = "default_endpoint")]
    pub default_endpoint: String,

    /// Preferred endpoint name when endpoint fallback is enabled.
    ///
    /// If unset, fallback order starts from `default_endpoint`. Per-credential
    /// `endpoint` still has highest priority.
    #[serde(default)]
    pub preferred_endpoint: Option<String>,

    /// Whether to try Kiro-Go-compatible fallback endpoints on the same credential.
    ///
    /// Default false preserves existing single-endpoint behavior.
    #[serde(default = "default_endpoint_fallback")]
    pub endpoint_fallback: bool,

    /// 是否启用请求链路追踪（写 traces.db）。默认 true。
    ///
    /// 关闭后：不再写入 trace 记录、不走 TraceSink，但 `GET /api/admin/traces`
    /// 仍可查询历史已存记录。适合隐私敏感或磁盘紧张的场景。
    #[serde(default = "default_trace_enabled")]
    pub trace_enabled: bool,

    /// 请求链路追踪记录保留天数（默认 7）。后台任务每天清理超期记录。
    #[serde(default = "default_trace_retention_days")]
    pub trace_retention_days: u32,

    /// 请求用量日志（usage_log.*.jsonl + 聚合桶）保留天数（默认 31）。
    #[serde(default = "default_usage_log_retention_days")]
    pub usage_log_retention_days: u32,

    /// 中转层 prompt cache（CacheMeter）的条目容量上限（默认 131072）。
    ///
    /// 这是缓存命中率的关键旋钮：表满后按 LRU 淘汰，容量须 ≥ 写入速率 × TTL，
    /// 否则历史前缀会在跨轮复用前被挤掉，长会话表现为 cache_read 恒为 0、
    /// 每轮重建整段。高并发生产可按 `峰值 req/min × 每轮段数 × (300s/60)` 估算下限。
    /// 运行时会被 clamp 到 `>= 256`。每条约 80B，131072 满载约 10MiB。
    #[serde(default = "default_cache_meter_capacity")]
    pub cache_meter_capacity: usize,

    /// 账号并发槽获取是否阻塞等待。默认 false（非阻塞快速失败）。
    ///
    /// false（默认）：所有匹配凭据的并发槽都满时，acquire 立即返回"池忙"，由 provider
    /// 的重试链（退避后换 attempt）兜底——打 429 重试链这一最大瓶颈，避免单请求在高峰期
    /// 死等数秒。true：回退到旧行为，等待最多 `account_acquire_timeout_secs`(默认 30s)
    /// 直到任一账号释放槽位，适用于账号极少、宁可排队也不要"池忙"报错的部署。
    #[serde(default)]
    pub account_acquire_blocking: bool,

    /// `/cc/v1/messages` usage-gated streaming 开关（A1 首包优化）。
    ///
    /// 开启（默认）：`/cc` 流式只缓冲到能确定 `message_start.usage.input_tokens`
    /// 的那一刻（收到 contextUsageEvent 或首个可见内容事件）即放闸边收边发，
    /// 大幅降低 Claude Code 首包延迟，且保持 SSE 顺序与 usage 兼容。
    /// 关闭：回退到原全缓冲行为（等整条上游流结束才一次性下发）。
    #[serde(default = "default_usage_gated_streaming_enabled")]
    pub usage_gated_streaming_enabled: bool,

    /// 全局流式连接复用开关（A2）。默认 false（保守：每条流新连接，杜绝偶发断流）。
    ///
    /// true：流式请求复用连接池（走 `client_for`，pool_idle=15s < ALB 60s），
    /// 省每请求 ~100-200ms 的 TCP+TLS 握手，高并发下显著降首字节延迟。代价是上游 ALB 若在
    /// 长 prefill 静默期掐断复用连接，可能偶发"断流"（仅首字节前可由重试兜底）。
    #[serde(default)]
    pub stream_conn_reuse_enabled: bool,

    /// 事故熔断阈值（A4，0.0~1.0，默认 0.5）。
    ///
    /// 全池近期错误率 EWMA(`pool_ewma_error`)超过此值时,判定为上游全局性事故(如 hr20:
    /// QPS 2640、错误率 53.5%、并发 375 > 容量 275),provider 对瞬态错误(408/429/5xx)
    /// 改用 3× 退避,避免重试风暴把错误率进一步放大、把稀缺并发槽全耗在注定失败的重试上。
    /// 设为 >= 1.0 即关闭熔断(永不触发)。不影响非事故期的正常重试节奏。
    #[serde(default = "default_circuit_breaker_threshold")]
    pub circuit_breaker_threshold: f64,

    /// 大请求(≥128K token)调度排序惩罚（A4，整数千分比，默认 500）。
    ///
    /// Long 档请求按命中账号的在途数 × (此值/1000) 加排序惩罚,使其优先选当前更空闲的账号、
    /// 避免多个大请求堆到同一账号拖垮该账号首字节。仅改排序偏好,不改并发上限、不硬阻塞、
    /// 不破坏 P2C/priority 语义。设为 0 即关闭(退化到纯 effective_load 排序)。
    #[serde(default = "default_large_request_rank_penalty")]
    pub large_request_rank_penalty: usize,

    /// 端点特定的配置
    ///
    /// 键为端点名（如 "ide" / "cli"），值为该端点自由定义的参数对象。
    /// 未在此表出现的端点沿用实现内置默认值。
    #[serde(default)]
    pub endpoints: HashMap<String, serde_json::Value>,

    /// 配置文件路径（运行时元数据，不写入 JSON）
    #[serde(skip)]
    config_path: Option<PathBuf>,
}

/// serde 默认值：`usage_gated_streaming_enabled` 默认开启。
fn default_usage_gated_streaming_enabled() -> bool {
    true
}

/// serde 默认值：事故熔断阈值（全池错误率 EWMA 超此值则瞬态错误 3× 退避）。
fn default_circuit_breaker_threshold() -> f64 {
    0.5
}

/// serde 默认值：大请求调度排序惩罚（整数千分比，按在途数缩放）。
fn default_large_request_rank_penalty() -> usize {
    500
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_kiro_version() -> String {
    "2.3.0".to_string()
}

fn default_system_version() -> String {
    "macos".to_string()
}

fn default_node_version() -> String {
    "22.22.0".to_string()
}

fn default_count_tokens_auth_type() -> String {
    "x-api-key".to_string()
}

fn default_tls_backend() -> TlsBackend {
    TlsBackend::Rustls
}

fn default_load_balancing_mode() -> String {
    "priority".to_string()
}

fn default_account_throttle_failover() -> bool {
    true
}

fn default_account_throttle_cooldown_secs() -> u64 {
    30 * 60
}

fn default_rate_limit_cooldown_secs() -> u64 {
    5
}

fn default_account_max_concurrency() -> usize {
    2
}

fn default_account_acquire_timeout_secs() -> u64 {
    30
}

fn default_update_auto_apply_time() -> String {
    "03:00".to_string()
}

fn default_extract_thinking() -> bool {
    true
}

fn default_endpoint() -> String {
    crate::kiro::endpoint::ide::IDE_ENDPOINT_NAME.to_string()
}

fn default_endpoint_fallback() -> bool {
    false
}

fn default_trace_enabled() -> bool {
    true
}

fn default_trace_retention_days() -> u32 {
    7
}

fn default_usage_log_retention_days() -> u32 {
    31
}

fn default_cache_meter_capacity() -> usize {
    131072
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            callback_base_url: None,
            region: default_region(),
            auth_region: None,
            api_region: None,
            kiro_version: default_kiro_version(),
            machine_id: None,
            api_key: None,
            system_version: default_system_version(),
            node_version: default_node_version(),
            tls_backend: default_tls_backend(),
            count_tokens_api_url: None,
            count_tokens_api_key: None,
            count_tokens_auth_type: default_count_tokens_auth_type(),
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            admin_api_key: None,
            update_previous_version: None,
            github_token: None,
            update_last_applied_at: None,
            update_auto_apply: false,
            update_auto_apply_time: default_update_auto_apply_time(),
            load_balancing_mode: default_load_balancing_mode(),
            account_throttle_failover: default_account_throttle_failover(),
            account_throttle_cooldown_secs: default_account_throttle_cooldown_secs(),
            rate_limit_cooldown_secs: default_rate_limit_cooldown_secs(),
            account_max_concurrency: default_account_max_concurrency(),
            account_acquire_timeout_secs: default_account_acquire_timeout_secs(),
            extract_thinking: default_extract_thinking(),
            default_endpoint: default_endpoint(),
            preferred_endpoint: None,
            endpoint_fallback: default_endpoint_fallback(),
            trace_enabled: default_trace_enabled(),
            trace_retention_days: default_trace_retention_days(),
            usage_log_retention_days: default_usage_log_retention_days(),
            cache_meter_capacity: default_cache_meter_capacity(),
            account_acquire_blocking: false,
            usage_gated_streaming_enabled: default_usage_gated_streaming_enabled(),
            stream_conn_reuse_enabled: false,
            circuit_breaker_threshold: default_circuit_breaker_threshold(),
            large_request_rank_penalty: default_large_request_rank_penalty(),
            endpoints: HashMap::new(),
            config_path: None,
        }
    }
}

impl Config {
    /// 获取默认配置文件路径
    pub fn default_config_path() -> &'static str {
        "config.json"
    }

    /// 获取有效的 Auth Region（用于 Token 刷新）
    /// 优先使用 auth_region，未配置时回退到 region
    pub fn effective_auth_region(&self) -> &str {
        self.auth_region.as_deref().unwrap_or(&self.region)
    }

    /// 获取有效的 API Region（用于 API 请求）
    /// 优先使用 api_region，未配置时回退到 region
    pub fn effective_api_region(&self) -> &str {
        self.api_region.as_deref().unwrap_or(&self.region)
    }

    /// 从文件加载配置
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            // 配置文件不存在，返回默认配置
            let mut config = Self::default();
            config.config_path = Some(path.to_path_buf());
            return Ok(config);
        }

        let content = fs::read_to_string(path)?;
        let mut config: Config = serde_json::from_str(&content)?;
        config.config_path = Some(path.to_path_buf());

        // 用户手工把字符串字段清空（如 `"updateAutoApplyTime": ""`）时，serde 默认值不会
        // 介入；这里把"看起来像空"的关键字段回退到默认值，避免后续业务用到
        // 空字符串导致难以诊断的错误。
        if config.update_auto_apply_time.trim().is_empty() {
            config.update_auto_apply_time = default_update_auto_apply_time();
        }

        Ok(config)
    }

    /// 获取配置文件路径（如果有）
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// 将当前配置写回原始配置文件
    pub fn save(&self) -> anyhow::Result<()> {
        let path = self
            .config_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("配置文件路径未知，无法保存配置"))?;

        let content = serde_json::to_string_pretty(self).context("序列化配置失败")?;
        fs::write(path, content)
            .with_context(|| format!("写入配置文件失败: {}", path.display()))?;
        Ok(())
    }
}
