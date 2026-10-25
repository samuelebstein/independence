//! Ordered playback for direct audio-to-audio responses.
//!
//! This deliberately reuses [`RodioPlayback`](super::playback::RodioPlayback)
//! without constructing Piper. The text-to-speech worker remains separate:
//! it owns synthesis plus playback, while this worker owns only already
//! generated WAV bytes returned by an audio-capable backend.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use tokio::sync::mpsc as tokio_mpsc;
use tracing::{info, warn};

use crate::{
    protocol::TurnId,
    telemetry::{self, Interaction},
};

use super::playback::{
    OutputSelection, Playback, PlaybackError, PlaybackRequest, PlaybackResult, RodioPlayback,
};

const DIRECT_PLAYBACK_QUEUE_CAPACITY: usize = 1;

/// Process-local identity for one controller-authorized direct-audio result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DirectPlaybackId(u64);

impl DirectPlaybackId {
    pub fn value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectPlaybackOutcome {
    Played,
    Cancelled,
    Failed,
}

/// A direct playback lifecycle fact. It cannot authorize a result or mutate
/// conversation state—the controller already made that decision before bytes
/// were enqueued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectPlaybackEvent {
    pub id: DirectPlaybackId,
    pub turn_id: TurnId,
    pub outcome: DirectPlaybackOutcome,
}

#[derive(Clone)]
struct DirectRequest {
    id: DirectPlaybackId,
    turn_id: TurnId,
    generation: u64,
    wav: Vec<u8>,
    interaction: Option<Interaction>,
    enqueued_at: Instant,
}

enum Command {
    Play(DirectRequest),
}

/// Non-blocking control for the direct WAV worker.
#[derive(Clone)]
pub struct DirectPlaybackHandle {
    sender: SyncSender<Command>,
    generation: Arc<AtomicU64>,
    next_id: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
}

impl DirectPlaybackHandle {
    /// Queues one current WAV. A full queue is a visible failure rather than
    /// unbounded stored audio; normal audio-to-audio mode has at most one
    /// authorized turn, so this is defensive rather than a throughput queue.
    pub fn play(
        &self,
        turn_id: TurnId,
        wav: Vec<u8>,
        interaction: Option<Interaction>,
    ) -> Option<DirectPlaybackId> {
        if self.shutdown.load(Ordering::Acquire) {
            warn!(
                target: "speech",
                event = "direct_audio_playback_shutting_down",
                turn_id = turn_id.value(),
                "direct audio was not queued"
            );
            return None;
        }
        let id = DirectPlaybackId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let request = DirectRequest {
            id,
            turn_id,
            generation: self.generation.load(Ordering::Acquire),
            wav,
            interaction,
            enqueued_at: Instant::now(),
        };
        match self.sender.try_send(Command::Play(request)) {
            Ok(()) => Some(id),
            Err(TrySendError::Full(_)) => {
                warn!(
                    target: "speech",
                    event = "direct_audio_playback_queue_full",
                    turn_id = turn_id.value(),
                    "direct audio was not queued"
                );
                None
            }
            Err(TrySendError::Disconnected(_)) => {
                warn!(
                    target: "speech",
                    event = "direct_audio_playback_unavailable",
                    turn_id = turn_id.value(),
                    "direct audio worker is unavailable"
                );
                None
            }
        }
    }

    /// Makes queued or currently playing earlier audio obsolete. The Rodio
    /// adapter polls this generation while its player is active.
    pub fn cancel_all(&self, reason: &'static str) {
        let generation = self
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        info!(
            target: "speech",
            event = "direct_audio_playback_cancellation_requested",
            generation,
            reason,
            "direct audio playback"
        );
    }
}

/// Owns the one WAV-only playback worker used by the direct-audio experiment.
pub struct DirectPlaybackService {
    handle: DirectPlaybackHandle,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl DirectPlaybackService {
    pub fn with_configuration(
        selection: OutputSelection,
        event_sender: tokio_mpsc::Sender<DirectPlaybackEvent>,
    ) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(DIRECT_PLAYBACK_QUEUE_CAPACITY);
        let generation = Arc::new(AtomicU64::new(1));
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = DirectPlaybackHandle {
            sender,
            generation: generation.clone(),
            next_id: Arc::new(AtomicU64::new(1)),
            shutdown: shutdown.clone(),
        };
        let worker_shutdown = shutdown.clone();
        let worker = thread::Builder::new()
            .name("independence-direct-audio-playback".to_owned())
            .spawn(move || {
                let playback = RodioPlayback::open(&selection);
                match &playback {
                    Ok(player) => info!(
                        target: "speech",
                        event = "direct_audio_output_selected",
                        device = ?player.device_name(),
                        device_id = %player.device_id(),
                        "direct audio playback"
                    ),
                    Err(error) => warn!(
                        target: "speech",
                        event = "direct_audio_output_unavailable",
                        error = %error,
                        "direct audio responses will be reported but cannot play"
                    ),
                }
                worker_loop(
                    receiver,
                    event_sender,
                    playback,
                    generation,
                    worker_shutdown,
                );
            })?;
        Ok(Self {
            handle,
            shutdown,
            worker: Some(worker),
        })
    }

    pub fn handle(&self) -> DirectPlaybackHandle {
        self.handle.clone()
    }

    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.handle.cancel_all("direct_audio_shutdown");
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    #[cfg(test)]
    pub(crate) fn test_parts(
        event_sender: tokio_mpsc::Sender<DirectPlaybackEvent>,
        playback: impl Playback + Send + 'static,
    ) -> Self {
        let (sender, receiver) = mpsc::sync_channel(DIRECT_PLAYBACK_QUEUE_CAPACITY);
        let generation = Arc::new(AtomicU64::new(1));
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = DirectPlaybackHandle {
            sender,
            generation: generation.clone(),
            next_id: Arc::new(AtomicU64::new(1)),
            shutdown: shutdown.clone(),
        };
        let worker_shutdown = shutdown.clone();
        let worker = thread::spawn(move || {
            worker_loop(
                receiver,
                event_sender,
                Ok(playback),
                generation,
                worker_shutdown,
            )
        });
        Self {
            handle,
            shutdown,
            worker: Some(worker),
        }
    }
}

impl Drop for DirectPlaybackService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker_loop<P>(
    receiver: Receiver<Command>,
    event_sender: tokio_mpsc::Sender<DirectPlaybackEvent>,
    mut playback: Result<P, PlaybackError>,
    generation: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
) where
    P: Playback,
{
    while !shutdown.load(Ordering::Acquire) {
        let command = match receiver.recv_timeout(Duration::from_millis(20)) {
            Ok(command) => command,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let Command::Play(request) = command;
        let outcome = if request.generation != generation.load(Ordering::Acquire) {
            DirectPlaybackOutcome::Cancelled
        } else {
            play_request(&mut playback, &request, generation.as_ref())
        };
        finish(&event_sender, request, outcome);
    }
}

fn play_request<P>(
    playback: &mut Result<P, PlaybackError>,
    request: &DirectRequest,
    generation: &AtomicU64,
) -> DirectPlaybackOutcome
where
    P: Playback,
{
    if let Some(interaction) = &request.interaction {
        interaction.queue_wait(
            "audio_to_audio.playback",
            request.enqueued_at.elapsed(),
            0,
            1,
        );
        interaction.mark("audio_to_audio.playback_started");
    }
    info!(
        target: "speech",
        event = "direct_audio_playback_started",
        playback_id = request.id.value(),
        turn_id = request.turn_id.value(),
        bytes = request.wav.len(),
        "direct audio playback"
    );
    let stage = request
        .interaction
        .as_ref()
        .map(|interaction| interaction.stage("audio_to_audio.playback", telemetry::Fields::new()));
    let result = match playback {
        Ok(playback) => playback.play(
            PlaybackRequest {
                utterance_id: request.id.value(),
                generation: request.generation,
                wav: request.wav.clone(),
            },
            generation,
        ),
        Err(_) => Err(PlaybackError::NoDefaultDevice),
    };
    match result {
        Ok(PlaybackResult::Completed) => {
            if let Some(stage) = stage {
                stage.finish("completed", telemetry::Fields::new());
            }
            DirectPlaybackOutcome::Played
        }
        Ok(PlaybackResult::Cancelled) => {
            if let Some(stage) = stage {
                stage.finish("cancelled", telemetry::Fields::new());
            }
            DirectPlaybackOutcome::Cancelled
        }
        Err(error) => {
            if let Some(stage) = stage {
                stage.finish(
                    "failed",
                    telemetry::fields([(
                        "failure_class",
                        serde_json::json!("output_device_failure"),
                    )]),
                );
            }
            warn!(
                target: "speech",
                event = "direct_audio_playback_failed",
                playback_id = request.id.value(),
                turn_id = request.turn_id.value(),
                error = %error,
                "direct audio playback"
            );
            DirectPlaybackOutcome::Failed
        }
    }
}

fn finish(
    event_sender: &tokio_mpsc::Sender<DirectPlaybackEvent>,
    request: DirectRequest,
    outcome: DirectPlaybackOutcome,
) {
    info!(
        target: "speech",
        event = "direct_audio_playback_completed",
        playback_id = request.id.value(),
        turn_id = request.turn_id.value(),
        outcome = ?outcome,
        "direct audio playback"
    );
    if let Some(interaction) = &request.interaction {
        interaction.event(
            "audio_to_audio.playback",
            match outcome {
                DirectPlaybackOutcome::Played => "completed",
                DirectPlaybackOutcome::Cancelled => "cancelled",
                DirectPlaybackOutcome::Failed => "failed",
            },
            telemetry::fields([("playback_id", serde_json::json!(request.id.value()))]),
        );
    }
    let _ = event_sender.blocking_send(DirectPlaybackEvent {
        id: request.id,
        turn_id: request.turn_id,
        outcome,
    });
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
            mpsc as std_mpsc,
        },
        time::{Duration, Instant},
    };

    use tokio::sync::mpsc;

    use super::{
        super::playback::{Playback, PlaybackError, PlaybackRequest, PlaybackResult},
        DirectPlaybackOutcome, DirectPlaybackService,
    };
    use crate::protocol::TurnId;

    #[derive(Clone)]
    struct FakePlayback {
        seen: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl Playback for FakePlayback {
        fn play(
            &mut self,
            request: PlaybackRequest,
            _generation: &AtomicU64,
        ) -> Result<PlaybackResult, PlaybackError> {
            self.seen.lock().unwrap().push(request.wav);
            Ok(PlaybackResult::Completed)
        }
    }

    struct BlockingPlayback {
        started: std_mpsc::Sender<()>,
    }

    impl Playback for BlockingPlayback {
        fn play(
            &mut self,
            request: PlaybackRequest,
            current_generation: &AtomicU64,
        ) -> Result<PlaybackResult, PlaybackError> {
            let _ = self.started.send(());
            while current_generation.load(Ordering::Acquire) == request.generation {
                std::thread::yield_now();
            }
            Ok(PlaybackResult::Cancelled)
        }
    }

    #[tokio::test]
    async fn direct_worker_plays_authorized_wav_without_synthesis() {
        let (sender, mut receiver) = mpsc::channel(2);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut service =
            DirectPlaybackService::test_parts(sender, FakePlayback { seen: seen.clone() });
        let id = service
            .handle()
            .play(TurnId::new(7), vec![1, 2, 3], None)
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.id, id);
        assert_eq!(event.outcome, DirectPlaybackOutcome::Played);
        assert_eq!(*seen.lock().unwrap(), vec![vec![1, 2, 3]]);
        service.shutdown();
    }

    #[test]
    fn shutdown_releases_a_worker_even_when_the_bounded_play_queue_is_full() {
        let (event_sender, _event_receiver) = mpsc::channel(4);
        let (started_sender, started_receiver) = std_mpsc::channel();
        let mut service = DirectPlaybackService::test_parts(
            event_sender,
            BlockingPlayback {
                started: started_sender,
            },
        );
        let handle = service.handle();
        assert!(handle.play(TurnId::new(1), vec![1], None).is_some());
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("first playback should have started");
        // The worker is busy with the first request, so this fills the one
        // bounded queue slot. Shutdown must not need to enqueue a command
        // behind it before it can join the worker.
        assert!(handle.play(TurnId::new(2), vec![2], None).is_some());

        let started = Instant::now();
        service.shutdown();
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
