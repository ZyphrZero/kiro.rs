//! 请求链路追踪（Trace）持久化
//!
//! 记录每次 `/v1/messages` 请求的完整重试链路，用于排查"中断"类问题：
//! - 一个外部请求 = 1 条 [`TraceRecord`] 汇总 + N 条 [`TraceAttempt`] 子记录
//! - 每跳记录命中凭据、HTTP 状态码、失败分类、上游错误体片段、耗时
//!
//! 存储：SQLite（`traces.db`），WAL 模式。前端查询直接走 SQL（索引 + WHERE + LIMIT），
//! 不维护内存缓冲。后台任务定期清理超过保留天数的记录（保留天数与启用开关运行时可改）。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use chrono::Utc;
use parking_lot::Mutex;
use rusqlite::{Connection, types::Type};
use serde::{Deserialize, Serialize};

/// trace 记录默认保留天数
const DEFAULT_RETENTION_DAYS: u64 = 7;
/// 上游错误体片段最大长度（字节）
const ERROR_SNIPPET_MAX: usize = 2048;
/// 查询默认返回条数
pub const DEFAULT_QUERY_LIMIT: usize = 200;

/// 单次上游尝试的结果
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceAttempt {
    /// 第几次尝试（0-based）
    pub attempt: u32,
    /// 命中的上游凭据 id；0 表示未取到凭据
    pub credential_id: u64,
    /// 端点名（ide / cli）
    pub endpoint: String,
    /// 上游 HTTP 状态码；None 表示网络层失败（请求未发出/无响应）
    pub http_status: Option<u16>,
    /// 失败分类，见 [`Outcome`]
    pub outcome: String,
    /// 上游错误体片段（截断到 [`ERROR_SNIPPET_MAX`]）
    pub error_snippet: Option<String>,
    /// 本跳耗时（毫秒）
    pub duration_ms: u64,
}

/// 调用方使用的入口 Key 类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TraceKeySource {
    /// 管理员API密钥。
    MasterApiKey,
    /// Admin UI 中创建并分发的客户端 Key。
    ClientKey,
}

impl TraceKeySource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MasterApiKey => "masterApiKey",
            Self::ClientKey => "clientKey",
        }
    }

    fn from_db(value: &str, column: usize) -> rusqlite::Result<Self> {
        match value {
            "masterApiKey" => Ok(Self::MasterApiKey),
            "clientKey" => Ok(Self::ClientKey),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                column,
                Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("未知 trace key_source: {other}"),
                )),
            )),
        }
    }
}

/// 一个外部请求的完整链路
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceRecord {
    /// 链路 id（uuid v4），前端 key
    pub trace_id: String,
    /// 请求开始时间（RFC3339）
    pub ts: String,
    /// 客户端 Key id；0 表示 master apiKey
    pub key_id: u64,
    /// 入口 Key 类型，区分管理员API密钥与创建的客户端 Key。
    pub key_source: TraceKeySource,
    /// 模型名
    pub model: String,
    /// 是否流式
    pub is_stream: bool,
    /// 最终状态：success / error / interrupted
    pub final_status: String,
    /// 最终命中（成功）或最后尝试的凭据 id
    pub final_credential_id: u64,
    /// 失败分类（顶层，便于筛选）
    pub error_type: Option<String>,
    /// 给用户的简明错误信息
    pub error_message: Option<String>,
    /// 总尝试次数
    pub total_attempts: u32,
    /// 端到端耗时（毫秒）
    pub duration_ms: u64,
    /// 流式中断时已发送的字节数（区分完整失败 vs 半截中断）
    pub interrupted_after_bytes: Option<u64>,
    /// 输入 token（Anthropic 口径）
    #[serde(default)]
    pub input_tokens: u64,
    /// 输出 token
    #[serde(default)]
    pub output_tokens: u64,
    /// 缓存创建 token
    #[serde(default)]
    pub cache_creation_tokens: u64,
    /// 缓存读取 token
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// 费用（上游 meteringEvent 累计的 credits）
    #[serde(default)]
    pub credits: f64,
    /// 首 Token 延迟（毫秒，仅流式有值；非流式为 None）
    #[serde(default)]
    pub first_token_ms: Option<u64>,
    /// 每跳明细
    pub attempts: Vec<TraceAttempt>,
}

/// 失败分类（attempt.outcome / record.error_type 取值）
pub mod outcome {
    pub const SUCCESS: &str = "success";
    pub const QUOTA_EXHAUSTED: &str = "quota_exhausted";
    pub const ACCOUNT_THROTTLED: &str = "account_throttled";
    pub const AUTH_FAILED: &str = "auth_failed";
    pub const TRANSIENT: &str = "transient";
    pub const NETWORK_ERROR: &str = "network_error";
    pub const BAD_REQUEST: &str = "bad_request";
    pub const UNKNOWN: &str = "unknown";
    /// 仅用作 record.error_type：流式响应已开始但上游中途断开
    pub const STREAM_INTERRUPTED: &str = "stream_interrupted";
}

/// 把上游错误体截断到安全长度（按字符边界，避免切碎 UTF-8）
pub fn truncate_snippet(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() <= ERROR_SNIPPET_MAX {
        return Some(trimmed.to_string());
    }
    let mut end = ERROR_SNIPPET_MAX;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}…(truncated)", &trimmed[..end]))
}

/// 链路上报接收端：provider 在重试循环里每跳调用 [`Self::on_attempt`]
pub trait TraceSink: Send + Sync {
    fn on_attempt(&self, attempt: TraceAttempt);
}

/// 查询过滤条件
#[derive(Debug, Default, Clone)]
pub struct TraceQuery {
    /// final_status 精确匹配（success/error/interrupted）
    pub status: Option<String>,
    /// error_type 精确匹配
    pub error_type: Option<String>,
    /// 最终凭据 id
    pub credential_id: Option<u64>,
    /// 客户端 Key id（0 = master apiKey）
    pub key_id: Option<u64>,
    /// 该凭据在某一跳失败过（attempt 级，跨 trace 最终状态）。
    /// 用于"凭据失败详情"：即便整条 trace 最终成功，只要该凭据某跳失败也会命中。
    pub failed_attempt_credential_id: Option<u64>,
    /// 模型名
    pub model: Option<String>,
    /// 仅返回非 success
    pub only_failed: bool,
    /// 返回条数上限
    pub limit: usize,
    /// 偏移量（分页用）
    pub offset: usize,
}

/// SQLite 持久化存储
pub struct TraceStore {
    conn: Mutex<Connection>,
    /// 是否启用 trace 写入（运行时可改）。false 时 insert 直接短路。
    enabled: AtomicBool,
    /// 记录保留天数（运行时可改），cleanup 时读取。
    retention_days: AtomicU64,
}

impl TraceStore {
    /// 打开（或创建）数据库并建表。空路径归一为当前目录下的 traces.db。
    pub fn open(path: PathBuf, enabled: bool, retention_days: u32) -> rusqlite::Result<Self> {
        let path = if path.as_os_str().is_empty() {
            PathBuf::from("traces.db")
        } else {
            path
        };
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    tracing::warn!("创建 traces.db 目录失败 {}: {}", parent.display(), e);
                }
            }
        }
        let conn = Connection::open(&path)?;
        // WAL：并发读不阻塞写；synchronous=NORMAL：写吞吐与崩溃安全的平衡
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            enabled: AtomicBool::new(enabled),
            retention_days: AtomicU64::new(retention_days.max(1) as u64),
        })
    }

    /// 内存数据库（traces.db 打开失败时的兜底；进程退出即丢，但保证 Admin 查询不崩）
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            enabled: AtomicBool::new(true),
            retention_days: AtomicU64::new(DEFAULT_RETENTION_DAYS),
        })
    }

    /// 旧库迁移：为 traces 表补齐新增列（幂等，缺哪列加哪列）。
    /// 老版本的 traces.db 只有基础列，新增的 token/credits/first_token_ms/key_source 需在此 ALTER。
    fn migrate(conn: &Connection) -> rusqlite::Result<()> {
        let mut existing: std::collections::HashSet<String> = std::collections::HashSet::new();
        {
            let mut stmt = conn.prepare("PRAGMA table_info(traces)")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
            for name in rows {
                existing.insert(name?);
            }
        }
        // (列名, 定义) —— 与 SCHEMA 中新增列保持一致
        // 注意 key_source 不带 NOT NULL：老库已有行需先以 NULL 添加再回填（SQLite ALTER ADD COLUMN
        // NOT NULL 不带常量 DEFAULT 时无法对已有行赋值）。新插入永远写入合法值。
        let columns: [(&str, &str); 7] = [
            ("input_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("output_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("cache_creation_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("cache_read_tokens", "INTEGER NOT NULL DEFAULT 0"),
            ("credits", "REAL NOT NULL DEFAULT 0"),
            ("first_token_ms", "INTEGER"),
            ("key_source", "TEXT"),
        ];
        let key_source_added = !existing.contains("key_source");
        for (name, def) in columns {
            if !existing.contains(name) {
                conn.execute_batch(&format!(
                    "ALTER TABLE traces ADD COLUMN {} {};",
                    name, def
                ))?;
            }
        }
        // 老库 key_source 列首次添加后，按 key_id 语义回填：master apiKey (key_id=0) 之外都视为客户端 Key。
        if key_source_added {
            conn.execute_batch(
                "UPDATE traces SET key_source = CASE WHEN key_id = 0 \
                 THEN 'masterApiKey' ELSE 'clientKey' END WHERE key_source IS NULL;",
            )?;
        }
        Ok(())
    }

    /// 是否启用 trace 写入
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// 设置启用开关
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// 获取保留天数
    pub fn retention_days(&self) -> u64 {
        self.retention_days.load(Ordering::Relaxed)
    }

    /// 设置保留天数（>=1）
    pub fn set_retention_days(&self, days: u32) {
        self.retention_days
            .store(days.max(1) as u64, Ordering::Relaxed);
    }

    /// 写入一条完整链路（traces + attempts 在一个事务里）。失败仅 warn，不阻塞请求。
    /// trace 关闭时直接短路。
    pub fn insert(&self, rec: &TraceRecord) {
        if !self.is_enabled() {
            return;
        }
        let mut conn = self.conn.lock();
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("trace 事务开启失败: {}", e);
                return;
            }
        };
        let ts_epoch = chrono::DateTime::parse_from_rfc3339(&rec.ts)
            .map(|d| d.timestamp())
            .unwrap_or_else(|_| Utc::now().timestamp());
        let res = (|| -> rusqlite::Result<()> {
            tx.execute(
                "INSERT OR REPLACE INTO traces (trace_id, ts, ts_epoch, key_id, key_source, model, \
                 is_stream, final_status, final_credential_id, error_type, error_message, \
                 total_attempts, duration_ms, interrupted_after_bytes, \
                 input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, \
                 credits, first_token_ms) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
                rusqlite::params![
                    rec.trace_id,
                    rec.ts,
                    ts_epoch,
                    rec.key_id as i64,
                    rec.key_source.as_str(),
                    rec.model,
                    rec.is_stream as i64,
                    rec.final_status,
                    rec.final_credential_id as i64,
                    rec.error_type,
                    rec.error_message,
                    rec.total_attempts as i64,
                    rec.duration_ms as i64,
                    rec.interrupted_after_bytes.map(|v| v as i64),
                    rec.input_tokens as i64,
                    rec.output_tokens as i64,
                    rec.cache_creation_tokens as i64,
                    rec.cache_read_tokens as i64,
                    rec.credits,
                    rec.first_token_ms.map(|v| v as i64),
                ],
            )?;
            for a in &rec.attempts {
                tx.execute(
                    "INSERT OR REPLACE INTO trace_attempts (trace_id, attempt, credential_id, \
                     endpoint, http_status, outcome, error_snippet, duration_ms) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                    rusqlite::params![
                        rec.trace_id,
                        a.attempt as i64,
                        a.credential_id as i64,
                        a.endpoint,
                        a.http_status.map(|v| v as i64),
                        a.outcome,
                        a.error_snippet,
                        a.duration_ms as i64,
                    ],
                )?;
            }
            Ok(())
        })();
        match res {
            Ok(()) => {
                if let Err(e) = tx.commit() {
                    tracing::warn!("trace 提交失败: {}", e);
                }
            }
            Err(e) => {
                tracing::warn!("trace 写入失败: {}", e);
            }
        }
    }

    /// 分页查询：返回 (当前页记录, 符合条件的总数)。仅 warn 失败，返回 (空, 0)。
    pub fn query_paged(&self, q: &TraceQuery) -> (Vec<TraceRecord>, usize) {
        let conn = self.conn.lock();
        match Self::query_inner(&conn, q) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("trace 查询失败: {}", e);
                (Vec::new(), 0)
            }
        }
    }

    /// 测试辅助：仅取记录、忽略总数
    #[cfg(test)]
    fn query(&self, q: &TraceQuery) -> Vec<TraceRecord> {
        self.query_paged(q).0
    }

    /// 把 [`TraceQuery`] 的过滤条件拼成 WHERE 子句 + 参数（值全部参数化绑定）
    fn build_where(q: &TraceQuery) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
        let mut clauses: Vec<&str> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(s) = &q.status {
            clauses.push("final_status = ?");
            params.push(Box::new(s.clone()));
        }
        if let Some(t) = &q.error_type {
            clauses.push("error_type = ?");
            params.push(Box::new(t.clone()));
        }
        if let Some(c) = q.credential_id {
            clauses.push("final_credential_id = ?");
            params.push(Box::new(c as i64));
        }
        if let Some(k) = q.key_id {
            clauses.push("key_id = ?");
            params.push(Box::new(k as i64));
        }
        if let Some(c) = q.failed_attempt_credential_id {
            // 该凭据在某一跳失败过（不论 trace 最终成功与否）
            clauses.push(
                "EXISTS (SELECT 1 FROM trace_attempts a \
                 WHERE a.trace_id = traces.trace_id \
                 AND a.credential_id = ? AND a.outcome != 'success')",
            );
            params.push(Box::new(c as i64));
        }
        if let Some(m) = &q.model {
            clauses.push("model = ?");
            params.push(Box::new(m.clone()));
        }
        if q.only_failed {
            clauses.push("final_status != 'success'");
        }
        let where_sql = if clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", clauses.join(" AND "))
        };
        (where_sql, params)
    }

    fn query_inner(
        conn: &Connection,
        q: &TraceQuery,
    ) -> rusqlite::Result<(Vec<TraceRecord>, usize)> {
        let (where_sql, params) = Self::build_where(q);
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();

        // 总数（用于前端分页）
        let count_sql = format!("SELECT COUNT(*) FROM traces {}", where_sql);
        let total: i64 = conn.query_row(&count_sql, param_refs.as_slice(), |row| row.get(0))?;

        let limit = if q.limit == 0 {
            DEFAULT_QUERY_LIMIT
        } else {
            q.limit
        };
        let sql = format!(
            "SELECT trace_id, ts, key_id, key_source, model, is_stream, final_status, final_credential_id, \
             error_type, error_message, total_attempts, duration_ms, interrupted_after_bytes, \
             input_tokens, output_tokens, cache_creation_tokens, cache_read_tokens, credits, first_token_ms \
             FROM traces {} ORDER BY ts_epoch DESC LIMIT {} OFFSET {}",
            where_sql, limit, q.offset
        );

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(TraceRecord {
                trace_id: row.get(0)?,
                ts: row.get(1)?,
                key_id: row.get::<_, i64>(2)? as u64,
                key_source: TraceKeySource::from_db(row.get::<_, String>(3)?.as_str(), 3)?,
                model: row.get(4)?,
                is_stream: row.get::<_, i64>(5)? != 0,
                final_status: row.get(6)?,
                final_credential_id: row.get::<_, i64>(7)? as u64,
                error_type: row.get(8)?,
                error_message: row.get(9)?,
                total_attempts: row.get::<_, i64>(10)? as u32,
                duration_ms: row.get::<_, i64>(11)? as u64,
                interrupted_after_bytes: row.get::<_, Option<i64>>(12)?.map(|v| v as u64),
                input_tokens: row.get::<_, i64>(13)? as u64,
                output_tokens: row.get::<_, i64>(14)? as u64,
                cache_creation_tokens: row.get::<_, i64>(15)? as u64,
                cache_read_tokens: row.get::<_, i64>(16)? as u64,
                credits: row.get::<_, f64>(17)?,
                first_token_ms: row.get::<_, Option<i64>>(18)?.map(|v| v as u64),
                attempts: Vec::new(),
            })
        })?;
        let mut records: Vec<TraceRecord> = rows.collect::<rusqlite::Result<_>>()?;

        // 批量取每条 trace 的 attempts
        let mut attempt_stmt = conn.prepare(
            "SELECT attempt, credential_id, endpoint, http_status, outcome, error_snippet, \
             duration_ms FROM trace_attempts WHERE trace_id = ? ORDER BY attempt ASC",
        )?;
        for rec in &mut records {
            let attempts = attempt_stmt.query_map([&rec.trace_id], |row| {
                Ok(TraceAttempt {
                    attempt: row.get::<_, i64>(0)? as u32,
                    credential_id: row.get::<_, i64>(1)? as u64,
                    endpoint: row.get(2)?,
                    http_status: row.get::<_, Option<i64>>(3)?.map(|v| v as u16),
                    outcome: row.get(4)?,
                    error_snippet: row.get(5)?,
                    duration_ms: row.get::<_, i64>(6)? as u64,
                })
            })?;
            rec.attempts = attempts.collect::<rusqlite::Result<_>>()?;
        }
        Ok((records, total as usize))
    }

    /// 删除超过保留期的记录（traces + 关联 attempts）。仅 warn 失败。
    pub fn cleanup(&self) {
        let cutoff =
            (Utc::now() - chrono::Duration::days(self.retention_days() as i64)).timestamp();
        let mut conn = self.conn.lock();
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("trace 清理事务失败: {}", e);
                return;
            }
        };
        let res = (|| -> rusqlite::Result<usize> {
            tx.execute(
                "DELETE FROM trace_attempts WHERE trace_id IN \
                 (SELECT trace_id FROM traces WHERE ts_epoch < ?1)",
                [cutoff],
            )?;
            let n = tx.execute("DELETE FROM traces WHERE ts_epoch < ?1", [cutoff])?;
            Ok(n)
        })();
        match res {
            Ok(n) => {
                if let Err(e) = tx.commit() {
                    tracing::warn!("trace 清理提交失败: {}", e);
                } else if n > 0 {
                    tracing::info!("已清理 {} 条过期 trace 记录", n);
                }
            }
            Err(e) => tracing::warn!("trace 清理失败: {}", e),
        }
    }

    /// 按凭据聚合失败跳数，归并为三类：鉴权 / 账号风控 / 其他。
    /// 统计 trace_attempts 里 outcome != 'success' 的跳，按 credential_id + outcome 分组。
    /// 返回 credential_id → (auth, throttle, other)。仅 warn 失败，返回空。
    pub fn failure_stats(&self) -> std::collections::HashMap<u64, FailureStats> {
        let conn = self.conn.lock();
        let mut out: std::collections::HashMap<u64, FailureStats> =
            std::collections::HashMap::new();
        let mut stmt = match conn.prepare(
            "SELECT credential_id, outcome, COUNT(*) FROM trace_attempts \
             WHERE outcome != 'success' AND credential_id != 0 \
             GROUP BY credential_id, outcome",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("trace failure_stats prepare 失败: {}", e);
                return out;
            }
        };
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)? as u64,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u64,
            ))
        });
        let rows = match rows {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("trace failure_stats 查询失败: {}", e);
                return out;
            }
        };
        for r in rows.flatten() {
            let (cred, outcome_str, cnt) = r;
            let s = out.entry(cred).or_default();
            match outcome_str.as_str() {
                "auth_failed" => s.auth += cnt,
                "account_throttled" => s.throttle += cnt,
                _ => s.other += cnt,
            }
        }
        out
    }
}

/// 按凭据的失败分类计数（鉴权 / 账号风控 / 其他）
#[derive(Debug, Default, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureStats {
    pub auth: u64,
    pub throttle: u64,
    pub other: u64,
}

/// 共享存储句柄
pub type SharedTraceStore = Arc<TraceStore>;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS traces (
    trace_id          TEXT PRIMARY KEY,
    ts                TEXT NOT NULL,
    ts_epoch          INTEGER NOT NULL,
    key_id            INTEGER NOT NULL,
    key_source        TEXT,
    model             TEXT NOT NULL,
    is_stream         INTEGER NOT NULL,
    final_status      TEXT NOT NULL,
    final_credential_id INTEGER NOT NULL,
    error_type        TEXT,
    error_message     TEXT,
    total_attempts    INTEGER NOT NULL,
    duration_ms       INTEGER NOT NULL,
    interrupted_after_bytes INTEGER,
    input_tokens      INTEGER NOT NULL DEFAULT 0,
    output_tokens     INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    credits           REAL NOT NULL DEFAULT 0,
    first_token_ms    INTEGER
);
CREATE INDEX IF NOT EXISTS idx_traces_ts ON traces(ts_epoch DESC);
CREATE INDEX IF NOT EXISTS idx_traces_status ON traces(final_status);
CREATE INDEX IF NOT EXISTS idx_traces_cred ON traces(final_credential_id);

CREATE TABLE IF NOT EXISTS trace_attempts (
    trace_id      TEXT NOT NULL,
    attempt       INTEGER NOT NULL,
    credential_id INTEGER NOT NULL,
    endpoint      TEXT NOT NULL,
    http_status   INTEGER,
    outcome       TEXT NOT NULL,
    error_snippet TEXT,
    duration_ms   INTEGER NOT NULL,
    PRIMARY KEY (trace_id, attempt)
);
CREATE INDEX IF NOT EXISTS idx_attempts_trace ON trace_attempts(trace_id);
";

#[cfg(test)]
mod tests {
    use super::*;

    struct TraceSample<'a> {
        trace_id: &'a str,
        status: &'a str,
        credential_id: u64,
        model: &'a str,
    }

    fn sample(input: TraceSample<'_>) -> TraceRecord {
        TraceRecord {
            trace_id: input.trace_id.to_string(),
            ts: Utc::now().to_rfc3339(),
            key_id: 1,
            key_source: TraceKeySource::ClientKey,
            model: input.model.to_string(),
            is_stream: true,
            final_status: input.status.to_string(),
            final_credential_id: input.credential_id,
            error_type: if input.status == "success" {
                None
            } else {
                Some(outcome::ACCOUNT_THROTTLED.to_string())
            },
            error_message: if input.status == "success" {
                None
            } else {
                Some("blocked".to_string())
            },
            total_attempts: 2,
            duration_ms: 1200,
            interrupted_after_bytes: None,
            input_tokens: 1093,
            output_tokens: 779,
            cache_creation_tokens: 0,
            cache_read_tokens: 101760,
            credits: 0.0,
            first_token_ms: None,
            attempts: vec![
                TraceAttempt {
                    attempt: 0,
                    credential_id: 9,
                    endpoint: "ide".to_string(),
                    http_status: Some(429),
                    outcome: outcome::ACCOUNT_THROTTLED.to_string(),
                    error_snippet: Some("suspicious activity".to_string()),
                    duration_ms: 400,
                },
                TraceAttempt {
                    attempt: 1,
                    credential_id: input.credential_id,
                    endpoint: "ide".to_string(),
                    http_status: if input.status == "success" {
                        Some(200)
                    } else {
                        None
                    },
                    outcome: input.status.to_string(),
                    error_snippet: None,
                    duration_ms: 800,
                },
            ],
        }
    }

    fn mem_store() -> TraceStore {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        TraceStore {
            conn: Mutex::new(conn),
            enabled: AtomicBool::new(true),
            retention_days: AtomicU64::new(DEFAULT_RETENTION_DAYS),
        }
    }

    #[test]
    fn insert_and_query_roundtrip() {
        let store = mem_store();
        store.insert(&sample(TraceSample {
            trace_id: "t1",
            status: "success",
            credential_id: 5,
            model: "claude-opus-4-7",
        }));
        let out = store.query(&TraceQuery {
            limit: 50,
            ..Default::default()
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].trace_id, "t1");
        assert_eq!(out[0].attempts.len(), 2);
        assert_eq!(out[0].attempts[0].outcome, outcome::ACCOUNT_THROTTLED);
        assert_eq!(out[0].key_source, TraceKeySource::ClientKey);
        // token 分项往返
        assert_eq!(out[0].input_tokens, 1093);
        assert_eq!(out[0].output_tokens, 779);
        assert_eq!(out[0].cache_read_tokens, 101760);
        assert_eq!(out[0].cache_creation_tokens, 0);
    }

    #[test]
    fn disabled_skips_insert() {
        let store = mem_store();
        store.set_enabled(false);
        store.insert(&sample(TraceSample {
            trace_id: "t1",
            status: "success",
            credential_id: 5,
            model: "m1",
        }));
        let out = store.query(&TraceQuery {
            limit: 50,
            ..Default::default()
        });
        assert_eq!(out.len(), 0, "trace 关闭时不应写入");
        // 重新开启后写入恢复
        store.set_enabled(true);
        store.insert(&sample(TraceSample {
            trace_id: "t2",
            status: "success",
            credential_id: 5,
            model: "m1",
        }));
        assert_eq!(
            store
                .query(&TraceQuery {
                    limit: 50,
                    ..Default::default()
                })
                .len(),
            1
        );
    }

    #[test]
    fn filter_only_failed_and_status() {
        let store = mem_store();
        store.insert(&sample(TraceSample {
            trace_id: "ok",
            status: "success",
            credential_id: 5,
            model: "m1",
        }));
        store.insert(&sample(TraceSample {
            trace_id: "bad",
            status: "error",
            credential_id: 6,
            model: "m1",
        }));
        store.insert(&sample(TraceSample {
            trace_id: "cut",
            status: "interrupted",
            credential_id: 7,
            model: "m2",
        }));

        let failed = store.query(&TraceQuery {
            only_failed: true,
            limit: 50,
            ..Default::default()
        });
        assert_eq!(failed.len(), 2);
        assert!(failed.iter().all(|r| r.final_status != "success"));

        let by_status = store.query(&TraceQuery {
            status: Some("interrupted".to_string()),
            limit: 50,
            ..Default::default()
        });
        assert_eq!(by_status.len(), 1);
        assert_eq!(by_status[0].trace_id, "cut");

        let by_model = store.query(&TraceQuery {
            model: Some("m2".to_string()),
            limit: 50,
            ..Default::default()
        });
        assert_eq!(by_model.len(), 1);
        assert_eq!(by_model[0].trace_id, "cut");
    }

    #[test]
    fn cleanup_removes_old() {
        let store = mem_store();
        store.insert(&sample(TraceSample {
            trace_id: "recent",
            status: "success",
            credential_id: 5,
            model: "m1",
        }));
        // 手动塞一条 8 天前的记录
        {
            let conn = store.conn.lock();
            let old = (Utc::now() - chrono::Duration::days(8)).timestamp();
            conn.execute(
                "INSERT INTO traces (trace_id, ts, ts_epoch, key_id, key_source, model, is_stream, \
                 final_status, final_credential_id, total_attempts, duration_ms) \
                 VALUES ('old','2020',?1,1,'clientKey','m',1,'success',1,1,1)",
                [old],
            )
            .unwrap();
        }
        store.cleanup();
        let out = store.query(&TraceQuery {
            limit: 50,
            ..Default::default()
        });
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].trace_id, "recent");
    }

    #[test]
    fn query_inner_rejects_unknown_key_source() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO traces (trace_id, ts, ts_epoch, key_id, key_source, model, is_stream, \
             final_status, final_credential_id, total_attempts, duration_ms) \
             VALUES ('bad-source','2020',1,1,'unknown','m',1,'success',1,1,1)",
            [],
        )
        .unwrap();

        let result = TraceStore::query_inner(
            &conn,
            &TraceQuery {
                limit: 50,
                ..Default::default()
            },
        );

        assert!(result.is_err());
    }

    #[test]
    fn truncate_snippet_respects_limit() {
        assert_eq!(truncate_snippet("  "), None);
        assert_eq!(truncate_snippet("hi"), Some("hi".to_string()));
        let long = "x".repeat(ERROR_SNIPPET_MAX + 100);
        let out = truncate_snippet(&long).unwrap();
        assert!(out.ends_with("…(truncated)"));
        assert!(out.len() <= ERROR_SNIPPET_MAX + 20);
    }
}
