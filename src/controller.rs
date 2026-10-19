//! Session state and output authorization.
//!
//! The controller is deliberately synchronous. It turns facts (`Event`) into
//! requested side effects (`Effect`) and owns the only accepted scene/history.
//! Worker tasks receive snapshots and can therefore never mutate this state.

use crate::{
    protocol::{
        CameraPreparation, CameraRequestId, Effect, Event, Input, Output, TurnFinished, TurnId,
        TurnRequest, TurnResult, VisualRequest,
    },
    see::Photo,
    think::ConversationTurn,
};

/// A small, explicit context cap. It is a turn cap, not token accounting.
pub const MAX_HISTORY_TURNS: usize = 6;

#[derive(Debug, Default)]
pub struct Controller {
    session: Session,
    active: Option<ActiveTurn>,
    pending_camera: Option<PendingCamera>,
    next_turn_id: u64,
    next_camera_request_id: u64,
}

#[derive(Debug, Default)]
struct Session {
    scene: Option<Photo>,
    history: Vec<ConversationTurn>,
}

#[derive(Debug)]
struct ActiveTurn {
    id: TurnId,
    question: String,
    replaces_scene: bool,
}

#[derive(Debug)]
struct PendingCamera {
    id: CameraRequestId,
    question: String,
}

impl Controller {
    pub fn new() -> Self {
        Self {
            next_turn_id: 1,
            next_camera_request_id: 1,
            ..Self::default()
        }
    }

    /// Reacts to one fact without performing I/O itself.
    pub fn handle(&mut self, event: Event) -> Vec<Effect> {
        match event {
            Event::Input(input) => self.handle_input(input),
            Event::CamerasListed(result) => self.handle_cameras_listed(result),
            Event::CameraPrepared { id, result } => self.handle_camera_prepared(id, result),
            Event::CameraCaptureStarted { id, camera } => {
                self.handle_camera_capture_started(id, camera)
            }
            Event::CameraCaptureReady { id, source } => {
                self.handle_camera_capture_ready(id, source)
            }
            Event::TurnFinished(finished) => self.handle_turn_finished(finished),
        }
    }

    pub fn accepted_scene(&self) -> Option<&Photo> {
        self.session.scene.as_ref()
    }

    pub fn accepted_history(&self) -> &[ConversationTurn] {
        &self.session.history
    }

    pub fn has_active_turn(&self) -> bool {
        self.active.is_some()
    }

    fn handle_input(&mut self, input: Input) -> Vec<Effect> {
        match input {
            Input::Ask(question) => self.start_turn(
                question,
                VisualRequest::UseAccepted(self.session.scene.clone()),
                false,
            ),
            Input::Look { path, question } => {
                self.start_turn(question, VisualRequest::ProposeNew(path), true)
            }
            Input::Cameras => vec![Effect::ListCameras],
            Input::Camera {
                requested,
                question,
            } => self.prepare_camera(requested, question),
            Input::Stop => {
                self.pending_camera = None;
                self.cancel_active()
            }
            Input::Reset => {
                self.pending_camera = None;
                let mut effects = self.cancel_active();
                self.session = Session::default();
                effects.push(Effect::Display(Output::Diagnostic(
                    "session cleared; original files and terminal scrollback are unchanged".into(),
                )));
                effects
            }
            Input::Help => vec![Effect::Display(Output::Help)],
            Input::Quit | Input::EndOfInput | Input::Interrupt => {
                self.pending_camera = None;
                let mut effects = self.cancel_active();
                effects.push(Effect::Exit);
                effects
            }
            Input::Blank => Vec::new(),
            Input::Malformed(message) => vec![Effect::Display(Output::Diagnostic(format!(
                "invalid command: {message}"
            )))],
        }
    }

    fn start_turn(
        &mut self,
        question: String,
        visual: VisualRequest,
        replaces_scene: bool,
    ) -> Vec<Effect> {
        // A newer valid question makes an outstanding no-stream camera
        // preflight stale before it can supersede anything.
        self.pending_camera = None;

        if !replaces_scene && self.session.history.len() >= MAX_HISTORY_TURNS {
            return vec![Effect::Display(Output::Diagnostic(format!(
                "this session has reached {MAX_HISTORY_TURNS} accepted exchanges; use /reset or /look to start a new scene"
            )))];
        }

        let mut effects = self.cancel_active();
        let id = TurnId::new(self.next_turn_id);
        self.next_turn_id = self
            .next_turn_id
            .checked_add(1)
            .expect("turn identifiers exhausted");

        // A new photograph begins a prospective new scene, so it must not
        // inherit conversations that describe the old scene.
        let history = if replaces_scene {
            Vec::new()
        } else {
            self.session.history.clone()
        };

        self.active = Some(ActiveTurn {
            id,
            question: question.clone(),
            replaces_scene,
        });
        effects.push(Effect::StartTurn(TurnRequest {
            id,
            question,
            history,
            visual,
        }));
        effects
    }

    fn prepare_camera(
        &mut self,
        requested: Option<crate::see::camera::CameraId>,
        question: String,
    ) -> Vec<Effect> {
        let id = CameraRequestId::new(self.next_camera_request_id);
        self.next_camera_request_id = self
            .next_camera_request_id
            .checked_add(1)
            .expect("camera request identifiers exhausted");
        self.pending_camera = Some(PendingCamera { id, question });

        vec![Effect::PrepareCamera(CameraPreparation { id, requested })]
    }

    fn cancel_active(&mut self) -> Vec<Effect> {
        self.active
            .take()
            .map(|active| vec![Effect::CancelTurn(active.id)])
            .unwrap_or_default()
    }

    fn handle_cameras_listed(
        &mut self,
        result: Result<Vec<crate::see::camera::CameraDescriptor>, String>,
    ) -> Vec<Effect> {
        match result {
            Ok(cameras) => vec![Effect::Display(Output::Cameras(cameras))],
            Err(message) => vec![Effect::Display(Output::Diagnostic(format!(
                "could not list cameras: {message}"
            )))],
        }
    }

    fn handle_camera_prepared(
        &mut self,
        id: CameraRequestId,
        result: Result<crate::see::camera::CameraDescriptor, String>,
    ) -> Vec<Effect> {
        if self.pending_camera.as_ref().map(|pending| pending.id) != Some(id) {
            return Vec::new();
        }

        let pending = self
            .pending_camera
            .take()
            .expect("pending camera request was checked above");
        match result {
            Ok(camera) => {
                self.start_turn(pending.question, VisualRequest::ProposeCamera(camera), true)
            }
            Err(message) => vec![Effect::Display(Output::Diagnostic(format!(
                "could not prepare camera: {message}"
            )))],
        }
    }

    fn handle_camera_capture_started(
        &mut self,
        id: TurnId,
        camera: crate::see::camera::CameraDescriptor,
    ) -> Vec<Effect> {
        if self.active.as_ref().map(|active| active.id) != Some(id) {
            return Vec::new();
        }

        vec![Effect::Display(Output::Diagnostic(format!(
            "activating camera {} ({}) to capture one still image",
            camera.id(),
            camera.name()
        )))]
    }

    fn handle_camera_capture_ready(
        &mut self,
        id: TurnId,
        source: crate::see::camera::CameraPhotoSource,
    ) -> Vec<Effect> {
        if self.active.as_ref().map(|active| active.id) != Some(id) {
            return Vec::new();
        }

        vec![Effect::Display(Output::Diagnostic(format!(
            "captured camera {} ({}) at {}x{} as JPEG from {}; camera released, asking local Ollama",
            source.camera().id(),
            source.camera().name(),
            source.width(),
            source.height(),
            source.input_format(),
        )))]
    }

    fn handle_turn_finished(&mut self, finished: TurnFinished) -> Vec<Effect> {
        // A result is eligible only while its ID is the controller's active ID.
        // This makes a late completion from a stopped, reset, or superseded
        // turn harmless.
        if self.active.as_ref().map(|active| active.id) != Some(finished.id) {
            return Vec::new();
        }

        let active = self.active.take().expect("active turn was checked above");

        match finished.result {
            TurnResult::Cancelled => Vec::new(),
            TurnResult::Failed { message } => vec![Effect::Display(Output::Diagnostic(message))],
            TurnResult::Answer {
                answer,
                proposed_scene,
            } => self.accept_answer(active, answer, proposed_scene),
        }
    }

    fn accept_answer(
        &mut self,
        active: ActiveTurn,
        answer: String,
        proposed_scene: Option<Photo>,
    ) -> Vec<Effect> {
        if answer.trim().is_empty() {
            return vec![Effect::Display(Output::Diagnostic(
                "worker returned an empty answer; no answer was accepted".into(),
            ))];
        }

        match (active.replaces_scene, proposed_scene) {
            (true, Some(scene)) => {
                // This is the single point where a proposed photo replaces the
                // accepted scene. Failed/cancelled photo turns never reach it.
                self.session.scene = Some(scene);
                self.session.history.clear();
            }
            (false, None) => {}
            (true, None) | (false, Some(_)) => {
                return vec![Effect::Display(Output::Diagnostic(
                    "worker returned an inconsistent scene result; no answer was accepted".into(),
                ))];
            }
        }

        self.session.history.push(ConversationTurn {
            user: active.question,
            assistant: answer.clone(),
        });

        vec![Effect::Display(Output::Answer(answer))]
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::{
        protocol::{
            CameraPreparation, Effect, Event, Input, Output, TurnFinished, TurnId, TurnRequest,
            TurnResult, VisualRequest,
        },
        see::{
            Photo, PhotoSource,
            camera::{CameraDescriptor, CameraId},
        },
    };

    use super::{Controller, MAX_HISTORY_TURNS};

    fn start_ask(controller: &mut Controller, question: &str) -> TurnRequest {
        let effects = controller.handle(Event::Input(Input::Ask(question.into())));
        effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartTurn(request) => Some(request),
                _ => None,
            })
            .expect("an accepted question should start a turn")
    }

    fn start_look(controller: &mut Controller, path: &str, question: &str) -> TurnRequest {
        let effects = controller.handle(Event::Input(Input::Look {
            path: PathBuf::from(path),
            question: question.into(),
        }));
        effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartTurn(request) => Some(request),
                _ => None,
            })
            .expect("a valid /look should start a turn")
    }

    fn camera(id: u32) -> CameraDescriptor {
        CameraDescriptor::new(CameraId::new(id), format!("Test Camera {id}"))
    }

    fn prepare_camera(
        controller: &mut Controller,
        requested: Option<CameraId>,
        question: &str,
    ) -> CameraPreparation {
        controller
            .handle(Event::Input(Input::Camera {
                requested,
                question: question.into(),
            }))
            .into_iter()
            .find_map(|effect| match effect {
                Effect::PrepareCamera(preparation) => Some(preparation),
                _ => None,
            })
            .expect("a syntactically valid camera command should preflight")
    }

    fn successful_answer(
        controller: &mut Controller,
        id: TurnId,
        answer: &str,
        scene: Option<Photo>,
    ) {
        controller.handle(Event::TurnFinished(TurnFinished {
            id,
            result: TurnResult::Answer {
                answer: answer.into(),
                proposed_scene: scene,
            },
        }));
    }

    #[test]
    fn a_new_question_makes_an_older_completion_harmless() {
        let mut controller = Controller::new();
        let older = start_ask(&mut controller, "first question");
        let newer_effects = controller.handle(Event::Input(Input::Ask("second question".into())));
        let newer = newer_effects
            .iter()
            .find_map(|effect| match effect {
                Effect::StartTurn(request) => Some(request.clone()),
                _ => None,
            })
            .unwrap();

        assert!(newer_effects.contains(&Effect::CancelTurn(older.id)));
        assert!(
            controller
                .handle(Event::TurnFinished(TurnFinished {
                    id: older.id,
                    result: TurnResult::Answer {
                        answer: "old answer".into(),
                        proposed_scene: None,
                    },
                }))
                .is_empty()
        );

        successful_answer(&mut controller, newer.id, "new answer", None);
        assert_eq!(controller.accepted_history().len(), 1);
        assert_eq!(controller.accepted_history()[0].user, "second question");
    }

    #[test]
    fn stopped_turn_cannot_print_or_change_history() {
        let mut controller = Controller::new();
        let request = start_ask(&mut controller, "question");

        assert_eq!(
            controller.handle(Event::Input(Input::Stop)),
            vec![Effect::CancelTurn(request.id)]
        );
        assert!(
            controller
                .handle(Event::TurnFinished(TurnFinished {
                    id: request.id,
                    result: TurnResult::Answer {
                        answer: "too late".into(),
                        proposed_scene: None,
                    },
                }))
                .is_empty()
        );
        assert!(controller.accepted_history().is_empty());
    }

    #[test]
    fn reset_prevents_a_late_result_from_restoring_session_state() {
        let mut controller = Controller::new();
        let old_photo = Photo::test_photo("old.png");
        let look = start_look(&mut controller, "old.png", "what is this?");
        successful_answer(&mut controller, look.id, "an old scene", Some(old_photo));

        let request = start_ask(&mut controller, "follow up");
        controller.handle(Event::Input(Input::Reset));
        assert!(controller.accepted_scene().is_none());
        assert!(controller.accepted_history().is_empty());

        assert!(
            controller
                .handle(Event::TurnFinished(TurnFinished {
                    id: request.id,
                    result: TurnResult::Answer {
                        answer: "too late".into(),
                        proposed_scene: None,
                    },
                }))
                .is_empty()
        );
        assert!(controller.accepted_scene().is_none());
        assert!(controller.accepted_history().is_empty());
    }

    #[test]
    fn failed_new_photo_preserves_the_accepted_scene_and_history() {
        let mut controller = Controller::new();
        let old_photo = Photo::test_photo("old.png");
        let first = start_look(&mut controller, "old.png", "old question");
        successful_answer(&mut controller, first.id, "old answer", Some(old_photo));

        let replacement = start_look(&mut controller, "missing.png", "new question");
        let effects = controller.handle(Event::TurnFinished(TurnFinished {
            id: replacement.id,
            result: TurnResult::Failed {
                message: "could not use photo".into(),
            },
        }));

        assert_eq!(
            effects,
            vec![Effect::Display(Output::Diagnostic(
                "could not use photo".into()
            ))]
        );
        assert_eq!(
            controller.accepted_scene().unwrap().path(),
            Some(PathBuf::from("old.png").as_path())
        );
        assert_eq!(controller.accepted_history().len(), 1);
        assert_eq!(controller.accepted_history()[0].user, "old question");
    }

    #[test]
    fn stopped_new_photo_preserves_the_accepted_scene_and_history() {
        let mut controller = Controller::new();
        let old_photo = Photo::test_photo("old.png");
        let first = start_look(&mut controller, "old.png", "old question");
        successful_answer(&mut controller, first.id, "old answer", Some(old_photo));

        let replacement = start_look(&mut controller, "new.png", "new question");
        controller.handle(Event::Input(Input::Stop));
        assert!(
            controller
                .handle(Event::TurnFinished(TurnFinished {
                    id: replacement.id,
                    result: TurnResult::Answer {
                        answer: "too late".into(),
                        proposed_scene: Some(Photo::test_photo("new.png")),
                    },
                }))
                .is_empty()
        );

        assert_eq!(
            controller.accepted_scene().unwrap().path(),
            Some(PathBuf::from("old.png").as_path())
        );
        assert_eq!(controller.accepted_history().len(), 1);
        assert_eq!(controller.accepted_history()[0].user, "old question");
    }

    #[test]
    fn follow_up_carries_the_accepted_photo_and_history() {
        let mut controller = Controller::new();
        let photo = Photo::test_photo("tree.png");
        let first = start_look(&mut controller, "tree.png", "what tree is this?");
        successful_answer(&mut controller, first.id, "a test tree", Some(photo));

        let follow_up = start_ask(&mut controller, "what should I notice next?");
        assert_eq!(follow_up.history.len(), 1);
        assert_eq!(follow_up.history[0].assistant, "a test tree");
        assert!(matches!(
            follow_up.visual,
            VisualRequest::UseAccepted(Some(ref photo)) if photo.path() == Some(PathBuf::from("tree.png").as_path())
        ));
    }

    #[test]
    fn blank_and_malformed_input_do_not_cancel_valid_work() {
        let mut controller = Controller::new();
        let established = start_ask(&mut controller, "accepted question");
        successful_answer(&mut controller, established.id, "accepted answer", None);
        let request = start_ask(&mut controller, "keep working");

        assert!(controller.handle(Event::Input(Input::Blank)).is_empty());
        assert!(controller.has_active_turn());
        assert_eq!(
            controller.handle(Event::Input(Input::Malformed("bad /look syntax".into()))),
            vec![Effect::Display(Output::Diagnostic(
                "invalid command: bad /look syntax".into()
            ))]
        );
        assert!(controller.has_active_turn());
        assert_eq!(controller.accepted_history().len(), 1);
        assert_eq!(
            controller.handle(Event::Input(Input::Stop)),
            vec![Effect::CancelTurn(request.id)]
        );
    }

    #[test]
    fn history_limit_requires_an_explicit_new_scene_or_reset() {
        let mut controller = Controller::new();
        for number in 0..MAX_HISTORY_TURNS {
            let request = start_ask(&mut controller, &format!("question {number}"));
            successful_answer(&mut controller, request.id, "answer", None);
        }

        let effects = controller.handle(Event::Input(Input::Ask("one too many".into())));
        assert_eq!(controller.accepted_history().len(), MAX_HISTORY_TURNS);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Display(Output::Diagnostic(message))] if message.contains("/reset or /look")
        ));
    }

    #[test]
    fn invalid_camera_selection_does_not_interrupt_an_active_turn() {
        let mut controller = Controller::new();
        let active = start_ask(&mut controller, "keep answering this");
        let preparation = prepare_camera(
            &mut controller,
            Some(CameraId::new(99)),
            "what should I notice?",
        );

        assert!(controller.has_active_turn());
        let effects = controller.handle(Event::CameraPrepared {
            id: preparation.id,
            result: Err("camera 99 is not available".into()),
        });

        assert_eq!(
            effects,
            vec![Effect::Display(Output::Diagnostic(
                "could not prepare camera: camera 99 is not available".into()
            ))]
        );
        assert!(controller.has_active_turn());
        assert!(
            controller
                .handle(Event::TurnFinished(TurnFinished {
                    id: active.id,
                    result: TurnResult::Answer {
                        answer: "still accepted".into(),
                        proposed_scene: None,
                    },
                }))
                .contains(&Effect::Display(Output::Answer("still accepted".into())))
        );
    }

    #[test]
    fn failed_or_cancelled_camera_scene_preserves_the_previous_camera_scene() {
        let mut controller = Controller::new();
        let old_camera = camera(0);
        let first = start_look(&mut controller, "old.png", "old scene");
        successful_answer(
            &mut controller,
            first.id,
            "old answer",
            Some(Photo::test_camera_photo(old_camera.clone())),
        );

        let preparation = prepare_camera(&mut controller, Some(CameraId::new(1)), "new scene");
        let replacement = controller
            .handle(Event::CameraPrepared {
                id: preparation.id,
                result: Ok(camera(1)),
            })
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartTurn(request) => Some(request),
                _ => None,
            })
            .expect("a selected camera should start a staged scene");
        assert!(matches!(
            replacement.visual,
            VisualRequest::ProposeCamera(_)
        ));

        controller.handle(Event::TurnFinished(TurnFinished {
            id: replacement.id,
            result: TurnResult::Failed {
                message: "camera timed out".into(),
            },
        }));
        assert!(matches!(
            controller.accepted_scene().unwrap().source(),
            PhotoSource::Camera(source) if source.camera() == &old_camera
        ));
        assert_eq!(controller.accepted_history().len(), 1);

        let preparation = prepare_camera(&mut controller, Some(CameraId::new(1)), "new scene");
        let replacement = controller
            .handle(Event::CameraPrepared {
                id: preparation.id,
                result: Ok(camera(1)),
            })
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartTurn(request) => Some(request),
                _ => None,
            })
            .unwrap();
        controller.handle(Event::Input(Input::Stop));
        assert!(
            controller
                .handle(Event::TurnFinished(TurnFinished {
                    id: replacement.id,
                    result: TurnResult::Answer {
                        answer: "too late".into(),
                        proposed_scene: Some(Photo::test_camera_photo(camera(1))),
                    },
                }))
                .is_empty()
        );
        assert!(matches!(
            controller.accepted_scene().unwrap().source(),
            PhotoSource::Camera(source) if source.camera() == &old_camera
        ));
        assert_eq!(controller.accepted_history().len(), 1);
    }

    #[test]
    fn reset_makes_late_camera_preflight_and_capture_results_harmless() {
        let mut controller = Controller::new();
        let preparation = prepare_camera(&mut controller, None, "a camera question");
        controller.handle(Event::Input(Input::Reset));
        assert!(
            controller
                .handle(Event::CameraPrepared {
                    id: preparation.id,
                    result: Ok(camera(0)),
                })
                .is_empty()
        );

        let preparation = prepare_camera(&mut controller, None, "a camera question");
        let request = controller
            .handle(Event::CameraPrepared {
                id: preparation.id,
                result: Ok(camera(0)),
            })
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartTurn(request) => Some(request),
                _ => None,
            })
            .unwrap();
        controller.handle(Event::Input(Input::Reset));
        assert!(
            controller
                .handle(Event::TurnFinished(TurnFinished {
                    id: request.id,
                    result: TurnResult::Answer {
                        answer: "too late".into(),
                        proposed_scene: Some(Photo::test_camera_photo(camera(0))),
                    },
                }))
                .is_empty()
        );
        assert!(controller.accepted_scene().is_none());
        assert!(controller.accepted_history().is_empty());
    }

    #[test]
    fn a_new_question_makes_late_camera_preflight_and_capture_results_harmless() {
        let mut controller = Controller::new();
        let preparation = prepare_camera(&mut controller, Some(CameraId::new(0)), "camera turn");
        let newer = start_ask(&mut controller, "newer text turn");
        assert!(
            controller
                .handle(Event::CameraPrepared {
                    id: preparation.id,
                    result: Ok(camera(0)),
                })
                .is_empty()
        );

        let preparation = prepare_camera(&mut controller, Some(CameraId::new(0)), "camera turn");
        let camera_turn = controller
            .handle(Event::CameraPrepared {
                id: preparation.id,
                result: Ok(camera(0)),
            })
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartTurn(request) => Some(request),
                _ => None,
            })
            .unwrap();
        let replacement = start_ask(&mut controller, "newer replacement");
        assert!(
            controller
                .handle(Event::TurnFinished(TurnFinished {
                    id: camera_turn.id,
                    result: TurnResult::Answer {
                        answer: "late camera answer".into(),
                        proposed_scene: Some(Photo::test_camera_photo(camera(0))),
                    },
                }))
                .is_empty()
        );
        successful_answer(&mut controller, replacement.id, "new accepted answer", None);
        assert!(controller.accepted_scene().is_none());
        assert_eq!(controller.accepted_history().len(), 1);
        assert_eq!(controller.accepted_history()[0].user, "newer replacement");
        assert_ne!(newer.id, replacement.id);
    }
}
