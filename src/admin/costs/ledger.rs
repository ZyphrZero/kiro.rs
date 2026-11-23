use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::CostStore;

pub(super) const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=FULL;
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS cost_lots (
    id TEXT PRIMARY KEY NOT NULL,
    credential_id INTEGER NOT NULL,
    paid_at INTEGER,
    effective_at INTEGER NOT NULL,
    expires_at INTEGER,
    expiry_confirmed_at INTEGER,
    amount TEXT NOT NULL,
    credits TEXT NOT NULL,
    kind TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS cost_lots_credential ON cost_lots (credential_id, effective_at);
CREATE TABLE IF NOT EXISTS cost_calls (
    id TEXT PRIMARY KEY NOT NULL,
    credential_id INTEGER NOT NULL,
    started_at INTEGER NOT NULL,
    model TEXT NOT NULL,
    endpoint TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    kind TEXT NOT NULL,
    request_id TEXT,
    status TEXT
) STRICT;
CREATE INDEX IF NOT EXISTS cost_calls_time ON cost_calls (started_at, id);
CREATE INDEX IF NOT EXISTS cost_calls_credential ON cost_calls (credential_id, started_at, id);
CREATE TABLE IF NOT EXISTS cost_balances (
    credential_id INTEGER PRIMARY KEY NOT NULL,
    observed_at INTEGER NOT NULL,
    data TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS cost_meters (
    call_id TEXT NOT NULL REFERENCES cost_calls (id),
    sequence INTEGER NOT NULL,
    raw TEXT,
    PRIMARY KEY (call_id, sequence)
) STRICT;
";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Purchase {
    pub credential_id: u64,
    id: String,
    paid_at: Option<DateTime<Utc>>,
    effective_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    amount: String,
    credits: String,
    kind: LotKind,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LotKind {
    Plan,
    Purchase,
    Bonus,
    AddOn,
}

impl LotKind {
    fn name(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Purchase => "purchase",
            Self::Bonus => "bonus",
            Self::AddOn => "add_on",
        }
    }
}

fn add(a: Decimal, b: Decimal) -> Result<Decimal> {
    let a = a.normalize();
    let b = b.normalize();
    let mut scale = a.scale().max(b.scale());
    let at_scale = |value: Decimal| {
        value
            .mantissa()
            .checked_mul(10_i128.pow(scale - value.scale()))
    };
    let mut sum = at_scale(a)
        .zip(at_scale(b))
        .and_then(|(a, b)| a.checked_add(b))
        .context("成本账本数值超出计算范围")?;
    while scale > 0 && sum % 10 == 0 {
        scale -= 1;
        sum /= 10;
    }
    Ok(Decimal::try_from_i128_with_scale(sum, scale)?)
}

fn decimal(raw: &str) -> Result<Decimal> {
    ensure!(raw.len() <= 128, "数字过长");
    if let Some((base, exponent)) = raw.split_once(['e', 'E']) {
        let exp: i32 = exponent.parse().context("无效的十进制指数")?;
        ensure!((-28..=28).contains(&exp), "十进制指数超出范围");
        let base = Decimal::from_str_exact(base)?.normalize();
        if base.is_zero() {
            return Ok(Decimal::ZERO);
        }
        let scale = base.scale() as i32 - exp;
        let magnitude = base
            .mantissa()
            .checked_mul(10_i128.pow((-scale).max(0) as u32))
            .context("十进制数超出范围")?;
        return Ok(Decimal::try_from_i128_with_scale(
            magnitude,
            scale.max(0) as u32,
        )?);
    }
    Ok(Decimal::from_str_exact(raw)?)
}

impl Purchase {
    pub fn validate(&self) -> Result<()> {
        Uuid::parse_str(&self.id).context("无效采购单 ID")?;
        let amount = decimal(&self.amount)?;
        let credits = decimal(&self.credits)?;
        ensure!(
            self.credential_id > 0 && self.credential_id <= i64::MAX as u64,
            "无效凭据"
        );
        ensure!(
            amount >= Decimal::ZERO
                && amount.normalize().scale() <= 8
                && amount <= Decimal::from(1_000_000_000),
            "金额须为非负且最多 8 位小数"
        );
        ensure!(
            credits > Decimal::ZERO && credits <= Decimal::from(1_000_000_000),
            "到账积分须大于 0 且不超过十亿"
        );
        ensure!(
            self.expires_at
                .is_none_or(|expires| expires > self.effective_at
                    && expires.timestamp_millis() > self.effective_at.timestamp_millis()),
            "到期时间须晚于生效时间"
        );
        ensure!(
            self.paid_at.is_none_or(|paid| paid <= Utc::now()),
            "付款时间不能晚于当前时间"
        );
        Ok(())
    }
}

pub(super) fn insert_purchase(conn: &Connection, purchase: Purchase) -> Result<()> {
    let id = Uuid::parse_str(&purchase.id)?.to_string();
    let paid = purchase.paid_at.map(|t| t.timestamp_millis());
    let effective = purchase.effective_at.timestamp_millis();
    let expires = purchase.expires_at.map(|t| t.timestamp_millis());
    let amount = decimal(&purchase.amount)?.normalize().to_string();
    let credits = decimal(&purchase.credits)?.normalize().to_string();
    let kind = purchase.kind.name();
    let affected = conn.execute(
        "INSERT INTO cost_lots (id, credential_id, paid_at, effective_at, expires_at, amount, credits, kind)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET id=excluded.id
         WHERE credential_id=excluded.credential_id AND paid_at IS excluded.paid_at
         AND effective_at=excluded.effective_at AND expires_at IS excluded.expires_at
         AND amount=excluded.amount AND credits=excluded.credits AND kind=excluded.kind",
        params![id, purchase.credential_id, paid, effective, expires, amount, credits, kind],
    )?;
    ensure!(affected == 1, "采购单 ID 已用于另一笔记录");
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Window {
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    credential_id: Option<u64>,
}

impl Window {
    pub fn validate(self) -> Result<Self> {
        ensure!(
            self.from < self.to
                && self.from <= Utc::now()
                && self.to - self.from <= chrono::Duration::days(366),
            "请选择最多 366 天的有效查询区间"
        );
        ensure!(
            self.credential_id
                .is_none_or(|id| id > 0 && id <= i64::MAX as u64),
            "无效凭据"
        );
        Ok(self)
    }

    fn includes(&self, time: i64) -> bool {
        (self.from.timestamp_millis()..self.to.timestamp_millis()).contains(&time)
    }
}

struct Lot {
    id: String,
    paid_at: Option<i64>,
    starts: i64,
    expires: Option<i64>,
    confirmed: Option<i64>,
    amount: Decimal,
    credits: Decimal,
    kind: String,
    used: Decimal,
    assigned: Decimal,
}

impl Lot {
    fn allocate(&mut self, credits: Decimal) -> Result<(Decimal, Decimal)> {
        let portion = add(self.credits, -self.used)?.min(credits);
        if portion.is_zero() {
            return Ok((Decimal::ZERO, Decimal::ZERO));
        }
        // Round the cumulative amount and then subtract the prior projection.
        // This conserves the purchase amount even with many small calls.
        self.used = add(self.used, portion)?;
        let next = if self.used == self.credits {
            self.amount
        } else {
            self.used
                .checked_div(self.credits)
                .and_then(|share| self.amount.checked_mul(share))
                .context("采购分摊超出计算范围")?
                .round_dp_with_strategy(8, RoundingStrategy::MidpointNearestEven)
        };
        let assigned = next - self.assigned;
        self.assigned = next;
        Ok((portion, assigned))
    }
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Totals {
    #[serde(serialize_with = "as_string")]
    paid: Decimal,
    #[serde(serialize_with = "as_string")]
    usage_cost: Decimal,
    #[serde(serialize_with = "as_string")]
    reported_credits: Decimal,
    #[serde(serialize_with = "as_string")]
    unpriced_credits: Decimal,
    #[serde(serialize_with = "as_string")]
    remaining_cost: Decimal,
    #[serde(serialize_with = "as_string")]
    expiry_loss: Decimal,
    #[serde(serialize_with = "as_string")]
    pending_expiry_loss: Decimal,
    attempts: u64,
    missing_metering_calls: u64,
    partial_metering_calls: u64,
    unavailable_metering_calls: u64,
}

fn as_string<T: serde::Serializer>(value: &Decimal, serializer: T) -> Result<T::Ok, T::Error> {
    serializer.serialize_str(&value.normalize().to_string())
}

impl Totals {
    fn combine(&mut self, other: &Self) -> Result<()> {
        self.paid = add(self.paid, other.paid)?;
        self.usage_cost = add(self.usage_cost, other.usage_cost)?;
        self.reported_credits = add(self.reported_credits, other.reported_credits)?;
        self.unpriced_credits = add(self.unpriced_credits, other.unpriced_credits)?;
        self.remaining_cost = add(self.remaining_cost, other.remaining_cost)?;
        self.expiry_loss = add(self.expiry_loss, other.expiry_loss)?;
        self.pending_expiry_loss = add(self.pending_expiry_loss, other.pending_expiry_loss)?;
        self.attempts += other.attempts;
        self.missing_metering_calls += other.missing_metering_calls;
        self.partial_metering_calls += other.partial_metering_calls;
        self.unavailable_metering_calls += other.unavailable_metering_calls;
        Ok(())
    }
}

#[derive(Default)]
struct Account {
    lots: Vec<Lot>,
    totals: Totals,
}

struct Call {
    id: String,
    credential: u64,
    at: i64,
    model: String,
    endpoint: String,
    attempt: u32,
    kind: String,
    request: Option<String>,
    status: Option<String>,
    credits: Decimal,
    valid: bool,
    invalid: bool,
}

fn record_call(
    accounts: &mut BTreeMap<u64, Account>,
    details: &mut VecDeque<Value>,
    window: &Window,
    call: Call,
) -> Result<()> {
    let account = accounts.entry(call.credential).or_default();
    let mut remaining = call.credits;
    let mut cost = Decimal::ZERO;
    let mut allocations = Vec::new();
    let detail = window.credential_id.is_some() && window.includes(call.at);
    for lot in &mut account.lots {
        if remaining.is_zero() {
            break;
        }
        if lot.starts <= call.at && lot.expires.is_none_or(|end| call.at < end) {
            let (portion, assigned) = lot.allocate(remaining)?;
            remaining = add(remaining, -portion)?;
            cost = add(cost, assigned)?;
            if detail && !portion.is_zero() {
                allocations.push(json!({"lotId": lot.id, "credits": portion.to_string(),
                    "cost": assigned.to_string()}));
            }
        }
    }
    if window.includes(call.at) {
        let metering = match (
            call.valid,
            call.invalid,
            call.status.as_deref(),
            call.kind.as_str(),
        ) {
            (_, _, _, "mcp") => "unavailable",
            (false, _, _, _) => "missing",
            (true, false, Some("complete"), _) => "complete",
            _ => "partial",
        };
        let totals = &mut account.totals;
        totals.attempts += 1;
        totals.missing_metering_calls += u64::from(metering == "missing");
        totals.partial_metering_calls += u64::from(metering == "partial");
        totals.unavailable_metering_calls += u64::from(metering == "unavailable");
        totals.reported_credits = add(totals.reported_credits, call.credits)?;
        totals.unpriced_credits = add(totals.unpriced_credits, remaining)?;
        totals.usage_cost = add(totals.usage_cost, cost)?;
        if detail {
            details.push_back(json!({
                "id": call.id, "model": call.model, "startedAt": call.at, "endpoint": call.endpoint,
                "attempt": call.attempt, "trafficClass": call.kind, "requestId": call.request,
                "metering": metering, "credits": call.valid.then(|| call.credits.to_string()),
                "cost": (call.valid && (remaining != call.credits || call.credits.is_zero()))
                    .then(|| cost.to_string()),
                "unpricedCredits": remaining.to_string(), "allocations": allocations
            }));
            if details.len() > 100 {
                details.pop_front();
            }
        }
    }
    Ok(())
}

pub(super) fn report(store: &CostStore, window: Window) -> Result<Value> {
    let now = Utc::now();
    // Future end bounds (e.g. midnight) show a report through the current instant.
    let cutoff = window.to.min(now).timestamp_millis();
    let mut conn = Connection::open_with_flags(&store.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_secs(2))?;
    let db = conn.transaction()?;
    let mut accounts: BTreeMap<u64, Account> = BTreeMap::new();
    let mut lots = db.prepare(
        "SELECT id, credential_id, paid_at, effective_at, expires_at, expiry_confirmed_at,
                amount, credits, kind
         FROM cost_lots WHERE (effective_at < ?1 OR paid_at < ?1)
         AND (?2 IS NULL OR credential_id = ?2)
         ORDER BY CASE kind WHEN 'plan' THEN 0 WHEN 'add_on' THEN 2 ELSE 1 END,
            expires_at IS NULL, expires_at, effective_at, id",
    )?;
    let mut rows = lots.query(params![cutoff, window.credential_id])?;
    while let Some(row) = rows.next()? {
        let lot = Lot {
            id: row.get(0)?,
            paid_at: row.get(2)?,
            starts: row.get(3)?,
            expires: row.get(4)?,
            confirmed: row.get(5)?,
            amount: decimal(&row.get::<_, String>(6)?)?,
            credits: decimal(&row.get::<_, String>(7)?)?,
            kind: row.get(8)?,
            assigned: Decimal::ZERO,
            used: Decimal::ZERO,
        };
        accounts.entry(row.get(1)?).or_default().lots.push(lot);
    }
    let mut calls = db.prepare(
        "SELECT c.id, c.credential_id, c.started_at, c.model, c.endpoint, c.attempt, c.kind,
                c.request_id, c.status, m.sequence, m.raw
         FROM cost_calls c LEFT JOIN cost_meters m ON m.call_id=c.id
         WHERE c.started_at < ?1 AND (?2 IS NULL OR c.credential_id=?2)
         ORDER BY c.started_at, c.id, m.sequence",
    )?;
    let mut rows = calls.query(params![cutoff, window.credential_id])?;
    let mut current: Option<Call> = None;
    let mut details = VecDeque::new();
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        if current.as_ref().is_none_or(|call| call.id != id) {
            if let Some(call) = current.take() {
                record_call(&mut accounts, &mut details, &window, call)?;
            }
            current = Some(Call {
                id,
                credential: row.get(1)?,
                at: row.get(2)?,
                model: row.get(3)?,
                endpoint: row.get(4)?,
                attempt: row.get(5)?,
                kind: row.get(6)?,
                request: row.get(7)?,
                status: row.get(8)?,
                credits: Decimal::ZERO,
                valid: false,
                invalid: false,
            });
        }
        let call = current.as_mut().ok_or_else(|| anyhow!("缺少计量主体"))?;
        if row.get::<_, Option<u32>>(9)?.is_some() {
            let raw: Option<String> = row.get(10)?;
            let value = raw.as_deref().and_then(|raw| decimal(raw).ok());
            if let Some(value) = value.and_then(|v| add(call.credits, v).ok()) {
                call.valid = true;
                call.credits = value;
            } else {
                call.invalid = true;
            }
        }
    }
    if let Some(call) = current {
        record_call(&mut accounts, &mut details, &window, call)?;
    }
    let mut totals = Totals::default();
    let mut summary = Vec::new();
    let mut lot_details = Vec::new();
    for (id, account) in accounts {
        let mut row = account.totals;
        for lot in account.lots {
            let unused = lot.amount - lot.assigned;
            if lot.paid_at.is_some_and(|paid| window.includes(paid)) {
                row.paid = add(row.paid, lot.amount)?;
            }
            let state = if lot.starts >= cutoff {
                "pending"
            } else if lot.expires.is_some_and(|end| end <= cutoff) {
                if let Some(at) = lot.confirmed.filter(|at| *at < cutoff) {
                    if window.includes(at) {
                        row.expiry_loss = add(row.expiry_loss, unused)?;
                    }
                    "expired"
                } else {
                    row.pending_expiry_loss = add(row.pending_expiry_loss, unused)?;
                    "pending_expiry"
                }
            } else {
                row.remaining_cost = add(row.remaining_cost, unused)?;
                "active"
            };
            if window.credential_id.is_some() {
                let remaining_credits = add(lot.credits, -lot.used)?;
                lot_details.push(json!({
                    "id": lot.id, "paidAt": lot.paid_at, "effectiveAt": lot.starts,
                    "expiresAt": lot.expires, "amount": lot.amount.to_string(), "kind": lot.kind,
                    "credits": lot.credits.to_string(), "remainingCredits": remaining_credits.to_string(),
                    "remainingCost": unused.to_string(), "state": state,
                }));
            }
        }
        totals.combine(&row)?;
        summary.push(json!({"credentialId": id, "totals": row}));
    }
    let balance = if let Some(id) = window.credential_id {
        db.query_row(
            "SELECT observed_at, data FROM cost_balances WHERE credential_id=?1",
            [id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .map(|(at, data)| -> Result<Value> {
            Ok(json!({ "observedAt": at, "data": serde_json::from_str::<Value>(&data)? }))
        })
        .transpose()?
    } else {
        None
    };
    let capture_errors = store.capture_errors.load(Ordering::Relaxed);
    let partial = capture_errors > 0
        || totals.missing_metering_calls > 0
        || totals.partial_metering_calls > 0
        || totals.unavailable_metering_calls > 0
        || !totals.unpriced_credits.is_zero();
    Ok(json!({
        "currency": "CNY", "from": window.from, "to": window.to, "generatedAt": now,
        "policyVersion": "prepaid-v1", "coverageState": if partial {"partial"} else {"unverified"},
        "captureErrorsSinceStart": capture_errors, "totals": totals, "accounts": summary,
        "lots": lot_details, "recentCalls": details.into_iter().rev().collect::<Vec<_>>(),
        "latestBalance": balance,
    }))
}
