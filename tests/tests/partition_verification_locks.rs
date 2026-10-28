// Copyright (c) 2023-2026 ParadeDB, Inc.
// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::Result;
use rstest::rstest;
use sqlx::Connection;
use std::time::Duration;
use tests::fixtures::*;

#[rstest]
#[async_std::test]
async fn parent_verification_locks_leaf_heap_first(
    database: Db,
    #[values(false, true)] nested: bool,
) -> Result<()> {
    let mut writer = database.connection().await;
    "CREATE EXTENSION IF NOT EXISTS pg_search CASCADE;
     CREATE TABLE lock_parent(id integer) PARTITION BY RANGE(id);"
        .execute(&mut writer);
    let leaf_parent = if nested {
        "CREATE TABLE lock_intermediate PARTITION OF lock_parent
         FOR VALUES FROM (0) TO (20) PARTITION BY RANGE(id);"
            .execute(&mut writer);
        "lock_intermediate"
    } else {
        "lock_parent"
    };
    format!(
        "CREATE TABLE lock_leaf PARTITION OF {leaf_parent} FOR VALUES FROM (0) TO (10);
         CREATE TABLE lock_other PARTITION OF {leaf_parent} FOR VALUES FROM (10) TO (20);
         INSERT INTO lock_parent VALUES (1), (11);
         CREATE INDEX lock_parent_idx ON lock_parent USING paradedb(id);"
    )
    .execute(&mut writer);
    let mut reader = database.connection().await;
    "SET statement_timeout = '15s'".execute(&mut writer);
    "SET statement_timeout = '15s'".execute(&mut reader);
    let (reader_pid,): (i32,) = "SELECT pg_backend_pid()".fetch_one(&mut reader);
    let mut observer = database.connection().await;

    let mut transaction = writer.begin().await?;
    sqlx::query("LOCK TABLE lock_leaf IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *transaction)
        .await?;
    let pending = async_std::task::spawn(async move {
        sqlx::query_as::<_, (bool, i64)>(
            "SELECT bool_and(passed),
                    count(*) FILTER (WHERE check_name LIKE '%: heap_references_valid')
             FROM pdb.verify_index('lock_parent_idx', heapallindexed => true)",
        )
        .fetch_one(&mut reader)
        .await
    });

    // Synchronize on the real heap-lock wait, not a sleep or the reader starting.
    async_std::future::timeout(Duration::from_secs(10), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT FROM pg_locks
                 WHERE pid = $1 AND relation = 'lock_leaf'::regclass
                   AND mode = 'AccessShareLock' AND NOT granted)",
            )
            .bind(reader_pid)
            .fetch_one(&mut observer)
            .await?;
            if waiting {
                return Ok::<_, sqlx::Error>(());
            }
            async_std::task::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    let index_locked_before_heap: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT FROM pg_locks l JOIN pg_index i ON i.indexrelid = l.relation
         WHERE l.pid = $1 AND i.indrelid = 'lock_leaf'::regclass AND l.granted)",
    )
    .bind(reader_pid)
    .fetch_one(&mut observer)
    .await?;

    // REINDEX needs the leaf-index lock. Acquiring it before the heap in the
    // reader would make these sessions wait on each other.
    let reindex = sqlx::query("REINDEX TABLE lock_leaf")
        .execute(&mut *transaction)
        .await;
    if reindex.is_ok() {
        transaction.commit().await?;
    } else {
        transaction.rollback().await?;
    }
    let verified = pending.await;
    reindex?;
    assert_eq!(verified?, (true, 2));
    assert!(
        !index_locked_before_heap,
        "the verifier locked the leaf index before obtaining the leaf heap lock"
    );
    Ok(())
}
