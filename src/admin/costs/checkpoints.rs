//! Rebuildable daily allocation checkpoints. Raw accounting facts are never deleted.
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

pub(super) const DAY: i64 = 86_400_000;

pub(super) fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS cost_capture_health (
        id INTEGER PRIMARY KEY CHECK(id=1), errors INTEGER NOT NULL);
        INSERT OR IGNORE INTO cost_capture_health VALUES (1,0);
        CREATE TABLE IF NOT EXISTS cost_revision (id INTEGER PRIMARY KEY CHECK(id=1), revision INTEGER NOT NULL);
        INSERT OR IGNORE INTO cost_revision VALUES (1,0);
        CREATE TABLE IF NOT EXISTS cost_change_days (
            credential_id INTEGER NOT NULL, day INTEGER NOT NULL, revision INTEGER NOT NULL,
            PRIMARY KEY(credential_id,day));
        CREATE TABLE IF NOT EXISTS cost_checkpoints (
            scope INTEGER NOT NULL, at INTEGER NOT NULL, state TEXT NOT NULL,
            PRIMARY KEY(scope,at));")?;
    for (table, at, credential) in [
        ("cost_calls", "{row}.started_at", "{row}.credential_id"),
        ("cost_lots", "{row}.effective_at", "{row}.credential_id"),
        (
            "cost_meters",
            "(SELECT started_at FROM cost_calls WHERE id={row}.call_id)",
            "(SELECT credential_id FROM cost_calls WHERE id={row}.call_id)",
        ),
    ] {
        for (action, rows) in [
            ("INSERT", vec!["NEW"]),
            ("UPDATE", vec!["OLD", "NEW"]),
            ("DELETE", vec!["OLD"]),
        ] {
            let mut body = String::from("UPDATE cost_revision SET revision=revision+1 WHERE id=1;");
            for row in rows {
                let at = at.replace("{row}", row);
                let credential = credential.replace("{row}", row);
                body.push_str(&format!(
                    "DELETE FROM cost_checkpoints WHERE at > {at}
                    AND (scope=0 OR scope={credential});
                    INSERT INTO cost_change_days(credential_id,day,revision)
                    SELECT {credential}, ({at}) - ((({at}) % {DAY} + {DAY}) % {DAY}), revision
                    FROM cost_revision WHERE id=1 AND {credential} IS NOT NULL
                    ON CONFLICT(credential_id,day) DO UPDATE SET revision=excluded.revision;"
                ));
            }
            conn.execute_batch(&format!(
                "CREATE TRIGGER IF NOT EXISTS {table}_checkpoint_{action}
                AFTER {action} ON {table} BEGIN {body} END;"
            ))?;
        }
    }
    Ok(())
}

pub(super) fn load(conn: &Connection, scope: u64, before: i64) -> Result<Option<(i64, String)>> {
    Ok(conn
        .query_row(
            "SELECT at,state FROM cost_checkpoints WHERE scope=?1 AND at<=?2
        ORDER BY at DESC LIMIT 1",
            params![scope, before],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// Check for changes *before each checkpoint*, not merely any concurrent writes.
/// Current traffic must not starve saving checkpoints for older, stable days.
pub(super) fn save(path: &Path, scope: u64, revision: i64, states: &[(i64, String)]) -> Result<()> {
    if states.is_empty() {
        return Ok(());
    }
    let mut conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_millis(50))?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for (at, state) in states {
        let changed: i64 = tx.query_row(
            "SELECT COALESCE(MAX(revision),0) FROM cost_change_days
            WHERE day < ?1 AND (?2=0 OR credential_id=?2)",
            params![at, scope],
            |r| r.get(0),
        )?;
        if changed <= revision {
            tx.execute(
                "INSERT INTO cost_checkpoints(scope,at,state) VALUES (?1,?2,?3)
                ON CONFLICT(scope,at) DO UPDATE SET state=excluded.state",
                params![scope, at, state],
            )?;
        }
    }
    tx.execute(
        "DELETE FROM cost_checkpoints WHERE scope=?1 AND at NOT IN
        (SELECT at FROM cost_checkpoints WHERE scope=?1 ORDER BY at DESC LIMIT 400)",
        [scope],
    )?;
    tx.commit()?;
    Ok(())
}
