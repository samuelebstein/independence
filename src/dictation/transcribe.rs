//! Local whisper.cpp transcription behind a deliberately small test seam.

use std::{
    env,
    ffi::c_void,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::protocol::CapturedAudio;
use crate::telemetry::{self, Interaction};

/// Returns a stable pointer to the cancellation flag for whisper.cpp's native
/// abort callback. The caller keeps the `Arc` alive for the whole synchronous
/// `WhisperState::full` call.
fn cancellation_callback_data(cancelled: &Arc<AtomicBool>) -> *mut c_void {
    Arc::as_ptr(cancelled).cast_mut().cast()
}

/// Native callback used by whisper.cpp while encoding and decoding.
///
/// `whisper-rs` 0.16.0 exposes this lower-level API. Using it here avoids its
/// convenience callback wrapper, whose pointer ownership is not suitable for
/// repeated dictation calls. The only operation is an atomic read, so it is
/// safe when whisper.cpp invokes it from its worker threads.
unsafe extern "C" fn transcription_cancelled_callback(user_data: *mut c_void) -> bool {
    if user_data.is_null() {
        // A null callback pointer should never be configured by this module.
        // Aborting is safer than dereferencing an invalid address.
        return true;
    }

    // SAFETY: `cancellation_callback_data` derives this pointer from an
    // `Arc<AtomicBool>` that remains owned by `transcribe` until
    // `WhisperState::full` returns. whisper.cpp uses it only during that
    // synchronous call, and AtomicBool permits concurrent reads/writes.
    let cancelled = unsafe { &*user_data.cast::<AtomicBool>() };
    cancelled.load(Ordering::Acquire)
}

fn install_cancellation_callback(parameters: &mut FullParams<'_, '_>, cancelled: &Arc<AtomicBool>) {
    let user_data = cancellation_callback_data(cancelled);

    // SAFETY: the callback and pointer have the lifetime and thread-safety
    // guarantees documented directly above. `state.full` is synchronous, so
    // it cannot retain this pointer after `transcribe` returns.
    unsafe {
        parameters.set_abort_callback(Some(transcription_cancelled_callback));
        parameters.set_abort_callback_user_data(user_data);
    }
}

/// The recognizer boundary has no notion of questions, commands, Ollama, or
/// cameras. It receives prepared audio and returns only text or an error.
pub trait Transcriber: Send + Sync {
    fn model_path(&self) -> &Path;
    fn model_is_available(&self) -> bool;
    fn transcribe(
        &self,
        audio: CapturedAudio,
        cancelled: Arc<AtomicBool>,
        kind: TranscriptionKind,
        interaction: Option<&Interaction>,
    ) -> Result<String, TranscriptionError>;
}

/// The UI purpose of a recognizer call. Both paths use the same local
/// Whisper model, but partial previews must remain distinguishable from the
/// final submission in telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptionKind {
    Partial,
    Final,
}

impl TranscriptionKind {
    fn operation(self) -> &'static str {
        match self {
            Self::Partial => "whisper.partial.recognition",
            Self::Final => "whisper.final.recognition",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Partial => "partial",
            Self::Final => "final",
        }
    }
}

#[derive(Debug)]
pub enum TranscriptionError {
    Cancelled,
    Failed(String),
}

/// CPU-only whisper.cpp recognizer. The immutable model context is loaded on
/// the first transcription and reused; each recording gets its own state.
pub struct WhisperTranscriber {
    model_path: PathBuf,
    context: Mutex<Option<Arc<WhisperContext>>>,
}

impl WhisperTranscriber {
    pub fn from_environment() -> Self {
        let model_path = env::var_os("INDEPENDENCE_WHISPER_MODEL")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("local-data/whisper/ggml-base.en.bin"));
        Self::new(model_path)
    }

    pub fn new(model_path: PathBuf) -> Self {
        // This prevents whisper.cpp from writing model/progress diagnostics to
        // the terminal, where they could interleave with intentionally shown
        // transcript text. It is safe to call more than once.
        whisper_rs::install_logging_hooks();
        Self {
            model_path,
            context: Mutex::new(None),
        }
    }

    fn context(
        &self,
        cancelled: &AtomicBool,
        interaction: Option<&Interaction>,
    ) -> Result<Arc<WhisperContext>, TranscriptionError> {
        if cancelled.load(Ordering::Acquire) {
            return Err(TranscriptionError::Cancelled);
        }

        let mut slot = self.context.lock().map_err(|_| {
            TranscriptionError::Failed("local recognizer model state lock was poisoned".into())
        })?;
        if let Some(context) = slot.as_ref() {
            return Ok(context.clone());
        }

        let mut loading = interaction
            .map(|interaction| interaction.stage("whisper.context.load", telemetry::Fields::new()));

        if !self.model_path.is_file() {
            if let Some(stage) = loading.take() {
                stage.finish(
                    "failed",
                    telemetry::fields([("failure_class", serde_json::json!("model_file_missing"))]),
                );
            }
            return Err(TranscriptionError::Failed(format!(
                "Whisper model file is missing: {}; set INDEPENDENCE_WHISPER_MODEL to a local ggml base.en model",
                self.model_path.display()
            )));
        }
        if cancelled.load(Ordering::Acquire) {
            if let Some(stage) = loading.take() {
                stage.finish("cancelled", telemetry::Fields::new());
            }
            return Err(TranscriptionError::Cancelled);
        }

        let context =
            WhisperContext::new_with_params(&self.model_path, WhisperContextParameters::default())
                .map_err(|error| {
                    if let Some(stage) = loading.take() {
                        stage.finish(
                            "failed",
                            telemetry::fields([(
                                "failure_class",
                                serde_json::json!("model_context_failure"),
                            )]),
                        );
                    }
                    TranscriptionError::Failed(format!(
                        "could not load local Whisper model {}: {error}",
                        self.model_path.display()
                    ))
                })?;
        if cancelled.load(Ordering::Acquire) {
            // Model construction completed, but it must not be retained for
            // this cancelled interaction as a successful context-load stage.
            if let Some(stage) = loading.take() {
                stage.finish("cancelled", telemetry::Fields::new());
            }
            return Err(TranscriptionError::Cancelled);
        }

        let context = Arc::new(context);
        *slot = Some(context.clone());
        if let Some(stage) = loading.take() {
            stage.finish("completed", telemetry::Fields::new());
        }
        Ok(context)
    }
}

impl Transcriber for WhisperTranscriber {
    fn model_path(&self) -> &Path {
        &self.model_path
    }

    fn model_is_available(&self) -> bool {
        self.model_path.is_file()
    }

    fn transcribe(
        &self,
        audio: CapturedAudio,
        cancelled: Arc<AtomicBool>,
        kind: TranscriptionKind,
        interaction: Option<&Interaction>,
    ) -> Result<String, TranscriptionError> {
        let audio_samples = audio.samples().len();
        let recognition = interaction.map(|interaction| {
            interaction.stage(
                kind.operation(),
                telemetry::fields([
                    ("transcription_kind", serde_json::json!(kind.label())),
                    ("input_samples", serde_json::json!(audio_samples)),
                    (
                        "input_audio_duration_ms",
                        serde_json::json!((audio_samples as u64).saturating_mul(1_000) / 16_000),
                    ),
                ]),
            )
        });
        let result = (|| {
            let context = self.context(&cancelled, interaction)?;
            if cancelled.load(Ordering::Acquire) {
                return Err(TranscriptionError::Cancelled);
            }

            let mut state = context.create_state().map_err(|error| {
                TranscriptionError::Failed(format!(
                    "could not create local recognition state: {error}"
                ))
            })?;
            let mut parameters = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            parameters.set_language(Some("en"));
            parameters.set_translate(false);
            parameters.set_no_context(true);
            parameters.set_no_timestamps(true);
            parameters.set_print_progress(false);
            parameters.set_print_realtime(false);
            parameters.set_print_timestamps(false);
            let threads = std::thread::available_parallelism()
                .map(|parallelism| parallelism.get().min(4) as i32)
                .unwrap_or(1);
            parameters.set_n_threads(threads);

            install_cancellation_callback(&mut parameters, &cancelled);
            state.full(parameters, audio.samples()).map_err(|error| {
                if cancelled.load(Ordering::Acquire) {
                    TranscriptionError::Cancelled
                } else {
                    TranscriptionError::Failed(format!("local transcription failed: {error}"))
                }
            })?;

            if cancelled.load(Ordering::Acquire) {
                return Err(TranscriptionError::Cancelled);
            }

            let mut text = String::new();
            for segment in state.as_iter() {
                let segment = segment.to_str().map_err(|error| {
                    TranscriptionError::Failed(format!(
                        "local transcription returned invalid text: {error}"
                    ))
                })?;
                text.push_str(segment);
            }
            let text = text.trim().to_owned();
            if text.is_empty() {
                return Err(TranscriptionError::Failed(
                    "local transcription returned no text".into(),
                ));
            }

            Ok(text)
        })();
        if let Some(stage) = recognition {
            let (outcome, fields) = match &result {
                Ok(_) => ("completed", telemetry::Fields::new()),
                Err(TranscriptionError::Cancelled) => ("cancelled", telemetry::Fields::new()),
                Err(TranscriptionError::Failed(_)) => (
                    "failed",
                    telemetry::fields([(
                        "failure_class",
                        serde_json::json!("recognition_failure"),
                    )]),
                ),
            };
            stage.finish(outcome, fields);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    use crate::protocol::CapturedAudio;

    use super::{
        Transcriber, TranscriptionError, TranscriptionKind, WhisperTranscriber,
        cancellation_callback_data, transcription_cancelled_callback,
    };

    #[test]
    fn native_cancellation_callback_reads_the_expected_flag() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let callback_data = cancellation_callback_data(&cancelled);

        // SAFETY: callback_data was just produced from the live Arc above.
        assert!(!unsafe { transcription_cancelled_callback(callback_data) });
        cancelled.store(true, Ordering::Release);
        // SAFETY: the Arc remains alive for this callback invocation.
        assert!(unsafe { transcription_cancelled_callback(callback_data) });
    }

    #[test]
    fn missing_model_is_reported_without_running_recognition() {
        let transcriber = WhisperTranscriber::new(PathBuf::from("/definitely/missing/model.bin"));
        assert!(!transcriber.model_is_available());
        assert!(matches!(
            transcriber.transcribe(
                CapturedAudio::new(vec![0.1; 160]),
                Arc::new(AtomicBool::new(false)),
                TranscriptionKind::Final,
                None,
            ),
            Err(TranscriptionError::Failed(message)) if message.contains("missing")
        ));
    }

    #[test]
    fn cancellation_is_observed_before_model_loading() {
        let transcriber = WhisperTranscriber::new(PathBuf::from("/definitely/missing/model.bin"));
        assert!(matches!(
            transcriber.transcribe(
                CapturedAudio::new(vec![0.1; 160]),
                Arc::new(AtomicBool::new(true)),
                TranscriptionKind::Final,
                None,
            ),
            Err(TranscriptionError::Cancelled)
        ));
    }

    #[test]
    #[ignore = "requires the locally downloaded Whisper base.en model"]
    fn local_model_handles_two_consecutive_transcriptions() {
        let transcriber =
            WhisperTranscriber::new(PathBuf::from("local-data/whisper/ggml-base.en.bin"));
        assert!(
            transcriber.model_is_available(),
            "download the local Whisper model before running this test"
        );

        // Silence is sufficient to exercise the native encoder twice. It may
        // legitimately yield no words, which remains a normal application
        // error; the regression under test is a native encoder failure on the
        // second call.
        for attempt in 1..=2 {
            let result = transcriber.transcribe(
                CapturedAudio::new(vec![0.0; 16_000 * 2]),
                Arc::new(AtomicBool::new(false)),
                TranscriptionKind::Final,
                None,
            );
            let completed_without_native_failure = match &result {
                Ok(_) => true,
                Err(TranscriptionError::Failed(message)) => {
                    message == "local transcription returned no text"
                }
                Err(TranscriptionError::Cancelled) => false,
            };
            assert!(
                completed_without_native_failure,
                "native Whisper call {attempt} failed unexpectedly: {result:?}"
            );
        }
    }
}
