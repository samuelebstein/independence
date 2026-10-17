//! Session state and output authorization.
//!
//! The controller is deliberately synchronous. It turns facts (`Event`) into
//! requested side effects (`Effect`) and owns the only accepted scene/history.
//! Worker tasks receive snapshots and can therefore never mutate this state.

use crate::{
    protocol::{
        Effect, Event, Input, Output, TurnFinished, TurnId, TurnRequest, TurnResult, VisualRequest,
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
    next_turn_id: u64,
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

impl Controller {
    pub fn new() -> Self {
        Self {
            next_turn_id: 1,
            ..Self::default()
        }
    }

    /// Reacts to one fact without performing I/O itself.
    pub fn handle(&mut self, event: Event) -> Vec<Effect> {
        match event {
            Event::Input(input) => self.handle_input(input),
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
            Input::Stop => self.cancel_active(),
            Input::Reset => {
                let mut effects = self.cancel_active();
                self.session = Session::default();
                effects.push(Effect::Display(Output::Diagnostic(
                    "session cleared; original files and terminal scrollback are unchanged".into(),
                )));
                effects
            }
            Input::Help => vec![Effect::Display(Output::Help)],
            Input::Quit | Input::EndOfInput | Input::Interrupt => {
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

    fn cancel_active(&mut self) -> Vec<Effect> {
        self.active
            .take()
            .map(|active| vec![Effect::CancelTurn(active.id)])
            .unwrap_or_default()
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
            Effect, Event, Input, Output, TurnFinished, TurnId, TurnRequest, TurnResult,
            VisualRequest,
        },
        see::Photo,
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
            PathBuf::from("old.png")
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
            PathBuf::from("old.png")
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
            VisualRequest::UseAccepted(Some(ref photo)) if photo.path() == PathBuf::from("tree.png")
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
}
