//! Local whisper.cpp transcription behind a deliberately small test seam.

use std::{
    env,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::protocol::CapturedAudio;

/// The recognizer boundary has no notion of questions, commands, Ollama, or
/// cameras. It receives prepared audio and returns only text or an error.
pub trait Transcriber: Send + Sync {
    fn model_path(&self) -> &Path;
    fn model_is_available(&self) -> bool;
    fn transcribe(
        &self,
        audio: CapturedAudio,
        cancelled: Arc<AtomicBool>,
    ) -> Result<String, TranscriptionError>;
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

    fn context(&self, cancelled: &AtomicBool) -> Result<Arc<WhisperContext>, TranscriptionError> {
        if cancelled.load(Ordering::Acquire) {
            return Err(TranscriptionError::Cancelled);
        }

        let mut slot = self.context.lock().map_err(|_| {
            TranscriptionError::Failed("local recognizer model state lock was poisoned".into())
        })?;
        if let Some(context) = slot.as_ref() {
            return Ok(context.clone());
        }

        if !self.model_path.is_file() {
            return Err(TranscriptionError::Failed(format!(
                "Whisper model file is missing: {}; set INDEPENDENCE_WHISPER_MODEL to a local ggml base.en model",
                self.model_path.display()
            )));
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(TranscriptionError::Cancelled);
        }

        let context =
            WhisperContext::new_with_params(&self.model_path, WhisperContextParameters::default())
                .map_err(|error| {
                    TranscriptionError::Failed(format!(
                        "could not load local Whisper model {}: {error}",
                        self.model_path.display()
                    ))
                })?;
        if cancelled.load(Ordering::Acquire) {
            return Err(TranscriptionError::Cancelled);
        }

        let context = Arc::new(context);
        *slot = Some(context.clone());
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
    ) -> Result<String, TranscriptionError> {
        let context = self.context(&cancelled)?;
        if cancelled.load(Ordering::Acquire) {
            return Err(TranscriptionError::Cancelled);
        }

        let mut state = context.create_state().map_err(|error| {
            TranscriptionError::Failed(format!("could not create local recognition state: {error}"))
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

        let abort = cancelled.clone();
        parameters.set_abort_callback_safe(Some(move || abort.load(Ordering::Acquire)));
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
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{Arc, atomic::AtomicBool},
    };

    use crate::protocol::CapturedAudio;

    use super::{Transcriber, TranscriptionError, WhisperTranscriber};

    #[test]
    fn missing_model_is_reported_without_running_recognition() {
        let transcriber = WhisperTranscriber::new(PathBuf::from("/definitely/missing/model.bin"));
        assert!(!transcriber.model_is_available());
        assert!(matches!(
            transcriber.transcribe(
                CapturedAudio::new(vec![0.1; 160]),
                Arc::new(AtomicBool::new(false)),
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
            ),
            Err(TranscriptionError::Cancelled)
        ));
    }
}
