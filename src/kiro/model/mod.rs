//! Kiro 数据模型
//!
//! 包含 Kiro API 的所有数据类型定义：
//! - `common`: 共享类型（枚举和辅助结构体）
//! - `events`: 响应事件类型
//! - `requests`: 请求类型
//! - `credentials`: OAuth 凭证
//! - `token_refresh`: Token 刷新
//! - `usage_limits`: 使用额度查询
//! - `available_models`: 可用模型查询
//! - `available_profiles`: 可用 Profile 查询（Enterprise/IdC 真实 profileArn）

pub mod available_models;
pub mod available_profiles;
pub mod common;
pub mod credentials;
pub mod events;
pub mod requests;
pub mod token_refresh;
pub mod usage_limits;

/// Treat absent and explicit null collections alike, but reject other types.
pub(crate) fn null_vec<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    use serde::Deserialize;
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

#[cfg(test)]
mod null_collection_tests {
    use super::*;
    #[test]
    fn management_collections_accept_null_and_missing() {
        for input in ["{}", r#"{"models":null,"profiles":null,"usageBreakdownList":null}"#] {
            assert!(serde_json::from_str::<available_models::ListAvailableModelsResponse>(input).unwrap().models.is_empty());
            assert!(serde_json::from_str::<available_profiles::ListAvailableProfilesResponse>(input).unwrap().profiles.is_empty());
            assert!(serde_json::from_str::<usage_limits::UsageLimitsResponse>(input).unwrap().usage_breakdown_list.is_empty());
        }
        assert!(serde_json::from_str::<usage_limits::UsageBreakdown>(r#"{"bonuses":null}"#).unwrap().bonuses.is_empty());
        assert!(serde_json::from_str::<available_models::ListAvailableModelsResponse>(r#"{"models":false}"#).is_err());
    }
}
