use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{Extension, Json, extract::State};
use futures::StreamExt;
use serde_json::{Value, json};

use super::{
    handlers::post_messages,
    middleware::{AppState, KeyContext},
};
use crate::{
    admin::trace_db::{TraceKeySource, TraceQuery, TraceStore},
    kiro::{
        endpoint::{CliEndpoint, IdeEndpoint, KiroEndpoint},
        model::credentials::CredentialsConfig,
        provider::KiroProvider,
        token_manager::MultiTokenManager,
    },
    model::config::{Config, ToolCompatibilityMode},
};

#[tokio::test]
#[ignore = "requires explicit KIRO_RS_LIVE_CONFIG, KIRO_RS_LIVE_CREDENTIALS and KIRO_RS_LIVE_MODEL; consumes upstream quota"]
async fn live_tool_modes_preserve_input_and_measure_streaming_without_execution() {
    let config_path = std::env::var("KIRO_RS_LIVE_CONFIG").expect("set KIRO_RS_LIVE_CONFIG");
    let credentials_path =
        std::env::var("KIRO_RS_LIVE_CREDENTIALS").expect("set KIRO_RS_LIVE_CREDENTIALS");
    let model = std::env::var("KIRO_RS_LIVE_MODEL").expect("set KIRO_RS_LIVE_MODEL");
    let config_bytes = std::fs::read(&config_path).expect("read live config");
    let credentials_bytes = std::fs::read(&credentials_path).expect("read live credentials");
    let config: Config = serde_json::from_slice(&config_bytes).expect("parse live config");
    crate::model::custom_models::init(config.custom_models.clone());
    let credentials: CredentialsConfig =
        serde_json::from_slice(&credentials_bytes).expect("parse live credentials");
    let credentials = credentials.into_sorted_credentials();
    for mode in [
        ToolCompatibilityMode::Raw,
        ToolCompatibilityMode::ClaudeCode,
    ] {
        let mut mode_config = config.clone();
        mode_config.tool_compatibility_mode = mode;
        let manager = Arc::new(
            MultiTokenManager::new(mode_config, credentials.clone(), None, None, false)
                .expect("initialize in-memory credentials without persistence"),
        );
        let endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::from([
            (
                "ide".to_owned(),
                Arc::new(IdeEndpoint::new()) as Arc<dyn KiroEndpoint>,
            ),
            (
                "cli".to_owned(),
                Arc::new(CliEndpoint::new()) as Arc<dyn KiroEndpoint>,
            ),
        ]);
        let provider = Arc::new(KiroProvider::new(
            manager,
            endpoints,
            config.default_endpoint.clone(),
        ));
        let trace = Arc::new(TraceStore::open_in_memory().expect("initialize in-memory trace"));
        let payload = json!({
            "model":model, "max_tokens":2048, "stream":true,
            "thinking":{"type":"disabled"},
            "messages":[{"role":"user","content":
                "Call Write exactly once. Use file_path /diagnostic-not-executed.txt. Set diagnostic_marker to passthrough-verified. For content write 30 numbered lines about testing streaming. Do not output prose outside the tool call. The client only inspects arguments and will not execute the tool."}],
            "metadata":{"user_id":format!("user_live_test_account__session_{}", uuid::Uuid::new_v4())},
            "tools":[{
                "name":"Write", "description":"Return the supplied file path, text content and diagnostic marker.",
                "input_schema":{
                    "type":"object",
                    "properties":{
                        "file_path":{"type":"string"},
                        "content":{"type":"string"},
                        "diagnostic_marker":{"type":"string","enum":["passthrough-verified"]}
                    },
                    "required":["file_path","content","diagnostic_marker"],
                    "additionalProperties":false
                }
            }]
        });
        let started = std::time::Instant::now();
        let response = tokio::time::timeout(
            Duration::from_secs(120),
            post_messages(
                State(
                    AppState::new(true, mode)
                        .with_shared_kiro_provider(provider.clone())
                        .with_trace_store(Some(trace.clone())),
                ),
                Extension(KeyContext {
                    key_id: 0,
                    group: None,
                    key_source: TraceKeySource::MasterApiKey,
                    client_ip: None,
                }),
                Json(serde_json::from_value(payload).unwrap()),
            ),
        )
        .await
        .expect("upstream response deadline");
        if !response.status().is_success() {
            let (records, _) = trace.query_paged(&TraceQuery {
                limit: 1,
                ..Default::default()
            });
            for record in records {
                for attempt in record.attempts {
                    let detail = attempt
                        .error_snippet
                        .as_deref()
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    println!(
                        "LIVE_FAILURE mode={mode:?} final_type={:?} attempt_outcome={} upstream_status={:?} timeout={} connect={} tls={} profile={} refresh={}",
                        record.error_type,
                        attempt.outcome,
                        attempt.http_status,
                        detail.contains("timed out") || detail.contains("timeout"),
                        detail.contains("connect"),
                        detail.contains("tls") || detail.contains("certificate"),
                        detail.contains("profile"),
                        detail.contains("refresh"),
                    );
                }
            }
        }
        assert!(
            response.status().is_success(),
            "mode={mode:?}, HTTP status={}",
            response.status()
        );
        let mut body = response.into_body().into_data_stream();
        let mut fragments: HashMap<i64, String> = HashMap::new();
        let mut names = HashMap::new();
        let mut first_fragment = None;
        let mut last_fragment = None;
        let mut fragment_count = 0;
        let mut stopped = false;
        let mut pending = String::new();
        tokio::time::timeout(Duration::from_secs(120), async {
            while let Some(chunk) = body.next().await {
                pending.push_str(std::str::from_utf8(&chunk.expect("read live stream")).unwrap());
                while let Some(end) = pending.find("\n\n") {
                    let block: String = pending.drain(..end + 2).collect();
                    let Some(data) = block.lines().find_map(|line| line.strip_prefix("data: "))
                    else {
                        continue;
                    };
                    let event: Value = serde_json::from_str(data).unwrap();
                    assert_ne!(
                        event["type"], "error",
                        "upstream reported a streaming error in {mode:?}"
                    );
                    if event["content_block"]["type"] == "tool_use" {
                        names.insert(
                            event["index"].as_i64().unwrap(),
                            event["content_block"]["name"].as_str().unwrap().to_owned(),
                        );
                    }
                    if event["delta"]["type"] == "input_json_delta" {
                        first_fragment.get_or_insert(started.elapsed());
                        last_fragment = Some(started.elapsed());
                        fragment_count += 1;
                        fragments
                            .entry(event["index"].as_i64().unwrap())
                            .or_default()
                            .push_str(event["delta"]["partial_json"].as_str().unwrap());
                    }
                    stopped |= event["type"] == "message_stop";
                }
            }
        })
        .await
        .expect("upstream streaming deadline");
        assert!(stopped, "missing message_stop in {mode:?}");
        assert_eq!(names.len(), 1, "expected exactly one tool call in {mode:?}");
        let (index, name) = names.iter().next().unwrap();
        assert_eq!(name, "Write");
        let input: Value = serde_json::from_str(fragments.get(index).expect("tool arguments"))
            .expect("complete tool JSON");
        assert_eq!(input["file_path"], "/diagnostic-not-executed.txt");
        assert!(
            input["content"]
                .as_str()
                .is_some_and(|text| text.len() > 200)
        );
        if mode == ToolCompatibilityMode::Raw {
            assert_eq!(input["diagnostic_marker"], "passthrough-verified");
            assert!(
                fragment_count > 1,
                "raw tool input did not stream incrementally"
            );
        }
        println!(
            "LIVE mode={mode:?} fragments={fragment_count} first_ms={} last_ms={} total_ms={} custom_field_preserved={} tools_executed=false",
            first_fragment.unwrap().as_millis(),
            last_fragment.unwrap().as_millis(),
            started.elapsed().as_millis(),
            input.get("diagnostic_marker").is_some()
        );
    }
    assert!(
        std::fs::read(config_path).unwrap() == config_bytes,
        "live config was changed"
    );
    assert!(
        std::fs::read(credentials_path).unwrap() == credentials_bytes,
        "live credentials file was changed"
    );
}
