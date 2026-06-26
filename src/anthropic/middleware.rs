//! Anthropic API 中间件

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};

use crate::admin::client_keys::SharedClientKeyManager;
use crate::admin::trace_db::{SharedTraceStore, TraceKeySource};
use crate::admin::usage_stats::{SharedAggregator, SharedRecorder};
use crate::common::auth;
use crate::kiro::provider::KiroProvider;

use super::cache_metering::SharedCacheMeter;
use super::types::ErrorResponse;

/// 命中的鉴权上下文（注入到请求扩展，供 handler 记录用量）
#[derive(Clone, Debug)]
pub struct KeyContext {
    /// 命中的客户端 Key id
    pub key_id: u64,
    /// 该 Key 绑定的账号分组；None 表示未绑定，可使用全部账号
    pub group: Option<String>,
    /// 是否为该入口 Key 启用中转层 prompt cache。
    pub cache_enabled: bool,
    /// 是否为该入口 Key 启用历史上限（已结合 per-key 三态覆盖与全局默认的最终判定）。
    pub history_cap_enabled: bool,
    /// 是否为该入口 Key 启用快速模式（已结合 per-key 三态覆盖与全局默认的最终判定）。
    pub fast_mode_enabled: bool,
    /// 命中的入口 Key 类型。
    pub key_source: TraceKeySource,
}

/// 应用共享状态
#[derive(Clone)]
pub struct AppState {
    /// Kiro Provider（可选，用于实际 API 调用）
    /// 内部使用 MultiTokenManager，已支持线程安全的多凭据管理
    pub kiro_provider: Option<Arc<KiroProvider>>,
    /// 是否开启非流式响应的 thinking 块提取
    pub extract_thinking: bool,
    /// 客户端 Key 管理器（可选，未启用 Admin 时为 None）
    pub client_keys: Option<SharedClientKeyManager>,
    /// 用量日志记录器
    pub usage_recorder: Option<SharedRecorder>,
    /// 用量聚合器
    pub usage_aggregator: Option<SharedAggregator>,
    /// 中转层缓存计量（基于 cache_control 断点的内存缓存）
    pub cache_meter: Option<SharedCacheMeter>,
    /// 请求链路追踪存储（SQLite，可选）
    pub trace_store: Option<SharedTraceStore>,
    /// 历史上限全局配置。`enabled` 是全局默认；per-key 三态可覆盖。
    pub history_cap: HistoryCapState,
    /// 快速模式全局配置快照（来自 config.fast_mode_*）。
    pub fast_mode: FastModeState,
}

/// AppState 内的快速模式配置快照。
#[derive(Clone, Copy, Debug)]
pub struct FastModeState {
    /// 全局默认开关（per-key `fastMode` 为 None 时采用）。
    pub default_enabled: bool,
    /// 快速模式下历史裁剪的字节预算（比普通 cap 更激进）。
    pub history_cap_max_bytes: usize,
}

impl Default for FastModeState {
    fn default() -> Self {
        Self {
            default_enabled: false,
            history_cap_max_bytes: 400_000,
        }
    }
}

impl FastModeState {
    /// 快速模式下的历史裁剪配置（更激进预算，head 保留 1 轮）。
    pub fn to_cap_config(self) -> super::converter::HistoryCapConfig {
        super::converter::HistoryCapConfig {
            max_bytes: self.history_cap_max_bytes,
            head_turns: 1,
        }
    }
}

/// AppState 内的历史上限配置快照（来自 config.history_cap_*）。
#[derive(Clone, Copy, Debug)]
pub struct HistoryCapState {
    /// 全局默认开关（per-key `historyCap` 为 None 时采用）。
    pub default_enabled: bool,
    /// 字节预算上限。
    pub max_bytes: usize,
    /// 始终保留的头部条数。
    pub head_turns: usize,
}

impl Default for HistoryCapState {
    fn default() -> Self {
        Self {
            default_enabled: false,
            max_bytes: 900_000,
            head_turns: 1,
        }
    }
}

impl HistoryCapState {
    /// 转换为转换器侧的运行时配置。
    pub fn to_cap_config(self) -> super::converter::HistoryCapConfig {
        super::converter::HistoryCapConfig {
            max_bytes: self.max_bytes,
            head_turns: self.head_turns,
        }
    }
}

impl AppState {
    /// 创建新的应用状态（不含 client_keys 的基础构造，供嵌入 / 测试使用）
    #[allow(dead_code)]
    pub fn new(extract_thinking: bool) -> Self {
        Self {
            kiro_provider: None,
            extract_thinking,
            client_keys: None,
            usage_recorder: None,
            usage_aggregator: None,
            cache_meter: None,
            trace_store: None,
            history_cap: HistoryCapState::default(),
            fast_mode: FastModeState::default(),
        }
    }

    /// 设置 KiroProvider
    pub fn with_kiro_provider(mut self, provider: KiroProvider) -> Self {
        self.kiro_provider = Some(Arc::new(provider));
        self
    }

    /// 注入历史上限全局配置
    pub fn with_history_cap(mut self, cap: HistoryCapState) -> Self {
        self.history_cap = cap;
        self
    }

    /// 注入快速模式全局配置
    pub fn with_fast_mode(mut self, fm: FastModeState) -> Self {
        self.fast_mode = fm;
        self
    }

    /// 注入用量记录组件
    pub fn with_usage(
        mut self,
        client_keys: Option<SharedClientKeyManager>,
        recorder: Option<SharedRecorder>,
        aggregator: Option<SharedAggregator>,
    ) -> Self {
        self.client_keys = client_keys;
        self.usage_recorder = recorder;
        self.usage_aggregator = aggregator;
        self
    }

    /// 注入缓存计量器
    pub fn with_cache_meter(mut self, cache: Option<SharedCacheMeter>) -> Self {
        self.cache_meter = cache;
        self
    }

    /// 注入链路追踪存储
    pub fn with_trace_store(mut self, store: Option<SharedTraceStore>) -> Self {
        self.trace_store = store;
        self
    }
}

/// API Key 认证中间件
///
/// 鉴权顺序：master apiKey → 客户端 Key（`csk_*`）。命中后向请求扩展注入
/// [`KeyContext`]，供 handler 记录用量时使用。
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let presented = match auth::extract_api_key(&request) {
        Some(k) => k,
        None => {
            let error = ErrorResponse::authentication_error();
            return (StatusCode::UNAUTHORIZED, Json(error)).into_response();
        }
    };

    // 所有 Key 统一走客户端 Key 管理器校验
    if let Some(mgr) = &state.client_keys {
        if let Some(id) = mgr.verify_and_touch(&presented) {
            let group = mgr.group_of(id);
            let cache_enabled = mgr.cache_enabled_of(id);
            // 三态解析：per-key 覆盖优先，否则随全局默认。
            let history_cap_enabled = mgr
                .history_cap_of(id)
                .unwrap_or(state.history_cap.default_enabled);
            let fast_mode_enabled = mgr
                .fast_mode_of(id)
                .unwrap_or(state.fast_mode.default_enabled);
            request.extensions_mut().insert(KeyContext {
                key_id: id,
                group,
                cache_enabled,
                history_cap_enabled,
                fast_mode_enabled,
                key_source: TraceKeySource::ClientKey,
            });
            return next.run(request).await;
        }
    }

    let error = ErrorResponse::authentication_error();
    (StatusCode::UNAUTHORIZED, Json(error)).into_response()
}

/// CORS 中间件层
///
/// **安全说明**：当前配置允许所有来源（Any），这是为了支持公开 API 服务。
/// 如果需要更严格的安全控制，请根据实际需求配置具体的允许来源、方法和头信息。
///
/// # 配置说明
/// - `allow_origin(Any)`: 允许任何来源的请求
/// - `allow_methods(Any)`: 允许任何 HTTP 方法
/// - `allow_headers(Any)`: 允许任何请求头
pub fn cors_layer() -> tower_http::cors::CorsLayer {
    use tower_http::cors::{Any, CorsLayer};

    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
}
