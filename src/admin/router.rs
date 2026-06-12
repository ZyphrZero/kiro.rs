//! Admin API 路由配置

use axum::{
    Router, middleware,
    routing::{delete, get, post, put},
};

use super::{
    handlers::{
        add_credential, add_proxy, apply_image_update, assign_proxies_round_robin,
        assign_proxy_to_credential, batch_add_proxies, check_all_proxies, check_proxy,
        check_rate_limit, check_update, clear_throttle, complete_social_login,
        complete_social_relogin, create_client_key, create_group, delete_client_key,
        delete_credential, delete_group, delete_proxy, disable_quota_exceeded, enable_overage_all,
        export_credentials, force_refresh_token, get_account_throttle_config,
        get_all_credentials, get_credential_balance, get_credential_models, get_global_proxy,
        get_load_balancing_mode, get_log_governance_config, get_proxy_pool, get_update_config,
        list_client_keys, list_groups, list_traces, trace_failure_stats, poll_idc_login,
        poll_idc_relogin, poll_social_login,
        poll_social_relogin, pull_update_image, reset_all_success_count, reset_client_key_stats,
        reset_failure_count, reset_success_count, rollback_image_update, rotate_client_key,
        set_account_throttle_config, set_client_key_disabled, set_credential_disabled,
        set_credential_overage, set_credential_priority, set_global_proxy,
        set_load_balancing_mode, set_log_governance_config, set_proxy_enabled, set_update_config,
        start_idc_login, start_idc_relogin, start_social_login, start_social_relogin,
        stats_by_credential, stats_by_model, stats_overview, stats_timeseries, update_admin_key,
        update_api_key, update_client_key, update_credential, update_group, update_refresh_token,
    },
    middleware::{AdminState, admin_auth_middleware},
};

/// 创建 Admin API 路由
///
/// # 端点
/// - `GET /credentials` - 获取所有凭据状态
/// - `POST /credentials` - 添加新凭据
/// - `DELETE /credentials/:id` - 删除凭据
/// - `PUT /credentials/:id` - 更新凭据可编辑字段（email、proxy 等）
/// - `POST /credentials/:id/disabled` - 设置凭据禁用状态
/// - `POST /credentials/:id/priority` - 设置凭据优先级
/// - `POST /credentials/:id/reset` - 重置失败计数
/// - `POST /credentials/:id/refresh` - 强制刷新 Token
/// - `GET /credentials/:id/balance` - 获取凭据余额
/// - `GET /config/load-balancing` - 获取负载均衡模式
/// - `PUT /config/load-balancing` - 设置负载均衡模式
///
/// # 认证
/// 需要登录API密钥认证，支持：
/// - `x-api-key` header
/// - `Authorization: Bearer <token>` header
pub fn create_admin_router(state: AdminState) -> Router {
    // 需要登录API密钥认证的路由
    let authenticated = Router::new()
        .route(
            "/credentials",
            get(get_all_credentials).post(add_credential),
        )
        .route("/credentials/export", get(export_credentials))
        .route(
            "/credentials/{id}",
            delete(delete_credential).put(update_credential),
        )
        .route("/credentials/{id}/disabled", post(set_credential_disabled))
        .route("/credentials/{id}/priority", post(set_credential_priority))
        .route("/credentials/{id}/reset", post(reset_failure_count))
        .route("/credentials/{id}/clear-throttle", post(clear_throttle))
        .route("/credentials/{id}/reset-stats", post(reset_success_count))
        .route("/credentials/reset-stats", post(reset_all_success_count))
        .route(
            "/credentials/disable-quota-exceeded",
            post(disable_quota_exceeded),
        )
        .route(
            "/credentials/overage/enable-all",
            post(enable_overage_all),
        )
        .route("/credentials/{id}/overage", post(set_credential_overage))
        .route("/credentials/{id}/refresh", post(force_refresh_token))
        .route("/credentials/{id}/refresh-token", put(update_refresh_token))
        .route("/credentials/{id}/balance", get(get_credential_balance))
        .route("/credentials/{id}/models", get(get_credential_models))
        .route("/credentials/{id}/proxy", post(assign_proxy_to_credential))
        .route("/proxy-pool", get(get_proxy_pool).post(add_proxy))
        .route("/proxy-pool/batch", post(batch_add_proxies))
        .route("/proxy-pool/check-all", post(check_all_proxies))
        .route("/proxy-pool/assign-round-robin", post(assign_proxies_round_robin))
        .route("/proxy-pool/{id}", delete(delete_proxy))
        .route("/proxy-pool/{id}/enabled", post(set_proxy_enabled))
        .route("/proxy-pool/{id}/check", post(check_proxy))
        .route(
            "/config/load-balancing",
            get(get_load_balancing_mode).put(set_load_balancing_mode),
        )
        .route(
            "/config/account-throttle",
            get(get_account_throttle_config).put(set_account_throttle_config),
        )
        .route(
            "/config/log-governance",
            get(get_log_governance_config).put(set_log_governance_config),
        )
        .route(
            "/config/global-proxy",
            get(get_global_proxy).put(set_global_proxy),
        )
        .route(
            "/config/update",
            get(get_update_config).put(set_update_config),
        )
        .route("/config/admin-key", put(update_admin_key))
        .route("/config/api-key", put(update_api_key))
        .route("/system/update/pull", post(pull_update_image))
        .route("/system/update/apply", post(apply_image_update))
        .route("/system/update/rollback", post(rollback_image_update))
        .route("/system/update/check", get(check_update))
        .route("/system/update/rate-limit", post(check_rate_limit))
        .route("/auth/idc/start", post(start_idc_login))
        .route("/auth/idc/poll/{session_id}", post(poll_idc_login))
        .route("/auth/social/start", post(start_social_login))
        .route("/auth/social/poll/{session_id}", post(poll_social_login))
        .route(
            "/auth/social/complete/{session_id}",
            post(complete_social_login),
        )
        .route(
            "/credentials/{id}/relogin/social/start",
            post(start_social_relogin),
        )
        .route(
            "/credentials/{id}/relogin/social/poll/{session_id}",
            post(poll_social_relogin),
        )
        .route(
            "/credentials/{id}/relogin/social/complete/{session_id}",
            post(complete_social_relogin),
        )
        .route(
            "/credentials/{id}/relogin/idc/start",
            post(start_idc_relogin),
        )
        .route(
            "/credentials/{id}/relogin/idc/poll/{session_id}",
            post(poll_idc_relogin),
        )
        .route("/client-keys", get(list_client_keys).post(create_client_key))
        .route(
            "/client-keys/{id}",
            delete(delete_client_key).put(update_client_key),
        )
        .route("/client-keys/{id}/disabled", post(set_client_key_disabled))
        .route("/client-keys/{id}/reset-stats", post(reset_client_key_stats))
        .route("/client-keys/{id}/rotate", post(rotate_client_key))
        .route("/groups", get(list_groups).post(create_group))
        .route(
            "/groups/{name}",
            delete(delete_group).patch(update_group),
        )
        .route("/stats/overview", get(stats_overview))
        .route("/stats/timeseries", get(stats_timeseries))
        .route("/stats/by-model", get(stats_by_model))
        .route("/stats/by-credential", get(stats_by_credential))
        .route("/traces/failure-stats", get(trace_failure_stats))
        .route("/traces", get(list_traces))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            admin_auth_middleware,
        ));

    Router::new().merge(authenticated).with_state(state)
}
