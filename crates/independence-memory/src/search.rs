use rusqlite::params;
use std::time::Instant;

use tracing::{info, info_span};

use crate::{MemoryError, MemoryId, MemoryRecord, MemoryStatus, database::load_memory};

const MAX_RECALL_LIMIT: usize = 20;

use crate::MemoryStore;

impl MemoryStore {
    /// Searches current memories through SQLite FTS5.
    ///
    /// A query is converted into literal searchable terms before reaching
    /// SQLite, so user/model punctuation cannot become raw FTS syntax. The
    /// caller chooses a bounded result count from 1 through 20.
    pub fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryRecord>, MemoryError> {
        if !(1..=MAX_RECALL_LIMIT).contains(&limit) {
            return Err(MemoryError::InvalidRecallLimit {
                maximum: MAX_RECALL_LIMIT,
            });
        }
        let fts_query = searchable_query(query)?;
        let span = info_span!("independence_memory.recall", requested_limit = limit);
        let _entered = span.enter();
        let operation_started = Instant::now();
        let ids = {
            let search_span = info_span!("independence_memory.fts_search", requested_limit = limit);
            let _search_entered = search_span.enter();
            let search_started = Instant::now();
            let mut statement = self
                .connection()
                .prepare(
                    "SELECT memory_fts.rowid
                     FROM memory_fts
                     JOIN memories ON memories.id = memory_fts.rowid
                     WHERE memory_fts MATCH ?1 AND memories.status = ?2
                     ORDER BY bm25(memory_fts), memories.updated_at_ms DESC
                     LIMIT ?3",
                )
                .map_err(MemoryError::Database)?;
            let limit = i64::try_from(limit).expect("bounded recall limit fits i64");
            let ids = statement
                .query_map(
                    params![fts_query, MemoryStatus::Current.as_database(), limit],
                    |row| Ok(MemoryId::from_database(row.get(0)?)),
                )
                .map_err(MemoryError::Database)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(MemoryError::Database)?;
            info!(
                target: "independence_memory",
                event = "fts_search_completed",
                result_count = ids.len(),
                duration_ms = search_started.elapsed().as_millis() as u64,
                "full-text memory search completed"
            );
            ids
        };
        let mut results = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(memory) = load_memory(self.connection(), id)? {
                results.push(memory);
            }
        }
        info!(
            target: "independence_memory",
            event = "memory_recalled",
            result_count = results.len(),
            duration_ms = operation_started.elapsed().as_millis() as u64,
            "memory recall completed"
        );
        Ok(results)
    }
}

fn searchable_query(query: &str) -> Result<String, MemoryError> {
    let terms = query
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{term}\""))
        .collect::<Vec<_>>();
    if terms.is_empty() {
        return Err(MemoryError::EmptyRecallQuery);
    }
    Ok(terms.join(" AND "))
}

#[cfg(test)]
mod tests {
    use super::searchable_query;
    use crate::MemoryError;

    #[test]
    fn searchable_query_never_passes_raw_fts_operators() {
        assert_eq!(
            searchable_query("model: Pi OR crash!").unwrap(),
            "\"model\" AND \"Pi\" AND \"OR\" AND \"crash\""
        );
        assert!(matches!(
            searchable_query("!!!"),
            Err(MemoryError::EmptyRecallQuery)
        ));
    }
}
