//! 管理端账号成本账本。采集独立于请求日志和客户端 Key 用量。
mod capture;
mod checkpoints;
mod ledger;
#[cfg(test)]
mod tests;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::admin::middleware::AdminState;
use crate::kiro::parser::frame::Frame;

pub(crate) use capture::CallMeter;
pub use capture::CostStore;

#[derive(Deserialize)]
struct MeteringLiteral<'a> {
    #[serde(borrow)]
    usage: Option<&'a serde_json::value::RawValue>,
    unit: Option<&'a str>,
}

fn metering_literal(frame: &Frame) -> Option<String> {
    let meter: MeteringLiteral<'_> = serde_json::from_slice(&frame.payload).ok()?;
    if !matches!(meter.unit, Some("credit" | "credits")) {
        return None;
    }
    let raw = meter.usage?.get();
    if raw.len() > 128 || !raw.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        return None;
    }
    Some(raw.to_owned())
}

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn error(status: StatusCode, message: impl ToString) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({ "error": { "message": message.to_string() } })),
    )
}

fn get_store(state: &AdminState) -> Result<CostStore, (StatusCode, Json<Value>)> {
    state.service.cost_store().cloned().ok_or_else(|| {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "成本账本不可用。请检查 costTrackingEnabled 和数据目录。",
        )
    })
}

pub(super) fn router() -> Router<AdminState> {
    Router::new()
        .route("/costs/overview", get(overview))
        .route("/costs/purchases", post(purchase))
        .route("/costs/lots/{id}/confirm-expiry", post(confirm_expiry))
}

async fn overview(
    State(state): State<AdminState>,
    Query(window): Query<ledger::Window>,
) -> ApiResult {
    let store = get_store(&state)?;
    let window = window
        .validate()
        .map_err(|e| error(StatusCode::BAD_REQUEST, e))?;
    let report = tokio::task::spawn_blocking(move || ledger::report(&store, window))
        .await
        .map_err(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "成本查询失败"))?
        .map_err(|e| {
            tracing::error!(error = %e, "成本查询失败");
            error(StatusCode::INTERNAL_SERVER_ERROR, "成本查询失败")
        })?;
    let credentials = state
        .service
        .token_manager()
        .snapshot()
        .entries
        .into_iter()
        .map(|entry| entry.id)
        .collect::<Vec<_>>();
    let mut report = report;
    report["activeCredentialIds"] = json!(credentials);
    Ok(Json(report))
}

async fn purchase(
    State(state): State<AdminState>,
    Json(purchase): Json<ledger::Purchase>,
) -> ApiResult {
    let store = get_store(&state)?;
    purchase
        .validate()
        .map_err(|e| error(StatusCode::BAD_REQUEST, e))?;
    if state
        .service
        .token_manager()
        .clone_credential(purchase.credential_id)
        .is_none()
    {
        return Err(error(StatusCode::NOT_FOUND, "凭据不存在"));
    }
    store
        .purchase(purchase)
        .await
        .map_err(|e| error(StatusCode::CONFLICT, e))?;
    Ok(Json(json!({ "success": true })))
}

async fn confirm_expiry(State(state): State<AdminState>, Path(id): Path<String>) -> ApiResult {
    get_store(&state)?
        .confirm_expiry(id)
        .await
        .map_err(|e| error(StatusCode::CONFLICT, e))?;
    Ok(Json(json!({ "success": true })))
}
