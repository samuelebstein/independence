//! Session state and output authorization.
//!
//! The controller is deliberately synchronous. It turns facts (`Event`) into
//! requested side effects (`Effect`) and owns the only accepted scene/history.
//! Worker tasks receive snapshots and can therefore never mutate this state.

use crate::{
    protocol::{
        CameraPreparation, CameraRequestId, DecisionId, DecisionRequest, DecisionResult, Effect,
        Event, Input, Output, PermissionDecision, PhotoDecision, TurnFinished, TurnId, TurnRequest,
        TurnResult, VisualRequest,
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
    active_decision: Option<ActiveDecision>,
    pending_camera: Option<PendingCamera>,
    pending_permission: Option<PendingPermission>,
    next_turn_id: u64,
    next_decision_id: u64,
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
enum ActiveDecision {
    Photo {
        id: DecisionId,
        question: String,
        /// Snapshot whether the model received an accepted scene. The
        /// controller carries it through the no-stream camera preflight so a
        /// later permission question can accurately describe an additional
        /// view rather than implying the existing image disappeared.
        has_accepted_scene: bool,
    },
    Permission {
        id: DecisionId,
        request: PendingPermission,
    },
}

#[derive(Debug)]
enum PendingCamera {
    Explicit {
        id: CameraRequestId,
        question: String,
    },
    Permission {
        id: CameraRequestId,
        question: String,
        needed_visual_evidence: String,
        has_accepted_scene: bool,
    },
}

impl PendingCamera {
    fn id(&self) -> CameraRequestId {
        match self {
            Self::Explicit { id, .. } | Self::Permission { id, .. } => *id,
        }
    }
}

#[derive(Clone, Debug)]
struct PendingPermission {
    original_question: String,
    camera: crate::see::camera::CameraDescriptor,
    proposed_action: String,
    permission_question: String,
}

impl Controller {
    pub fn new() -> Self {
        Self {
            next_turn_id: 1,
            next_decision_id: 1,
            next_camera_request_id: 1,
            ..Self::default()
        }
    }

    /// Reacts to one fact without performing I/O itself.
    pub fn handle(&mut self, event: Event) -> Vec<Effect> {
        match event {
            Event::Input(input) => self.handle_input(input),
            Event::VoiceTranscript(text) => self.handle_voice_transcript(text),
            Event::CamerasListed(result) => self.handle_cameras_listed(result),
            Event::CameraPrepared { id, result } => self.handle_camera_prepared(id, result),
            Event::CameraCaptureStarted { id, camera } => {
                self.handle_camera_capture_started(id, camera)
            }
            Event::CameraCaptureReady { id, source } => {
                self.handle_camera_capture_ready(id, source)
            }
            Event::DecisionFinished { id, result } => self.handle_decision_finished(id, result),
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

    #[cfg(test)]
    pub(crate) fn has_active_decision(&self) -> bool {
        self.active_decision.is_some()
    }

    /// Lets the raw-key voice runtime preserve an already-shown permission
    /// request while Enter starts the microphone for its reply.
    pub(crate) fn awaits_photo_permission(&self) -> bool {
        self.pending_permission.is_some()
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
                self.pending_permission = None;
                self.cancel_active_work()
            }
            Input::Reset => {
                self.pending_camera = None;
                self.pending_permission = None;
                let mut effects = self.cancel_active_work();
                self.session = Session::default();
                effects.push(Effect::Display(Output::Diagnostic(
                    "session cleared; original files and terminal scrollback are unchanged".into(),
                )));
                effects
            }
            Input::Help => vec![Effect::Display(Output::Help)],
            Input::Quit | Input::EndOfInput | Input::Interrupt => {
                self.pending_camera = None;
                self.pending_permission = None;
                let mut effects = self.cancel_active_work();
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
        self.pending_permission = None;

        if !replaces_scene && self.session.history.len() >= MAX_HISTORY_TURNS {
            return vec![Effect::Display(Output::Diagnostic(format!(
                "this session has reached {MAX_HISTORY_TURNS} accepted exchanges; use /reset or /look to start a new scene"
            )))];
        }

        let mut effects = self.cancel_active_work();
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

    /// Starts the model-mediated choice for one accepted spoken question.
    ///
    /// This is intentionally separate from `Input::Ask`: a spoken transcript
    /// has no terminal-command authority, and a later permission reply needs
    /// to be distinguishable from a new question.
    fn handle_voice_transcript(&mut self, text: String) -> Vec<Effect> {
        if text.trim().is_empty() {
            return Vec::new();
        }

        match self.pending_permission.take() {
            Some(permission) => self.start_permission_decision(permission, text),
            None => self.start_photo_decision(text),
        }
    }

    fn start_photo_decision(&mut self, question: String) -> Vec<Effect> {
        if question.trim().is_empty() {
            return Vec::new();
        }

        self.pending_camera = None;
        self.pending_permission = None;
        let mut effects = self.cancel_active_work();
        let id = DecisionId::new(self.next_decision_id);
        self.next_decision_id = self
            .next_decision_id
            .checked_add(1)
            .expect("decision identifiers exhausted");
        let has_accepted_scene = self.session.scene.is_some();
        self.active_decision = Some(ActiveDecision::Photo {
            id,
            question: question.clone(),
            has_accepted_scene,
        });
        effects.push(Effect::StartDecision(DecisionRequest::ChoosePhoto {
            id,
            question,
            history: self.session.history.clone(),
            scene: self.session.scene.clone(),
        }));
        effects
    }

    fn start_permission_decision(
        &mut self,
        request: PendingPermission,
        reply: String,
    ) -> Vec<Effect> {
        let mut effects = self.cancel_active_work();
        let id = DecisionId::new(self.next_decision_id);
        self.next_decision_id = self
            .next_decision_id
            .checked_add(1)
            .expect("decision identifiers exhausted");
        self.active_decision = Some(ActiveDecision::Permission {
            id,
            request: request.clone(),
        });
        effects.push(Effect::StartDecision(
            DecisionRequest::InterpretPermission {
                id,
                proposed_action: request.proposed_action,
                selected_camera: request.camera,
                permission_question: request.permission_question,
                reply,
            },
        ));
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
        self.pending_permission = None;
        self.pending_camera = Some(PendingCamera::Explicit { id, question });

        vec![Effect::PrepareCamera(CameraPreparation { id, requested })]
    }

    fn prepare_permission_camera(
        &mut self,
        question: String,
        needed_visual_evidence: String,
        has_accepted_scene: bool,
    ) -> Vec<Effect> {
        let needed_visual_evidence = needed_visual_evidence.trim().to_owned();
        if needed_visual_evidence.is_empty() {
            return vec![Effect::Display(Output::Diagnostic(
                "local photo decision requested a photo without naming the missing visual detail; no camera was activated".into(),
            ))];
        }

        let id = CameraRequestId::new(self.next_camera_request_id);
        self.next_camera_request_id = self
            .next_camera_request_id
            .checked_add(1)
            .expect("camera request identifiers exhausted");
        self.pending_camera = Some(PendingCamera::Permission {
            id,
            question,
            needed_visual_evidence,
            has_accepted_scene,
        });

        // This is inventory/selection only. `TurnRuntime` does not open a
        // capture stream for `PrepareCamera`; the camera remains off until a
        // current permission result later starts a normal camera turn.
        vec![Effect::PrepareCamera(CameraPreparation {
            id,
            requested: None,
        })]
    }

    fn cancel_active(&mut self) -> Vec<Effect> {
        self.active
            .take()
            .map(|active| vec![Effect::CancelTurn(active.id)])
            .unwrap_or_default()
    }

    fn cancel_active_decision(&mut self) -> Vec<Effect> {
        self.active_decision
            .take()
            .map(|active| {
                let id = match active {
                    ActiveDecision::Photo { id, .. } => id,
                    ActiveDecision::Permission { id, .. } => id,
                };
                vec![Effect::CancelDecision(id)]
            })
            .unwrap_or_default()
    }

    fn cancel_active_work(&mut self) -> Vec<Effect> {
        let mut effects = self.cancel_active();
        effects.extend(self.cancel_active_decision());
        effects
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
        if self.pending_camera.as_ref().map(PendingCamera::id) != Some(id) {
            return Vec::new();
        }

        let pending = self
            .pending_camera
            .take()
            .expect("pending camera request was checked above");
        match (pending, result) {
            (PendingCamera::Explicit { question, .. }, Ok(camera)) => {
                self.start_turn(question, VisualRequest::ProposeCamera(camera), true)
            }
            (PendingCamera::Explicit { .. }, Err(message)) => {
                vec![Effect::Display(Output::Diagnostic(format!(
                    "could not prepare camera: {message}"
                )))]
            }
            (
                PendingCamera::Permission {
                    question,
                    needed_visual_evidence,
                    has_accepted_scene,
                    ..
                },
                Ok(camera),
            ) => self.ask_photo_permission(
                question,
                camera,
                needed_visual_evidence,
                has_accepted_scene,
            ),
            (PendingCamera::Permission { .. }, Err(message)) => {
                vec![Effect::Display(Output::Diagnostic(format!(
                    "a photograph may help, but no single camera is ready for this request: {message}"
                )))]
            }
        }
    }

    fn ask_photo_permission(
        &mut self,
        original_question: String,
        camera: crate::see::camera::CameraDescriptor,
        needed_visual_evidence: String,
        has_accepted_scene: bool,
    ) -> Vec<Effect> {
        let (proposed_action, permission_question) = if has_accepted_scene {
            (
                format!(
                    "Capture one additional still photograph now from camera {} ({}) to inspect {}.",
                    camera.id(),
                    camera.name(),
                    needed_visual_evidence,
                ),
                format!(
                    "I already have one accepted still photo. A different new photo may help me inspect {}. May I capture one additional still image now from camera {} ({})?",
                    needed_visual_evidence,
                    camera.id(),
                    camera.name(),
                ),
            )
        } else {
            (
                format!(
                    "Capture one still photograph now from camera {} ({}) to inspect {}.",
                    camera.id(),
                    camera.name(),
                    needed_visual_evidence,
                ),
                format!(
                    "A photograph may help me inspect {}. May I capture one still image now from camera {} ({})?",
                    needed_visual_evidence,
                    camera.id(),
                    camera.name(),
                ),
            )
        };
        self.pending_permission = Some(PendingPermission {
            original_question,
            camera,
            proposed_action,
            permission_question: permission_question.clone(),
        });
        vec![Effect::Display(Output::PermissionQuestion(
            permission_question,
        ))]
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

    fn handle_decision_finished(&mut self, id: DecisionId, result: DecisionResult) -> Vec<Effect> {
        let Some(active) = self.active_decision.as_ref() else {
            return Vec::new();
        };
        let active_id = match active {
            ActiveDecision::Photo { id, .. } => *id,
            ActiveDecision::Permission { id, .. } => *id,
        };
        if active_id != id {
            return Vec::new();
        }

        let active = self
            .active_decision
            .take()
            .expect("active decision was checked above");
        match (active, result) {
            (
                ActiveDecision::Photo {
                    question,
                    has_accepted_scene,
                    ..
                },
                DecisionResult::Photo(decision),
            ) => match decision {
                PhotoDecision::Answer { answer } => self.accept_decision_answer(question, answer),
                PhotoDecision::RequestPhoto {
                    needed_visual_evidence,
                } => self.prepare_permission_camera(
                    question,
                    needed_visual_evidence,
                    has_accepted_scene,
                ),
            },
            (ActiveDecision::Permission { request, .. }, DecisionResult::Permission(decision)) => {
                self.handle_permission_decision(request, decision)
            }
            (ActiveDecision::Permission { request, .. }, DecisionResult::Failed { message }) => {
                self.pending_permission = Some(request);
                vec![Effect::Display(Output::Diagnostic(format!(
                    "could not interpret the photo permission reply: {message}; please reply again"
                )))]
            }
            (_, DecisionResult::Cancelled) => Vec::new(),
            (_, DecisionResult::Failed { message }) => vec![Effect::Display(Output::Diagnostic(
                format!("local photo decision failed: {message}"),
            ))],
            (ActiveDecision::Permission { request, .. }, unexpected) => {
                self.pending_permission = Some(request);
                vec![Effect::Display(Output::Diagnostic(format!(
                    "local permission interpreter returned an unexpected result: {unexpected:?}; please reply again"
                )))]
            }
            (_, unexpected) => vec![Effect::Display(Output::Diagnostic(format!(
                "local photo decision returned an unexpected result: {unexpected:?}"
            )))],
        }
    }

    fn handle_permission_decision(
        &mut self,
        request: PendingPermission,
        decision: PermissionDecision,
    ) -> Vec<Effect> {
        match decision {
            PermissionDecision::Yes => self.start_turn(
                request.original_question,
                VisualRequest::ProposeCamera(request.camera),
                true,
            ),
            PermissionDecision::No => self.start_turn(
                request.original_question,
                VisualRequest::UseAccepted(self.session.scene.clone()),
                false,
            ),
            PermissionDecision::Unclear => {
                let question = request.permission_question.clone();
                self.pending_permission = Some(request);
                vec![Effect::Display(Output::PermissionClarification(question))]
            }
        }
    }

    fn accept_decision_answer(&mut self, question: String, answer: String) -> Vec<Effect> {
        if self.session.history.len() >= MAX_HISTORY_TURNS {
            return vec![Effect::Display(Output::Diagnostic(format!(
                "this session has reached {MAX_HISTORY_TURNS} accepted exchanges; use /reset or /look to start a new scene"
            )))];
        }

        if answer.trim().is_empty() {
            return vec![Effect::Display(Output::Diagnostic(
                "local photo decision returned an empty answer; no answer was accepted".into(),
            ))];
        }

        self.session.history.push(ConversationTurn {
            user: question,
            assistant: answer.clone(),
        });
        vec![Effect::Display(Output::Answer(answer))]
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
            CameraPreparation, DecisionRequest, DecisionResult, Effect, Event, Input, Output,
            PermissionDecision, PhotoDecision, TurnFinished, TurnId, TurnRequest, TurnResult,
            VisualRequest,
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

    fn requested_photo(needed_visual_evidence: &str) -> PhotoDecision {
        PhotoDecision::RequestPhoto {
            needed_visual_evidence: needed_visual_evidence.into(),
        }
    }

    fn establish_accepted_camera_scene(controller: &mut Controller) -> Photo {
        let photo = Photo::test_camera_photo(camera(7));
        let first = start_look(controller, "accepted-camera.jpeg", "what is in this photo?");
        successful_answer(
            controller,
            first.id,
            "The accepted photo contains a test object.",
            Some(photo.clone()),
        );
        photo
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

    #[test]
    fn voice_photo_decision_can_accept_a_text_answer_without_starting_a_turn() {
        let mut controller = Controller::new();
        let question = "Why is the sky blue?";
        let request = controller
            .handle(Event::VoiceTranscript(question.into()))
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartDecision(request) => Some(request),
                _ => None,
            })
            .expect("an accepted voice transcript should start a photo decision");

        let DecisionRequest::ChoosePhoto {
            id, history, scene, ..
        } = request
        else {
            panic!("voice input should begin a photo decision");
        };
        assert!(history.is_empty());
        assert!(scene.is_none());

        let effects = controller.handle(Event::DecisionFinished {
            id,
            result: DecisionResult::Photo(PhotoDecision::Answer {
                answer: "Sunlight scatters from air molecules, with blue light scattering more strongly.".into(),
            }),
        });

        assert!(matches!(
            effects.as_slice(),
            [Effect::Display(Output::Answer(answer))] if answer.starts_with("Sunlight scatters")
        ));
        assert_eq!(controller.accepted_history().len(), 1);
        assert_eq!(controller.accepted_history()[0].user, question);
        assert!(!controller.has_active_turn());
    }

    #[test]
    fn nonvisual_voice_question_can_accept_an_honest_limitation_without_camera_work() {
        let mut controller = Controller::new();
        let question = "What time and day is it?";
        let decision_id = controller
            .handle(Event::VoiceTranscript(question.into()))
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartDecision(DecisionRequest::ChoosePhoto { id, .. }) => Some(id),
                _ => None,
            })
            .unwrap();

        let effects = controller.handle(Event::DecisionFinished {
            id: decision_id,
            result: DecisionResult::Photo(PhotoDecision::Answer {
                answer: "I do not have a system clock or calendar, so I cannot tell the current time or day.".into(),
            }),
        });

        assert!(matches!(
            effects.as_slice(),
            [Effect::Display(Output::Answer(answer))] if answer.contains("system clock")
        ));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::PrepareCamera(_)))
        );
        assert!(controller.accepted_scene().is_none());
        assert_eq!(controller.accepted_history().len(), 1);
    }

    #[test]
    fn many_voice_follow_ups_keep_the_same_accepted_photo_and_history() {
        let mut controller = Controller::new();
        let accepted_photo = establish_accepted_camera_scene(&mut controller);
        let follow_ups = [
            (
                "What was in the photo?",
                "It contains the test object from the accepted still.",
            ),
            (
                "Which visible feature should I inspect first?",
                "Start with the object's edge and surface texture.",
            ),
            (
                "What remains uncertain from this view?",
                "The far side is outside the accepted still.",
            ),
        ];

        for (index, (question, answer)) in follow_ups.iter().enumerate() {
            let request = controller
                .handle(Event::VoiceTranscript((*question).into()))
                .into_iter()
                .find_map(|effect| match effect {
                    Effect::StartDecision(request) => Some(request),
                    _ => None,
                })
                .expect("each current spoken follow-up should start one decision");
            let DecisionRequest::ChoosePhoto {
                id, history, scene, ..
            } = request
            else {
                panic!("spoken follow-up should remain a photo decision");
            };

            assert_eq!(history.len(), index + 1);
            assert_eq!(scene.as_ref(), Some(&accepted_photo));

            let effects = controller.handle(Event::DecisionFinished {
                id,
                result: DecisionResult::Photo(PhotoDecision::Answer {
                    answer: (*answer).into(),
                }),
            });
            assert!(matches!(
                effects.as_slice(),
                [Effect::Display(Output::Answer(displayed))] if displayed == answer
            ));
            assert_eq!(controller.accepted_scene(), Some(&accepted_photo));
        }

        assert_eq!(controller.accepted_history().len(), follow_ups.len() + 1);
        assert_eq!(
            controller.accepted_history().last().unwrap().user,
            "What remains uncertain from this view?"
        );
    }

    #[test]
    fn additional_photo_request_names_a_missing_view_without_erasing_the_scene() {
        let mut controller = Controller::new();
        let accepted_photo = establish_accepted_camera_scene(&mut controller);
        let original_question = "Can you read the text on the far side?";
        let decision_id = controller
            .handle(Event::VoiceTranscript(original_question.into()))
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartDecision(DecisionRequest::ChoosePhoto { id, .. }) => Some(id),
                _ => None,
            })
            .unwrap();

        let effects = controller.handle(Event::DecisionFinished {
            id: decision_id,
            result: DecisionResult::Photo(requested_photo("the text on the object's far side")),
        });
        let preparation = effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::PrepareCamera(preparation) => Some(preparation),
                _ => None,
            })
            .expect("a concrete additional-view need may preflight a camera");

        assert_eq!(controller.accepted_scene(), Some(&accepted_photo));
        assert_eq!(controller.accepted_history().len(), 1);

        let effects = controller.handle(Event::CameraPrepared {
            id: preparation.id,
            result: Ok(camera(7)),
        });
        let shown_question = effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Display(Output::PermissionQuestion(question)) => Some(question),
                _ => None,
            })
            .expect("a selected camera should show an additional-view permission question");
        assert!(shown_question.contains("already have one accepted still photo"));
        assert!(shown_question.contains("different new photo"));
        assert!(shown_question.contains("text on the object's far side"));

        let permission = controller
            .handle(Event::VoiceTranscript("Please explain why first.".into()))
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartDecision(DecisionRequest::InterpretPermission {
                    proposed_action,
                    ..
                }) => Some(proposed_action),
                _ => None,
            })
            .expect("the reply should be interpreted against the exact new-view request");
        assert!(permission.contains("additional still photograph"));
        assert!(permission.contains("text on the object's far side"));
        assert_eq!(controller.accepted_scene(), Some(&accepted_photo));
    }

    #[test]
    fn declining_an_additional_photo_uses_the_existing_scene_for_the_original_question() {
        let mut controller = Controller::new();
        let accepted_photo = establish_accepted_camera_scene(&mut controller);
        let original_question = "Can you tell whether the hidden side has a label?";
        let decision_id = controller
            .handle(Event::VoiceTranscript(original_question.into()))
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartDecision(DecisionRequest::ChoosePhoto { id, .. }) => Some(id),
                _ => None,
            })
            .unwrap();
        let preparation = controller
            .handle(Event::DecisionFinished {
                id: decision_id,
                result: DecisionResult::Photo(requested_photo("the hidden side of the object")),
            })
            .into_iter()
            .find_map(|effect| match effect {
                Effect::PrepareCamera(preparation) => Some(preparation),
                _ => None,
            })
            .unwrap();
        controller.handle(Event::CameraPrepared {
            id: preparation.id,
            result: Ok(camera(7)),
        });
        let permission_id = controller
            .handle(Event::VoiceTranscript(
                "Please answer from the photo you already have.".into(),
            ))
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartDecision(DecisionRequest::InterpretPermission { id, .. }) => Some(id),
                _ => None,
            })
            .unwrap();

        let effects = controller.handle(Event::DecisionFinished {
            id: permission_id,
            result: DecisionResult::Permission(PermissionDecision::No),
        });
        let turn = effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartTurn(request) => Some(request),
                _ => None,
            })
            .expect("a refusal should answer the original question with the current scene");
        assert_eq!(turn.question, original_question);
        assert!(matches!(
            turn.visual,
            VisualRequest::UseAccepted(Some(scene)) if scene == accepted_photo
        ));
        assert_eq!(controller.accepted_scene(), Some(&accepted_photo));
    }

    #[test]
    fn blank_photo_purpose_cannot_start_camera_preflight_or_change_state() {
        let mut controller = Controller::new();
        let accepted_photo = establish_accepted_camera_scene(&mut controller);
        let decision_id = controller
            .handle(Event::VoiceTranscript("What detail is missing?".into()))
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartDecision(DecisionRequest::ChoosePhoto { id, .. }) => Some(id),
                _ => None,
            })
            .unwrap();

        let effects = controller.handle(Event::DecisionFinished {
            id: decision_id,
            result: DecisionResult::Photo(requested_photo("   ")),
        });
        assert!(matches!(
            effects.as_slice(),
            [Effect::Display(Output::Diagnostic(message))]
                if message.contains("without naming the missing visual detail")
        ));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::PrepareCamera(_)))
        );
        assert_eq!(controller.accepted_scene(), Some(&accepted_photo));
        assert_eq!(controller.accepted_history().len(), 1);
        assert!(!controller.awaits_photo_permission());
    }

    #[test]
    fn newer_voice_question_makes_an_old_photo_decision_harmless() {
        let mut controller = Controller::new();
        let older = controller
            .handle(Event::VoiceTranscript("first spoken question".into()))
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartDecision(DecisionRequest::ChoosePhoto { id, .. }) => Some(id),
                _ => None,
            })
            .unwrap();
        let newer_effects =
            controller.handle(Event::VoiceTranscript("second spoken question".into()));
        let newer = newer_effects
            .iter()
            .find_map(|effect| match effect {
                Effect::StartDecision(DecisionRequest::ChoosePhoto { id, .. }) => Some(*id),
                _ => None,
            })
            .unwrap();

        assert!(newer_effects.contains(&Effect::CancelDecision(older)));
        assert!(
            controller
                .handle(Event::DecisionFinished {
                    id: older,
                    result: DecisionResult::Photo(PhotoDecision::Answer {
                        answer: "late answer".into(),
                    }),
                })
                .is_empty()
        );

        controller.handle(Event::DecisionFinished {
            id: newer,
            result: DecisionResult::Photo(PhotoDecision::Answer {
                answer: "current answer".into(),
            }),
        });
        assert_eq!(controller.accepted_history().len(), 1);
        assert_eq!(
            controller.accepted_history()[0].user,
            "second spoken question"
        );
    }

    #[test]
    fn photo_suggestion_preflights_without_capture_then_asks_permission() {
        let mut controller = Controller::new();
        let id = controller
            .handle(Event::VoiceTranscript(
                "What plant is in front of me?".into(),
            ))
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartDecision(DecisionRequest::ChoosePhoto { id, .. }) => Some(id),
                _ => None,
            })
            .unwrap();

        let effects = controller.handle(Event::DecisionFinished {
            id,
            result: DecisionResult::Photo(PhotoDecision::RequestPhoto {
                needed_visual_evidence: "the plant's leaf shape and bark texture".into(),
            }),
        });
        let preparation = effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::PrepareCamera(preparation) => Some(preparation),
                _ => None,
            })
            .expect("a photo suggestion should only preflight camera selection");

        let effects = controller.handle(Event::CameraPrepared {
            id: preparation.id,
            result: Ok(camera(7)),
        });
        assert!(matches!(
            effects.as_slice(),
            [Effect::Display(Output::PermissionQuestion(question))]
                if question.contains("camera 7")
                    && question.contains("May I capture")
                    && question.contains("leaf shape and bark texture")
                    && !question.contains("already have one accepted still photo")
        ));
        assert!(controller.awaits_photo_permission());
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::StartTurn(_)))
        );
        assert!(controller.accepted_history().is_empty());
    }

    fn ask_for_photo_permission(
        controller: &mut Controller,
        question: &str,
        camera: CameraDescriptor,
    ) -> String {
        let decision_id = controller
            .handle(Event::VoiceTranscript(question.into()))
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartDecision(DecisionRequest::ChoosePhoto { id, .. }) => Some(id),
                _ => None,
            })
            .unwrap();
        let preparation = controller
            .handle(Event::DecisionFinished {
                id: decision_id,
                result: DecisionResult::Photo(PhotoDecision::RequestPhoto {
                    needed_visual_evidence: "the object's visible surface details".into(),
                }),
            })
            .into_iter()
            .find_map(|effect| match effect {
                Effect::PrepareCamera(preparation) => Some(preparation),
                _ => None,
            })
            .unwrap();
        controller
            .handle(Event::CameraPrepared {
                id: preparation.id,
                result: Ok(camera),
            })
            .into_iter()
            .find_map(|effect| match effect {
                Effect::Display(Output::PermissionQuestion(question)) => Some(question),
                _ => None,
            })
            .expect("selected camera should produce a permission question")
    }

    #[test]
    fn permission_reply_is_a_structured_model_request_with_its_actual_context() {
        let mut controller = Controller::new();
        let camera = camera(3);
        let original_question = "What kind of leaf is this?";
        let shown_question =
            ask_for_photo_permission(&mut controller, original_question, camera.clone());
        let reply = "That particular picture is fine with me.";

        let request = controller
            .handle(Event::VoiceTranscript(reply.into()))
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartDecision(request) => Some(request),
                _ => None,
            })
            .expect("the accepted reply should start permission interpretation");
        let DecisionRequest::InterpretPermission {
            proposed_action,
            selected_camera,
            permission_question,
            reply: supplied_reply,
            ..
        } = request
        else {
            panic!("permission reply must not become a normal question");
        };

        assert_eq!(selected_camera, camera);
        assert!(proposed_action.contains("camera 3"));
        assert_eq!(permission_question, shown_question);
        assert_eq!(supplied_reply, reply);
        assert!(!controller.awaits_photo_permission());
        assert!(controller.accepted_history().is_empty());
    }

    #[test]
    fn approval_is_consumed_once_and_starts_the_original_camera_turn() {
        let mut controller = Controller::new();
        let camera = camera(2);
        let original_question = "What should I notice about this object?";
        ask_for_photo_permission(&mut controller, original_question, camera.clone());
        let decision_id = controller
            .handle(Event::VoiceTranscript(
                "Yes, that capture is okay now.".into(),
            ))
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartDecision(DecisionRequest::InterpretPermission { id, .. }) => Some(id),
                _ => None,
            })
            .unwrap();

        let effects = controller.handle(Event::DecisionFinished {
            id: decision_id,
            result: DecisionResult::Permission(PermissionDecision::Yes),
        });
        let turn = effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartTurn(request) => Some(request),
                _ => None,
            })
            .expect("approval should start the existing camera-and-answer path");
        assert_eq!(turn.question, original_question);
        assert!(matches!(
            turn.visual,
            VisualRequest::ProposeCamera(selected) if selected == camera
        ));
        assert!(!controller.awaits_photo_permission());
        assert!(
            controller
                .handle(Event::DecisionFinished {
                    id: decision_id,
                    result: DecisionResult::Permission(PermissionDecision::Yes),
                })
                .is_empty()
        );
    }

    #[test]
    fn no_and_unclear_permission_results_never_start_a_camera_turn() {
        let mut controller = Controller::new();
        let camera = camera(1);
        ask_for_photo_permission(&mut controller, "first original question", camera.clone());
        let no_id = controller
            .handle(Event::VoiceTranscript(
                "I'd rather continue without another photo.".into(),
            ))
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartDecision(DecisionRequest::InterpretPermission { id, .. }) => Some(id),
                _ => None,
            })
            .unwrap();
        let no_effects = controller.handle(Event::DecisionFinished {
            id: no_id,
            result: DecisionResult::Permission(PermissionDecision::No),
        });
        let no_turn = no_effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartTurn(request) => Some(request),
                _ => None,
            })
            .expect("a refusal should continue with the original question");
        assert!(matches!(no_turn.visual, VisualRequest::UseAccepted(_)));
        controller.handle(Event::Input(Input::Stop));

        ask_for_photo_permission(&mut controller, "second original question", camera);
        let unclear_id = controller
            .handle(Event::VoiceTranscript(
                "Could you tell me more first?".into(),
            ))
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartDecision(DecisionRequest::InterpretPermission { id, .. }) => Some(id),
                _ => None,
            })
            .unwrap();
        let unclear_effects = controller.handle(Event::DecisionFinished {
            id: unclear_id,
            result: DecisionResult::Permission(PermissionDecision::Unclear),
        });
        assert!(matches!(
            unclear_effects.as_slice(),
            [Effect::Display(Output::PermissionClarification(_))]
        ));
        assert!(controller.awaits_photo_permission());
        assert!(
            !unclear_effects
                .iter()
                .any(|effect| matches!(effect, Effect::StartTurn(_)))
        );
    }

    #[test]
    fn failed_or_stale_permission_interpretation_cannot_activate_a_camera() {
        let mut controller = Controller::new();
        ask_for_photo_permission(&mut controller, "original question", camera(5));
        let decision_id = controller
            .handle(Event::VoiceTranscript("Please do it.".into()))
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartDecision(DecisionRequest::InterpretPermission { id, .. }) => Some(id),
                _ => None,
            })
            .unwrap();

        let failed = controller.handle(Event::DecisionFinished {
            id: decision_id,
            result: DecisionResult::Failed {
                message: "invalid JSON".into(),
            },
        });
        assert!(matches!(
            failed.as_slice(),
            [Effect::Display(Output::Diagnostic(message))] if message.contains("please reply again")
        ));
        assert!(controller.awaits_photo_permission());
        assert!(
            !failed
                .iter()
                .any(|effect| matches!(effect, Effect::StartTurn(_)))
        );

        controller.handle(Event::Input(Input::Stop));
        assert!(
            controller
                .handle(Event::DecisionFinished {
                    id: decision_id,
                    result: DecisionResult::Permission(PermissionDecision::Yes),
                })
                .is_empty()
        );
        assert!(!controller.has_active_turn());
    }
}
