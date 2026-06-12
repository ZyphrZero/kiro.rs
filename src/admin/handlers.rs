//! Admin API HTTP 处理器

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::{Datelike, Duration, Local, NaiveDate, TimeZone};

use super::{
    client_keys::mask_client_key,
    middleware::AdminState,
    trace_db::TraceQuery,
    types::{
        AddCredentialRequest, AddProxyRequest, AssignProxyRequest, AssignRoundRobinRequest,
        BatchAddProxyRequest, ClientKeyItem, ClientKeysResponse, CompleteSocialLoginRequest,
        CreateClientKeyRequest, CreateClientKeyResponse, GlobalProxyResponse,
        SetAccountThrottleConfigRequest, SetDisabledRequest, SetGlobalProxyRequest,
        SetLoadBalancingModeRequest, SetLogGovernanceConfigRequest, SetPriorityRequest,
        SetUpdateConfigRequest, StartIdcLoginRequest, StartSocialLoginRequest, SuccessResponse,
        UpdateAdminKeyRequest, UpdateClientKeyRequest, UpdateCredentialRequest,
        UpdateRefreshTokenRequest,
    },
    usage_stats::{Range, StatsGranularity, StatsQueryWindow},
};

// Path 元组提取：(credential_id, session_id)
type CredSessionPath = (u64, String);

/// GET /api/admin/credentials
/// 获取所有凭据状态
pub async fn get_all_credentials(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.get_all_credentials();
    Json(response)
}

/// GET /api/admin/credentials/export
/// 导出凭据为兼容 JSON（含 refreshToken 等敏感字段）
///
/// 可选 query 参数 `ids`（逗号分隔）限定导出哪些凭据；省略则导出全部。
pub async fn export_credentials(
    State(state): State<AdminState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let id_filter: Option<std::collections::HashSet<u64>> = params
        .get("ids")
        .map(|raw| {
            raw.split(',')
                .filter_map(|s| {
                    let t = s.trim();
                    if t.is_empty() {
                        None
                    } else {
                        t.parse::<u64>().ok()
                    }
                })
                .collect::<std::collections::HashSet<u64>>()
        })
        .filter(|s| !s.is_empty());

    let response = state.service.export_credentials(id_filter.as_ref());
    Json(response)
}

/// POST /api/admin/credentials/:id/disabled
/// 设置凭据禁用状态
pub async fn set_credential_disabled(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetDisabledRequest>,
) -> impl IntoResponse {
    match state.service.set_disabled(id, payload.disabled) {
        Ok(_) => {
            let action = if payload.disabled { "禁用" } else { "启用" };
            Json(SuccessResponse::new(format!("凭据 #{} 已{}", id, action))).into_response()
        }
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/priority
/// 设置凭据优先级
pub async fn set_credential_priority(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetPriorityRequest>,
) -> impl IntoResponse {
    match state.service.set_priority(id, payload.priority) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 优先级已设置为 {}",
            id, payload.priority
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/reset
/// 重置失败计数并重新启用
pub async fn reset_failure_count(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.reset_and_enable(id) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 失败计数已重置并重新启用",
            id
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/clear-throttle
/// 手动解除凭据的账号级风控冷却
pub async fn clear_throttle(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.clear_throttle(id) {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 风控冷却已解除", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/credentials/:id/balance
/// 获取指定凭据的余额
pub async fn get_credential_balance(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.get_balance(id).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/credentials/:id/models
/// 获取指定凭据当前可用的模型列表（按需实时查询上游）
pub async fn get_credential_models(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.get_available_models(id).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/disable-quota-exceeded
/// 一键禁用所有"已超额"凭据（remaining ≤ 0 或 usage_percentage ≥ 100）
pub async fn disable_quota_exceeded(State(state): State<AdminState>) -> impl IntoResponse {
    let result = state.service.disable_quota_exceeded();
    Json(result).into_response()
}

/// POST /api/admin/credentials/:id/overage
/// 开启或关闭指定凭据的超额能力
pub async fn set_credential_overage(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<super::types::SetOverageRequest>,
) -> impl IntoResponse {
    match state.service.set_overage(id, payload.enabled).await {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} 已{}超额",
            id,
            if payload.enabled { "开启" } else { "关闭" }
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/overage/enable-all
/// 一键开启所有"可开启超额且当前未开启"凭据的超额（基于 balance_cache 判断）
pub async fn enable_overage_all(State(state): State<AdminState>) -> impl IntoResponse {
    let result = state.service.enable_overage_for_all_capable().await;
    Json(result).into_response()
}

/// POST /api/admin/credentials
/// 添加新凭据
pub async fn add_credential(
    State(state): State<AdminState>,
    Json(payload): Json<AddCredentialRequest>,
) -> impl IntoResponse {
    match state.service.add_credential(payload).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// DELETE /api/admin/credentials/:id
/// 删除凭据
pub async fn delete_credential(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.delete_credential(id) {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 已删除", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// PUT /api/admin/credentials/:id
/// 更新凭据可编辑字段（email、proxy 等）
pub async fn update_credential(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<UpdateCredentialRequest>,
) -> impl IntoResponse {
    match state.service.update_credential(id, payload) {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 已更新", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// PUT /api/admin/credentials/:id/refresh-token
/// 更新已禁用凭据的 refreshToken
pub async fn update_refresh_token(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<UpdateRefreshTokenRequest>,
) -> impl IntoResponse {
    match state.service.update_refresh_token(id, payload) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} refreshToken 已更新（当前仍为禁用状态，请手动启用）",
            id
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/refresh
/// 强制刷新凭据 Token
pub async fn force_refresh_token(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.force_refresh_token(id).await {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} Token 已强制刷新",
            id
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/reset-stats
/// 重置所有凭据的 success_count
pub async fn reset_all_success_count(State(state): State<AdminState>) -> impl IntoResponse {
    match state.service.reset_success_count(None) {
        Ok(count) => Json(SuccessResponse::new(format!(
            "已重置 {} 个凭据的 success_count",
            count
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/reset-stats
/// 重置指定凭据的 success_count
pub async fn reset_success_count(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.reset_success_count(Some(id)) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "凭据 #{} success_count 已重置",
            id
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/proxy-pool
/// 获取代理池列表
pub async fn get_proxy_pool(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.get_proxy_pool();
    Json(response)
}

/// POST /api/admin/proxy-pool
/// 添加代理到池中
pub async fn add_proxy(
    State(state): State<AdminState>,
    Json(payload): Json<AddProxyRequest>,
) -> impl IntoResponse {
    match state.service.add_proxy(payload.url, payload.label) {
        Ok(entry) => Json(entry).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/proxy-pool/batch
/// 批量添加代理
pub async fn batch_add_proxies(
    State(state): State<AdminState>,
    Json(payload): Json<BatchAddProxyRequest>,
) -> impl IntoResponse {
    let (added, errors) = state.service.batch_add_proxies(payload);
    Json(serde_json::json!({
        "added": added.len(),
        "errors": errors.len(),
        "proxies": added,
        "errorMessages": errors
    }))
}

/// DELETE /api/admin/proxy-pool/:id
/// 删除代理
pub async fn delete_proxy(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.delete_proxy(id) {
        Ok(_) => Json(SuccessResponse::new(format!("代理 #{} 已删除", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/proxy-pool/:id/enabled
/// 设置代理启用/禁用
pub async fn set_proxy_enabled(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<serde_json::Value>,
) -> impl IntoResponse {
    let enabled = payload
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    match state.service.set_proxy_enabled(id, enabled) {
        Ok(_) => Json(SuccessResponse::new(format!(
            "代理 #{} 已{}",
            id,
            if enabled { "启用" } else { "禁用" }
        )))
        .into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/proxy
/// 将代理池中的代理分配给凭据
pub async fn assign_proxy_to_credential(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<AssignProxyRequest>,
) -> impl IntoResponse {
    match state.service.assign_proxy_to_credential(id, payload) {
        Ok(_) => Json(SuccessResponse::new(format!("凭据 #{} 代理已更新", id))).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/proxy-pool/:id/check
/// 即时探测单个代理的连通性
pub async fn check_proxy(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    match state.service.check_proxy(id).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/proxy-pool/check-all
/// 触发全部代理的健康检查
pub async fn check_all_proxies(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.check_all_proxies().await)
}

/// POST /api/admin/proxy-pool/assign-round-robin
/// 将可用代理轮询批量分配给凭据
pub async fn assign_proxies_round_robin(
    State(state): State<AdminState>,
    Json(payload): Json<AssignRoundRobinRequest>,
) -> impl IntoResponse {
    match state
        .service
        .assign_proxies_round_robin(payload.credential_ids)
    {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/load-balancing
/// 获取负载均衡模式
pub async fn get_load_balancing_mode(State(state): State<AdminState>) -> impl IntoResponse {
    let response = state.service.get_load_balancing_mode();
    Json(response)
}

/// PUT /api/admin/config/load-balancing
/// 设置负载均衡模式
pub async fn set_load_balancing_mode(
    State(state): State<AdminState>,
    Json(payload): Json<SetLoadBalancingModeRequest>,
) -> impl IntoResponse {
    match state.service.set_load_balancing_mode(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/account-throttle
/// 获取账号级风控故障转移配置
pub async fn get_account_throttle_config(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_account_throttle_config())
}

/// PUT /api/admin/config/account-throttle
/// 更新账号级风控故障转移配置
pub async fn set_account_throttle_config(
    State(state): State<AdminState>,
    Json(payload): Json<SetAccountThrottleConfigRequest>,
) -> impl IntoResponse {
    match state.service.set_account_throttle_config(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/log-governance
/// 获取日志治理配置（trace 开关 / trace 保留 / usage 保留）
pub async fn get_log_governance_config(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_log_governance_config())
}

/// PUT /api/admin/config/log-governance
/// 更新日志治理配置（运行时生效 + 持久化 config.json）
pub async fn set_log_governance_config(
    State(state): State<AdminState>,
    Json(payload): Json<SetLogGovernanceConfigRequest>,
) -> impl IntoResponse {
    match state.service.set_log_governance_config(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/idc/start
/// 发起 IdC 设备授权登录
pub async fn start_idc_login(
    State(state): State<AdminState>,
    Json(payload): Json<StartIdcLoginRequest>,
) -> impl IntoResponse {
    match state.service.start_idc_login(payload).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/idc/poll/:session_id
/// 轮询 IdC 登录状态（由前端按 poll_interval 调用）
pub async fn poll_idc_login(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    match state.service.poll_idc_login(&session_id).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/social/start
/// 发起 Social 登录，返回 portal URL
pub async fn start_social_login(
    State(state): State<AdminState>,
    Json(payload): Json<StartSocialLoginRequest>,
) -> impl IntoResponse {
    match state.service.start_social_login(payload).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/social/poll/:session_id
/// 轮询 Social 登录状态
pub async fn poll_social_login(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    match state.service.poll_social_login(&session_id).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/auth/social/complete/:session_id
///
/// 远程访问场景下手动完成 Social 登录：
/// 用户从浏览器地址栏复制 OAuth 回调 URL，前端提取 code/state/login_option 后调用此接口。
pub async fn complete_social_login(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
    Json(payload): Json<CompleteSocialLoginRequest>,
) -> impl IntoResponse {
    match state
        .service
        .complete_social_login(
            &session_id,
            payload.code,
            payload.state,
            payload.login_option,
            payload.path,
        )
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/global-proxy
/// 获取当前全局代理配置
pub async fn get_global_proxy(State(state): State<AdminState>) -> impl IntoResponse {
    Json(GlobalProxyResponse {
        proxy_url: state.service.get_global_proxy(),
    })
}

/// PUT /api/admin/config/global-proxy
/// 设置或清除全局代理配置
pub async fn set_global_proxy(
    State(state): State<AdminState>,
    Json(payload): Json<SetGlobalProxyRequest>,
) -> impl IntoResponse {
    match state.service.set_global_proxy(payload.proxy_url) {
        Ok(_) => Json(SuccessResponse::new("全局代理已更新")).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/config/update
/// 获取在线更新配置（不回显 GitHub Token 明文）
pub async fn get_update_config(State(state): State<AdminState>) -> impl IntoResponse {
    Json(state.service.get_update_config())
}

/// PUT /api/admin/config/update
/// 设置在线更新配置
pub async fn set_update_config(
    State(state): State<AdminState>,
    Json(payload): Json<SetUpdateConfigRequest>,
) -> impl IntoResponse {
    match state.service.set_update_config(payload) {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/system/update/pull
/// 下载新版二进制并校验（不替换当前进程）
pub async fn pull_update_image(State(state): State<AdminState>) -> impl IntoResponse {
    match state.service.pull_update_image().await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/system/update/apply
/// 下载新版二进制、替换 exe，进程退出由容器重启策略接管
pub async fn apply_image_update(State(state): State<AdminState>) -> impl IntoResponse {
    match state.service.apply_image_update().await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/system/update/rollback
/// 用 `<exe>.backup` 还原可执行文件并退出进程
pub async fn rollback_image_update(State(state): State<AdminState>) -> impl IntoResponse {
    match state.service.rollback_image_update().await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// GET /api/admin/system/update/check?force=true
/// 查询 GitHub Releases 是否有新版本（带 30 分钟缓存）
pub async fn check_update(
    State(state): State<AdminState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let force = matches!(params.get("force").map(String::as_str), Some("true" | "1"));
    let info = state.service.check_update(force).await;
    Json(info).into_response()
}

/// POST /api/admin/system/update/rate-limit
/// 查询 GitHub API 当前限流配额（可附带 token 用于"保存前先验证"）
pub async fn check_rate_limit(
    State(state): State<AdminState>,
    payload: Option<Json<super::types::CheckRateLimitRequest>>,
) -> impl IntoResponse {
    let req = payload.map(|Json(p)| p).unwrap_or_default();
    let info = state.service.check_rate_limit(req).await;
    Json(info).into_response()
}

/// POST /api/admin/credentials/:id/relogin/social/start
/// 发起 Social 重新登录（更新已有凭据的 Token 而非创建新凭据）
pub async fn start_social_relogin(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<StartSocialLoginRequest>,
) -> impl IntoResponse {
    match state.service.start_social_relogin(id, payload).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/relogin/social/poll/:session_id
/// 轮询 Social 重新登录状态
pub async fn poll_social_relogin(
    State(state): State<AdminState>,
    Path((_, session_id)): Path<CredSessionPath>,
) -> impl IntoResponse {
    match state.service.poll_social_login(&session_id).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/relogin/social/complete/:session_id
/// 远程模式下手动完成 Social 重新登录
pub async fn complete_social_relogin(
    State(state): State<AdminState>,
    Path((_, session_id)): Path<CredSessionPath>,
    Json(payload): Json<CompleteSocialLoginRequest>,
) -> impl IntoResponse {
    match state
        .service
        .complete_social_login(
            &session_id,
            payload.code,
            payload.state,
            payload.login_option,
            payload.path,
        )
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/relogin/idc/start
/// 发起 IdC 重新登录（更新已有凭据的 Token 而非创建新凭据）
pub async fn start_idc_relogin(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<StartIdcLoginRequest>,
) -> impl IntoResponse {
    match state.service.start_idc_relogin(id, payload).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// POST /api/admin/credentials/:id/relogin/idc/poll/:session_id
/// 轮询 IdC 重新登录状态
pub async fn poll_idc_relogin(
    State(state): State<AdminState>,
    Path((_, session_id)): Path<CredSessionPath>,
) -> impl IntoResponse {
    match state.service.poll_idc_login(&session_id).await {
        Ok(response) => Json(response).into_response(),
        Err(e) => (e.status_code(), Json(e.into_response())).into_response(),
    }
}

/// PUT /api/admin/config/admin-key
/// 修改登录API密钥并持久化到配置文件
pub async fn update_admin_key(
    State(state): State<AdminState>,
    Json(payload): Json<UpdateAdminKeyRequest>,
) -> impl IntoResponse {
    use axum::http::StatusCode;
    let new_key = payload.new_key.trim().to_string();
    if new_key.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(super::types::AdminErrorResponse::invalid_request(
                "新登录API密钥不能为空",
            )),
        )
            .into_response();
    }

    // 更新内存中的认证 key
    *state.admin_api_key.write() = new_key.clone();

    // 通过 service 持久化到 config.json（从磁盘加载最新后再写，避免覆盖其他字段）
    state.service.persist_admin_key(&new_key);

    Json(SuccessResponse::new("登录API密钥已更新")).into_response()
}

/// PUT /api/admin/config/api-key
/// 修改管理员API密钥并持久化到配置文件
///
/// 内存中的认证 key 与 anthropic 路由共享，调用后 `/v1/*` 立刻使用新 key。
pub async fn update_api_key(
    State(state): State<AdminState>,
    Json(payload): Json<UpdateAdminKeyRequest>,
) -> impl IntoResponse {
    use axum::http::StatusCode;
    let new_key = payload.new_key.trim().to_string();
    if new_key.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(super::types::AdminErrorResponse::invalid_request(
                "新管理员API密钥不能为空",
            )),
        )
            .into_response();
    }
    *state.api_key.write() = new_key.clone();
    state.service.persist_api_key(&new_key);
    Json(SuccessResponse::new("管理员API密钥已更新")).into_response()
}

// ============ 客户端 API Key 分发 ============

fn key_to_item(k: &super::client_keys::ClientKey) -> ClientKeyItem {
    ClientKeyItem {
        id: k.id,
        masked_key: mask_client_key(&k.key),
        name: k.name.clone(),
        description: k.description.clone(),
        disabled: k.disabled,
        created_at: k.created_at.clone(),
        last_used_at: k.last_used_at.clone(),
        total_calls: k.total_calls,
        total_input_tokens: k.total_input_tokens,
        total_output_tokens: k.total_output_tokens,
        total_cache_creation_tokens: k.total_cache_creation_tokens,
        total_cache_read_tokens: k.total_cache_read_tokens,
        group: k.group.clone(),
    }
}

/// GET /api/admin/client-keys
pub async fn list_client_keys(State(state): State<AdminState>) -> impl IntoResponse {
    let keys = state.client_keys.list();
    let items: Vec<ClientKeyItem> = keys.iter().map(key_to_item).collect();
    Json(ClientKeysResponse {
        total: items.len(),
        keys: items,
    })
}

/// POST /api/admin/client-keys
pub async fn create_client_key(
    State(state): State<AdminState>,
    Json(payload): Json<CreateClientKeyRequest>,
) -> impl IntoResponse {
    use axum::http::StatusCode;
    let name = payload.name.trim();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(super::types::AdminErrorResponse::invalid_request(
                "name 不能为空",
            )),
        )
            .into_response();
    }
    let entry = state.client_keys.create(
        name.to_string(),
        payload
            .description
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty()),
        payload
            .group
            .map(|g| g.trim().to_string())
            .filter(|g| !g.is_empty()),
    );
    Json(CreateClientKeyResponse {
        id: entry.id,
        key: entry.key,
        name: entry.name,
        created_at: entry.created_at,
    })
    .into_response()
}

/// DELETE /api/admin/client-keys/:id
pub async fn delete_client_key(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    use axum::http::StatusCode;
    if state.client_keys.delete(id) {
        Json(SuccessResponse::new(format!("Key #{} 已删除", id))).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(super::types::AdminErrorResponse::not_found(format!(
                "Key #{} 不存在",
                id
            ))),
        )
            .into_response()
    }
}

/// PUT /api/admin/client-keys/:id
pub async fn update_client_key(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<UpdateClientKeyRequest>,
) -> impl IntoResponse {
    use axum::http::StatusCode;
    let description = payload
        .description
        .map(|d| if d.is_empty() { None } else { Some(d) });
    let group = payload
        .group
        .map(|g| {
            let t = g.trim();
            if t.is_empty() { None } else { Some(t.to_string()) }
        });
    if state.client_keys.update_meta(id, payload.name, description, group) {
        Json(SuccessResponse::new(format!("Key #{} 已更新", id))).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(super::types::AdminErrorResponse::not_found(format!(
                "Key #{} 不存在",
                id
            ))),
        )
            .into_response()
    }
}

/// POST /api/admin/client-keys/:id/disabled
pub async fn set_client_key_disabled(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
    Json(payload): Json<SetDisabledRequest>,
) -> impl IntoResponse {
    use axum::http::StatusCode;
    if state.client_keys.set_disabled(id, payload.disabled) {
        let action = if payload.disabled { "禁用" } else { "启用" };
        Json(SuccessResponse::new(format!("Key #{} 已{}", id, action))).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(super::types::AdminErrorResponse::not_found(format!(
                "Key #{} 不存在",
                id
            ))),
        )
            .into_response()
    }
}

/// POST /api/admin/client-keys/:id/reset-stats
pub async fn reset_client_key_stats(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    use axum::http::StatusCode;
    if state.client_keys.reset_stats(id) {
        Json(SuccessResponse::new(format!("Key #{} 统计已重置", id))).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(super::types::AdminErrorResponse::not_found(format!(
                "Key #{} 不存在",
                id
            ))),
        )
            .into_response()
    }
}

/// POST /api/admin/client-keys/:id/rotate
///
/// 轮换 Key 值：旧明文立即失效，生成新明文返回（仅此一次可见）。
/// 保留 id/name/description/group/统计/disabled 不变，无需重新分组绑定。
pub async fn rotate_client_key(
    State(state): State<AdminState>,
    Path(id): Path<u64>,
) -> impl IntoResponse {
    use axum::http::StatusCode;
    match state.client_keys.rotate(id) {
        Some(entry) => Json(CreateClientKeyResponse {
            id: entry.id,
            key: entry.key,
            name: entry.name,
            created_at: entry.created_at,
        })
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(super::types::AdminErrorResponse::not_found(format!(
                "Key #{} 不存在",
                id
            ))),
        )
            .into_response(),
    }
}

// ============ 用量统计 ============

fn parse_range(params: &std::collections::HashMap<String, String>) -> Result<Range, String> {
    let Some(range) = params.get("range") else {
        return Err("range 必须是 24h、7d 或 30d".to_string());
    };
    Range::parse(range.as_str()).ok_or_else(|| "range 必须是 24h、7d 或 30d".to_string())
}

fn parse_key_id(params: &HashMap<String, String>) -> Result<Option<u64>, String> {
    match params.get("keyId") {
        Some(s) => s
            .parse::<u64>()
            .map(Some)
            .map_err(|_| "keyId 必须是数字".to_string()),
        None => Ok(None),
    }
}

/// 解析可选的分组筛选参数。空字符串视为不传。
fn parse_group_filter(params: &HashMap<String, String>) -> Option<String> {
    params
        .get("group")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 把 group 名转换为该分组下所有凭据 id 的白名单，给 UsageAggregator 用。
/// 返回 None 表示未指定分组（不过滤）；返回 Some(空集) 也是合法值——意味着该分组下没有凭据，
/// 所有 query 都会自然返回空结果。
fn group_to_cred_ids(
    state: &AdminState,
    group: Option<&str>,
) -> Option<std::collections::HashSet<u64>> {
    let g = group?;
    let snapshot = state.service.get_all_credentials();
    Some(
        snapshot
            .credentials
            .iter()
            .filter(|c| c.groups.iter().any(|cg| cg == g))
            .map(|c| c.id)
            .collect(),
    )
}

fn parse_granularity(params: &HashMap<String, String>) -> Result<StatsGranularity, String> {
    match params.get("granularity") {
        Some(s) => {
            StatsGranularity::parse(s).ok_or_else(|| "granularity 必须是 hour 或 day".to_string())
        }
        None => Err("granularity 必须是 hour 或 day".to_string()),
    }
}

fn parse_stats_window(params: &HashMap<String, String>) -> Result<StatsQueryWindow, String> {
    let granularity = parse_granularity(params)?;
    match (params.get("startDate"), params.get("endDate")) {
        (Some(start), Some(end)) => custom_stats_window(start, end, granularity),
        (None, None) => Ok(StatsQueryWindow::preset(parse_range(params)?, granularity)),
        _ => Err("startDate 和 endDate 必须同时提供".to_string()),
    }
}

fn custom_stats_window(
    start: &str,
    end: &str,
    granularity: StatsGranularity,
) -> Result<StatsQueryWindow, String> {
    let start_date = parse_stats_date(start, "startDate")?;
    let end_date = parse_stats_date(end, "endDate")?;
    if end_date < start_date {
        return Err("endDate 不能早于 startDate".to_string());
    }
    let start_ts = local_midnight_ts(start_date)?;
    let end_ts = local_midnight_ts(end_date + Duration::days(1))?;
    Ok(StatsQueryWindow {
        start_ts,
        end_ts,
        granularity,
    })
}

fn parse_stats_date(value: &str, name: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| format!("{} 必须使用 YYYY-MM-DD 格式", name))
}

fn local_midnight_ts(date: NaiveDate) -> Result<i64, String> {
    Local
        .with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
        .single()
        .map(|d| d.timestamp())
        .ok_or_else(|| format!("日期 {} 无法转换为本地时间", date))
}

fn stats_query_parts(
    params: &HashMap<String, String>,
) -> Result<(StatsQueryWindow, Option<u64>), String> {
    Ok((parse_stats_window(params)?, parse_key_id(params)?))
}

fn stats_bad_request(message: String) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(super::types::AdminErrorResponse::invalid_request(message)),
    )
        .into_response()
}

/// GET /api/admin/stats/overview
pub async fn stats_overview(State(state): State<AdminState>) -> impl IntoResponse {
    let overview = state.usage_aggregator.overview();
    // 附加：当前活跃 Key / 凭据数
    let active_keys = state.client_keys.active_count() as u64;
    let snapshot = state.service.get_all_credentials();
    let active_credentials = snapshot.credentials.iter().filter(|c| !c.disabled).count() as u64;
    let response = serde_json::json!({
        "todayCalls": overview.today_calls,
        "todayInputTokens": overview.today_input_tokens,
        "todayOutputTokens": overview.today_output_tokens,
        "todayErrors": overview.today_errors,
        "todayCredits": overview.today_credits,
        "weekCalls": overview.week_calls,
        "weekInputTokens": overview.week_input_tokens,
        "weekOutputTokens": overview.week_output_tokens,
        "weekCredits": overview.week_credits,
        "activeClientKeys": active_keys,
        "activeCredentials": active_credentials,
    });
    Json(response)
}

/// GET /api/admin/stats/timeseries?range=24h|7d|30d&granularity=hour|day&group=...
pub async fn stats_timeseries(
    State(state): State<AdminState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let (window, key_id) = match stats_query_parts(&params) {
        Ok(parts) => parts,
        Err(message) => return stats_bad_request(message),
    };
    let group = parse_group_filter(&params);
    let cred_ids = group_to_cred_ids(&state, group.as_deref());
    let points = state.usage_aggregator.query_timeseries(window, key_id, cred_ids.as_ref());
    Json(points).into_response()
}

/// GET /api/admin/stats/by-model?range=24h|7d|30d
pub async fn stats_by_model(
    State(state): State<AdminState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let (window, key_id) = match stats_query_parts(&params) {
        Ok(parts) => parts,
        Err(message) => return stats_bad_request(message),
    };
    let data = state.usage_aggregator.query_by_model(window, key_id);
    Json(data).into_response()
}

/// GET /api/admin/stats/by-credential?range=24h|7d|30d
pub async fn stats_by_credential(
    State(state): State<AdminState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let (window, key_id) = match stats_query_parts(&params) {
        Ok(parts) => parts,
        Err(message) => return stats_bad_request(message),
    };
    let group = parse_group_filter(&params);
    // 拉一份凭据快照（既给响应附加 email，也用来按 group 构建 cred_ids 白名单，
    // 避免分别查两次）
    let snapshot = state.service.get_all_credentials();
    let email_map: std::collections::HashMap<u64, Option<String>> = snapshot
        .credentials
        .iter()
        .map(|c| (c.id, c.email.clone()))
        .collect();
    let cred_ids: Option<std::collections::HashSet<u64>> = group.as_deref().map(|g| {
        snapshot
            .credentials
            .iter()
            .filter(|c| c.groups.iter().any(|cg| cg == g))
            .map(|c| c.id)
            .collect()
    });
    let data = state.usage_aggregator.query_by_credential(window, key_id, cred_ids.as_ref());
    let enriched: Vec<serde_json::Value> = data
        .into_iter()
        .map(|d| {
            let email = email_map.get(&d.credential_id).cloned().flatten();
            serde_json::json!({
                "credentialId": d.credential_id,
                "email": email,
                "calls": d.calls,
                "inputTokens": d.input_tokens,
                "outputTokens": d.output_tokens,
                "errors": d.errors,
            })
        })
        .collect();
    Json(enriched).into_response()
}

/// GET /api/admin/traces
/// 查询请求链路追踪记录（含每跳明细）。
/// query 参数：status / errorType / credentialId / model / onlyFailed / limit / offset
/// 返回：{ records: [...], total: N }
pub async fn list_traces(
    State(state): State<AdminState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let query = TraceQuery {
        status: params.get("status").filter(|s| !s.is_empty()).cloned(),
        error_type: params.get("errorType").filter(|s| !s.is_empty()).cloned(),
        credential_id: params
            .get("credentialId")
            .and_then(|s| s.parse::<u64>().ok()),
        key_id: params.get("keyId").and_then(|s| s.parse::<u64>().ok()),
        failed_attempt_credential_id: params
            .get("failedAttemptCredentialId")
            .and_then(|s| s.parse::<u64>().ok()),
        model: params.get("model").filter(|s| !s.is_empty()).cloned(),
        only_failed: params
            .get("onlyFailed")
            .map(|s| s == "true" || s == "1")
            .unwrap_or(false),
        limit: params
            .get("limit")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(crate::admin::trace_db::DEFAULT_QUERY_LIMIT)
            .min(1000),
        offset: params
            .get("offset")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0),
    };
    let (records, total) = state.trace_store.query_paged(&query);

    // 附加 credential email 方便前端展示（与 stats_by_credential 一致）
    let snapshot = state.service.get_all_credentials();
    let email_map: HashMap<u64, Option<String>> = snapshot
        .credentials
        .iter()
        .map(|c| (c.id, c.email.clone()))
        .collect();
    let client_key_name_map: HashMap<u64, String> = state
        .client_keys
        .list()
        .into_iter()
        .map(|k| (k.id, k.name))
        .collect();
    // 客户端 Key 名称解析：master apiKey (key_id=0) 在前端显示为"管理员API密钥"，
    // 其余客户端 Key 命中名称表则取名称、未命中则回退 #id。
    let key_label = |key_id: u64| -> String {
        if key_id == 0 {
            "master".to_string()
        } else {
            client_key_name_map
                .get(&key_id)
                .cloned()
                .unwrap_or_else(|| format!("#{}", key_id))
        }
    };

    let enriched: Vec<serde_json::Value> = records
        .into_iter()
        .map(|r| {
            let final_email = email_map.get(&r.final_credential_id).cloned().flatten();
            let key_name = key_label(r.key_id);
            // attempts 里每跳也附 email
            let attempts: Vec<serde_json::Value> = r
                .attempts
                .iter()
                .map(|a| {
                    let email = email_map.get(&a.credential_id).cloned().flatten();
                    serde_json::json!({
                        "attempt": a.attempt,
                        "credentialId": a.credential_id,
                        "email": email,
                        "endpoint": a.endpoint,
                        "httpStatus": a.http_status,
                        "outcome": a.outcome,
                        "errorSnippet": a.error_snippet,
                        "durationMs": a.duration_ms,
                    })
                })
                .collect();
            serde_json::json!({
                "traceId": r.trace_id,
                "ts": r.ts,
                "keyId": r.key_id,
                "keySource": r.key_source,
                "keyName": key_name,
                "model": r.model,
                "isStream": r.is_stream,
                "finalStatus": r.final_status,
                "finalCredentialId": r.final_credential_id,
                "finalEmail": final_email,
                "errorType": r.error_type,
                "errorMessage": r.error_message,
                "totalAttempts": r.total_attempts,
                "durationMs": r.duration_ms,
                "interruptedAfterBytes": r.interrupted_after_bytes,
                "inputTokens": r.input_tokens,
                "outputTokens": r.output_tokens,
                "cacheCreationTokens": r.cache_creation_tokens,
                "cacheReadTokens": r.cache_read_tokens,
                "totalTokens": r.input_tokens + r.output_tokens + r.cache_creation_tokens + r.cache_read_tokens,
                "credits": r.credits,
                "firstTokenMs": r.first_token_ms,
                "attempts": attempts,
            })
        })
        .collect();
    Json(serde_json::json!({ "records": enriched, "total": total }))
}

/// GET /api/admin/traces/failure-stats
/// 按凭据聚合失败次数（鉴权 / 账号风控 / 其他三类），用于卡片分色展示。
/// 返回 { "<credentialId>": { auth, throttle, other }, ... }
pub async fn trace_failure_stats(State(state): State<AdminState>) -> impl IntoResponse {
    let stats = state.trace_store.failure_stats();
    let map: std::collections::HashMap<String, serde_json::Value> = stats
        .into_iter()
        .map(|(id, s)| {
            (
                id.to_string(),
                serde_json::json!({
                    "auth": s.auth,
                    "throttle": s.throttle,
                    "other": s.other,
                }),
            )
        })
        .collect();
    Json(map)
}

// ============ 账号分组（独立实体）============

fn group_to_item(
    g: &super::groups::Group,
    state: &AdminState,
) -> super::types::GroupItem {
    super::types::GroupItem {
        name: g.name.clone(),
        description: g.description.clone(),
        created_at: g.created_at.clone(),
        credential_count: state
            .service
            .token_manager()
            .count_credentials_with_group(&g.name),
        client_key_count: state.client_keys.count_with_group(&g.name),
    }
}

/// GET /api/admin/groups
pub async fn list_groups(State(state): State<AdminState>) -> impl IntoResponse {
    let groups = state.groups.list();
    let items: Vec<super::types::GroupItem> =
        groups.iter().map(|g| group_to_item(g, &state)).collect();
    Json(super::types::GroupsResponse {
        total: items.len(),
        groups: items,
    })
}

/// POST /api/admin/groups
pub async fn create_group(
    State(state): State<AdminState>,
    Json(payload): Json<super::types::CreateGroupRequest>,
) -> impl IntoResponse {
    match state
        .groups
        .create(payload.name, payload.description)
    {
        Ok(g) => Json(group_to_item(&g, &state)).into_response(),
        Err(e) => {
            let msg = e.to_string();
            // "已存在" → 409；其他校验失败 → 400
            let (code, resp) = if msg.contains("已存在") {
                (
                    StatusCode::CONFLICT,
                    super::types::AdminErrorResponse::invalid_request(msg),
                )
            } else {
                (
                    StatusCode::BAD_REQUEST,
                    super::types::AdminErrorResponse::invalid_request(msg),
                )
            };
            (code, Json(resp)).into_response()
        }
    }
}

/// PATCH /api/admin/groups/:name
///
/// 改名 / 改备注。改名时级联更新所有引用该分组的凭据 / 客户端 Key。
pub async fn update_group(
    State(state): State<AdminState>,
    Path(name): Path<String>,
    Json(payload): Json<super::types::UpdateGroupRequest>,
) -> impl IntoResponse {
    if !state.groups.exists(&name) {
        return (
            StatusCode::NOT_FOUND,
            Json(super::types::AdminErrorResponse::not_found(format!(
                "分组 {} 不存在",
                name
            ))),
        )
            .into_response();
    }

    // 1. 改名（先校验目标名再级联）
    let mut current_name = name.clone();
    if let Some(new_name) = payload.new_name.as_deref() {
        let trimmed = new_name.trim();
        if !trimmed.is_empty() && trimmed != name {
            // GroupManager 内做唯一性 / 长度 / 空校验
            match state.groups.rename(&name, trimmed) {
                Ok(_) => {}
                Err(e) => {
                    let msg = e.to_string();
                    let code = if msg.contains("已存在") {
                        StatusCode::CONFLICT
                    } else {
                        StatusCode::BAD_REQUEST
                    };
                    return (
                        code,
                        Json(super::types::AdminErrorResponse::invalid_request(msg)),
                    )
                        .into_response();
                }
            }
            // 级联：失败时尝试回滚分组改名（避免注册表与凭据 / Key 不一致）
            let cred_res = state
                .service
                .token_manager()
                .rename_credential_group(&name, trimmed);
            if let Err(e) = cred_res {
                let _ = state.groups.rename(trimmed, &name);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(super::types::AdminErrorResponse::internal_error(format!(
                        "级联更新凭据失败: {}",
                        e
                    ))),
                )
                    .into_response();
            }
            state.client_keys.rename_group(&name, trimmed);
            current_name = trimmed.to_string();
        }
    }

    // 2. 改备注
    if let Some(desc) = payload.description {
        let desc_opt = if desc.trim().is_empty() {
            None
        } else {
            Some(desc)
        };
        if let Err(e) = state.groups.update_description(&current_name, desc_opt) {
            return (
                StatusCode::BAD_REQUEST,
                Json(super::types::AdminErrorResponse::invalid_request(e.to_string())),
            )
                .into_response();
        }
    }

    let group = match state.groups.get(&current_name) {
        Some(g) => g,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(super::types::AdminErrorResponse::internal_error(
                    "分组在更新过程中消失，状态异常",
                )),
            )
                .into_response();
        }
    };
    Json(group_to_item(&group, &state)).into_response()
}

/// DELETE /api/admin/groups/:name?force=true
///
/// 默认拒绝删除仍被引用的分组；带 `force=true` 时级联清理所有引用并删除。
pub async fn delete_group(
    State(state): State<AdminState>,
    Path(name): Path<String>,
    Query(query): Query<super::types::DeleteGroupQuery>,
) -> impl IntoResponse {
    if !state.groups.exists(&name) {
        return (
            StatusCode::NOT_FOUND,
            Json(super::types::AdminErrorResponse::not_found(format!(
                "分组 {} 不存在",
                name
            ))),
        )
            .into_response();
    }

    let cred_count = state
        .service
        .token_manager()
        .count_credentials_with_group(&name);
    let key_count = state.client_keys.count_with_group(&name);

    if (cred_count > 0 || key_count > 0) && !query.force {
        return (
            StatusCode::CONFLICT,
            Json(super::types::AdminErrorResponse::invalid_request(format!(
                "分组仍被引用（凭据 {} / 客户端 Key {}），传 ?force=true 级联清理",
                cred_count, key_count
            ))),
        )
            .into_response();
    }

    if query.force {
        if let Err(e) = state
            .service
            .token_manager()
            .remove_credential_group(&name)
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(super::types::AdminErrorResponse::internal_error(format!(
                    "级联清理凭据失败: {}",
                    e
                ))),
            )
                .into_response();
        }
        state.client_keys.clear_group(&name);
    }

    state.groups.delete(&name);
    Json(super::types::SuccessResponse::new(format!(
        "分组 {} 已删除",
        name
    )))
    .into_response()
}
