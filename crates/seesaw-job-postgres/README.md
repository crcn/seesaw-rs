# seesaw-job-postgres

PostgreSQL implementation of the Seesaw job queue.

## Features

- ✅ Production-ready Postgres implementation of `JobStore`
- ✅ Optimistic locking with `FOR UPDATE SKIP LOCKED`
- ✅ Exponential backoff retry logic
- ✅ Dead letter queue for failed jobs
- ✅ Worker heartbeats for long-running jobs
- ✅ Configurable lease timeouts
- ✅ Queue statistics and maintenance utilities

## Installation

```toml
[dependencies]
seesaw = "0.1"
seesaw-job-postgres = "0.1"
sqlx = { version = "0.8", features = ["runtime-tokio", "postgres"] }
```

## Database Setup

Run the migration to create the required schema:

```sql
CREATE TYPE job_status AS ENUM ('pending', 'running', 'succeeded', 'failed', 'dead_letter');
CREATE TYPE error_kind AS ENUM ('retryable', 'non_retryable');

CREATE TABLE jobs (
    id UUID PRIMARY KEY,
    job_type TEXT NOT NULL,
    payload JSONB NOT NULL,
    version INTEGER NOT NULL DEFAULT 1,

    -- Execution
    status job_status NOT NULL DEFAULT 'pending',
    attempt INTEGER NOT NULL DEFAULT 1,
    max_retries INTEGER NOT NULL DEFAULT 3,

    -- Scheduling
    priority INTEGER NOT NULL DEFAULT 0,
    run_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    -- Worker tracking
    worker_id TEXT,
    lease_expires_at TIMESTAMPTZ,

    -- Error tracking
    error_message TEXT,
    error_kind error_kind,

    -- Timestamps
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_jobs_ready ON jobs (priority, run_at)
    WHERE status = 'pending' AND run_at <= NOW();
CREATE INDEX idx_jobs_lease ON jobs (lease_expires_at)
    WHERE status = 'running' AND lease_expires_at IS NOT NULL;
```

## Usage

```rust
use seesaw_job_postgres::PgJobStore;
use sqlx::PgPool;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Connect to Postgres
    let pool = PgPool::connect("postgres://localhost/mydb").await?;

    // Create job store
    let store = PgJobStore::new(pool);

    // Use with seesaw dispatcher
    let dispatcher = Dispatcher::with_job_queue(
        deps,
        bus,
        Arc::new(store)
    );

    Ok(())
}
```

## Custom Lease Timeout

```rust
// 5 minute lease timeout
let store = PgJobStore::with_lease_timeout(pool, 300_000);
```

## Maintenance Tasks

### Reclaim Abandoned Jobs

Run periodically to reclaim jobs with expired leases:

```rust
let reclaimed = store.reclaim_expired().await?;
println!("Reclaimed {} abandoned jobs", reclaimed);
```

### Clean Up Old Jobs

Remove succeeded jobs older than a threshold:

```rust
use chrono::{Utc, Duration};

let cutoff = Utc::now() - Duration::days(7);
let deleted = store.cleanup_succeeded(cutoff).await?;
println!("Deleted {} old jobs", deleted);
```

### Queue Statistics

Monitor queue health:

```rust
let stats = store.stats().await?;
println!("Pending: {}", stats.pending);
println!("Running: {}", stats.running);
println!("Dead letter: {}", stats.dead_letter);
```

## License

MIT
