use independence_memory::{MemoryInput, MemoryStore};
use tempfile::tempdir;

#[test]
fn fts_recall_is_bounded_and_returns_no_results_without_error() {
    let directory = tempdir().unwrap();
    let mut store = MemoryStore::open(directory.path()).unwrap();
    for index in 0..4 {
        store
            .remember(MemoryInput::new(format!(
                "Raspberry Pi model experiment number {index}"
            )))
            .unwrap();
    }
    store
        .remember(MemoryInput::new("Unrelated garden observation"))
        .unwrap();

    let results = store.recall("Raspberry model", 2).unwrap();
    assert_eq!(results.len(), 2);
    assert!(
        results
            .iter()
            .all(|memory| memory.content.contains("Raspberry Pi"))
    );
    assert!(store.recall("nothing matching here", 5).unwrap().is_empty());
}
