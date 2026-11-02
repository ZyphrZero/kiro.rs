use std::time::Duration;

use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

#[derive(Clone, Default)]
pub struct FeishuChannel {
    pub webhook: String,
    pub secret: String,
}

impl FeishuChannel {
    pub fn validate(&self) -> Result<(), String> {
        if self.secret.len() > 1024 {
            return Err("签名密钥过长".into());
        }
        if self.webhook.is_empty() {
            return Ok(());
        }
        let valid = reqwest::Url::parse(&self.webhook).ok().is_some_and(|url| {
            url.scheme() == "https"
                && matches!(
                    url.host_str(),
                    Some("open.feishu.cn" | "open.larksuite.com")
                )
                && url.username().is_empty()
                && url.password().is_none()
                && url.port().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url
                    .path()
                    .strip_prefix("/open-apis/bot/v2/hook/")
                    .is_some_and(|token| {
                        !token.is_empty()
                            && token.len() <= 128
                            && token.bytes().all(|byte| {
                                byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
                            })
                    })
        });
        if !valid {
            return Err("Webhook 必须为飞书或 Lark 官方 HTTPS 自定义机器人地址".into());
        }
        Ok(())
    }
}

pub fn payload(text: &str, secret: &str, timestamp: i64) -> Value {
    let mut body = json!({"msg_type": "text", "content": {"text": text}});
    if !secret.is_empty() {
        // 飞书以 timestamp + 换行 + secret 为 HMAC 密钥，对空消息签名。
        let key = format!("{timestamp}\n{secret}");
        let mac =
            Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
        body["timestamp"] = json!(timestamp.to_string());
        body["sign"] =
            json!(base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()));
    }
    body
}

#[derive(Debug)]
struct SendError {
    message: String,
    retryable: bool,
}

async fn send_once(
    client: &reqwest::Client,
    channel: &FeishuChannel,
    text: &str,
) -> Result<(), SendError> {
    let mut response = client
        .post(&channel.webhook)
        .json(&payload(
            text,
            &channel.secret,
            chrono::Utc::now().timestamp(),
        ))
        .send()
        .await
        .map_err(|error| SendError {
            message: if error.is_timeout() {
                "飞书请求超时"
            } else {
                "飞书网络请求失败"
            }
            .into(),
            retryable: true,
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(SendError {
            message: format!("飞书 HTTP 状态码 {}", status.as_u16()),
            retryable: status.is_server_error() || status.as_u16() == 429,
        });
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| SendError {
        message: "读取飞书响应失败".into(),
        retryable: true,
    })? {
        if bytes.len() + chunk.len() > 16_384 {
            return Err(SendError {
                message: "飞书响应超过大小限制".into(),
                retryable: false,
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    let body: Value = serde_json::from_slice(&bytes).map_err(|_| SendError {
        message: "飞书响应不是合法 JSON".into(),
        retryable: false,
    })?;
    let code = body
        .get("code")
        .or_else(|| body.get("StatusCode"))
        .and_then(Value::as_i64);
    match code {
        Some(0) => Ok(()),
        Some(code) => Err(SendError {
            message: format!("飞书业务错误码 {code}（请检查机器人安全设置）"),
            retryable: matches!(code, 9499 | 11232),
        }),
        None => Err(SendError {
            message: "飞书响应缺少成功状态码".into(),
            retryable: false,
        }),
    }
}

pub async fn send(
    client: &reqwest::Client,
    channel: &FeishuChannel,
    text: &str,
) -> Result<(), String> {
    if channel.webhook.is_empty() {
        return Err("尚未配置飞书 Webhook".into());
    }
    for attempt in 0..3 {
        match send_once(client, channel, text).await {
            Ok(()) => return Ok(()),
            Err(error) if error.retryable && attempt < 2 => {
                tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
            }
            Err(error) => return Err(error.message),
        }
    }
    unreachable!()
}
