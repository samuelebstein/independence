//! Standalone local dictation mode.
//!
//! This mode deliberately stops at `TranscriptReady`. It shares the
//! application's event/effect shape, task ownership, and terminal lifecycle,
//! but it never creates an Ollama client, calls camera code, or parses a
//! transcript as terminal input.

pub mod audio;
mod controller;
mod transcribe;

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use thiserror::Error;
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    listen,
    protocol::{
        CapturedAudio, DictationEffect, DictationEvent, DictationInput, DictationOutput,
        RecordingId,
    },
    speak,
};

use self::{
    audio::{CpalRecorder, Recorder, RecordingHandle},
    controller::DictationController,
    transcribe::{Transcriber, TranscriptionError, WhisperTranscriber},
};

const EVENT_CHANNEL_CAPACITY: usize = 64;

#[derive(Debug, Error)]
pub enum DictationError {
    #[error("could not start dictation terminal input: {0}")]
    InputAdapter(#[source] io::Error),
}

/// Runs the deliberately separate dictation-only terminal experience.
pub async fn run() -> Result<(), DictationError> {
    run_with_services(
        Arc::new(CpalRecorder::from_environment()),
        Arc::new(WhisperTranscriber::from_environment()),
    )
    .await
}

async fn run_with_services(
    recorder: Arc<dyn Recorder>,
    transcriber: Arc<dyn Transcriber>,
) -> Result<(), DictationError> {
    let microphone = recorder.selected_microphone();
    let (event_sender, mut event_receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let mut input = listen::spawn_dictation_key_reader(event_sender.clone())
        .map_err(DictationError::InputAdapter)?;

    let microphone_label = microphone.as_ref().map(|microphone| microphone.label());
    let microphone_for_display = match &microphone_label {
        Ok(label) => Ok(label.as_str()),
        Err(message) => Err((*message).as_str()),
    };
    speak::dictation_welcome(
        microphone_for_display,
        transcriber.model_path(),
        transcriber.model_is_available(),
    );
    speak::emit_dictation(DictationOutput::Ready);

    let mut controller = DictationController::new();
    let mut runtime = DictationRuntime::new(recorder, transcriber, event_sender);

    loop {
        let event = tokio::select! {
            _ = tokio::signal::ctrl_c() => DictationEvent::Input(DictationInput::Interrupt),
            event = event_receiver.recv() => event.unwrap_or(DictationEvent::Input(DictationInput::EndOfInput)),
        };

        runtime.retire_completed_worker(&event).await;
        let effects = controller.handle(event);
        let mut should_exit = false;
        for effect in effects {
            if !runtime.apply(effect).await {
                should_exit = true;
                break;
            }
        }
        if should_exit {
            break;
        }
    }

    // Raw mode belongs to this input adapter alone. Its poll loop has a short
    // timeout, so joining restores terminal settings without a blocked line
    // reader competing for the same input.
    input.shutdown();
    runtime.shutdown().await;
    Ok(())
}

struct DictationRuntime {
    recorder: Arc<dyn Recorder>,
    transcriber: Arc<dyn Transcriber>,
    event_sender: mpsc::Sender<DictationEvent>,
    active_recording: Option<RunningRecording>,
    active_transcription: Option<RunningTranscription>,
}

struct RunningRecording {
    id: RecordingId,
    handle: Box<dyn RecordingHandle>,
}

struct RunningTranscription {
    id: RecordingId,
    cancelled: Arc<AtomicBool>,
    completion: JoinHandle<()>,
}

impl DictationRuntime {
    fn new(
        recorder: Arc<dyn Recorder>,
        transcriber: Arc<dyn Transcriber>,
        event_sender: mpsc::Sender<DictationEvent>,
    ) -> Self {
        Self {
            recorder,
            transcriber,
            event_sender,
            active_recording: None,
            active_transcription: None,
        }
    }

    async fn apply(&mut self, effect: DictationEffect) -> bool {
        match effect {
            DictationEffect::StartRecording { recording_id } => {
                self.start_recording(recording_id).await;
            }
            DictationEffect::FinishRecording { recording_id } => {
                if let Some(recording) = self.active_recording.as_ref()
                    && recording.id == recording_id
                {
                    recording.handle.finish();
                }
            }
            DictationEffect::CancelRecording { recording_id } => {
                if let Some(recording) = self.active_recording.as_ref()
                    && recording.id == recording_id
                {
                    recording.handle.cancel();
                }
            }
            DictationEffect::StartTranscription {
                recording_id,
                audio,
            } => self.start_transcription(recording_id, audio),
            DictationEffect::CancelTranscription { recording_id } => {
                if let Some(transcription) = self.active_transcription.as_ref()
                    && transcription.id == recording_id
                {
                    // This is cooperative: it requests whisper.cpp's abort
                    // callback. It intentionally does not abort a wrapper and
                    // pretend a native computation vanished instantly.
                    transcription.cancelled.store(true, Ordering::Release);
                }
            }
            DictationEffect::Display(output) => speak::emit_dictation(output),
            DictationEffect::Exit => return false,
        }

        true
    }

    async fn start_recording(&mut self, recording_id: RecordingId) {
        if self.active_recording.is_some() || self.active_transcription.is_some() {
            let _ = self
                .event_sender
                .send(DictationEvent::RecordingFailed {
                    recording_id,
                    message: "previous dictation work is still cleaning up; wait for READY".into(),
                })
                .await;
            return;
        }

        // Avoid recording speech that cannot be transcribed because the user
        // has not supplied the explicitly local model file.
        if !self.transcriber.model_is_available() {
            let _ = self
                .event_sender
                .send(DictationEvent::RecordingFailed {
                    recording_id,
                    message: format!(
                        "Whisper model file is missing: {}; set INDEPENDENCE_WHISPER_MODEL before recording",
                        self.transcriber.model_path().display()
                    ),
                })
                .await;
            return;
        }

        match self.recorder.start(recording_id, self.event_sender.clone()) {
            Ok(handle) => {
                self.active_recording = Some(RunningRecording {
                    id: recording_id,
                    handle,
                });
            }
            Err(message) => {
                let _ = self
                    .event_sender
                    .send(DictationEvent::RecordingFailed {
                        recording_id,
                        message,
                    })
                    .await;
            }
        }
    }

    fn start_transcription(&mut self, recording_id: RecordingId, audio: CapturedAudio) {
        if self.active_transcription.is_some() {
            // This should be unreachable under the controller's state machine.
            // Dropping `audio` preserves the one-active-recognizer invariant.
            return;
        }

        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let transcriber = self.transcriber.clone();
        let event_sender = self.event_sender.clone();
        let completion = tokio::spawn(async move {
            let task_cancelled = worker_cancelled.clone();
            let result =
                tokio::task::spawn_blocking(move || transcriber.transcribe(audio, task_cancelled))
                    .await;

            let event = if worker_cancelled.load(Ordering::Acquire) {
                DictationEvent::TranscriptionCancelled { recording_id }
            } else {
                match result {
                    Ok(Ok(text)) => DictationEvent::TranscriptReady { recording_id, text },
                    Ok(Err(TranscriptionError::Cancelled)) => {
                        DictationEvent::TranscriptionCancelled { recording_id }
                    }
                    Ok(Err(TranscriptionError::Failed(message))) => {
                        DictationEvent::TranscriptionFailed {
                            recording_id,
                            message,
                        }
                    }
                    Err(error) => DictationEvent::TranscriptionFailed {
                        recording_id,
                        message: format!("local transcription worker ended unexpectedly: {error}"),
                    },
                }
            };
            let _ = event_sender.send(event).await;
        });
        self.active_transcription = Some(RunningTranscription {
            id: recording_id,
            cancelled,
            completion,
        });
    }

    async fn retire_completed_worker(&mut self, event: &DictationEvent) {
        match event {
            DictationEvent::RecordingFinished { recording_id, .. }
            | DictationEvent::RecordingCancelled { recording_id }
            | DictationEvent::RecordingFailed { recording_id, .. } => {
                self.retire_recording(*recording_id).await;
            }
            DictationEvent::TranscriptReady { recording_id, .. }
            | DictationEvent::TranscriptionCancelled { recording_id }
            | DictationEvent::TranscriptionFailed { recording_id, .. } => {
                self.retire_transcription(*recording_id).await;
            }
            DictationEvent::Input(_)
            | DictationEvent::RecordingStarted { .. }
            | DictationEvent::RecordingElapsed { .. }
            | DictationEvent::RecordingLimitReached { .. } => {}
        }
    }

    async fn retire_recording(&mut self, recording_id: RecordingId) {
        let Some(recording) = self.active_recording.as_ref() else {
            return;
        };
        if recording.id != recording_id {
            return;
        }
        let recording = self
            .active_recording
            .take()
            .expect("active recording was checked above");
        recording.handle.wait().await;
    }

    async fn retire_transcription(&mut self, recording_id: RecordingId) {
        let Some(transcription) = self.active_transcription.as_ref() else {
            return;
        };
        if transcription.id != recording_id {
            return;
        }
        let transcription = self
            .active_transcription
            .take()
            .expect("active transcription was checked above");
        let _ = transcription.completion.await;
    }

    async fn shutdown(&mut self) {
        if let Some(recording) = self.active_recording.take() {
            recording.handle.cancel();
            recording.handle.wait().await;
        }
        if let Some(transcription) = self.active_transcription.take() {
            transcription.cancelled.store(true, Ordering::Release);
            // whisper-rs receives this via FullParams' supported abort callback.
            // Model loading/native work may only observe it at their next safe
            // callback point, so this is cleanup coordination, not a claim of
            // immediate process interruption.
            let _ = transcription.completion.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        path::{Path, PathBuf},
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use tokio::sync::mpsc;

    use crate::protocol::{
        CapturedAudio, DictationEffect, DictationEvent, DictationOutput, RecordingId,
    };

    use super::{
        DictationRuntime,
        audio::{MicrophoneDescription, Recorder, RecordingHandle},
        transcribe::{Transcriber, TranscriptionError},
    };

    #[derive(Default)]
    struct FakeRecorder {
        starts: Arc<AtomicUsize>,
        finishes: Arc<AtomicUsize>,
        cancels: Arc<AtomicUsize>,
    }

    impl Recorder for FakeRecorder {
        fn selected_microphone(&self) -> Result<MicrophoneDescription, String> {
            Ok(MicrophoneDescription::new(
                "synthetic microphone".into(),
                48_000,
                2,
            ))
        }

        fn start(
            &self,
            _recording_id: RecordingId,
            _event_sender: mpsc::Sender<DictationEvent>,
        ) -> Result<Box<dyn RecordingHandle>, String> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeRecordingHandle {
                finishes: self.finishes.clone(),
                cancels: self.cancels.clone(),
            }))
        }
    }

    struct FakeRecordingHandle {
        finishes: Arc<AtomicUsize>,
        cancels: Arc<AtomicUsize>,
    }

    impl RecordingHandle for FakeRecordingHandle {
        fn finish(&self) {
            self.finishes.fetch_add(1, Ordering::SeqCst);
        }

        fn cancel(&self) {
            self.cancels.fetch_add(1, Ordering::SeqCst);
        }

        fn wait(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            Box::pin(async {})
        }
    }

    struct FailingRecorder;

    impl Recorder for FailingRecorder {
        fn selected_microphone(&self) -> Result<MicrophoneDescription, String> {
            Err("synthetic microphone unavailable".into())
        }

        fn start(
            &self,
            _recording_id: RecordingId,
            _event_sender: mpsc::Sender<DictationEvent>,
        ) -> Result<Box<dyn RecordingHandle>, String> {
            Err("synthetic microphone disconnected".into())
        }
    }

    struct FakeTranscriber {
        path: PathBuf,
        available: bool,
        calls: AtomicUsize,
        text: String,
    }

    impl FakeTranscriber {
        fn available() -> Self {
            Self {
                path: PathBuf::from("synthetic-model.bin"),
                available: true,
                calls: AtomicUsize::new(0),
                text: "/quit".into(),
            }
        }
    }

    impl Transcriber for FakeTranscriber {
        fn model_path(&self) -> &Path {
            &self.path
        }

        fn model_is_available(&self) -> bool {
            self.available
        }

        fn transcribe(
            &self,
            _audio: CapturedAudio,
            cancelled: Arc<AtomicBool>,
        ) -> Result<String, TranscriptionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if cancelled.load(Ordering::Acquire) {
                Err(TranscriptionError::Cancelled)
            } else {
                Ok(self.text.clone())
            }
        }
    }

    fn id(value: u64) -> RecordingId {
        RecordingId::new(value)
    }

    fn audio() -> CapturedAudio {
        CapturedAudio::new(vec![0.1; 160])
    }

    #[tokio::test]
    async fn missing_model_prevents_microphone_activation() {
        let recorder = Arc::new(FakeRecorder::default());
        let transcriber = Arc::new(FakeTranscriber {
            path: PathBuf::from("missing.bin"),
            available: false,
            calls: AtomicUsize::new(0),
            text: String::new(),
        });
        let (sender, mut receiver) = mpsc::channel(4);
        let mut runtime = DictationRuntime::new(recorder.clone(), transcriber, sender);

        runtime
            .apply(DictationEffect::StartRecording {
                recording_id: id(1),
            })
            .await;

        assert_eq!(recorder.starts.load(Ordering::SeqCst), 0);
        assert!(matches!(
            receiver.recv().await,
            Some(DictationEvent::RecordingFailed { message, .. }) if message.contains("missing")
        ));
    }

    #[tokio::test]
    async fn transcription_result_is_a_typed_event_not_a_command() {
        let recorder = Arc::new(FakeRecorder::default());
        let transcriber = Arc::new(FakeTranscriber::available());
        let (sender, mut receiver) = mpsc::channel(4);
        let mut runtime = DictationRuntime::new(recorder, transcriber.clone(), sender);

        runtime
            .apply(DictationEffect::StartTranscription {
                recording_id: id(1),
                audio: audio(),
            })
            .await;
        let event = receiver.recv().await.expect("transcription event");
        runtime.retire_completed_worker(&event).await;

        assert!(matches!(
            event,
            DictationEvent::TranscriptReady { text, .. } if text == "/quit"
        ));
        assert_eq!(transcriber.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn shutdown_cancels_an_owned_recording_handle() {
        let recorder = Arc::new(FakeRecorder::default());
        let transcriber = Arc::new(FakeTranscriber::available());
        let (sender, _receiver) = mpsc::channel(4);
        let mut runtime = DictationRuntime::new(recorder.clone(), transcriber, sender);

        runtime
            .apply(DictationEffect::StartRecording {
                recording_id: id(1),
            })
            .await;
        runtime.shutdown().await;

        assert!(runtime.active_recording.is_none());
        assert_eq!(recorder.starts.load(Ordering::SeqCst), 1);
        assert_eq!(recorder.cancels.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn finish_and_completion_release_a_recording_handle() {
        let recorder = Arc::new(FakeRecorder::default());
        let transcriber = Arc::new(FakeTranscriber::available());
        let (sender, _receiver) = mpsc::channel(4);
        let mut runtime = DictationRuntime::new(recorder.clone(), transcriber, sender);

        runtime
            .apply(DictationEffect::StartRecording {
                recording_id: id(1),
            })
            .await;
        runtime
            .apply(DictationEffect::FinishRecording {
                recording_id: id(1),
            })
            .await;
        assert_eq!(recorder.finishes.load(Ordering::SeqCst), 1);

        runtime
            .retire_completed_worker(&DictationEvent::RecordingCancelled {
                recording_id: id(1),
            })
            .await;
        assert!(runtime.active_recording.is_none());
    }

    #[tokio::test]
    async fn capture_failure_does_not_invoke_local_recognition() {
        let recorder = Arc::new(FailingRecorder);
        let transcriber = Arc::new(FakeTranscriber::available());
        let (sender, mut receiver) = mpsc::channel(4);
        let mut runtime = DictationRuntime::new(recorder, transcriber.clone(), sender);

        runtime
            .apply(DictationEffect::StartRecording {
                recording_id: id(1),
            })
            .await;

        assert!(matches!(
            receiver.recv().await,
            Some(DictationEvent::RecordingFailed { message, .. }) if message.contains("disconnected")
        ));
        assert_eq!(transcriber.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn transcript_display_output_has_no_command_variant() {
        let output = DictationOutput::Transcript("/camera 0 | take a photo".into());
        assert!(matches!(output, DictationOutput::Transcript(_)));
    }
}
