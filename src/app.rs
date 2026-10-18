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
    dictation, listen,
    protocol::{
        CameraPreparation, Effect, Event, Input, TurnFinished, TurnId, TurnRequest, TurnResult,
        VisualRequest,
    },
    see::{
        Photo, PhotoSource,
        camera::{CameraRunError, CameraService, select_camera},
    },
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
    let mut runtime = TurnRuntime::new(client, CameraService::v4l(), event_sender);
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

/// Runs standalone local dictation without constructing Ollama or camera
/// services. Its transcript deliberately stops at terminal display.
pub async fn run_dictation() -> Result<(), AppError> {
    dictation::run().await.map_err(AppError::Dictation)
}

struct TurnRuntime {
    client: Arc<OllamaClient>,
    camera: CameraService,
    event_sender: mpsc::Sender<Event>,
    active: Option<RunningTurn>,
}

struct RunningTurn {
    id: TurnId,
    cancellation: CancellationToken,
    handle: JoinHandle<()>,
}

impl TurnRuntime {
    fn new(client: OllamaClient, camera: CameraService, event_sender: mpsc::Sender<Event>) -> Self {
        Self {
            client: Arc::new(client),
            camera,
            event_sender,
            active: None,
        }
    }

    async fn apply(&mut self, effect: Effect) -> bool {
        match effect {
            Effect::StartTurn(request) => self.start(request).await,
            Effect::PrepareCamera(preparation) => self.prepare_camera(preparation),
            Effect::ListCameras => self.list_cameras(),
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
        // Unlike application shutdown, this does not wait for a previously
        // cancelled camera's bounded cleanup: a new text turn must stay
        // responsive while that closure releases its gate.
        self.stop_active_turn().await;

        let id = request.id;
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let client = self.client.clone();
        let camera = self.camera.clone();
        let event_sender = self.event_sender.clone();
        let handle = tokio::spawn(async move {
            let result = run_turn(
                client,
                camera,
                event_sender.clone(),
                request,
                worker_cancellation.clone(),
            )
            .await;

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

    fn list_cameras(&self) {
        let camera = self.camera.clone();
        let event_sender = self.event_sender.clone();
        tokio::spawn(async move {
            let result = camera.list().await.map_err(|error| error.to_string());
            let _ = event_sender.send(Event::CamerasListed(result)).await;
        });
    }

    fn prepare_camera(&self, preparation: CameraPreparation) {
        let camera = self.camera.clone();
        let event_sender = self.event_sender.clone();
        tokio::spawn(async move {
            let result = camera
                .list()
                .await
                .and_then(|cameras| select_camera(&cameras, preparation.requested))
                .map_err(|error| error.to_string());
            let _ = event_sender
                .send(Event::CameraPrepared {
                    id: preparation.id,
                    result,
                })
                .await;
        });
    }

    async fn cancel(&mut self, id: TurnId) {
        if self.active.as_ref().map(|active| active.id) != Some(id) {
            return;
        }

        self.stop_active_turn().await;
    }

    async fn stop_active_turn(&mut self) {
        let RunningTurn {
            cancellation,
            handle,
            ..
        } = match self.active.take() {
            Some(active) => active,
            None => return,
        };
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
        self.stop_active_turn().await;
        // A capture cancelled earlier may already be detached from its outer
        // async task. It still owns this gate until V4L2 has released its
        // buffers and device handle, so wait before process shutdown.
        self.camera.wait_for_idle().await;
    }
}

async fn run_turn(
    client: Arc<OllamaClient>,
    camera_service: CameraService,
    event_sender: mpsc::Sender<Event>,
    request: TurnRequest,
    cancellation: CancellationToken,
) -> TurnResult {
    let TurnRequest {
        id,
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
        VisualRequest::ProposeCamera(camera) => {
            if !send_camera_event(
                &event_sender,
                &cancellation,
                Event::CameraCaptureStarted {
                    id,
                    camera: camera.clone(),
                },
            )
            .await
            {
                return TurnResult::Cancelled;
            }

            let photo = match camera_service.capture(camera, cancellation.clone()).await {
                Ok(photo) => photo,
                Err(CameraRunError::Cancelled) => return TurnResult::Cancelled,
                Err(CameraRunError::Failed(error)) => {
                    return TurnResult::Failed {
                        message: format!("could not capture camera: {error}"),
                    };
                }
            };

            if cancellation.is_cancelled() {
                return TurnResult::Cancelled;
            }

            let PhotoSource::Camera(source) = photo.source() else {
                return TurnResult::Failed {
                    message: "camera worker returned a non-camera photo; no answer was accepted"
                        .into(),
                };
            };
            if !send_camera_event(
                &event_sender,
                &cancellation,
                Event::CameraCaptureReady {
                    id,
                    source: source.clone(),
                },
            )
            .await
            {
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

async fn send_camera_event(
    event_sender: &mpsc::Sender<Event>,
    cancellation: &CancellationToken,
    event: Event,
) -> bool {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => false,
        result = event_sender.send(event) => result.is_ok(),
    }
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error("could not configure local inference: {0}")]
    Configuration(#[from] ThinkError),

    #[error("could not start terminal input: {0}")]
    InputAdapter(#[source] io::Error),

    #[error("could not run local dictation: {0}")]
    Dictation(#[from] dictation::DictationError),
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        future::Future,
        io::Cursor,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use base64::{Engine, engine::general_purpose::STANDARD};
    use image::{DynamicImage, ImageFormat};
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::sync::mpsc;
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
        see::{
            MAX_PHOTO_BYTES, Photo, PhotoSource,
            camera::{
                CameraBackend, CameraDescriptor, CameraError, CameraId, CameraPhotoSource,
                CameraService,
            },
        },
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

    fn start_camera_request(
        controller: &mut Controller,
        camera: CameraDescriptor,
        question: &str,
    ) -> TurnRequest {
        let preparation = controller
            .handle(Event::Input(Input::Camera {
                requested: Some(camera.id()),
                question: question.into(),
            }))
            .into_iter()
            .find_map(|effect| match effect {
                Effect::PrepareCamera(preparation) => Some(preparation),
                _ => None,
            })
            .expect("camera command should request a preflight");
        controller
            .handle(Event::CameraPrepared {
                id: preparation.id,
                result: Ok(camera),
            })
            .into_iter()
            .find_map(|effect| match effect {
                Effect::StartTurn(request) => Some(request),
                _ => None,
            })
            .expect("validated camera should start a turn")
    }

    fn unused_camera_service() -> CameraService {
        // These file-photo tests never invoke camera capture. Constructing the
        // backend is inert and does not enumerate or open hardware.
        CameraService::v4l()
    }

    fn unused_event_sender() -> mpsc::Sender<Event> {
        let (sender, _receiver) = mpsc::channel(4);
        sender
    }

    fn tiny_png() -> Vec<u8> {
        let image = DynamicImage::new_rgba8(1, 1);
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    fn tiny_jpeg() -> Vec<u8> {
        let image = DynamicImage::new_rgb8(1, 1);
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Jpeg).unwrap();
        bytes.into_inner()
    }

    #[derive(Clone)]
    struct FakeCameraBackend {
        camera: CameraDescriptor,
        photo: Photo,
        fail: bool,
        captures: Arc<AtomicUsize>,
    }

    impl CameraBackend for FakeCameraBackend {
        fn list(&self) -> Result<Vec<CameraDescriptor>, CameraError> {
            Ok(vec![self.camera.clone()])
        }

        fn capture(&self, _camera: &CameraDescriptor) -> Result<Photo, CameraError> {
            self.captures.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(CameraError::NoCameras)
            } else {
                Ok(self.photo.clone())
            }
        }
    }

    fn fake_camera() -> (CameraDescriptor, Photo) {
        let camera = CameraDescriptor::new(CameraId::new(7), "Mock Camera");
        let photo = Photo::from_camera_jpeg(
            CameraPhotoSource::new(camera.clone(), 1, 1, "MJPEG"),
            tiny_jpeg(),
        )
        .unwrap();
        (camera, photo)
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
                unused_camera_service(),
                unused_event_sender(),
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
        let first_result = within_test_timeout(run_turn(
            client.clone(),
            unused_camera_service(),
            unused_event_sender(),
            first,
            CancellationToken::new(),
        ))
        .await;
        controller.handle(Event::TurnFinished(TurnFinished {
            id: first_id,
            result: first_result,
        }));

        let follow_up = start_request(controller.handle(Event::Input(Input::Ask(
            "What should I observe next?".into(),
        ))));
        let follow_up_id = follow_up.id;
        let follow_up_result = within_test_timeout(run_turn(
            client,
            unused_camera_service(),
            unused_event_sender(),
            follow_up,
            CancellationToken::new(),
        ))
        .await;
        let effects = controller.handle(Event::TurnFinished(TurnFinished {
            id: follow_up_id,
            result: follow_up_result,
        }));

        assert!(matches!(effects.as_slice(), [Effect::Display(_)]));
        assert_eq!(controller.accepted_history().len(), 2);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn camera_capture_failure_never_reaches_ollama() {
        let server = MockServer::start().await;
        let (camera, photo) = fake_camera();
        let captures = Arc::new(AtomicUsize::new(0));
        let service = CameraService::with_backend(Arc::new(FakeCameraBackend {
            camera: camera.clone(),
            photo,
            fail: true,
            captures: captures.clone(),
        }));
        let (event_sender, _event_receiver) = mpsc::channel(4);
        let result = within_test_timeout(run_turn(
            client(&server),
            service,
            event_sender,
            TurnRequest {
                id: TurnId::new(1),
                question: "What is this?".into(),
                history: Vec::new(),
                visual: VisualRequest::ProposeCamera(camera),
            },
            CancellationToken::new(),
        ))
        .await;

        assert!(
            matches!(result, TurnResult::Failed { message } if message.contains("could not capture camera"))
        );
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn camera_image_and_question_reach_ollama_and_follow_up_keeps_the_scene() {
        let server = MockServer::start().await;
        let (camera, photo) = fake_camera();
        let image_base64 = STANDARD.encode(photo.bytes());

        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(json!({
                "model": "test-model",
                "stream": false,
                "messages": [
                    {"role": "system", "content": TEACHER_INSTRUCTIONS},
                    {"role": "user", "content": "What is visible?", "images": [image_base64.clone()]}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "A visible observation."},
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
                    {"role": "user", "content": "What is visible?"},
                    {"role": "assistant", "content": "A visible observation."},
                    {"role": "user", "content": "What should I inspect next?", "images": [image_base64]}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"role": "assistant", "content": "Inspect the bark texture."},
                "done": true,
                "done_reason": "stop"
            })))
            .mount(&server)
            .await;

        let captures = Arc::new(AtomicUsize::new(0));
        let service = CameraService::with_backend(Arc::new(FakeCameraBackend {
            camera: camera.clone(),
            photo,
            fail: false,
            captures: captures.clone(),
        }));
        let (event_sender, mut event_receiver) = mpsc::channel(4);
        let mut controller = Controller::new();
        let first = start_camera_request(&mut controller, camera.clone(), "What is visible?");
        let first_id = first.id;
        let first_result = within_test_timeout(run_turn(
            client(&server),
            service.clone(),
            event_sender.clone(),
            first,
            CancellationToken::new(),
        ))
        .await;

        assert!(matches!(
            within_test_timeout(event_receiver.recv()).await,
            Some(Event::CameraCaptureStarted { camera: started, .. }) if started == camera
        ));
        assert!(matches!(
            within_test_timeout(event_receiver.recv()).await,
            Some(Event::CameraCaptureReady { source, .. })
                if source.camera() == &camera
                    && source.width() == 1
                    && source.height() == 1
        ));
        controller.handle(Event::TurnFinished(TurnFinished {
            id: first_id,
            result: first_result,
        }));
        assert!(matches!(
            controller.accepted_scene().unwrap().source(),
            PhotoSource::Camera(source) if source.camera() == &camera
        ));

        let follow_up = start_request(controller.handle(Event::Input(Input::Ask(
            "What should I inspect next?".into(),
        ))));
        let follow_up_id = follow_up.id;
        let follow_up_result = within_test_timeout(run_turn(
            client(&server),
            service,
            event_sender,
            follow_up,
            CancellationToken::new(),
        ))
        .await;
        controller.handle(Event::TurnFinished(TurnFinished {
            id: follow_up_id,
            result: follow_up_result,
        }));

        assert_eq!(captures.load(Ordering::SeqCst), 1);
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
