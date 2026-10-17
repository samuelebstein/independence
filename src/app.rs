//! Runtime glue for the four current capabilities.
//!
//! `listen` sends typed input events, the controller authorizes effects,
//! `see` and `think` run in a cancellable worker, and `speak` emits only
//! controller-approved output.

use std::{io, sync::Arc};

use thiserror::Error;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    controller::Controller,
    listen,
    protocol::{
        Effect, Event, Input, TurnFinished, TurnId, TurnRequest, TurnResult, VisualRequest,
    },
    see::Photo,
    speak,
    think::{OllamaClient, ThinkError},
};

const EVENT_CHANNEL_CAPACITY: usize = 32;

/// Runs the interactive terminal application until `/quit`, EOF, or Ctrl+C.
pub async fn run() -> Result<(), AppError> {
    let client = OllamaClient::from_environment()?;
    let (event_sender, mut event_receiver) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    listen::spawn_stdin_reader(event_sender.clone()).map_err(AppError::InputAdapter)?;

    let mut controller = Controller::new();
    let mut runtime = TurnRuntime::new(client, event_sender);
    speak::welcome();

    loop {
        let event = tokio::select! {
            _ = tokio::signal::ctrl_c() => Event::Input(Input::Interrupt),
            event = event_receiver.recv() => event.unwrap_or(Event::Input(Input::EndOfInput)),
        };

        if let Event::TurnFinished(finished) = &event {
            runtime.retire(finished.id).await;
        }

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

    runtime.shutdown().await;
    Ok(())
}

struct TurnRuntime {
    client: Arc<OllamaClient>,
    event_sender: mpsc::Sender<Event>,
    active: Option<RunningTurn>,
}

struct RunningTurn {
    id: TurnId,
    cancellation: CancellationToken,
    handle: JoinHandle<()>,
}

impl TurnRuntime {
    fn new(client: OllamaClient, event_sender: mpsc::Sender<Event>) -> Self {
        Self {
            client: Arc::new(client),
            event_sender,
            active: None,
        }
    }

    async fn apply(&mut self, effect: Effect) -> bool {
        match effect {
            Effect::StartTurn(request) => self.start(request).await,
            Effect::CancelTurn(id) => self.cancel(id).await,
            Effect::Display(output) => speak::emit(output),
            Effect::Exit => {
                self.shutdown().await;
                return false;
            }
        }

        true
    }

    async fn start(&mut self, request: TurnRequest) {
        // The controller always emits CancelTurn before StartTurn, but retain a
        // defensive cleanup here so this runtime never keeps two owned tasks.
        self.shutdown().await;

        let id = request.id;
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let client = self.client.clone();
        let event_sender = self.event_sender.clone();
        let handle = tokio::spawn(async move {
            let result = run_turn(client, request, worker_cancellation.clone()).await;

            // A cancelled worker normally sends nothing. If cancellation races
            // with completion, the controller's turn ID check remains the final
            // output authorization step.
            if !worker_cancellation.is_cancelled() && !matches!(result, TurnResult::Cancelled) {
                let _ = event_sender
                    .send(Event::TurnFinished(TurnFinished { id, result }))
                    .await;
            }
        });

        self.active = Some(RunningTurn {
            id,
            cancellation,
            handle,
        });
    }

    async fn cancel(&mut self, id: TurnId) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        if active.id != id {
            return;
        }

        let RunningTurn {
            cancellation,
            handle,
            ..
        } = self.active.take().expect("active turn was checked above");
        cancellation.cancel();
        handle.abort();
        let _ = handle.await;
    }

    async fn retire(&mut self, id: TurnId) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        if active.id != id {
            return;
        }

        let RunningTurn { handle, .. } = self.active.take().expect("active turn was checked above");
        let _ = handle.await;
    }

    async fn shutdown(&mut self) {
        let Some(RunningTurn {
            cancellation,
            handle,
            ..
        }) = self.active.take()
        else {
            return;
        };

        cancellation.cancel();
        handle.abort();
        let _ = handle.await;
    }
}

async fn run_turn(
    client: Arc<OllamaClient>,
    request: TurnRequest,
    cancellation: CancellationToken,
) -> TurnResult {
    let TurnRequest {
        question,
        history,
        visual,
        ..
    } = request;

    let (image, proposed_scene) = match visual {
        VisualRequest::UseAccepted(scene) => (scene, None),
        VisualRequest::ProposeNew(path) => {
            let mut load_task = tokio::task::spawn_blocking(move || Photo::load(path));
            let photo = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    load_task.abort();
                    return TurnResult::Cancelled;
                }
                result = &mut load_task => match result {
                    Ok(Ok(photo)) => photo,
                    Ok(Err(error)) => {
                        return TurnResult::Failed {
                            message: format!("could not use photo: {error}"),
                        };
                    }
                    Err(error) => {
                        return TurnResult::Failed {
                            message: format!("photo loading task ended unexpectedly: {error}"),
                        };
                    }
                },
            };

            if cancellation.is_cancelled() {
                return TurnResult::Cancelled;
            }

            (Some(photo.clone()), Some(photo))
        }
    };

    let answer = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return TurnResult::Cancelled,
        answer = client.answer(&history, &question, image.as_ref().map(Photo::bytes)) => answer,
    };

    if cancellation.is_cancelled() {
        return TurnResult::Cancelled;
    }

    match answer {
        Ok(answer) => TurnResult::Answer {
            answer,
            proposed_scene,
        },
        Err(error) => TurnResult::Failed {
            message: format!("local inference failed: {error}"),
        },
    }
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error("could not configure local inference: {0}")]
    Configuration(#[from] ThinkError),

    #[error("could not start terminal input: {0}")]
    InputAdapter(#[source] io::Error),
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        future::Future,
        io::Cursor,
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };

    use base64::{Engine, engine::general_purpose::STANDARD};
    use image::{DynamicImage, ImageFormat};
    use serde_json::json;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_json, method, path},
    };

    use crate::{
        controller::Controller,
        protocol::{
            Effect, Event, Input, TurnFinished, TurnId, TurnRequest, TurnResult, VisualRequest,
        },
        see::MAX_PHOTO_BYTES,
        think::{OllamaClient, OllamaConfig, TEACHER_INSTRUCTIONS},
    };

    use super::run_turn;

    fn client(server: &MockServer) -> Arc<OllamaClient> {
        Arc::new(
            OllamaClient::new(OllamaConfig::new(&server.uri(), "test-model").unwrap()).unwrap(),
        )
    }

    async fn within_test_timeout<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(3), future)
            .await
            .expect("worker test exceeded its three-second timeout")
    }

    fn request_for_photo(path: PathBuf) -> TurnRequest {
        TurnRequest {
            id: TurnId::new(1),
            question: "What is this?".into(),
            history: Vec::new(),
            visual: VisualRequest::ProposeNew(path),
        }
    }

    fn tiny_png() -> Vec<u8> {
        let image = DynamicImage::new_rgba8(1, 1);
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    fn start_request(effects: Vec<Effect>) -> TurnRequest {
        effects
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartTurn(request) => Some(request),
                _ => None,
            })
            .expect("controller should start a turn")
    }

    #[tokio::test]
    async fn rejected_photo_inputs_never_reach_ollama() {
        let server = MockServer::start().await;
        let client = client(&server);
        let directory = tempdir().unwrap();

        let missing = directory.path().join("missing.png");
        let unsupported = directory.path().join("notes.txt");
        fs::write(&unsupported, "not an image").unwrap();
        let oversized = directory.path().join("large.png");
        fs::write(&oversized, vec![0; MAX_PHOTO_BYTES + 1]).unwrap();

        for path in [missing, unsupported, oversized] {
            let result = within_test_timeout(run_turn(
                client.clone(),
                request_for_photo(path),
                CancellationToken::new(),
            ))
            .await;
            assert!(matches!(result, TurnResult::Failed { .. }));
        }

        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn accepted_photo_follow_up_reaches_ollama_with_history_and_same_image() {
        let server = MockServer::start().await;
        let directory = tempdir().unwrap();
        let photo_path = directory.path().join("tree image.png");
        let image_bytes = tiny_png();
        fs::write(&photo_path, &image_bytes).unwrap();
        let image_base64 = STANDARD.encode(&image_bytes);

        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(json!({
                "model": "test-model",
                "stream": false,
                "messages": [
                    {"role": "system", "content": TEACHER_INSTRUCTIONS},
                    {"role": "user", "content": "What might this tree be?", "images": [image_base64.clone()]}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "A first observation."},
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(json!({
                "model": "test-model",
                "stream": false,
                "messages": [
                    {"role": "system", "content": TEACHER_INSTRUCTIONS},
                    {"role": "user", "content": "What might this tree be?"},
                    {"role": "assistant", "content": "A first observation."},
                    {"role": "user", "content": "What should I observe next?", "images": [image_base64]}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "Notice the leaf arrangement."},
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;

        let client = client(&server);
        let mut controller = Controller::new();
        let first = start_request(controller.handle(Event::Input(Input::Look {
            path: photo_path,
            question: "What might this tree be?".into(),
        })));
        let first_id = first.id;
        let first_result =
            within_test_timeout(run_turn(client.clone(), first, CancellationToken::new())).await;
        controller.handle(Event::TurnFinished(TurnFinished {
            id: first_id,
            result: first_result,
        }));

        let follow_up = start_request(controller.handle(Event::Input(Input::Ask(
            "What should I observe next?".into(),
        ))));
        let follow_up_id = follow_up.id;
        let follow_up_result =
            within_test_timeout(run_turn(client, follow_up, CancellationToken::new())).await;
        let effects = controller.handle(Event::TurnFinished(TurnFinished {
            id: follow_up_id,
            result: follow_up_result,
        }));

        assert!(matches!(effects.as_slice(), [Effect::Display(_)]));
        assert_eq!(controller.accepted_history().len(), 2);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[test]
    fn tiny_fixture_is_a_png_for_the_integration_test() {
        let bytes = tiny_png();
        assert_eq!(image::guess_format(&bytes).unwrap(), ImageFormat::Png);
        assert!(Path::new("tree image.png").to_string_lossy().contains(' '));
    }
}
