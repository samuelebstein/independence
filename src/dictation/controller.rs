//! State ownership and output authorization for standalone dictation.
//!
//! This controller intentionally has no dependency on `think`, `see`, or the
//! main session controller. A transcript is displayed as data, never parsed as
//! another terminal command.

use std::time::Duration;

use crate::protocol::{
    CapturedAudio, DictationEffect, DictationEvent, DictationInput, DictationOutput, RecordingId,
};

/// An explicit privacy and memory bound for one recording.
pub const MAX_RECORDING_DURATION: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct DictationController {
    phase: Phase,
    next_recording_id: u64,
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Ready,
    Starting { recording_id: RecordingId },
    Recording { recording_id: RecordingId },
    Finishing { recording_id: RecordingId },
    CancellingCapture { recording_id: RecordingId },
    Transcribing { recording_id: RecordingId },
    CancellingTranscription { recording_id: RecordingId },
    Exiting,
}

impl DictationController {
    pub fn new() -> Self {
        Self {
            phase: Phase::Ready,
            next_recording_id: 1,
        }
    }

    /// Reacts to one fact without performing I/O itself.
    pub fn handle(&mut self, event: DictationEvent) -> Vec<DictationEffect> {
        match event {
            DictationEvent::Input(input) => self.handle_input(input),
            DictationEvent::RecordingStarted { recording_id } => {
                self.recording_started(recording_id)
            }
            DictationEvent::RecordingElapsed {
                recording_id,
                elapsed,
            } => self.recording_elapsed(recording_id, elapsed),
            DictationEvent::RecordingLimitReached { recording_id } => {
                self.recording_limit_reached(recording_id)
            }
            DictationEvent::RecordingFinished {
                recording_id,
                audio,
            } => self.recording_finished(recording_id, audio),
            DictationEvent::RecordingCancelled { recording_id } => {
                self.recording_cancelled(recording_id)
            }
            DictationEvent::RecordingFailed {
                recording_id,
                message,
            } => self.recording_failed(recording_id, message),
            DictationEvent::TranscriptReady { recording_id, text } => {
                self.transcript_ready(recording_id, text)
            }
            DictationEvent::TranscriptionCancelled { recording_id } => {
                self.transcription_cancelled(recording_id)
            }
            DictationEvent::TranscriptionFailed {
                recording_id,
                message,
            } => self.transcription_failed(recording_id, message),
        }
    }

    fn handle_input(&mut self, input: DictationInput) -> Vec<DictationEffect> {
        match input {
            DictationInput::Enter => match self.phase {
                Phase::Ready => self.start_recording(),
                // The interface deliberately ignores Enter outside READY. In
                // particular it cannot queue a second recording while a
                // device or recognizer still has cleanup work to do.
                Phase::Starting { .. }
                | Phase::Finishing { .. }
                | Phase::CancellingCapture { .. }
                | Phase::Transcribing { .. }
                | Phase::CancellingTranscription { .. }
                | Phase::Exiting => Vec::new(),
                Phase::Recording { recording_id } => {
                    self.phase = Phase::Finishing { recording_id };
                    vec![DictationEffect::FinishRecording { recording_id }]
                }
            },
            DictationInput::Escape => match self.phase {
                Phase::Starting { recording_id }
                | Phase::Recording { recording_id }
                | Phase::Finishing { recording_id } => {
                    self.phase = Phase::CancellingCapture { recording_id };
                    vec![
                        DictationEffect::CancelRecording { recording_id },
                        DictationEffect::Display(DictationOutput::Discarding),
                    ]
                }
                Phase::Transcribing { recording_id } => {
                    self.phase = Phase::CancellingTranscription { recording_id };
                    vec![
                        DictationEffect::CancelTranscription { recording_id },
                        DictationEffect::Display(DictationOutput::Diagnostic(
                            "cancelling local transcription…".into(),
                        )),
                    ]
                }
                Phase::Ready
                | Phase::CancellingCapture { .. }
                | Phase::CancellingTranscription { .. }
                | Phase::Exiting => Vec::new(),
            },
            DictationInput::Interrupt | DictationInput::EndOfInput => self.exit(),
        }
    }

    fn start_recording(&mut self) -> Vec<DictationEffect> {
        let recording_id = RecordingId::new(self.next_recording_id);
        self.next_recording_id = self
            .next_recording_id
            .checked_add(1)
            .expect("dictation recording identifiers exhausted");
        self.phase = Phase::Starting { recording_id };
        vec![DictationEffect::StartRecording { recording_id }]
    }

    fn recording_started(&mut self, recording_id: RecordingId) -> Vec<DictationEffect> {
        if !matches!(self.phase, Phase::Starting { recording_id: active } if active == recording_id)
        {
            return Vec::new();
        }

        self.phase = Phase::Recording { recording_id };
        vec![DictationEffect::Display(DictationOutput::Recording {
            elapsed: Duration::ZERO,
        })]
    }

    fn recording_elapsed(
        &mut self,
        recording_id: RecordingId,
        elapsed: Duration,
    ) -> Vec<DictationEffect> {
        if !matches!(self.phase, Phase::Recording { recording_id: active } if active == recording_id)
        {
            return Vec::new();
        }

        vec![DictationEffect::Display(DictationOutput::Recording {
            elapsed,
        })]
    }

    fn recording_limit_reached(&mut self, recording_id: RecordingId) -> Vec<DictationEffect> {
        if !matches!(self.phase, Phase::Recording { recording_id: active } if active == recording_id)
        {
            return Vec::new();
        }

        self.phase = Phase::Finishing { recording_id };
        vec![
            DictationEffect::Display(DictationOutput::Diagnostic(format!(
                "{seconds}-second recording limit reached; finishing capture",
                seconds = MAX_RECORDING_DURATION.as_secs()
            ))),
            DictationEffect::FinishRecording { recording_id },
        ]
    }

    fn recording_finished(
        &mut self,
        recording_id: RecordingId,
        audio: CapturedAudio,
    ) -> Vec<DictationEffect> {
        match self.phase {
            Phase::Recording {
                recording_id: active,
            }
            | Phase::Finishing {
                recording_id: active,
            } if active == recording_id => {
                self.phase = Phase::Transcribing { recording_id };
                vec![
                    DictationEffect::Display(DictationOutput::Transcribing),
                    DictationEffect::StartTranscription {
                        recording_id,
                        audio,
                    },
                ]
            }
            Phase::CancellingCapture {
                recording_id: active,
            } if active == recording_id => {
                // The runtime has already released the microphone before it
                // sends this event. The late audio is deliberately dropped.
                self.phase = Phase::Ready;
                ready_effects()
            }
            _ => Vec::new(),
        }
    }

    fn recording_cancelled(&mut self, recording_id: RecordingId) -> Vec<DictationEffect> {
        match self.phase {
            Phase::CancellingCapture {
                recording_id: active,
            } if active == recording_id => {
                self.phase = Phase::Ready;
                ready_effects()
            }
            _ => Vec::new(),
        }
    }

    fn recording_failed(
        &mut self,
        recording_id: RecordingId,
        message: String,
    ) -> Vec<DictationEffect> {
        match self.phase {
            Phase::Starting {
                recording_id: active,
            }
            | Phase::Recording {
                recording_id: active,
            }
            | Phase::Finishing {
                recording_id: active,
            } if active == recording_id => {
                self.phase = Phase::Ready;
                let mut effects = vec![DictationEffect::Display(DictationOutput::Diagnostic(
                    message,
                ))];
                effects.extend(ready_effects());
                effects
            }
            Phase::CancellingCapture {
                recording_id: active,
            } if active == recording_id => {
                self.phase = Phase::Ready;
                ready_effects()
            }
            _ => Vec::new(),
        }
    }

    fn transcript_ready(
        &mut self,
        recording_id: RecordingId,
        text: String,
    ) -> Vec<DictationEffect> {
        match self.phase {
            Phase::Transcribing {
                recording_id: active,
            } if active == recording_id => {
                self.phase = Phase::Ready;
                if text.trim().is_empty() {
                    let mut effects = vec![DictationEffect::Display(DictationOutput::Diagnostic(
                        "local transcription returned no text".into(),
                    ))];
                    effects.extend(ready_effects());
                    return effects;
                }

                let mut effects = vec![DictationEffect::Display(DictationOutput::Transcript(text))];
                effects.extend(ready_effects());
                effects
            }
            // A cancelled transcription may still finish native work. Its
            // text is never displayed or treated as a command.
            Phase::CancellingTranscription {
                recording_id: active,
            } if active == recording_id => {
                self.phase = Phase::Ready;
                ready_effects()
            }
            _ => Vec::new(),
        }
    }

    fn transcription_cancelled(&mut self, recording_id: RecordingId) -> Vec<DictationEffect> {
        match self.phase {
            Phase::CancellingTranscription {
                recording_id: active,
            } if active == recording_id => {
                self.phase = Phase::Ready;
                ready_effects()
            }
            _ => Vec::new(),
        }
    }

    fn transcription_failed(
        &mut self,
        recording_id: RecordingId,
        message: String,
    ) -> Vec<DictationEffect> {
        match self.phase {
            Phase::Transcribing {
                recording_id: active,
            } if active == recording_id => {
                self.phase = Phase::Ready;
                let mut effects = vec![DictationEffect::Display(DictationOutput::Diagnostic(
                    message,
                ))];
                effects.extend(ready_effects());
                effects
            }
            Phase::CancellingTranscription {
                recording_id: active,
            } if active == recording_id => {
                self.phase = Phase::Ready;
                ready_effects()
            }
            _ => Vec::new(),
        }
    }

    fn exit(&mut self) -> Vec<DictationEffect> {
        let cancellation = match self.phase {
            Phase::Starting { recording_id }
            | Phase::Recording { recording_id }
            | Phase::Finishing { recording_id }
            | Phase::CancellingCapture { recording_id } => {
                Some(DictationEffect::CancelRecording { recording_id })
            }
            Phase::Transcribing { recording_id }
            | Phase::CancellingTranscription { recording_id } => {
                Some(DictationEffect::CancelTranscription { recording_id })
            }
            Phase::Ready | Phase::Exiting => None,
        };

        self.phase = Phase::Exiting;
        cancellation
            .into_iter()
            .chain([DictationEffect::Exit])
            .collect()
    }
}

fn ready_effects() -> Vec<DictationEffect> {
    vec![DictationEffect::Display(DictationOutput::Ready)]
}

#[cfg(test)]
mod tests {
    use super::{DictationController, MAX_RECORDING_DURATION, Phase};
    use crate::protocol::{
        CapturedAudio, DictationEffect, DictationEvent, DictationInput, DictationOutput,
        RecordingId,
    };

    fn start(controller: &mut DictationController) -> RecordingId {
        let effects = controller.handle(DictationEvent::Input(DictationInput::Enter));
        match effects.as_slice() {
            [DictationEffect::StartRecording { recording_id }] => *recording_id,
            _ => panic!("expected a recording start effect"),
        }
    }

    fn audio() -> CapturedAudio {
        CapturedAudio::new(vec![0.1; 160])
    }

    #[test]
    fn enter_starts_then_finishes_one_recording() {
        let mut controller = DictationController::new();
        let id = start(&mut controller);
        assert!(matches!(
            controller
                .handle(DictationEvent::RecordingStarted { recording_id: id })
                .as_slice(),
            [DictationEffect::Display(DictationOutput::Recording { .. })]
        ));

        assert!(matches!(
            controller.handle(DictationEvent::Input(DictationInput::Enter)).as_slice(),
            [DictationEffect::FinishRecording { recording_id }] if *recording_id == id
        ));

        let effects = controller.handle(DictationEvent::RecordingFinished {
            recording_id: id,
            audio: audio(),
        });
        assert!(matches!(
            effects.as_slice(),
            [
                DictationEffect::Display(DictationOutput::Transcribing),
                DictationEffect::StartTranscription { recording_id, .. }
            ] if *recording_id == id
        ));
    }

    #[test]
    fn recording_limit_finishes_the_current_recording() {
        let mut controller = DictationController::new();
        let id = start(&mut controller);
        controller.handle(DictationEvent::RecordingStarted { recording_id: id });

        let effects = controller.handle(DictationEvent::RecordingLimitReached { recording_id: id });
        assert!(matches!(
            effects.as_slice(),
            [
                DictationEffect::Display(DictationOutput::Diagnostic(message)),
                DictationEffect::FinishRecording { recording_id }
            ] if message.contains(&MAX_RECORDING_DURATION.as_secs().to_string()) && *recording_id == id
        ));
    }

    #[test]
    fn repeated_enter_does_not_queue_a_second_recording() {
        let mut controller = DictationController::new();
        let id = start(&mut controller);
        controller.handle(DictationEvent::RecordingStarted { recording_id: id });
        controller.handle(DictationEvent::Input(DictationInput::Enter));

        assert!(
            controller
                .handle(DictationEvent::Input(DictationInput::Enter))
                .is_empty()
        );
    }

    #[test]
    fn cancel_discards_late_capture_and_returns_to_ready() {
        let mut controller = DictationController::new();
        let id = start(&mut controller);
        controller.handle(DictationEvent::RecordingStarted { recording_id: id });

        assert!(matches!(
            controller.handle(DictationEvent::Input(DictationInput::Escape)).as_slice(),
            [
                DictationEffect::CancelRecording { recording_id },
                DictationEffect::Display(DictationOutput::Discarding)
            ] if *recording_id == id
        ));

        assert!(matches!(
            controller
                .handle(DictationEvent::RecordingFinished {
                    recording_id: id,
                    audio: audio(),
                })
                .as_slice(),
            [DictationEffect::Display(DictationOutput::Ready)]
        ));
        assert!(matches!(controller.phase, Phase::Ready));
    }

    #[test]
    fn cancelled_transcription_cannot_display_a_late_transcript() {
        let mut controller = DictationController::new();
        let id = start(&mut controller);
        controller.handle(DictationEvent::RecordingStarted { recording_id: id });
        controller.handle(DictationEvent::Input(DictationInput::Enter));
        controller.handle(DictationEvent::RecordingFinished {
            recording_id: id,
            audio: audio(),
        });
        controller.handle(DictationEvent::Input(DictationInput::Escape));

        assert!(matches!(
            controller
                .handle(DictationEvent::TranscriptReady {
                    recording_id: id,
                    text: "too late".into(),
                })
                .as_slice(),
            [DictationEffect::Display(DictationOutput::Ready)]
        ));
    }

    #[test]
    fn transcript_is_inert_data_even_when_it_looks_like_a_command() {
        let mut controller = DictationController::new();
        let id = start(&mut controller);
        controller.handle(DictationEvent::RecordingStarted { recording_id: id });
        controller.handle(DictationEvent::Input(DictationInput::Enter));
        controller.handle(DictationEvent::RecordingFinished {
            recording_id: id,
            audio: audio(),
        });

        let text = "/camera 0 | take a photo";
        let effects = controller.handle(DictationEvent::TranscriptReady {
            recording_id: id,
            text: text.into(),
        });
        assert!(matches!(
            effects.as_slice(),
            [
                DictationEffect::Display(DictationOutput::Transcript(transcript)),
                DictationEffect::Display(DictationOutput::Ready)
            ] if transcript == text
        ));
    }

    #[test]
    fn stale_completion_cannot_replace_a_new_recording() {
        let mut controller = DictationController::new();
        let old = start(&mut controller);
        controller.handle(DictationEvent::RecordingStarted { recording_id: old });
        controller.handle(DictationEvent::Input(DictationInput::Escape));
        controller.handle(DictationEvent::RecordingCancelled { recording_id: old });
        let current = start(&mut controller);
        assert_ne!(old, current);

        assert!(
            controller
                .handle(DictationEvent::RecordingFailed {
                    recording_id: old,
                    message: "old worker failed".into(),
                })
                .is_empty()
        );
    }

    #[test]
    fn capture_error_never_starts_transcription_and_allows_a_retry() {
        let mut controller = DictationController::new();
        let failed = start(&mut controller);

        let effects = controller.handle(DictationEvent::RecordingFailed {
            recording_id: failed,
            message: "synthetic microphone disconnected".into(),
        });
        assert!(matches!(
            effects.as_slice(),
            [
                DictationEffect::Display(DictationOutput::Diagnostic(message)),
                DictationEffect::Display(DictationOutput::Ready)
            ] if message.contains("disconnected")
        ));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, DictationEffect::StartTranscription { .. }))
        );

        let retry = start(&mut controller);
        assert_ne!(failed, retry);
    }

    #[test]
    fn two_recording_cycles_use_distinct_ids() {
        let mut controller = DictationController::new();
        let first = start(&mut controller);
        controller.handle(DictationEvent::RecordingStarted {
            recording_id: first,
        });
        controller.handle(DictationEvent::Input(DictationInput::Escape));
        controller.handle(DictationEvent::RecordingCancelled {
            recording_id: first,
        });

        let second = start(&mut controller);
        controller.handle(DictationEvent::RecordingStarted {
            recording_id: second,
        });
        assert_ne!(first, second);
        assert!(
            matches!(controller.phase, Phase::Recording { recording_id } if recording_id == second)
        );
    }

    #[test]
    fn interrupt_requests_owned_recording_cleanup_before_exit() {
        let mut controller = DictationController::new();
        let id = start(&mut controller);
        controller.handle(DictationEvent::RecordingStarted { recording_id: id });

        assert!(matches!(
            controller
                .handle(DictationEvent::Input(DictationInput::Interrupt))
                .as_slice(),
            [
                DictationEffect::CancelRecording { recording_id },
                DictationEffect::Exit
            ] if *recording_id == id
        ));
    }
}
