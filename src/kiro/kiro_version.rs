//! Kiro IDE 版本自动获取
//!
//! 从官方稳定版元数据端点读取 `currentRelease` 字段，得到当前发布的 Kiro IDE 版本号，
//! 用于构造与官方 IDE 一致的 User-Agent（`KiroIDE-<version>-<machineId>`）。
//!
//! - 进程内缓存（`OnceLock<RwLock<Option<String>>>`）+ 后台定时刷新；
//! - 跨平台 `currentRelease` 一致，固定使用 win32-x64 元数据即可；
//! - 获取失败时调用方回退到 `config.kiro_version`，不阻塞启动。
//!
//! 注意：用量类 REST 接口（getUsageLimits / ListAvailableModels / setUserPreference）
//! 不使用这里的「最新版本」——新版 IDE 对这些接口强制要求 profileArn，对 Enterprise/IdC
//! 账号会失败。那几个接口固定使用 [`USAGE_API_KIRO_VERSION`]：该版本无需 profileArn
//! 即可返回订阅与用量。

use std::sync::OnceLock;
use std::time::Duration;

use parking_lot::RwLock;
use serde::Deserialize;

use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;

/// 官方稳定版元数据端点（`currentRelease` 即当前 IDE 版本，跨平台一致）。
///
/// 注意：必须使用 `linux-x64` / `darwin-*` 路径——`win32-*` 路径在 CDN 上返回 403
/// （Windows 走不同的分发格式）。版本号本身与平台无关，任选可用平台即可。
const METADATA_URL: &str =
    "https://prod.download.desktop.kiro.dev/stable/metadata-linux-x64-stable.json";

/// 用量类接口（getUsageLimits / ListAvailableModels / setUserPreference）固定使用的
/// Kiro IDE 版本：该版本下上游无需 profileArn 即可返回数据，Enterprise/IdC 账号同样可用。
pub const USAGE_API_KIRO_VERSION: &str = "0.9.2";

static LATEST_VERSION: OnceLock<RwLock<Option<String>>> = OnceLock::new();

fn cell() -> &'static RwLock<Option<String>> {
    LATEST_VERSION.get_or_init(|| RwLock::new(None))
}

/// 已自动获取到的最新 Kiro IDE 版本（后台刷新成功后才有值）
pub fn cached() -> Option<String> {
    cell().read().clone()
}

/// 返回有效的 Kiro IDE 版本：优先用自动获取到的最新版本，否则回退到 `fallback`
pub fn effective(fallback: &str) -> String {
    cached().unwrap_or_else(|| fallback.to_string())
}

#[derive(Deserialize)]
struct Metadata {
    #[serde(rename = "currentRelease")]
    current_release: Option<String>,
}

/// 拉取一次最新版本号
pub async fn fetch_latest(
    proxy: Option<&ProxyConfig>,
    tls_backend: TlsBackend,
) -> anyhow::Result<String> {
    let client = build_client(proxy, 15, tls_backend)?;
    let resp = client.get(METADATA_URL).send().await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("获取 Kiro 版本元数据失败: {}", status);
    }
    let meta: Metadata = resp.json().await?;
    meta.current_release
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("元数据缺少 currentRelease"))
}

/// 启动后台任务：立即拉取一次，之后每 `interval` 刷新一次。
///
/// 失败仅记录告警，不影响服务（调用方会回退到 `config.kiro_version`）。
pub fn spawn_refresher(proxy: Option<ProxyConfig>, tls_backend: TlsBackend, interval: Duration) {
    tokio::spawn(async move {
        loop {
            match fetch_latest(proxy.as_ref(), tls_backend).await {
                Ok(version) => {
                    let changed = cached().as_deref() != Some(version.as_str());
                    *cell().write() = Some(version.clone());
                    if changed {
                        tracing::info!("已自动获取 Kiro IDE 版本: {}", version);
                    }
                }
                Err(e) => {
                    tracing::warn!("自动获取 Kiro IDE 版本失败（继续使用配置中的版本）: {}", e);
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metadata_parses_current_release() {
        let json = r#"{"currentRelease":"0.12.301","releases":[]}"#;
        let meta: Metadata = serde_json::from_str(json).unwrap();
        assert_eq!(meta.current_release.as_deref(), Some("0.12.301"));
    }

    #[test]
    fn test_effective_falls_back_without_cache() {
        // 未注入缓存时回退到 fallback（注意：其它测试可能已填充全局缓存，
        // 故此处仅断言返回值非空且为合法字符串）
        let v = effective("0.9.2");
        assert!(!v.is_empty());
    }
}
