/// Short, textual provenance supporting one stored memory.
///
/// This type deliberately represents selected evidence, not a raw transcript,
/// recording, image, or other source payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evidence {
    /// The concise supporting text selected by the caller.
    pub content: String,
    /// A caller-defined label such as `user_statement`, `observation`, or
    /// `assistant_inference`. The library preserves it without imposing a
    /// memory taxonomy.
    pub source_kind: String,
    /// UTC Unix milliseconds when the source was observed, when known.
    pub source_timestamp_ms: Option<i64>,
    /// A short opaque caller reference, when available. It is not interpreted
    /// or dereferenced by this library.
    pub source_reference: Option<String>,
}

impl Evidence {
    /// Builds concise evidence with no external source timestamp or reference.
    pub fn new(content: impl Into<String>, source_kind: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            source_kind: source_kind.into(),
            source_timestamp_ms: None,
            source_reference: None,
        }
    }
}
