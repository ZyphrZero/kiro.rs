//! HTTP Client 构建模块
//!
//! 提供统一的 HTTP Client 构建功能，支持代理配置

use reqwest::{Client, Proxy};
use std::time::Duration;

use crate::model::config::TlsBackend;

/// 读取一个以秒为单位的环境变量，缺失或非法时回退到 `default`。值为 0 也视为非法（回退默认）。
fn env_secs(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

/// 代理配置
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ProxyConfig {
    /// 代理地址，支持 http/https/socks5
    pub url: String,
    /// 代理认证用户名
    pub username: Option<String>,
    /// 代理认证密码
    pub password: Option<String>,
}

impl ProxyConfig {
    /// 从 url 创建代理配置
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            username: None,
            password: None,
        }
    }

    /// 设置认证信息
    pub fn with_auth(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }
}

/// 构建 HTTP Client
///
/// # Arguments
/// * `proxy` - 可选的代理配置
/// * `timeout_secs` - 超时时间（秒）
///
/// # Returns
/// 配置好的 reqwest::Client
pub fn build_client(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    // 分层超时（可经 KIRO_RS_HTTP_* 覆盖）：
    // - connect_timeout：仅 TCP+TLS 建连阶段。坏/挂死连接秒级失败重试，不再拖到总超时。
    // - read_timeout：每次读操作超时，**成功读一次即重置**。用于探测"建连后迟迟不吐字节"
    //   的挂死连接；首字节一到即重置，因此大上下文的长 prefill 与长生成都不会被误杀。
    // 这是高并发下的关键：避免少数挂死请求长时间霸占稀缺的账号并发槽，拖垮整个池子的首 token。
    let connect_timeout = env_secs("KIRO_RS_HTTP_CONNECT_TIMEOUT_SECS", 15);
    let read_timeout = env_secs("KIRO_RS_HTTP_READ_TIMEOUT_SECS", 300);
    let keepalive = env_secs("KIRO_RS_HTTP_TCP_KEEPALIVE_SECS", 60);
    // 连接池空闲超时**必须短于上游服务端的空闲关闭时间**(AWS ALB 默认 ~60s),
    // 否则池里会留存已被服务端 RST/FIN 的"半死"连接,下一个请求取到它直接
    // "socket closed unexpectedly"。取 15s 远低于 60s,使陈旧连接在被复用前先被淘汰;
    // 取连接瞬间仍可能撞上服务端刚关闭的竞态,由上层重试循环兜底(execute 失败即重试)。
    let pool_idle = env_secs("KIRO_RS_HTTP_POOL_IDLE_TIMEOUT_SECS", 15);

    let mut builder = Client::builder()
        // 总超时仍保留为大兜底（含完整流式响应）；read_timeout 才是挂死探测主力。
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(connect_timeout))
        .read_timeout(Duration::from_secs(read_timeout))
        .tcp_keepalive(Duration::from_secs(keepalive))
        // 复用空闲连接省掉重复 TCP+TLS 握手；但空闲超时短于上游关闭时间,避免取到死连接。
        .pool_idle_timeout(Duration::from_secs(pool_idle))
        .pool_max_idle_per_host(8);

    match tls_backend {
        TlsBackend::Rustls => {
            builder = builder.use_rustls_tls();
        }
        TlsBackend::NativeTls => {
            #[cfg(feature = "native-tls")]
            {
                builder = builder.use_native_tls();
            }
            #[cfg(not(feature = "native-tls"))]
            {
                anyhow::bail!("此构建版本未包含 native-tls 后端，请在配置中改用 rustls");
            }
        }
    }

    if let Some(proxy_config) = proxy {
        let mut proxy = Proxy::all(&proxy_config.url)?;

        // 设置代理认证
        if let (Some(username), Some(password)) = (&proxy_config.username, &proxy_config.password) {
            proxy = proxy.basic_auth(username, password);
        }

        builder = builder.proxy(proxy);
        tracing::debug!("HTTP Client 使用代理: {}", proxy_config.url);
    }

    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_config_new() {
        let config = ProxyConfig::new("http://127.0.0.1:7890");
        assert_eq!(config.url, "http://127.0.0.1:7890");
        assert!(config.username.is_none());
        assert!(config.password.is_none());
    }

    #[test]
    fn test_proxy_config_with_auth() {
        let config = ProxyConfig::new("socks5://127.0.0.1:1080").with_auth("user", "pass");
        assert_eq!(config.url, "socks5://127.0.0.1:1080");
        assert_eq!(config.username, Some("user".to_string()));
        assert_eq!(config.password, Some("pass".to_string()));
    }

    #[test]
    fn test_build_client_without_proxy() {
        let client = build_client(None, 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }

    #[test]
    fn test_build_client_with_proxy() {
        let config = ProxyConfig::new("http://127.0.0.1:7890");
        let client = build_client(Some(&config), 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }
}
