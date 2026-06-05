//! 可用 Profile 查询数据模型
//!
//! 包含 ListAvailableProfiles API 的响应类型定义。
//!
//! 上游接口（AWS JSON 1.0 协议）：
//! `POST https://q.{region}.amazonaws.com/`
//! Header: `x-amz-target: AmazonCodeWhispererService.ListAvailableProfiles`
//!
//! 返回当前凭据可访问的 Profile 列表。企业 IdC 账号的真实 profileArn
//! 只能通过此接口获取（KAM 导出与 IdC 刷新响应均不包含），用于替代
//! 硬编码占位 ARN，避免上游因 profileArn 不匹配返回 403。

use serde::Deserialize;

/// ListAvailableProfiles API 响应
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListAvailableProfilesResponse {
    /// 可用 Profile 列表
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

/// 单个 Profile
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    /// Profile ARN（含真实 AWS 账号 ID 与 region，如
    /// `arn:aws:codewhisperer:us-east-1:610548660232:profile/VNECVYCYYAWN`）
    pub arn: String,

    /// Profile 展示名（如 "KiroProfile-us-east-1"，可能不存在）
    /// 当前流程不依赖具体值，仅用于完整反序列化与排错。
    #[allow(dead_code)]
    #[serde(default)]
    pub profile_name: Option<String>,
}

impl Profile {
    /// 从 ARN 中解析出 region。
    ///
    /// ARN 格式：`arn:aws:codewhisperer:{region}:{account}:profile/{id}`
    /// 第 4 段（0-based index 3）即为 region。解析失败返回 None。
    pub fn region_from_arn(&self) -> Option<&str> {
        let region = self.arn.split(':').nth(3)?;
        if region.is_empty() {
            None
        } else {
            Some(region)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_full_response() {
        let json = r#"{
            "profiles": [
                {
                    "__type": "com.amazon.aws.codewhisperer#Profile",
                    "arn": "arn:aws:codewhisperer:us-east-1:610548660232:profile/VNECVYCYYAWN",
                    "identityDetails": {
                        "ssoIdentityDetails": {
                            "instanceArn": "arn:aws:sso:::instance/ssoins-7223474f9d1bdc7e",
                            "ssoRegion": "us-east-1"
                        }
                    },
                    "profileName": "KiroProfile-us-east-1"
                }
            ]
        }"#;
        let resp: ListAvailableProfilesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.profiles.len(), 1);
        let p = &resp.profiles[0];
        assert_eq!(
            p.arn,
            "arn:aws:codewhisperer:us-east-1:610548660232:profile/VNECVYCYYAWN"
        );
        assert_eq!(p.profile_name.as_deref(), Some("KiroProfile-us-east-1"));
        assert_eq!(p.region_from_arn(), Some("us-east-1"));
    }

    #[test]
    fn test_empty_profiles() {
        let json = r#"{"profiles": []}"#;
        let resp: ListAvailableProfilesResponse = serde_json::from_str(json).unwrap();
        assert!(resp.profiles.is_empty());
    }

    #[test]
    fn test_missing_profiles_field() {
        // profiles 字段缺失时回退为空 Vec
        let json = r#"{}"#;
        let resp: ListAvailableProfilesResponse = serde_json::from_str(json).unwrap();
        assert!(resp.profiles.is_empty());
    }

    #[test]
    fn test_region_from_arn_malformed() {
        let p = Profile {
            arn: "not-an-arn".to_string(),
            profile_name: None,
        };
        assert_eq!(p.region_from_arn(), None);
    }
}
