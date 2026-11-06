//! HTTP Client 构建模块
//!
//! 提供统一的 HTTP Client 构建功能，支持代理配置

use reqwest::{Client, Proxy};
use std::time::Duration;

use crate::model::config::TlsBackend;

/// 代理配置
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct ProxyConfig {
    /// 代理地址，支持 http/https/socks5
    pub url: String,
    /// 代理认证用户名
    pub username: Option<String>,
    /// 代理认证密码
    pub password: Option<String>,
}

impl std::fmt::Debug for ProxyConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ProxyConfig")
            .field("url", &mask_proxy_url(&self.url))
            .field("username", &self.username.as_ref().map(|_| "<set>"))
            .field("password", &self.password.as_ref().map(|_| "<set>"))
            .finish()
    }
}

pub(crate) fn mask_proxy_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    match rest.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://***@{host}"),
        None => url.to_string(),
    }
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
    Ok(client_builder(proxy, tls_backend)?
        .timeout(Duration::from_secs(timeout_secs))
        .build()?)
}

/// API streams need separate connection and idle deadlines; active reads reset
/// the idle timer, so upstream silence is bounded without bounding the answer.
///
/// `total_secs` is the deadline for the whole request, body included. It is a
/// wall clock that an active stream cannot reset, so any overall budget doubles
/// as a cap on answer length: a request still delivering tokens at the deadline
/// dies with a half-written body, and every token already generated is paid for
/// and lost. Pass 0 to drop that cap — a stalled upstream is still caught by
/// `read_secs`, which measures silence rather than total work.
pub fn build_client_with_timeouts(
    proxy: Option<&ProxyConfig>,
    total_secs: u64,
    tls_backend: TlsBackend,
    connect_secs: u64,
    read_secs: u64,
) -> anyhow::Result<Client> {
    let mut builder = client_builder(proxy, tls_backend)?
        .connect_timeout(Duration::from_secs(connect_secs.max(1)))
        .read_timeout(Duration::from_secs(read_secs.max(1)));
    if total_secs > 0 {
        builder = builder.timeout(Duration::from_secs(total_secs));
    }
    Ok(builder.build()?)
}

fn client_builder(
    proxy: Option<&ProxyConfig>,
    tls_backend: TlsBackend,
) -> anyhow::Result<reqwest::ClientBuilder> {
    let mut builder = Client::builder();

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
            proxy = proxy.basic_auth(&escape_percent(username), &escape_percent(password));
        }

        builder = builder.proxy(proxy);
        tracing::debug!("HTTP Client 使用代理: {}", mask_proxy_url(&proxy_config.url));
    }

    Ok(builder)
}

/// reqwest 0.12 stores `basic_auth` credentials in the proxy URL's userinfo,
/// where `url` leaves '%' unescaped, and percent-decodes them once when
/// connecting. Escaping '%' keeps a literal "p%40ss" from arriving as "p@ss".
pub(crate) fn escape_percent(value: &str) -> String {
    value.replace('%', "%25")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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

    const FIXTURE_DEADLINE: Duration = Duration::from_secs(5);

    /// Captures the CONNECT request headers, then refuses the tunnel so no TLS
    /// or external traffic follows.
    async fn capture_connect(proxy: impl FnOnce(std::net::SocketAddr) -> ProxyConfig) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(tokio::time::timeout(FIXTURE_DEADLINE, async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let mut buf = [0; 1024];
                let count = socket.read(&mut buf).await.unwrap();
                assert!(count > 0, "connection closed before complete headers");
                request.extend_from_slice(&buf[..count]);
            }
            socket
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8(request).unwrap()
        }));
        let client = build_client(Some(&proxy(addr)), 5, TlsBackend::Rustls).unwrap();
        client
            .get("https://example.invalid/")
            .send()
            .await
            .expect_err("fixture refuses the tunnel");
        server.await.unwrap().expect("proxy fixture timed out")
    }

    /// Decoded `user:pass` from the Basic Proxy-Authorization header.
    fn proxy_authorization(request: &str) -> Option<String> {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let value = request.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("proxy-authorization").then(|| value.trim())
        })?;
        let decoded = STANDARD.decode(value.strip_prefix("Basic ")?).unwrap();
        Some(String::from_utf8(decoded).unwrap())
    }

    #[tokio::test]
    async fn separate_proxy_credentials_keep_literal_percent_sequences() {
        let request = capture_connect(|addr| {
            ProxyConfig::new(format!("http://{addr}")).with_auth("u%3Aser", "p%40ss%25")
        })
        .await;
        assert_eq!(proxy_authorization(&request).as_deref(), Some("u%3Aser:p%40ss%25"));
    }

    #[tokio::test]
    async fn separate_proxy_credentials_keep_reserved_characters() {
        let request = capture_connect(|addr| {
            ProxyConfig::new(format!("http://{addr}")).with_auth("user@name", "p@ss:/#? 密码")
        })
        .await;
        assert_eq!(
            proxy_authorization(&request).as_deref(),
            Some("user@name:p@ss:/#? 密码")
        );
    }

    #[tokio::test]
    async fn url_embedded_proxy_credentials_are_decoded_once() {
        let request =
            capture_connect(|addr| ProxyConfig::new(format!("http://url-user:p%2540ss@{addr}")))
                .await;
        assert_eq!(proxy_authorization(&request).as_deref(), Some("url-user:p%40ss"));
    }

    #[tokio::test]
    async fn separate_socks5_credentials_keep_literal_percent_sequences() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(tokio::time::timeout(FIXTURE_DEADLINE, async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut greeting = [0; 2];
            socket.read_exact(&mut greeting).await.unwrap();
            let mut methods = vec![0; greeting[1] as usize];
            socket.read_exact(&mut methods).await.unwrap();
            assert!(methods.contains(&2), "must offer username/password auth");
            socket.write_all(&[5, 2]).await.unwrap();
            let mut header = [0; 2];
            socket.read_exact(&mut header).await.unwrap();
            let mut username = vec![0; header[1] as usize];
            socket.read_exact(&mut username).await.unwrap();
            let mut password = vec![0; socket.read_u8().await.unwrap() as usize];
            socket.read_exact(&mut password).await.unwrap();
            // Reject authentication so nothing is forwarded.
            socket.write_all(&[1, 1]).await.unwrap();
            (username, password)
        }));
        let proxy = ProxyConfig::new(format!("socks5h://{addr}")).with_auth("s%40user", "s%3Apass");
        build_client(Some(&proxy), 5, TlsBackend::Rustls)
            .unwrap()
            .get("https://example.invalid/")
            .send()
            .await
            .expect_err("fixture rejects SOCKS authentication");
        let (username, password) = server.await.unwrap().expect("SOCKS fixture timed out");
        assert_eq!(username, b"s%40user");
        assert_eq!(password, b"s%3Apass");
    }

    #[test]
    fn test_build_client_with_proxy() {
        let config = ProxyConfig::new("http://127.0.0.1:7890");
        let client = build_client(Some(&config), 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }

    /// Serves one chunked response that stays active the whole time: a chunk,
    /// a `gap` pause, then a final chunk. Never silent for longer than `gap`,
    /// so an idle deadline above `gap` must not fire.
    async fn serve_one_slow_stream(gap: Duration) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut discard = [0u8; 1024];
            let _ = socket.read(&mut discard).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                      Transfer-Encoding: chunked\r\n\r\n2\r\nhi\r\n",
                )
                .await
                .unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(gap).await;
            socket.write_all(b"2\r\nho\r\n0\r\n\r\n").await.unwrap();
            socket.flush().await.unwrap();
        });
        addr
    }

    /// The regression: a stream that keeps delivering must not be cut by the
    /// overall deadline. 0 means "no overall deadline"; the idle deadline
    /// still guards a stalled upstream.
    #[tokio::test]
    async fn active_stream_outlives_disabled_total_timeout() {
        let addr = serve_one_slow_stream(Duration::from_secs(2)).await;
        let client = build_client_with_timeouts(None, 0, TlsBackend::Rustls, 15, 120).unwrap();

        let body = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .expect("active stream must not be cut short");
        assert_eq!(body, "hiho");
    }

    /// Serves headers and one chunk, then holds the connection open and silent.
    /// Models the case the idle deadline exists for: an upstream that stopped
    /// producing without closing the socket.
    async fn serve_one_stalled_stream() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut discard = [0u8; 1024];
            let _ = socket.read(&mut discard).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                      Transfer-Encoding: chunked\r\n\r\n2\r\nhi\r\n",
                )
                .await
                .unwrap();
            socket.flush().await.unwrap();
            // Never write again, never close: hold the socket past the test.
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        addr
    }

    /// Disabling the overall deadline must not disable stall detection: the
    /// idle deadline still cuts an upstream that went quiet. This is what makes
    /// 0 a safe setting rather than an unbounded wait.
    #[tokio::test]
    async fn stalled_stream_is_cut_by_read_timeout_without_total_timeout() {
        let addr = serve_one_stalled_stream().await;
        let client = build_client_with_timeouts(None, 0, TlsBackend::Rustls, 15, 1).unwrap();

        let err = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .expect_err("a silent upstream must still be cut by the idle deadline");
        assert!(err.is_timeout(), "expected a timeout, got: {err}");
    }

    /// A non-zero overall deadline still applies, for callers that want one.
    #[tokio::test]
    async fn total_timeout_still_applies_when_set() {
        let addr = serve_one_slow_stream(Duration::from_secs(2)).await;
        let client = build_client_with_timeouts(None, 1, TlsBackend::Rustls, 15, 120).unwrap();

        let err = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .expect_err("overall deadline must still cut the stream");
        assert!(err.is_timeout(), "expected a timeout, got: {err}");
    }
}
