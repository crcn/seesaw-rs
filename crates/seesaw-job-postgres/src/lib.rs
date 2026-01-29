//! PostgreSQL implementation of Seesaw job queue.
//!
//! This crate provides a production-ready PostgreSQL implementation of the
//! `JobStore` trait from the Seesaw framework.
//!
//! # Features
//!
//! - Optimistic locking with `FOR UPDATE SKIP LOCKED`
//! - Exponential backoff retry logic
//! - Dead letter queue for permanently failed jobs
//! - Worker heartbeats for long-running jobs
//! - Configurable lease timeouts
//!
//! # Database Schema
//!
//! ```sql
//! CREATE TYPE job_status AS ENUM ('pending', 'running', 'succeeded', 'failed', 'dead_letter');
//! CREATE TYPE error_kind AS ENUM ('retryable', 'non_retryable');
//!
//! CREATE TABLE jobs (
//!     id UUID PRIMARY KEY,
//!     job_type TEXT NOT NULL,
//!     payload JSONB NOT NULL,
//!     version INTEGER NOT NULL DEFAULT 1,
//!
//!     -- Execution
//!     status job_status NOT NULL DEFAULT 'pending',
//!     attempt INTEGER NOT NULL DEFAULT 1,
//!     max_retries INTEGER NOT NULL DEFAULT 3,
//!
//!     -- Scheduling
//!     priority INTEGER NOT NULL DEFAULT 0,
//!     run_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
//!
//!     -- Worker tracking
//!     worker_id TEXT,
//!     lease_expires_at TIMESTAMPTZ,
//!
//!     -- Error tracking
//!     error_message TEXT,
//!     error_kind error_kind,
//!
//!     -- Timestamps
//!     created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
//!     updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
//! );
//!
//! CREATE INDEX idx_jobs_ready ON jobs (priority, run_at)
//!     WHERE status = 'pending' AND run_at <= NOW();
//! CREATE INDEX idx_jobs_lease ON jobs (lease_expires_at)
//!     WHERE status = 'running' AND lease_expires_at IS NOT NULL;
//! ```
//!
//! # Usage
//!
//! ```rust,ignore
//! use seesaw_job_postgres::PgJobStore;
//! use sqlx::PgPool;
//!
//! let pool = PgPool::connect("postgres://localhost/mydb").await?;
//! let store = PgJobStore::new(pool);
//!
//! // Use with seesaw dispatcher
//! let dispatcher = Dispatcher::with_job_queue(deps, bus, Arc::new(store));
//! ```

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use seesaw::job::{ClaimedJob, FailureKind, JobStore};
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// PostgreSQL job store implementation.
#[derive(Clone)]
pub struct PgJobStore {
    pool: PgPool,
    default_lease_ms: i64,
}

impl PgJobStore {
    /// Create a new PostgreSQL job store.
    ///
    /// # Arguments
    ///
    /// * `pool` - PostgreSQL connection pool
    ///
    /// # Default Settings
    ///
    /// - Lease timeout: 60 seconds
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            default_lease_ms: 60_000,
        }
    }

    /// Create a job store with custom lease timeout.
    ///
    /// The lease timeout determines how long a worker can hold a job
    /// before it's considered abandoned.
    pub fn with_lease_timeout(pool: PgPool, lease_ms: i64) -> Self {
        Self {
            pool,
            default_lease_ms: lease_ms,
        }
    }

    /// Get the underlying connection pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

#[async_trait]
impl JobStore for PgJobStore {
    /// Claim ready jobs for execution.
    ///
    /// Uses `FOR UPDATE SKIP LOCKED` for optimistic concurrency.
    async fn claim_ready(&self, worker_id: &str, limit: i64) -> Result<Vec<ClaimedJob>> {
        let lease_expires_at = Utc::now() + Duration::milliseconds(self.default_lease_ms);

        let rows = sqlx::query(
            r#"
            WITH claimable AS (
                SELECT id
                FROM jobs
                WHERE status = 'pending'
                  AND run_at <= NOW()
                ORDER BY priority ASC, run_at ASC
                LIMIT $1
                FOR UPDATE SKIP LOCKED
            )
            UPDATE jobs
            SET status = 'running',
                worker_id = $2,
                lease_expires_at = $3,
                updated_at = NOW()
            WHERE id IN (SELECT id FROM claimable)
            RETURNING id, job_type, payload, version, attempt
            "#,
        )
        .bind(limit)
        .bind(worker_id)
        .bind(lease_expires_at)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| ClaimedJob {
                id: row.get("id"),
                job_type: row.get("job_type"),
                payload: row.get("payload"),
                version: row.get("version"),
                attempt: row.get("attempt"),
            })
            .collect())
    }

    /// Mark a job as successfully completed.
    async fn mark_succeeded(&self, job_id: Uuid) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE jobs
            SET status = 'succeeded',
                updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(job_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Mark a job as failed and handle retries.
    ///
    /// # Retry Logic
    ///
    /// - Retryable failures: Schedules retry with exponential backoff (2^attempt seconds, max 1 hour)
    /// - Non-retryable failures: Immediately moves to dead letter
    /// - Max retries exceeded: Moves to dead letter
    async fn mark_failed(&self, job_id: Uuid, error: &str, kind: FailureKind) -> Result<()> {
        let mut tx = self.pool.begin().await?;

        // Fetch current job state
        let job = sqlx::query("SELECT attempt, max_retries FROM jobs WHERE id = $1 FOR UPDATE")
            .bind(job_id)
            .fetch_one(&mut *tx)
            .await?;

        let attempt: i32 = job.get("attempt");
        let max_retries: i32 = job.get("max_retries");

        match kind {
            FailureKind::Retryable if attempt < max_retries => {
                // Schedule retry with exponential backoff
                let delay_secs = 2i64.pow(attempt as u32).min(3600); // Max 1 hour
                let retry_at = Utc::now() + Duration::seconds(delay_secs);

                sqlx::query(
                    r#"
                    UPDATE jobs
                    SET status = 'pending',
                        run_at = $1,
                        attempt = attempt + 1,
                        error_message = $2,
                        error_kind = 'retryable',
                        worker_id = NULL,
                        lease_expires_at = NULL,
                        updated_at = NOW()
                    WHERE id = $3
                    "#,
                )
                .bind(retry_at)
                .bind(error)
                .bind(job_id)
                .execute(&mut *tx)
                .await?;
            }
            _ => {
                // No retries left or non-retryable failure - dead letter
                sqlx::query(
                    r#"
                    UPDATE jobs
                    SET status = 'dead_letter',
                        error_message = $1,
                        error_kind = $2,
                        updated_at = NOW()
                    WHERE id = $3
                    "#,
                )
                .bind(error)
                .bind(match kind {
                    FailureKind::Retryable => "retryable",
                    FailureKind::NonRetryable => "non_retryable",
                })
                .bind(job_id)
                .execute(&mut *tx)
                .await?;
            }
        }

        tx.commit().await?;
        Ok(())
    }

    /// Extend the lease for a running job.
    ///
    /// Workers should call this periodically for long-running jobs
    /// to prevent them from being reclaimed.
    async fn heartbeat(&self, job_id: Uuid) -> Result<()> {
        let lease_expires_at = Utc::now() + Duration::milliseconds(self.default_lease_ms);

        sqlx::query(
            r#"
            UPDATE jobs
            SET lease_expires_at = $1,
                updated_at = NOW()
            WHERE id = $2 AND status = 'running'
            "#,
        )
        .bind(lease_expires_at)
        .bind(job_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}

/// Utility functions for job management.
impl PgJobStore {
    /// Reclaim abandoned jobs (lease expired).
    ///
    /// This should be run periodically by a maintenance worker.
    pub async fn reclaim_expired(&self) -> Result<u64> {
        let result = sqlx::query(
            r#"
            UPDATE jobs
            SET status = 'pending',
                worker_id = NULL,
                lease_expires_at = NULL,
                updated_at = NOW()
            WHERE status = 'running'
              AND lease_expires_at < NOW()
            "#,
        )
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }

    /// Clean up old completed jobs.
    ///
    /// # Arguments
    ///
    /// * `older_than` - Delete jobs completed before this timestamp
    pub async fn cleanup_succeeded(&self, older_than: DateTime<Utc>) -> Result<u64> {
        let result = sqlx::query(
            r#"
            DELETE FROM jobs
            WHERE status = 'succeeded'
              AND updated_at < $1
            "#,
        )
        .bind(older_than)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }

    /// Get statistics about job queue health.
    pub async fn stats(&self) -> Result<QueueStats> {
        let row = sqlx::query(
            r#"
            SELECT
                COUNT(*) FILTER (WHERE status = 'pending') as pending,
                COUNT(*) FILTER (WHERE status = 'running') as running,
                COUNT(*) FILTER (WHERE status = 'succeeded') as succeeded,
                COUNT(*) FILTER (WHERE status = 'failed') as failed,
                COUNT(*) FILTER (WHERE status = 'dead_letter') as dead_letter
            FROM jobs
            "#,
        )
        .fetch_one(&self.pool)
        .await?;

        Ok(QueueStats {
            pending: row.get("pending"),
            running: row.get("running"),
            succeeded: row.get("succeeded"),
            failed: row.get("failed"),
            dead_letter: row.get("dead_letter"),
        })
    }
}

/// Job queue statistics.
#[derive(Debug, Clone, Copy)]
pub struct QueueStats {
    pub pending: i64,
    pub running: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub dead_letter: i64,
}
