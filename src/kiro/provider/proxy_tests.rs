use super::*;
use crate::admin::AdminService;
use crate::model::config::Config;
use axum::{Router, body::Body, http::HeaderMap, response::Response};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::StreamExt;
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle, time::timeout};

// These servers act as both the target and HTTP proxy stubs. All requests stay
// on loopback; the synthetic API credential never reaches a real Kiro endpoint.
struct LocalServer {
    url: String,
    task: JoinHandle<()>,
}

impl LocalServer {
    async fn serve(app: Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { url, task }
    }

    async fn named(name: &'static str) -> Self {
        Self::serve(
            Router::new().fallback(move |headers: HeaderMap| async move {
                let auth = headers
                    .get("proxy-authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("");
                format!("{name}|{auth}")
            }),
        )
        .await
    }

    fn proxy(&self) -> ProxyConfig {
        ProxyConfig::new(&self.url)
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct LocalEndpoint(String);

impl KiroEndpoint for LocalEndpoint {
    fn name(&self) -> &'static str {
        "loopback"
    }

    fn api_url(&self, _: &RequestContext<'_>) -> String {
        self.0.clone()
    }

    fn mcp_url(&self, _: &RequestContext<'_>) -> String {
        self.0.clone()
    }

    fn decorate_api(
        &self,
        request: reqwest::RequestBuilder,
        _: &RequestContext<'_>,
    ) -> reqwest::RequestBuilder {
        request
    }

    fn decorate_mcp(
        &self,
        request: reqwest::RequestBuilder,
        _: &RequestContext<'_>,
    ) -> reqwest::RequestBuilder {
        request
    }

    fn transform_api_body(&self, body: &str, _: &RequestContext<'_>) -> String {
        body.to_string()
    }
}

fn setup(target: &LocalServer, proxy: Option<ProxyConfig>) -> (KiroProvider, AdminService) {
    let credential = KiroCredentials {
        id: Some(1),
        auth_method: Some("api_key".to_string()),
        kiro_api_key: Some("ksk_offline_proxy_test".to_string()),
        ..Default::default()
    };
    let manager = Arc::new(
        MultiTokenManager::new(Config::default(), vec![credential], proxy, None, false).unwrap(),
    );
    let endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::from([(
        "loopback".to_string(),
        Arc::new(LocalEndpoint(target.url.clone())) as Arc<dyn KiroEndpoint>,
    )]);
    let provider = KiroProvider::new(manager.clone(), endpoints, "loopback".to_string());
    (provider, AdminService::new(manager, Vec::new()))
}

fn set_proxy(service: &AdminService, proxy: Option<ProxyConfig>) {
    let proxy = proxy.unwrap_or_default();
    service
        .set_global_proxy(
            (!proxy.url.is_empty()).then_some(proxy.url),
            proxy.username,
            Some(proxy.password),
        )
        .unwrap();
}

async fn model_response(provider: &KiroProvider) -> String {
    timeout(Duration::from_secs(5), async {
        provider
            .call_api("{}", None, None)
            .await
            .unwrap()
            .response
            .text()
            .await
            .unwrap()
    })
    .await
    .expect("loopback model request timed out")
}

async fn credential_response(
    provider: &KiroProvider,
    credential: &KiroCredentials,
    target: &LocalServer,
) -> String {
    timeout(Duration::from_secs(5), async {
        provider
            .client_for(credential)
            .unwrap()
            .get(&target.url)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    })
    .await
    .expect("loopback credential request timed out")
}

#[test]
fn client_cache_preserves_capacity_across_proxy_promotion_and_demotion() {
    let client = Client::builder().no_proxy().build().unwrap();
    let key = |port| Some(ProxyConfig::new(format!("http://127.0.0.1:{port}")));
    let original = key(31001);
    let first = key(31002);
    let promoted = key(31003);
    let newest = key(31004);
    let mut cache = ClientCache::new(original.clone(), client.clone(), 2);

    cache.insert(first.clone(), client.clone());
    cache.insert(promoted.clone(), client.clone());
    cache.insert(newest.clone(), client.clone());
    assert!(cache.get(&first).is_none());
    assert!(cache.get(&original).is_some());
    assert_eq!(cache.map.len(), 3);

    // A previously evictable credential proxy becomes the global proxy. Its
    // old queue position must not cause it to be evicted by subsequent inserts.
    cache.set_global_proxy(promoted.clone());
    assert!(cache.get(&original).is_none());
    cache.insert(original.clone(), client.clone());
    cache.insert(first.clone(), client.clone());
    cache.insert(newest.clone(), client.clone());
    assert!(cache.get(&promoted).is_some());
    assert!(cache.get(&original).is_none());
    assert!(cache.get(&first).is_some());
    assert!(cache.get(&newest).is_some());
    assert_eq!(cache.map.len(), 3);

    // Clearing the global proxy protects the direct client. The former global
    // proxy, if requested again by a credential, must be eligible for eviction.
    cache.set_global_proxy(None);
    cache.insert(None, client.clone());
    cache.insert(promoted.clone(), client.clone());
    cache.insert(original, client.clone());
    cache.insert(first, client);
    assert!(cache.get(&None).is_some());
    assert!(cache.get(&promoted).is_none());
    assert_eq!(cache.map.len(), 3);
}

#[tokio::test]
async fn hot_update_routes_model_stream_and_mcp_through_new_proxy() {
    let target = LocalServer::named("direct").await;
    let old = LocalServer::named("old").await;
    let new = LocalServer::named("new").await;
    let (provider, service) = setup(&target, Some(old.proxy()));

    assert_eq!(model_response(&provider).await, "old|");
    set_proxy(&service, Some(new.proxy()));
    assert_eq!(model_response(&provider).await, "new|");
    let streamed = timeout(Duration::from_secs(5), async {
        provider
            .call_api_stream("{}", None, None)
            .await
            .unwrap()
            .response
            .text()
            .await
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(streamed, "new|");
    let mcp = timeout(Duration::from_secs(5), async {
        provider
            .call_mcp("{}", None)
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(mcp, "new|");
}

#[tokio::test]
async fn hot_update_applies_password_and_username_changes_at_same_address() {
    let target = LocalServer::named("direct").await;
    let proxy = LocalServer::named("proxy").await;
    let (provider, service) = setup(
        &target,
        Some(proxy.proxy().with_auth("test-user", "before")),
    );

    assert_eq!(
        model_response(&provider).await,
        format!("proxy|Basic {}", STANDARD.encode("test-user:before"))
    );
    set_proxy(
        &service,
        Some(proxy.proxy().with_auth("test-user", "after")),
    );
    assert_eq!(
        model_response(&provider).await,
        format!("proxy|Basic {}", STANDARD.encode("test-user:after"))
    );
    // Omitting the password retains it, while the username changes immediately.
    service
        .set_global_proxy(Some(proxy.url.clone()), Some("new-user".to_string()), None)
        .unwrap();
    assert_eq!(
        model_response(&provider).await,
        format!("proxy|Basic {}", STANDARD.encode("new-user:after"))
    );
}

#[tokio::test]
async fn hot_update_can_enable_and_clear_proxy_without_overriding_credentials() {
    let target = LocalServer::named("direct").await;
    let first = LocalServer::named("first").await;
    let second = LocalServer::named("second").await;
    let own = LocalServer::named("own").await;
    let (provider, service) = setup(&target, None);
    let own_credential = KiroCredentials {
        proxy_url: Some(own.url.clone()),
        ..Default::default()
    };
    let direct_credential = KiroCredentials {
        proxy_url: Some("direct".to_string()),
        ..Default::default()
    };

    for (proxy, expected) in [
        (None, "direct|"),
        (Some(first.proxy()), "first|"),
        (Some(second.proxy()), "second|"),
        (None, "direct|"),
    ] {
        set_proxy(&service, proxy);
        assert_eq!(model_response(&provider).await, expected);
        assert_eq!(
            credential_response(&provider, &own_credential, &target).await,
            "own|"
        );
        assert_eq!(
            credential_response(&provider, &direct_credential, &target).await,
            "direct|"
        );
    }
}

#[tokio::test]
async fn hot_update_keeps_in_flight_response_alive() {
    let target = LocalServer::named("direct").await;
    let new = LocalServer::named("new").await;
    let (finish, finished) = oneshot::channel();
    let finished = Arc::new(Mutex::new(Some(finished)));
    let old = LocalServer::serve(Router::new().fallback(move || {
        let finished = finished.lock().take().unwrap();
        async move {
            let first = futures::stream::once(async { Ok::<_, std::io::Error>("old-") });
            let tail = futures::stream::once(async move {
                finished.await.unwrap();
                Ok::<_, std::io::Error>("complete")
            });
            Response::new(Body::from_stream(first.chain(tail)))
        }
    }))
    .await;
    let (provider, service) = setup(&target, Some(old.proxy()));
    let mut response = timeout(
        Duration::from_secs(5),
        provider.call_api_stream("{}", None, None),
    )
    .await
    .unwrap()
    .unwrap()
    .response;
    let first = timeout(Duration::from_secs(5), response.chunk())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(&first[..], b"old-");

    set_proxy(&service, Some(new.proxy()));
    assert_eq!(model_response(&provider).await, "new|");
    finish.send(()).unwrap();
    assert_eq!(
        timeout(Duration::from_secs(5), response.text())
            .await
            .unwrap()
            .unwrap(),
        "complete"
    );
}

#[tokio::test]
async fn retry_uses_proxy_changed_during_previous_attempt() {
    let target = LocalServer::named("direct").await;
    let new = LocalServer::named("new").await;
    let (arrived, arrival) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let gates = Arc::new(Mutex::new(Some((arrived, released))));
    let old = LocalServer::serve(Router::new().fallback(move || {
        let gates = gates.lock().take();
        async move {
            if let Some((arrived, released)) = gates {
                arrived.send(()).unwrap();
                released.await.unwrap();
            }
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "offline proxy test",
            )
        }
    }))
    .await;
    let (provider, service) = setup(&target, Some(old.proxy()));
    let (result, ()) = tokio::join!(model_response(&provider), async {
        timeout(Duration::from_secs(5), arrival)
            .await
            .unwrap()
            .unwrap();
        set_proxy(&service, Some(new.proxy()));
        release.send(()).unwrap();
    });
    assert_eq!(result, "new|");
}
