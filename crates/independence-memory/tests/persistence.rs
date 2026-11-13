use independence_memory::{Evidence, MemoryInput, MemoryStore};
use rusqlite::Connection;
use tempfile::tempdir;

#[test]
fn creates_reopens_and_preserves_a_memory() {
    let directory = tempdir().unwrap();
    let id = {
        let mut store = MemoryStore::open(directory.path()).unwrap();
        assert_eq!(store.database_path(), directory.path().join("memory.db"));
        let mut input = MemoryInput::new("The Pi experiment used a 4B model.");
        input.evidence.push(Evidence::new(
            "The user said the model was 4B.",
            "user_statement",
        ));
        store.remember(input).unwrap().id
    };

    let store = MemoryStore::open(directory.path()).unwrap();
    let results = store.recall("Pi experiment model", 5).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, id);
    assert_eq!(results[0].evidence.len(), 1);
    assert_eq!(results[0].evidence[0].source_kind, "user_statement");
}

#[test]
fn database_is_created_only_inside_the_selected_directory() {
    let directory = tempdir().unwrap();
    let requested = directory.path().join("chosen-memory-location");
    let store = MemoryStore::open(&requested).unwrap();
    assert_eq!(store.data_directory(), requested.canonicalize().unwrap());
    assert!(requested.join("memory.db").is_file());
}

#[test]
fn exact_repeated_remember_does_not_create_unbounded_duplicate_records() {
    let directory = tempdir().unwrap();
    let mut store = MemoryStore::open(directory.path()).unwrap();
    let first = store
        .remember(MemoryInput::new("The Pi experiment took 40 seconds."))
        .unwrap();
    let repeated = store
        .remember(MemoryInput::new("The Pi experiment took 40 seconds."))
        .unwrap();

    assert_eq!(first.id, repeated.id);
    assert_eq!(store.recall("Pi experiment", 5).unwrap().len(), 1);
}

#[test]
fn opens_a_version_one_database_and_applies_the_duplicate_lookup_index_migration() {
    let directory = tempdir().unwrap();
    {
        let _store = MemoryStore::open(directory.path()).unwrap();
    }

    // Reproduce the durable schema left by version 1 without hand-writing a
    // divergent old schema. A new open must upgrade it instead of discarding
    // data or requiring the caller to recreate the database.
    let database_path = directory.path().join("memory.db");
    {
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute("DROP INDEX memories_status_content_idx", [])
            .unwrap();
        connection
            .execute("DELETE FROM schema_migrations WHERE version = 2", [])
            .unwrap();
    }

    let _store = MemoryStore::open(directory.path()).unwrap();
    let connection = Connection::open(&database_path).unwrap();
    let version = connection
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    let index_exists = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'memories_status_content_idx')",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(version, 2);
    assert_eq!(index_exists, 1);
}
