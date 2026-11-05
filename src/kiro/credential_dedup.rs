use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::model::credentials::KiroCredentials;

#[derive(Hash, PartialEq, Eq)]
enum Identity {
    ApiKey([u8; 32]),
    RefreshToken([u8; 32]),
}

fn identity(credential: &KiroCredentials) -> Option<Identity> {
    if credential.is_api_key_credential() {
        credential
            .kiro_api_key
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(|value| Identity::ApiKey(Sha256::digest(value.as_bytes()).into()))
    } else {
        credential
            .refresh_token
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(|value| Identity::RefreshToken(Sha256::digest(value.as_bytes()).into()))
    }
}

pub fn duplicate_groups(credentials: &[KiroCredentials]) -> Vec<Vec<usize>> {
    let mut positions = HashMap::new();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (index, credential) in credentials.iter().enumerate() {
        if let Some(identity) = identity(credential) {
            let group = *positions.entry(identity).or_insert_with(|| {
                groups.push(Vec::new());
                groups.len() - 1
            });
            groups[group].push(index);
        }
    }
    groups.into_iter().filter(|group| group.len() > 1).collect()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateGroup {
    kind: &'static str,
    keep_position: usize,
    keep_id: Option<u64>,
    remove_positions: Vec<usize>,
    remove_ids: Vec<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DedupReport {
    applied: bool,
    original_count: usize,
    retained_count: usize,
    groups: Vec<DuplicateGroup>,
    backup: Option<PathBuf>,
    stats_backup: Option<PathBuf>,
}

#[derive(Deserialize, Default)]
struct CredentialStats {
    #[serde(default)]
    success_count: u64,
    #[serde(default)]
    total_failure_count: u64,
    last_used_at: Option<String>,
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

pub fn reserved_id(path: &Path) -> anyhow::Result<u64> {
    let path = sidecar(path, ".id-watermark");
    match fs::read_to_string(&path) {
        Ok(value) => value
            .trim()
            .parse()
            .with_context(|| format!("无法解析凭据 ID 保留文件 {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error).context("无法读取凭据 ID 保留文件"),
    }
}

fn write_private(path: &Path, content: &[u8]) -> anyhow::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).context("无法创建去重备份或临时文件")?;
    file.write_all(content)
        .context("写入去重备份或临时文件失败")?;
    file.sync_all().context("同步去重备份或临时文件失败")
}

fn replace_file(
    path: &Path,
    content: &[u8],
    permissions: Option<fs::Permissions>,
) -> anyhow::Result<()> {
    let temporary = sidecar(path, &format!(".dedup-{}.tmp", uuid::Uuid::new_v4()));
    let mut temporary_permissions = None;
    let result = (|| {
        write_private(&temporary, content)?;
        temporary_permissions = Some(fs::metadata(&temporary)?.permissions());
        if let Some(permissions) = permissions {
            fs::set_permissions(&temporary, permissions)?;
        }
        fs::rename(&temporary, path).context("原子替换去重文件失败")
    })();
    if result.is_err() {
        if let Some(permissions) = temporary_permissions {
            let _ = fs::set_permissions(&temporary, permissions);
        }
        let _ = fs::remove_file(temporary);
    }
    result
}

pub fn run(path: &Path, apply: bool) -> anyhow::Result<DedupReport> {
    let original = fs::read(path).context("读取凭据文件失败")?;
    let document: Value = serde_json::from_slice(&original).context("凭据文件不是有效 JSON")?;
    let records = match document {
        Value::Array(records) => records,
        Value::Object(record) => vec![Value::Object(record)],
        _ => bail!("凭据文件必须是对象或数组"),
    };
    let credentials: Vec<KiroCredentials> = records
        .iter()
        .enumerate()
        .map(|(index, value)| {
            serde_json::from_value(value.clone())
                .map_err(|_| anyhow::anyhow!("第 {} 条凭据的字段格式无效", index + 1))
        })
        .collect::<anyhow::Result<_>>()?;
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stats_path = directory.join("kiro_stats.json");
    let stats_bytes = match fs::read(&stats_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("读取凭据使用统计失败"),
    };
    let stats: HashMap<String, CredentialStats> = stats_bytes
        .as_deref()
        .map(serde_json::from_slice)
        .transpose()
        .map_err(|_| anyhow::anyhow!("凭据使用统计格式无效，无法安全选择保留项"))?
        .unwrap_or_default();
    let rank = |index: usize| {
        let credential = &credentials[index];
        let stats = credential.id.and_then(|id| stats.get(&id.to_string()));
        let success_count = stats.map_or(0, |entry| entry.success_count);
        let failure_count = stats.map_or(0, |entry| entry.total_failure_count);
        let used = success_count > 0
            || failure_count > 0
            || stats.is_some_and(|entry| entry.last_used_at.is_some());
        let created = credential
            .created_at
            .as_deref()
            .is_some_and(|value| chrono::DateTime::parse_from_rfc3339(value).is_ok());
        (
            used,
            created,
            success_count,
            Reverse(credential.id.unwrap_or(u64::MAX)),
            Reverse(index),
        )
    };
    let mut removed = HashSet::new();
    let mut groups = Vec::new();
    for duplicates in duplicate_groups(&credentials) {
        let keep = *duplicates.iter().max_by_key(|index| rank(**index)).unwrap();
        let remove: Vec<_> = duplicates
            .into_iter()
            .filter(|index| *index != keep)
            .collect();
        removed.extend(remove.iter().copied());
        groups.push(DuplicateGroup {
            kind: if credentials[keep].is_api_key_credential() {
                "kiroApiKey"
            } else {
                "refreshToken"
            },
            keep_position: keep + 1,
            keep_id: credentials[keep].id,
            remove_positions: remove.iter().map(|index| index + 1).collect(),
            remove_ids: remove
                .iter()
                .filter_map(|index| credentials[*index].id)
                .collect(),
        });
    }
    let mut report = DedupReport {
        applied: false,
        original_count: records.len(),
        retained_count: records.len() - removed.len(),
        groups,
        backup: None,
        stats_backup: None,
    };
    if !apply || removed.is_empty() {
        return Ok(report);
    }
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        bail!("清理仅支持普通文件，请显式指定凭据实际文件路径");
    }
    let mut retained_ids = HashSet::new();
    for (index, credential) in credentials.iter().enumerate() {
        if !removed.contains(&index)
            && let Some(id) = credential.id
            && !retained_ids.insert(id)
        {
            bail!("去重后仍存在重复凭据 ID #{id}，请先修复 ID 冲突");
        }
    }
    let watermark = credentials
        .iter()
        .filter_map(|credential| credential.id)
        .max()
        .unwrap_or(0)
        .max(reserved_id(path)?);
    if watermark == u64::MAX {
        bail!("凭据 ID 已耗尽，无法安全保留去重后的 ID 分配边界");
    }
    let retained: Vec<_> = records
        .into_iter()
        .enumerate()
        .filter_map(|(index, record)| (!removed.contains(&index)).then_some(record))
        .collect();
    let content = serde_json::to_vec_pretty(&retained)?;
    let suffix = format!(".pre-dedup-{}.bak", uuid::Uuid::new_v4());
    let backup = sidecar(path, &suffix);
    write_private(&backup, &original)?;
    report.backup = Some(backup.clone());
    if let Some(stats_bytes) = stats_bytes {
        let stats_backup = sidecar(&stats_path, &suffix);
        write_private(&stats_backup, &stats_bytes)?;
        report.stats_backup = Some(stats_backup);
    }
    if fs::read(path)? != original {
        bail!("凭据文件在预览后发生变化，已停止清理；请停止所有写入进程后重试");
    }
    replace_file(
        &sidecar(path, ".id-watermark"),
        format!("{watermark}\n").as_bytes(),
        None,
    )?;
    replace_file(path, &content, Some(fs::metadata(path)?.permissions()))
        .with_context(|| format!("清理未完成，原始凭据备份位于 {}", backup.display()))?;
    report.applied = true;
    Ok(report)
}

#[cfg(test)]
mod tests;
