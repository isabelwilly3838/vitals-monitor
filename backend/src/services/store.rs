//! Encrypted FHIR resource store on PostgreSQL + a resilient write path.
//!
//! Only non-sensitive index columns (type, patient id, LOINC code, effective time) are
//! clear text so searches run in SQL; the full FHIR JSON is AES-256-GCM encrypted.
//! Every query is parameterised (`$1` binds) - user input never becomes SQL text.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgPool, Postgres, QueryBuilder, Row};

use super::resilience::{BreakerError, CircuitBreaker, retry};
use crate::security::crypto::KeyRing;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("simulated storage outage")]
    Chaos,
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("decryption failed: {0}")]
    Crypto(#[from] crate::security::crypto::CryptoError),
    #[error("corrupt resource JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// One row to write: resource + index columns.
#[derive(Debug, Clone)]
pub struct Row_ {
    pub resource: Value,
    pub patient_id: Option<String>,
    pub code: Option<String>,
    pub effective: Option<DateTime<Utc>>,
}

pub struct PgStore {
    pub pool: PgPool,
    pub keys: KeyRing,
    /// Chaos switch: when set every call fails (demonstrates breaker + buffering).
    pub fail_mode: AtomicBool,
}

#[derive(Debug, Default)]
pub struct ObservationQuery {
    pub patient_id: Option<String>,
    pub code: Option<String>,
    pub date_ge: Option<DateTime<Utc>>,
    pub date_le: Option<DateTime<Utc>>,
    pub limit: i64,
}

impl PgStore {
    pub fn new(pool: PgPool, keys: KeyRing) -> Self {
        Self {
            pool,
            keys,
            fail_mode: AtomicBool::new(false),
        }
    }

    fn check(&self) -> Result<(), StoreError> {
        if self.fail_mode.load(Ordering::Relaxed) {
            Err(StoreError::Chaos)
        } else {
            Ok(())
        }
    }

    fn enc(&self, v: &Value) -> Vec<u8> {
        self.keys.encrypt(v.to_string().as_bytes())
    }

    fn dec(&self, blob: &[u8]) -> Result<Value, StoreError> {
        Ok(serde_json::from_slice(&self.keys.decrypt(blob)?)?)
    }

    #[tracing::instrument(name = "store.put", skip_all, fields(rtype, id))]
    pub async fn put(&self, row: &Row_) -> Result<(), StoreError> {
        self.check()?;
        self.put_many(std::slice::from_ref(row)).await
    }

    #[tracing::instrument(name = "store.put_many", skip_all, fields(count = rows.len()))]
    pub async fn put_many(&self, rows: &[Row_]) -> Result<(), StoreError> {
        self.check()?;
        let mut tx = self.pool.begin().await?;
        for r in rows {
            sqlx::query(
                "INSERT INTO resources (rtype, id, patient_id, code, effective, data)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (rtype, id) DO UPDATE SET patient_id = EXCLUDED.patient_id,
                   code = EXCLUDED.code, effective = EXCLUDED.effective, data = EXCLUDED.data",
            )
            .bind(r.resource["resourceType"].as_str())
            .bind(r.resource["id"].as_str())
            .bind(&r.patient_id)
            .bind(&r.code)
            .bind(r.effective)
            .bind(self.enc(&r.resource))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    #[tracing::instrument(name = "store.put_audit", skip_all)]
    pub async fn put_audit(&self, ev: &Value) -> Result<(), StoreError> {
        let recorded = ev["recorded"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok());
        sqlx::query("INSERT INTO audit_events (id, recorded, username, action, outcome, data) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(uuid::Uuid::parse_str(ev["id"].as_str().unwrap_or_default()).unwrap_or_else(|_| uuid::Uuid::new_v4()))
            .bind(recorded.map(|d| d.with_timezone(&Utc)).unwrap_or_else(Utc::now))
            .bind(ev["agent"][0]["who"]["identifier"]["value"].as_str().unwrap_or("unknown"))
            .bind(ev["action"].as_str().unwrap_or("E"))
            .bind(ev["outcome"].as_str().unwrap_or("0"))
            .bind(self.enc(ev))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get(&self, rtype: &str, id: &str) -> Result<Option<Value>, StoreError> {
        self.check()?;
        let row = sqlx::query("SELECT data FROM resources WHERE rtype = $1 AND id = $2")
            .bind(rtype)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| self.dec(r.get::<&[u8], _>("data"))).transpose()
    }

    pub async fn list_type(&self, rtype: &str) -> Result<Vec<Value>, StoreError> {
        self.check()?;
        let rows = sqlx::query("SELECT data FROM resources WHERE rtype = $1 ORDER BY id")
            .bind(rtype)
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| self.dec(r.get::<&[u8], _>("data")))
            .collect()
    }

    #[tracing::instrument(name = "store.search_observations", skip(self))]
    pub async fn search_observations(
        &self,
        q: &ObservationQuery,
    ) -> Result<(Vec<Value>, i64), StoreError> {
        self.check()?;
        fn filters<'a>(qb: &mut QueryBuilder<'a, Postgres>, q: &'a ObservationQuery) {
            qb.push(" WHERE rtype = 'Observation'");
            if let Some(p) = &q.patient_id {
                qb.push(" AND patient_id = ").push_bind(p);
            }
            if let Some(c) = &q.code {
                qb.push(" AND code = ").push_bind(c);
            }
            if let Some(d) = q.date_ge {
                qb.push(" AND effective >= ").push_bind(d);
            }
            if let Some(d) = q.date_le {
                qb.push(" AND effective <= ").push_bind(d);
            }
        }
        let mut count = QueryBuilder::new("SELECT COUNT(*) FROM resources");
        filters(&mut count, q);
        let total: i64 = count.build_query_scalar().fetch_one(&self.pool).await?;

        let mut sel = QueryBuilder::new("SELECT data FROM resources");
        filters(&mut sel, q);
        sel.push(" ORDER BY effective DESC LIMIT ")
            .push_bind(q.limit);
        let rows = sel.build().fetch_all(&self.pool).await?;
        let out = rows
            .iter()
            .map(|r| self.dec(r.get::<&[u8], _>("data")))
            .collect::<Result<_, _>>()?;
        Ok((out, total))
    }

    pub async fn list_audit(&self, limit: i64) -> Result<Vec<Value>, StoreError> {
        let rows = sqlx::query("SELECT data FROM audit_events ORDER BY recorded DESC LIMIT $1")
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| self.dec(r.get::<&[u8], _>("data")))
            .collect()
    }

    /// Re-encrypt every row with the current primary key, in one transaction.
    #[tracing::instrument(name = "store.rotate_keys", skip(self))]
    pub async fn rotate_keys(&self) -> Result<u64, StoreError> {
        let mut tx = self.pool.begin().await?;
        let mut n = 0;
        for r in sqlx::query("SELECT rtype, id, data FROM resources FOR UPDATE")
            .fetch_all(&mut *tx)
            .await?
        {
            let new = self.keys.rotate(r.get::<&[u8], _>("data"))?;
            sqlx::query("UPDATE resources SET data = $1 WHERE rtype = $2 AND id = $3")
                .bind(new)
                .bind(r.get::<String, _>("rtype"))
                .bind(r.get::<String, _>("id"))
                .execute(&mut *tx)
                .await?;
            n += 1;
        }
        for r in sqlx::query("SELECT id, data FROM audit_events FOR UPDATE")
            .fetch_all(&mut *tx)
            .await?
        {
            let new = self.keys.rotate(r.get::<&[u8], _>("data"))?;
            sqlx::query("UPDATE audit_events SET data = $1 WHERE id = $2")
                .bind(new)
                .bind(r.get::<uuid::Uuid, _>("id"))
                .execute(&mut *tx)
                .await?;
            n += 1;
        }
        tx.commit().await?;
        Ok(n)
    }

    pub async fn ping(&self) -> Result<(), StoreError> {
        self.check()?;
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }
}

/// Writes go through retry + circuit breaker. During an outage writes are buffered
/// (bounded) and drained once storage recovers: graceful degradation, no data loss for
/// short outages, and the live stream never waits on the database.
pub struct ResilientStore {
    pub store: Arc<PgStore>,
    pub breaker: CircuitBreaker,
    attempts: u32,
    buffer: Mutex<VecDeque<Row_>>,
    capacity: usize,
    dropped: AtomicU64,
}

impl ResilientStore {
    pub fn new(
        store: Arc<PgStore>,
        breaker: CircuitBreaker,
        attempts: u32,
        capacity: usize,
    ) -> Self {
        Self {
            store,
            breaker,
            attempts,
            buffer: Mutex::new(VecDeque::new()),
            capacity,
            dropped: AtomicU64::new(0),
        }
    }

    /// Returns true if persisted now, false if buffered for later.
    pub async fn save(&self, row: Row_) -> bool {
        let store = &self.store;
        let r = self
            .breaker
            .call(retry(
                "store.put",
                self.attempts,
                Duration::from_millis(20),
                || store.put(&row),
            ))
            .await;
        match r {
            Ok(()) => true,
            Err(e) => {
                let reason = match &e {
                    BreakerError::Open(_) => "circuit_open".to_string(),
                    BreakerError::Inner(e) => e.to_string(),
                };
                let mut buf = self.buffer.lock().unwrap();
                if buf.len() >= self.capacity {
                    buf.pop_front();
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
                buf.push_back(row);
                tracing::warn!(buffered = buf.len(), %reason, "storage degraded, buffering write");
                false
            }
        }
    }

    pub async fn drain(&self) -> usize {
        let batch: Vec<Row_> = self.buffer.lock().unwrap().iter().cloned().collect();
        if batch.is_empty() {
            return 0;
        }
        if self
            .breaker
            .call(self.store.put_many(&batch))
            .await
            .is_err()
        {
            return 0;
        }
        let mut buf = self.buffer.lock().unwrap();
        for _ in 0..batch.len().min(buf.len()) {
            buf.pop_front();
        }
        tracing::info!(count = batch.len(), "storage buffer drained");
        batch.len()
    }

    pub fn buffered(&self) -> usize {
        self.buffer.lock().unwrap().len()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}
