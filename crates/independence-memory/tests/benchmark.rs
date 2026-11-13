//! Deliberately ignored, repeatable local SQLite benchmark.
//!
//! Run with:
//! `cargo test -p independence-memory --test benchmark -- --ignored --nocapture`
//!
//! This uses temporary test directories and leaves no application database
//! behind. It is a measurement harness, not a performance assertion or CI
//! gate. Results depend on storage, CPU, compiler profile, and SQLite build.

use std::time::Instant;

use independence_memory::{MemoryInput, MemoryStore};
use tempfile::tempdir;

#[test]
#[ignore = "benchmark: run explicitly with --ignored --nocapture"]
fn measures_commit_and_fts_recall_at_increasing_database_sizes() {
    for count in [100usize, 1_000, 10_000, 100_000] {
        let directory = tempdir().expect("benchmark temporary directory");
        let mut store = MemoryStore::open(directory.path()).expect("benchmark database opens");

        let commit_started = Instant::now();
        for index in 0..count {
            store
                .remember(MemoryInput::new(format!(
                    "Benchmark memory {index}: Raspberry Pi model experiment measurement {} milliseconds",
                    10_000 + index
                )))
                .expect("benchmark memory write");
        }
        let commit_elapsed = commit_started.elapsed();

        let recall_started = Instant::now();
        let mut returned = 0usize;
        for _ in 0..100 {
            returned = returned.saturating_add(
                store
                    .recall("Raspberry Pi model experiment", 10)
                    .expect("benchmark FTS recall")
                    .len(),
            );
        }
        let recall_elapsed = recall_started.elapsed();
        println!(
            "memories={count} commit_total_ms={} commit_avg_us={} recall_100_total_ms={} recall_avg_us={} returned={returned}",
            commit_elapsed.as_millis(),
            commit_elapsed.as_micros() / count as u128,
            recall_elapsed.as_millis(),
            recall_elapsed.as_micros() / 100,
        );
    }
}
