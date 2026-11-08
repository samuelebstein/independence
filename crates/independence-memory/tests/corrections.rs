use independence_memory::{CorrectInput, MemoryError, MemoryInput, MemoryStatus, MemoryStore};
use tempfile::tempdir;

#[test]
fn correction_preserves_history_and_only_returns_current_record() {
    let directory = tempdir().unwrap();
    let mut store = MemoryStore::open(directory.path()).unwrap();
    let original = store
        .remember(MemoryInput::new("The Pi test used an 8B model."))
        .unwrap();

    let correction = store
        .correct(CorrectInput {
            memory_id: original.id,
            replacement: MemoryInput::new("The Pi test used a 4B model."),
        })
        .unwrap();

    assert_eq!(correction.previous.status, MemoryStatus::Superseded);
    assert_eq!(correction.current.status, MemoryStatus::Current);
    assert_eq!(correction.current.supersedes, Some(original.id));
    assert!(
        correction
            .current
            .related_memories
            .iter()
            .any(|related| related.memory_id == original.id && related.relationship == "corrects")
    );

    let recalled = store.recall("Pi test model", 5).unwrap();
    assert_eq!(recalled.len(), 1);
    assert_eq!(recalled[0].id, correction.current.id);
}

#[test]
fn failed_correction_rolls_back_the_prior_current_status() {
    let directory = tempdir().unwrap();
    let mut store = MemoryStore::open(directory.path()).unwrap();
    let original = store
        .remember(MemoryInput::new("The original measurement was 40 seconds."))
        .unwrap();

    let mut replacement = MemoryInput::new("The corrected measurement was 41 seconds.");
    replacement
        .related_memories
        .push(independence_memory::MemoryId::try_from(999_999).unwrap());
    let result = store.correct(CorrectInput {
        memory_id: original.id,
        replacement,
    });
    assert!(matches!(result, Err(MemoryError::MemoryNotFound(999_999))));

    let recalled = store.recall("original measurement", 5).unwrap();
    assert_eq!(recalled.len(), 1);
    assert_eq!(recalled[0].id, original.id);
    assert_eq!(recalled[0].status, MemoryStatus::Current);
}
