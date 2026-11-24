use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use chrono::Utc;
use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use super::ledger;
use crate::kiro::parser::frame::Frame;

const WRITE_DEADLINE: Duration = Duration::from_millis(250);
// Bound the recovery copy per request. Excess events remain explicitly partial.
const MAX_METERS: usize = 256;

#[derive(Clone)]
pub struct CostStore {
    pub(super) path: PathBuf,
    sender: mpsc::Sender<Command>,
    pub(super) capture_errors: Arc<AtomicU64>,
    pub(super) previous_capture_errors: u64,
    worker: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    shutdown_failed: Arc<AtomicBool>,
}

enum Write {
    Call {
        start: Arc<CallStart>,
        meters: Vec<Option<String>>,
        status: Option<&'static str>,
        initial: bool,
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
    #[cfg(test)]
    Flush,
    Shutdown,
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
    damaged: AtomicBool,
}

pub(crate) struct CallMeter {
    store: CostStore,
    start: Option<Arc<CallStart>>,
    meters: Vec<Option<String>>,
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
        super::checkpoints::initialize(&conn)?;
        let previous_capture_errors = conn.query_row(
            "SELECT errors FROM cost_capture_health WHERE id=1",
            [],
            |row| row.get::<_, u64>(0),
        )?;
        let (sender, mut receiver) = mpsc::channel::<Command>(2048);
        let capture_errors = Arc::new(AtomicU64::new(0));
        let errors = capture_errors.clone();
        let shutdown_failed = Arc::new(AtomicBool::new(false));
        let shutdown_error = shutdown_failed.clone();
        let worker = std::thread::Builder::new()
            .name("cost-ledger".into())
            .spawn(move || {
                let mut stopping = false;
                while let Some(first) = receiver.blocking_recv() {
                    let mut batch = Vec::with_capacity(64);
                    batch.push(first);
                    while batch.len() < 64 {
                        match receiver.try_recv() {
                            Ok(command) => batch.push(command),
                            Err(_) => break,
                        }
                    }
                    if batch
                        .iter()
                        .any(|(write, _)| matches!(write, Write::Shutdown))
                    {
                        stopping = true;
                        receiver.close();
                    }
                    // Retain commands until commit: retries replay idempotent writes.
                    // BEGIN IMMEDIATE waits once per batch, rather than per statement.
                    let mut results = Vec::new();
                    let mut committed = false;
                    for retry in 0..3 {
                        results.clear();
                        let result = (|| -> anyhow::Result<()> {
                            let tx =
                                conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                            for (write, _) in &batch {
                                results.push(apply(&tx, write));
                            }
                            tx.execute(
                                "UPDATE cost_capture_health SET errors=MAX(errors, ?1) WHERE id=1",
                                [previous_capture_errors
                                    .saturating_add(errors.load(Ordering::Relaxed))],
                            )?;
                            tx.commit()?;
                            Ok(())
                        })();
                        match result {
                            Ok(()) => {
                                committed = true;
                                break;
                            }
                            Err(error) => {
                                tracing::warn!(%error, retry, "成本账本事务失败，将有界重试");
                                errors.fetch_add(1, Ordering::Relaxed);
                                for (write, _) in &batch {
                                    mark_damaged(write);
                                }
                                if retry < 2 {
                                    std::thread::sleep(Duration::from_millis(50));
                                }
                            }
                        }
                    }
                    if !committed {
                        results = batch
                            .iter()
                            .map(|_| Err(anyhow::anyhow!("成本账本事务未提交")))
                            .collect();
                    }
                    for ((write, ack), result) in batch.into_iter().zip(results) {
                        if let Err(error) = &result {
                            if stopping {
                                shutdown_error.store(true, Ordering::Relaxed);
                            }
                            errors.fetch_add(1, Ordering::Relaxed);
                            mark_damaged(&write);
                            tracing::warn!(%error, "成本账本写入失败");
                        }
                        if let Some(ack) = ack {
                            let _ = ack.send(result);
                        }
                    }
                }
                if stopping
                    && let Err(error) = conn.execute(
                        "UPDATE cost_calls SET status='interrupted'
                    WHERE status IS NULL OR status='capture_degraded'",
                        [],
                    )
                {
                    shutdown_error.store(true, Ordering::Relaxed);
                    tracing::error!(%error, "成本账本无法标记停机中断的调用");
                }
                if let Err(error) = conn.execute(
                    "UPDATE cost_capture_health SET errors=MAX(errors, ?1) WHERE id=1",
                    [previous_capture_errors.saturating_add(errors.load(Ordering::Relaxed))],
                ) {
                    shutdown_error.store(true, Ordering::Relaxed);
                    tracing::error!(%error, "成本账本停机诊断无法保存");
                }
            })?;
        Ok(Self {
            path,
            sender,
            capture_errors,
            previous_capture_errors,
            worker: Arc::new(Mutex::new(Some(worker))),
            shutdown_failed,
        })
    }

    async fn acknowledged_with_timeout(
        &self,
        write: Write,
        deadline: Duration,
    ) -> anyhow::Result<()> {
        let (ack, result) = oneshot::channel();
        tokio::time::timeout(deadline, self.sender.send((write, Some(ack)))).await??;
        tokio::time::timeout(deadline, result).await??
    }

    async fn acknowledged(&self, write: Write) -> anyhow::Result<()> {
        self.acknowledged_with_timeout(write, WRITE_DEADLINE).await
    }

    #[cfg(test)]
    pub async fn flush(&self) -> anyhow::Result<()> {
        self.acknowledged_with_timeout(Write::Flush, Duration::from_secs(10))
            .await
    }

    /// Called after requests drain. Closing the receiver drains accepted commands
    /// from other clones as well. Only join a finished thread on the async runtime.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        tokio::time::timeout(Duration::from_secs(15), async {
            self.sender.send((Write::Shutdown, None)).await?;
            loop {
                let finished = self.worker.lock().as_ref().is_none_or(|h| h.is_finished());
                if finished {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            if let Some(worker) = self.worker.lock().take() {
                worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("成本写线程异常退出"))?;
            }
            anyhow::ensure!(
                !self.shutdown_failed.load(Ordering::Relaxed),
                "成本账本停机写入失败，请检查日志和数据目录"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await?
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
        let start = Arc::new(CallStart {
            id: Uuid::new_v4().to_string(),
            credential,
            at: Utc::now().timestamp_millis(),
            model: model.to_owned(),
            endpoint,
            attempt,
            kind,
            request_id: request_id.map(str::to_owned),
            damaged: AtomicBool::new(false),
        });
        self.enqueue(Write::Call {
            start: start.clone(),
            meters: Vec::new(),
            status: None,
            initial: true,
        });
        CallMeter {
            store: self.clone(),
            start: Some(start),
            meters: Vec::new(),
        }
    }

    fn enqueue(&self, write: Write) {
        if let Err(error) = self.sender.try_send((write, None)) {
            mark_damaged(&error.into_inner().0);
            self.capture_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl CallMeter {
    pub fn mark_incomplete(&mut self) {
        if let Some(start) = &self.start {
            start.damaged.store(true, Ordering::Relaxed);
        }
    }

    pub fn observe(&mut self, frame: &Frame) {
        if frame.event_type() != Some("meteringEvent") {
            if matches!(frame.message_type(), Some("exception" | "error")) {
                self.mark_incomplete();
            }
            return;
        }
        let Some(start) = &self.start else { return };
        if self.meters.len() == MAX_METERS {
            start.damaged.store(true, Ordering::Relaxed);
            return;
        }
        let raw = super::metering_literal(frame);
        if raw.is_none() {
            start.damaged.store(true, Ordering::Relaxed);
        }
        self.meters.push(raw);
        self.store.enqueue(Write::Call {
            start: start.clone(),
            meters: self.meters.clone(),
            status: None,
            initial: false,
        });
    }

    pub async fn finish(mut self, status: &'static str) {
        let Some(start) = self.start.take() else {
            return;
        };
        let write = Write::Call {
            start: start.clone(),
            meters: std::mem::take(&mut self.meters),
            status: Some(status),
            initial: false,
        };
        if self.store.acknowledged(write).await.is_err() {
            start.damaged.store(true, Ordering::Relaxed);
            self.store.capture_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Drop for CallMeter {
    fn drop(&mut self) {
        if let Some(start) = self.start.take() {
            self.store.enqueue(Write::Call {
                start,
                meters: std::mem::take(&mut self.meters),
                status: Some("interrupted"),
                initial: false,
            });
        }
    }
}

fn mark_damaged(write: &Write) {
    if let Write::Call { start, .. } = write {
        start.damaged.store(true, Ordering::Relaxed);
    }
}

fn apply(conn: &Connection, write: &Write) -> anyhow::Result<()> {
    match write {
        Write::Call {
            start,
            meters,
            status,
            initial,
        } => {
            conn.execute(
                "INSERT INTO cost_calls (id, credential_id, started_at, model, endpoint, attempt, kind, request_id, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) ON CONFLICT(id) DO NOTHING",
                params![start.id, start.credential, start.at, start.model, start.endpoint,
                    start.attempt, start.kind, start.request_id, if *initial { None } else { Some("capture_degraded") }],
            )?;
            for (index, raw) in meters.iter().enumerate() {
                conn.execute(
                    "INSERT INTO cost_meters (call_id, sequence, raw) VALUES (?1, ?2, ?3)
                    ON CONFLICT(call_id, sequence) DO NOTHING",
                    params![start.id, index + 1, raw],
                )?;
            }
            if let Some(status) = status {
                conn.execute(
                    "UPDATE cost_calls SET status=CASE WHEN ?2='complete' AND
                    (status='capture_degraded' OR ?3) THEN 'partial' ELSE ?2 END
                    WHERE id=?1 AND (status IS NULL OR status='capture_degraded')",
                    params![start.id, status, start.damaged.load(Ordering::Relaxed)],
                )?;
            }
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
                    "UPDATE cost_lots SET expiry_confirmed_at=COALESCE(expiry_confirmed_at, ?2)
                WHERE id=?1 AND expires_at <= ?2",
                    params![id, at]
                )? == 1,
                "只能确认已经到期的批次；重复确认保留首次时间"
            );
        }
        #[cfg(test)]
        Write::Flush => {}
        Write::Shutdown => {}
    }
    Ok(())
}
