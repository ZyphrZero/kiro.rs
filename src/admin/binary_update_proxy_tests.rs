//! Loopback fixtures never forward a connection or contact GitHub.

use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

const DEADLINE: Duration = Duration::from_secs(5);

async fn connect_proxy() -> (SocketAddr, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        tokio::time::timeout(DEADLINE, async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut buf = [0; 1024];
                let count = socket.read(&mut buf).await.unwrap();
                assert!(count > 0, "proxy connection closed before complete headers");
                request.extend_from_slice(&buf[..count]);
                assert!(request.len() <= 16 * 1024, "unexpectedly large headers");
                if request.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            // Refuse the tunnel so neither TLS nor external network access follows.
            socket
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            String::from_utf8(request).unwrap()
        })
        .await
        .expect("proxy fixture timed out")
    });
    (address, server)
}

fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

fn assert_auth(request: &str, credentials: Option<(&str, &str)>) {
    let expected = credentials
        .map(|(user, pass)| format!("Basic {}", STANDARD.encode(format!("{user}:{pass}"))));
    // Header names are case insensitive; base64 values are case sensitive.
    assert_eq!(header(request, "Proxy-Authorization"), expected.as_deref());
    assert_eq!(header(request, "Authorization"), None);
}

async fn connect_request(config: impl FnOnce(SocketAddr) -> ProxyConfig) -> String {
    let (address, server) = connect_proxy().await;
    let proxy = config(address);
    let client = build_http_client(Some(&proxy)).unwrap();
    client
        .get("https://example.invalid/update")
        .bearer_auth("synthetic-github-token")
        .timeout(DEADLINE)
        .send()
        .await
        .expect_err("fixture must refuse the tunnel");
    let request = server.await.unwrap();
    assert!(request.starts_with("CONNECT example.invalid:443 "));
    assert!(!request.contains("synthetic-github-token"));
    request
}

#[tokio::test]
async fn updater_sends_separate_proxy_credentials_on_connect() {
    let request = connect_request(|addr| {
        ProxyConfig::new(format!("http://{addr}")).with_auth("user", "pass")
    })
    .await;
    assert_auth(&request, Some(("user", "pass")));
}

#[tokio::test]
async fn updater_supports_proxy_without_credentials() {
    let request = connect_request(|addr| ProxyConfig::new(format!("http://{addr}"))).await;
    assert_auth(&request, None);
}

#[tokio::test]
async fn updater_preserves_url_embedded_credentials() {
    let request =
        connect_request(|addr| ProxyConfig::new(format!("http://url-user:p%40ss%3Aword@{addr}")))
            .await;
    assert_auth(&request, Some(("url-user", "p@ss:word")));
}

#[tokio::test]
async fn updater_decodes_url_credentials_only_once() {
    let request =
        connect_request(|addr| ProxyConfig::new(format!("http://url-user:p%2540ss@{addr}"))).await;
    assert_auth(&request, Some(("url-user", "p%40ss")));
}

#[tokio::test]
async fn updater_separate_credentials_override_url_credentials() {
    let request = connect_request(|addr| {
        ProxyConfig::new(format!("http://old:secret@{addr}")).with_auth("new", "password")
    })
    .await;
    assert_auth(&request, Some(("new", "password")));
}

#[tokio::test]
async fn updater_partial_credentials_preserve_url_credentials() {
    let request = connect_request(|addr| {
        let mut proxy = ProxyConfig::new(format!("http://old:secret@{addr}"));
        proxy.username = Some("incomplete-override".into());
        proxy
    })
    .await;
    assert_auth(&request, Some(("old", "secret")));
}

#[tokio::test]
async fn updater_partial_credentials_do_not_invent_authentication() {
    for username_only in [true, false] {
        let request = connect_request(|addr| {
            let mut proxy = ProxyConfig::new(format!("http://{addr}"));
            if username_only {
                proxy.username = Some("user".into());
            } else {
                proxy.password = Some("pass".into());
            }
            proxy
        })
        .await;
        assert_auth(&request, None);
    }
}

#[tokio::test]
async fn updater_supports_an_explicit_empty_password() {
    let request =
        connect_request(|addr| ProxyConfig::new(format!("http://{addr}")).with_auth("user", ""))
            .await;
    assert_auth(&request, Some(("user", "")));
}

#[tokio::test]
async fn updater_preserves_special_characters_in_separate_credentials() {
    let request = connect_request(|addr| {
        ProxyConfig::new(format!(" http://{addr} ")).with_auth("user@name", "p@ss:@/#? 密码")
    })
    .await;
    assert_auth(&request, Some(("user@name", "p@ss:@/#? 密码")));
}

#[test]
fn updater_accepts_missing_and_blank_proxy_configurations() {
    assert!(build_http_client(None).is_ok());
    assert!(build_http_client(Some(&ProxyConfig::new(" \t "))).is_ok());
}

#[test]
fn updater_rejects_invalid_proxy_without_exposing_credentials() {
    let proxy = ProxyConfig::new("http://fixture-user:fixture-password@127.0.0.1:invalid")
        .with_auth("separate-user", "separate-password");
    let error = build_http_client(Some(&proxy)).unwrap_err().to_string();
    for secret in [
        "fixture-user",
        "fixture-password",
        "separate-user",
        "separate-password",
    ] {
        assert!(
            !error.contains(secret),
            "error must not include credentials"
        );
    }
}

async fn socks_auth_request(scheme: &str, target: &str) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        tokio::time::timeout(DEADLINE, async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut greeting = [0; 2];
            socket.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting[0], 5);
            let mut methods = vec![0; greeting[1] as usize];
            socket.read_exact(&mut methods).await.unwrap();
            assert!(
                methods.contains(&2),
                "SOCKS5 must offer username/password authentication"
            );
            socket.write_all(&[5, 2]).await.unwrap();
            let mut auth = [0; 2];
            socket.read_exact(&mut auth).await.unwrap();
            assert_eq!(auth[0], 1);
            let mut username = vec![0; auth[1] as usize];
            socket.read_exact(&mut username).await.unwrap();
            let password_len = socket.read_u8().await.unwrap();
            let mut password = vec![0; password_len as usize];
            socket.read_exact(&mut password).await.unwrap();
            socket.write_all(&[1, 1]).await.unwrap();
            (username, password)
        })
        .await
        .expect("SOCKS5 fixture timed out")
    });
    let proxy =
        ProxyConfig::new(format!("{scheme}://{address}")).with_auth("socks@user", "socks:pass");
    build_http_client(Some(&proxy))
        .unwrap()
        .get(target)
        .timeout(DEADLINE)
        .send()
        .await
        .expect_err("fixture must reject SOCKS authentication");
    let (username, password) = server.await.unwrap();
    assert_eq!(username, b"socks@user");
    assert_eq!(password, b"socks:pass");
}

#[tokio::test]
async fn updater_supports_separate_socks5_credentials() {
    socks_auth_request("socks5", "https://192.0.2.1/update").await;
}

#[tokio::test]
async fn updater_supports_separate_socks5h_credentials() {
    socks_auth_request("socks5h", "https://example.invalid/update").await;
}

fn admin_service(proxy: ProxyConfig) -> crate::admin::service::AdminService {
    let mut config = crate::model::config::Config::default();
    config.github_token = Some("synthetic-github-token".into());
    let manager = crate::kiro::token_manager::MultiTokenManager::new(
        config,
        Vec::new(),
        Some(proxy),
        None,
        false,
    )
    .unwrap();
    crate::admin::service::AdminService::new(Arc::new(manager), Vec::new())
}

#[tokio::test]
async fn updater_release_check_uses_global_proxy_credentials() {
    let (address, server) = connect_proxy().await;
    let service =
        admin_service(ProxyConfig::new(format!("http://{address}")).with_auth("user", "pass"));
    let result = tokio::time::timeout(DEADLINE, service.check_update(true))
        .await
        .unwrap();
    assert!(result.warning.is_some(), "fixture refuses the tunnel");
    let request = server.await.unwrap();
    assert!(request.starts_with("CONNECT api.github.com:443 "));
    assert_auth(&request, Some(("user", "pass")));
    assert!(!request.contains("synthetic-github-token"));
}

#[tokio::test]
async fn updater_rate_limit_uses_global_proxy_credentials() {
    let (address, server) = connect_proxy().await;
    let service =
        admin_service(ProxyConfig::new(format!("http://{address}")).with_auth("user", "pass"));
    let result = tokio::time::timeout(
        DEADLINE,
        service.check_rate_limit(crate::admin::types::CheckRateLimitRequest { github_token: None }),
    )
    .await
    .unwrap();
    assert!(!result.valid, "fixture refuses the tunnel");
    assert!(result.authenticated);
    assert!(result.warning.is_some());
    let request = server.await.unwrap();
    assert!(request.starts_with("CONNECT api.github.com:443 "));
    assert_auth(&request, Some(("user", "pass")));
    assert!(!request.contains("synthetic-github-token"));
}

#[tokio::test]
async fn updater_archive_and_checksum_requests_use_proxy_authentication() {
    for checksum in [false, true] {
        let (address, server) = connect_proxy().await;
        let proxy = ProxyConfig::new(format!("http://{address}")).with_auth("user", "pass");
        let client = build_http_client(Some(&proxy)).unwrap();
        let unused_path =
            std::env::temp_dir().join(format!("updater-proxy-test-{}", uuid::Uuid::new_v4()));
        let result = tokio::time::timeout(DEADLINE, async {
            if checksum {
                verify_checksum(
                    &client,
                    "https://github.com/fixture/SHA256SUMS.txt",
                    Some("synthetic-github-token"),
                    "fixture.tar.gz",
                    &unused_path,
                )
                .await
            } else {
                download_to_file(
                    &client,
                    "https://github.com/fixture/archive.tar.gz",
                    Some("synthetic-github-token"),
                    &unused_path,
                )
                .await
            }
        })
        .await
        .unwrap();
        assert!(result.is_err(), "fixture refuses the tunnel");
        assert!(
            !unused_path.exists(),
            "failed download must not write a file"
        );
        let request = server.await.unwrap();
        assert!(request.starts_with("CONNECT github.com:443 "));
        assert_auth(&request, Some(("user", "pass")));
        assert!(!request.contains("synthetic-github-token"));
    }
}
