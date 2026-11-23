use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::Utc;
use rusqlite::{Connection, OpenFlags, params};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use super::ledger;
use crate::kiro::parser::frame::Frame;

const WRITE_DEADLINE: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub struct CostStore {
    pub(super) path: PathBuf,
    sender: mpsc::Sender<Command>,
    pub(super) capture_errors: Arc<AtomicU64>,
}

enum Write {
    Start(CallStart),
    Meter {
        id: String,
        sequence: u32,
        raw: Option<String>,
    },
    Finish {
        id: String,
        status: &'static str,
        meters_expected: u32,
    },
    Purchase(ledger::Purchase),
    Balance {
        credential: u64,
        observed_at: i64,
        data: String,
    },
    ConfirmExpiry {
        id: String,
        at: i64,
    },
}

struct CallStart {
    id: String,
    credential: u64,
    at: i64,
    model: String,
    endpoint: &'static str,
    attempt: usize,
    kind: &'static str,
    request_id: Option<String>,
}

pub(crate) struct CallMeter {
    store: CostStore,
    id: Option<String>,
    sequence: u32,
    damaged: bool,
}

type Ack = oneshot::Sender<anyhow::Result<()>>;
type Command = (Write, Option<Ack>);

impl CostStore {
    pub fn reserved_credential_id(path: &Path) -> anyhow::Result<u64> {
        if !path.try_exists()? {
            return Ok(0);
        }
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(Duration::from_secs(2))?;
        Ok(conn.query_row(
            "SELECT MAX(COALESCE((SELECT MAX(credential_id) FROM cost_calls), 0),
                        COALESCE((SELECT MAX(credential_id) FROM cost_lots), 0),
                        COALESCE((SELECT MAX(credential_id) FROM cost_balances), 0))",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn open(path: PathBuf) -> anyhow::Result<Self> {
        let mut conn = Connection::open(&path)?;
        conn.busy_timeout(Duration::from_secs(2))?;
        conn.execute_batch(ledger::SCHEMA)?;
        let (sender, mut receiver) = mpsc::channel::<Command>(2048);
        let capture_errors = Arc::new(AtomicU64::new(0));
        let errors = capture_errors.clone();
        std::thread::Builder::new()
            .name("cost-ledger".into())
            .spawn(move || {
                while let Some(first) = receiver.blocking_recv() {
                    let mut batch = Vec::with_capacity(64);
                    batch.push(first);
                    while batch.len() < 64 {
                        match receiver.try_recv() {
                            Ok(command) => batch.push(command),
                            Err(_) => break,
                        }
                    }
                    let mut results = Vec::with_capacity(batch.len());
                    let committed = conn.transaction().and_then(|tx| {
                        for (write, ack) in batch.drain(..) {
                            results.push((apply(&tx, write), ack));
                        }
                        tx.commit()
                    });
                    if let Err(error) = committed {
                        tracing::warn!(%error, "成本账本事务失败");
                        results.extend(batch.drain(..).map(|(_, ack)| (Ok(()), ack)));
                        for (result, _) in &mut results {
                            *result = Err(anyhow::anyhow!("成本账本事务未提交"));
                        }
                    }
                    for (result, ack) in results {
                        if let Err(error) = &result {
                            errors.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(%error, "成本账本写入失败");
                        }
                        if let Some(ack) = ack {
                            let _ = ack.send(result);
                        }
                    }
                }
            })?;
        Ok(Self {
            path,
            sender,
            capture_errors,
        })
    }

    async fn acknowledged(&self, write: Write) -> anyhow::Result<()> {
        let (ack, result) = oneshot::channel();
        tokio::time::timeout(WRITE_DEADLINE, self.sender.send((write, Some(ack)))).await??;
        tokio::time::timeout(WRITE_DEADLINE, result).await??
    }

    pub(super) async fn purchase(&self, purchase: ledger::Purchase) -> anyhow::Result<()> {
        self.acknowledged(Write::Purchase(purchase)).await
    }

    pub async fn confirm_expiry(&self, id: String) -> anyhow::Result<()> {
        self.acknowledged(Write::ConfirmExpiry {
            id,
            at: Utc::now().timestamp_millis(),
        })
        .await
    }

    pub(crate) fn observe_balance(&self, credential: u64, data: serde_json::Value) {
        self.enqueue(Write::Balance {
            credential,
            observed_at: Utc::now().timestamp_millis(),
            data: data.to_string(),
        });
    }

    pub(crate) async fn start_call(
        &self,
        credential: u64,
        model: &str,
        endpoint: &'static str,
        attempt: usize,
        kind: &'static str,
        request_id: Option<&str>,
    ) -> CallMeter {
        let id = Uuid::new_v4().to_string();
        let start = CallStart {
            id: id.clone(),
            credential,
            at: Utc::now().timestamp_millis(),
            model: model.to_owned(),
            endpoint,
            attempt,
            kind,
            request_id: request_id.map(str::to_owned),
        };
        let damaged = self.acknowledged(Write::Start(start)).await.is_err();
        if damaged {
            self.capture_errors.fetch_add(1, Ordering::Relaxed);
        }
        CallMeter {
            store: self.clone(),
            id: Some(id),
            sequence: 0,
            damaged,
        }
    }

    fn enqueue(&self, write: Write) -> bool {
        if self.sender.try_send((write, None)).is_ok() {
            return true;
        }
        self.capture_errors.fetch_add(1, Ordering::Relaxed);
        false
    }
}

impl CallMeter {
    pub fn mark_incomplete(&mut self) {
        self.damaged = true;
    }

    pub fn observe(&mut self, frame: &Frame) {
        if frame.event_type() != Some("meteringEvent") {
            if matches!(frame.message_type(), Some("exception" | "error")) {
                self.mark_incomplete();
            }
            return;
        }
        let Some(id) = self.id.clone() else { return };
        self.sequence += 1;
        let raw = super::metering_literal(frame);
        self.damaged |= raw.is_none();
        if !self.store.enqueue(Write::Meter {
            id,
            sequence: self.sequence,
            raw,
        }) {
            self.damaged = true;
        }
    }

    pub async fn finish(mut self, status: &'static str) {
        let Some(id) = self.id.take() else { return };
        let status = if self.damaged && status == "complete" {
            "partial"
        } else {
            status
        };
        if self
            .store
            .acknowledged(Write::Finish {
                id,
                status,
                meters_expected: self.sequence,
            })
            .await
            .is_err()
        {
            self.store.capture_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Drop for CallMeter {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.store.enqueue(Write::Finish {
                id,
                status: "interrupted",
                meters_expected: self.sequence,
            });
        }
    }
}

fn apply(conn: &Connection, write: Write) -> anyhow::Result<()> {
    match write {
        Write::Start(start) => {
            conn.execute(
                "INSERT INTO cost_calls (id, credential_id, started_at, model, endpoint, attempt, kind, request_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![start.id, start.credential, start.at, start.model, start.endpoint,
                    start.attempt, start.kind, start.request_id],
            )?;
        }
        Write::Meter { id, sequence, raw } => {
            conn.execute(
                "INSERT INTO cost_meters (call_id, sequence, raw) VALUES (?1, ?2, ?3)",
                params![id, sequence, raw],
            )?;
        }
        Write::Finish {
            id,
            status,
            meters_expected,
        } => {
            anyhow::ensure!(
                conn.execute(
                    "UPDATE cost_calls SET status = CASE WHEN ?2 = 'complete' AND
                     (SELECT COUNT(*) FROM cost_meters WHERE call_id = ?1) != ?3
                     THEN 'partial' ELSE ?2 END WHERE id = ?1 AND status IS NULL",
                    params![id, status, meters_expected]
                )? == 1,
                "调用终态无法归档"
            );
        }
        Write::Purchase(purchase) => ledger::insert_purchase(conn, purchase)?,
        Write::Balance {
            credential,
            observed_at,
            data,
        } => {
            conn.execute(
                "INSERT INTO cost_balances (credential_id, observed_at, data)
                 VALUES (?1, ?2, ?3) ON CONFLICT(credential_id) DO UPDATE
                 SET observed_at=excluded.observed_at, data=excluded.data
                 WHERE excluded.observed_at >= cost_balances.observed_at",
                params![credential, observed_at, data],
            )?;
        }
        Write::ConfirmExpiry { id, at } => {
            anyhow::ensure!(
                conn.execute(
                    "UPDATE cost_lots SET expiry_confirmed_at = COALESCE(expiry_confirmed_at, ?2)
                 WHERE id = ?1 AND expires_at <= ?2",
                    params![id, at]
                )? == 1,
                "只能确认已经到期且尚未确认的批次"
            );
        }
    }
    Ok(())
}
