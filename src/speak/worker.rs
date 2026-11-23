//! Ordered, cancellable local speech work.
//!
//! A single worker owns synthesis and playback so completed utterances cannot
//! overlap or overtake each other. The controller-facing handle only accepts
//! small text requests; it never contains audio bytes or blocks the event loop.

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

use super::{
    playback::{
        OutputSelection, Playback, PlaybackError, PlaybackRequest, PlaybackResult, RodioPlayback,
    },
    tts::{PiperCli, PiperConfig, SynthesisRequest, SynthesisResult, Synthesizer},
};

const PLAYBACK_QUEUE_CAPACITY: usize = 8;
/// How often the synthesis helper checks whether playback has finished while
/// it waits for the next utterance.
const PREFETCH_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The application state that authorized an utterance. A model answer carries
/// its existing `TurnId`; a fixed error notice is owned by the system. Either
/// becomes obsolete when the speech generation changes. Status changes such as
/// listening or thinking are never spoken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeechOwner {
    Turn(TurnId),
    System,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UtteranceId(u64);

impl UtteranceId {
    pub fn value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeechOutcome {
    Played,
    Cancelled,
    SynthesisFailed,
    PlaybackFailed,
}

/// A completion reaches the voice runtime only for lifecycle coordination—for
/// example, starting microphone capture after the listening cue. Worker logs
/// remain the source of detailed synthesis/playback diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpeechEvent {
    pub utterance_id: UtteranceId,
    pub owner: SpeechOwner,
    pub outcome: SpeechOutcome,
}

#[derive(Clone)]
struct SpeechRequest {
    utterance_id: UtteranceId,
    owner: SpeechOwner,
    generation: u64,
    text: String,
    interaction: Option<Interaction>,
    enqueued_at: Instant,
    pending_depth_before: usize,
}

enum SpeechCommand {
    Speak(SpeechRequest),
    Shutdown,
}

/// The two queue counters belong together because the worker updates them as
/// one lifecycle: pending work becomes active work. Passing them as one value
/// makes that ownership explicit without changing the queue itself.
#[derive(Clone)]
struct WorkerQueueMetrics {
    pending_depth: Arc<AtomicU64>,
    active_work: Arc<AtomicU64>,
}

/// Non-blocking application-facing speech control.
#[derive(Clone)]
pub struct SpeechHandle {
    sender: SyncSender<SpeechCommand>,
    generation: Arc<AtomicU64>,
    next_utterance_id: Arc<AtomicU64>,
    pending_depth: Arc<AtomicU64>,
    active_work: Arc<AtomicU64>,
}

impl SpeechHandle {
    /// How many utterances have ever been requested through this handle.
    /// Tests use it to prove that status changes never reach Piper.
    #[cfg(test)]
    pub(crate) fn utterances_requested(&self) -> u64 {
        self.next_utterance_id.load(Ordering::Relaxed) - 1
    }

    /// Queues exactly one authorized utterance. The bounded queue protects the
    /// controller when Piper or an audio device is slow.
    pub fn say(&self, owner: SpeechOwner, text: impl Into<String>) -> Option<UtteranceId> {
        self.say_with_interaction(owner, text, None)
    }

    /// Queues an authorized utterance with its privacy-preserving interaction
    /// correlation. The text itself never enters telemetry attributes.
    pub fn say_with_interaction(
        &self,
        owner: SpeechOwner,
        text: impl Into<String>,
        interaction: Option<Interaction>,
    ) -> Option<UtteranceId> {
        let utterance_id = UtteranceId(self.next_utterance_id.fetch_add(1, Ordering::Relaxed));
        let generation = self.generation.load(Ordering::Acquire);
        let pending_depth_before = self.pending_depth.fetch_add(1, Ordering::AcqRel) as usize;
        let request = SpeechRequest {
            utterance_id,
            owner,
            generation,
            text: text.into(),
            interaction: interaction.clone(),
            enqueued_at: Instant::now(),
            pending_depth_before,
        };

        match self.sender.try_send(SpeechCommand::Speak(request)) {
            Ok(()) => {
                if let Some(interaction) = interaction {
                    interaction.event(
                        "speech.queue",
                        "enqueued",
                        telemetry::fields([
                            ("utterance_id", serde_json::json!(utterance_id.value())),
                            ("pending_depth", serde_json::json!(pending_depth_before + 1)),
                            ("owner", serde_json::json!(speech_owner_label(owner))),
                        ]),
                    );
                    interaction.queue_state(
                        "speech",
                        pending_depth_before + 1,
                        self.active_work.load(Ordering::Acquire) as usize,
                    );
                }
                info!(
                    target: "speech",
                    event = "queued",
                    utterance_id = utterance_id.value(),
                    generation,
                    ?owner,
                    "speech"
                );
                Some(utterance_id)
            }
            Err(TrySendError::Full(_)) => {
                self.pending_depth.fetch_sub(1, Ordering::AcqRel);
                if let Some(interaction) = interaction {
                    interaction.event("speech.queue", "full", telemetry::Fields::new());
                    interaction.queue_state(
                        "speech",
                        pending_depth_before,
                        self.active_work.load(Ordering::Acquire) as usize,
                    );
                }
                warn!(
                    target: "speech",
                    event = "queue_full",
                    capacity = PLAYBACK_QUEUE_CAPACITY,
                    ?owner,
                    "speech was not queued; terminal dialogue remains available"
                );
                None
            }
            Err(TrySendError::Disconnected(_)) => {
                self.pending_depth.fetch_sub(1, Ordering::AcqRel);
                if let Some(interaction) = interaction {
                    interaction.event("speech.queue", "unavailable", telemetry::Fields::new());
                    interaction.queue_state(
                        "speech",
                        pending_depth_before,
                        self.active_work.load(Ordering::Acquire) as usize,
                    );
                }
                warn!(
                    target: "speech",
                    event = "worker_unavailable",
                    ?owner,
                    "speech was not queued; terminal dialogue remains available"
                );
                None
            }
        }
    }

    /// Makes every earlier synthesis and playback request obsolete immediately.
    /// The worker observes the generation while waiting for Piper and while
    /// polling a Rodio sink, then discards obsolete queued work in order.
    pub fn cancel_all(&self, reason: &'static str) {
        let generation = self
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        info!(
            target: "speech",
            event = "cancellation_requested",
            generation,
            reason,
            "speech"
        );
    }
}

/// Owns the dedicated speech worker and joins it during voice-mode shutdown.
pub struct SpeechService {
    handle: SpeechHandle,
    worker: Option<JoinHandle<()>>,
}

impl SpeechService {
    pub fn from_environment(event_sender: tokio_mpsc::Sender<SpeechEvent>) -> io::Result<Self> {
        let piper = PiperConfig::from_environment();
        let selection = OutputSelection::from_environment();
        Self::start(piper, selection, event_sender)
    }

    /// Uses an explicit command-line device selection when present; otherwise
    /// it preserves the documented environment/default-device behavior.
    pub fn with_output_id(
        output_id: Option<String>,
        event_sender: tokio_mpsc::Sender<SpeechEvent>,
    ) -> io::Result<Self> {
        let piper = PiperConfig::from_environment();
        let selection = match output_id {
            Some(id) => OutputSelection::from_requested_id(Some(id)),
            None => OutputSelection::from_environment(),
        };
        Self::start(piper, selection, event_sender)
    }

    /// Starts speech with already-resolved local configuration.  This keeps
    /// profile defaults and command-line overrides outside the worker, whose
    /// only job is ordered synthesis and playback.
    pub(crate) fn with_configuration(
        piper: PiperConfig,
        selection: OutputSelection,
        event_sender: tokio_mpsc::Sender<SpeechEvent>,
    ) -> io::Result<Self> {
        Self::start(piper, selection, event_sender)
    }

    fn start(
        piper: PiperConfig,
        selection: OutputSelection,
        event_sender: tokio_mpsc::Sender<SpeechEvent>,
    ) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(PLAYBACK_QUEUE_CAPACITY);
        let generation = Arc::new(AtomicU64::new(1));
        let handle = SpeechHandle {
            sender,
            generation: generation.clone(),
            next_utterance_id: Arc::new(AtomicU64::new(1)),
            pending_depth: Arc::new(AtomicU64::new(0)),
            active_work: Arc::new(AtomicU64::new(0)),
        };
        let worker_metrics = WorkerQueueMetrics {
            pending_depth: handle.pending_depth.clone(),
            active_work: handle.active_work.clone(),
        };
        let worker = thread::Builder::new()
            .name("independence-speech".to_owned())
            .spawn(move || {
                let synthesizer = PiperCli::new(piper);
                let playback = match RodioPlayback::open(&selection) {
                    Ok(playback) => {
                        telemetry::run_event(
                            "speech.output.initialize",
                            "ready",
                            telemetry::Fields::new(),
                        );
                        info!(
                            target: "speech",
                            event = "output_selected",
                            device = ?playback.device_name(),
                            device_id = %playback.device_id(),
                            "speech"
                        );
                        SystemPlayback::Ready(playback)
                    }
                    Err(_error) => {
                        telemetry::run_event(
                            "speech.output.initialize",
                            "unavailable",
                            telemetry::Fields::new(),
                        );
                        warn!(
                            target: "speech",
                            event = "output_unavailable",
                            failure_class = "output_device_unavailable",
                            "terminal dialogue will continue without audio"
                        );
                        SystemPlayback::Unavailable(_error)
                    }
                };
                worker_loop_with_metrics(
                    receiver,
                    event_sender,
                    synthesizer,
                    playback,
                    generation,
                    worker_metrics,
                );
            })?;

        Ok(Self {
            handle,
            worker: Some(worker),
        })
    }

    pub fn handle(&self) -> SpeechHandle {
        self.handle.clone()
    }

    /// A deterministic queue-only worker for higher-level application tests.
    /// It deliberately produces no synthesis or playback events: tests inject
    /// the precise completion they need, without a Piper model or an audio
    /// device. Production code has no path to this constructor.
    #[cfg(test)]
    pub(crate) fn test_stub() -> Self {
        let (sender, receiver) = mpsc::sync_channel(PLAYBACK_QUEUE_CAPACITY);
        let generation = Arc::new(AtomicU64::new(1));
        let handle = SpeechHandle {
            sender,
            generation,
            next_utterance_id: Arc::new(AtomicU64::new(1)),
            pending_depth: Arc::new(AtomicU64::new(0)),
            active_work: Arc::new(AtomicU64::new(0)),
        };
        let worker = thread::spawn(move || {
            while let Ok(command) = receiver.recv() {
                if matches!(command, SpeechCommand::Shutdown) {
                    break;
                }
            }
        });
        Self {
            handle,
            worker: Some(worker),
        }
    }

    /// Cancels active local process/playback, wakes the worker, and waits for
    /// app-owned cleanup. Piper cancellation means the child is killed by the
    /// worker; it does not make claims about an unrelated process.
    pub fn shutdown(&mut self) {
        if self.worker.is_none() {
            return;
        }
        self.handle.cancel_all("shutdown");
        // A full queue can only contain stale work after `cancel_all`; the
        // worker observes that generation and frees a slot before this send
        // returns. Joining is acceptable at process shutdown, not on the
        // controller event loop during normal interaction.
        let _ = self.handle.sender.send(SpeechCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                warn!(target: "speech", event = "worker_panicked", "speech worker");
            }
        }
    }
}

impl Drop for SpeechService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

enum SystemPlayback {
    Ready(RodioPlayback),
    Unavailable(PlaybackError),
}

impl Playback for SystemPlayback {
    fn play(
        &mut self,
        request: PlaybackRequest,
        current_generation: &AtomicU64,
    ) -> Result<PlaybackResult, PlaybackError> {
        match self {
            Self::Ready(playback) => playback.play(request, current_generation),
            Self::Unavailable(error) => Err(error.clone()),
        }
    }
}

#[cfg(test)]
fn worker_loop<S, P>(
    receiver: Receiver<SpeechCommand>,
    event_sender: tokio_mpsc::Sender<SpeechEvent>,
    synthesizer: S,
    playback: P,
    generation: Arc<AtomicU64>,
) where
    S: Synthesizer + Send,
    P: Playback,
{
    worker_loop_with_metrics(
        receiver,
        event_sender,
        synthesizer,
        playback,
        generation,
        WorkerQueueMetrics {
            pending_depth: Arc::new(AtomicU64::new(0)),
            active_work: Arc::new(AtomicU64::new(0)),
        },
    );
}

fn worker_loop_with_metrics<S, P>(
    receiver: Receiver<SpeechCommand>,
    event_sender: tokio_mpsc::Sender<SpeechEvent>,
    mut synthesizer: S,
    mut playback: P,
    generation: Arc<AtomicU64>,
    queue_metrics: WorkerQueueMetrics,
) where
    S: Synthesizer + Send,
    P: Playback,
{
    let mut receiver = receiver;
    // An utterance synthesized while the previous one was playing.
    let mut ready: Option<(SpeechRequest, Vec<u8>)> = None;
    let mut stopping = false;
    loop {
        let (request, audio) = match ready.take() {
            Some(ready) => ready,
            None if stopping => break,
            None => match receiver.recv() {
                Ok(SpeechCommand::Speak(request)) => {
                    match synthesize_request(
                        request,
                        &event_sender,
                        &mut synthesizer,
                        &generation,
                        &queue_metrics,
                    ) {
                        Some(ready) => ready,
                        None => continue,
                    }
                }
                Ok(SpeechCommand::Shutdown) | Err(_) => break,
            },
        };

        // Play this utterance on this thread while a helper thread
        // synthesizes the next queued one, so a streamed answer's sentences
        // follow each other without a synthesis gap. Order is unchanged: the
        // helper takes requests from the same queue, one at a time.
        let playing = AtomicBool::new(true);
        let prefetched = thread::scope(|scope| {
            let prefetch = scope.spawn(|| {
                prefetch_next(
                    &mut receiver,
                    &playing,
                    &event_sender,
                    &mut synthesizer,
                    &generation,
                    &queue_metrics,
                )
            });
            play_request(
                request,
                audio,
                &event_sender,
                &mut playback,
                &generation,
                &queue_metrics,
            );
            playing.store(false, Ordering::Release);
            prefetch.join().expect("speech synthesis helper panicked")
        });
        match prefetched {
            Prefetch::Ready(next) => ready = Some(next),
            Prefetch::Stop => stopping = true,
            Prefetch::Nothing => {}
        }
    }

    info!(target: "speech", event = "worker_stopped", "speech");
}

enum Prefetch {
    Ready((SpeechRequest, Vec<u8>)),
    Stop,
    Nothing,
}

/// Waits (while the current utterance plays) for the next request and
/// synthesizes it. Returns as soon as playback ends if nothing arrived.
fn prefetch_next<S>(
    receiver: &mut Receiver<SpeechCommand>,
    playing: &AtomicBool,
    event_sender: &tokio_mpsc::Sender<SpeechEvent>,
    synthesizer: &mut S,
    generation: &AtomicU64,
    queue_metrics: &WorkerQueueMetrics,
) -> Prefetch
where
    S: Synthesizer,
{
    loop {
        match receiver.recv_timeout(PREFETCH_POLL_INTERVAL) {
            Ok(SpeechCommand::Speak(request)) => {
                return match synthesize_request(
                    request,
                    event_sender,
                    synthesizer,
                    generation,
                    queue_metrics,
                ) {
                    Some(ready) => Prefetch::Ready(ready),
                    None => Prefetch::Nothing,
                };
            }
            Ok(SpeechCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                return Prefetch::Stop;
            }
            Err(RecvTimeoutError::Timeout) => {
                if !playing.load(Ordering::Acquire) {
                    return Prefetch::Nothing;
                }
            }
        }
    }
}

/// Synthesizes one request. On cancellation or failure it reports the
/// outcome itself and returns `None`.
fn synthesize_request<S>(
    request: SpeechRequest,
    event_sender: &tokio_mpsc::Sender<SpeechEvent>,
    synthesizer: &mut S,
    generation: &AtomicU64,
    queue_metrics: &WorkerQueueMetrics,
) -> Option<(SpeechRequest, Vec<u8>)>
where
    S: Synthesizer,
{
    queue_metrics.pending_depth.fetch_sub(1, Ordering::AcqRel);
    queue_metrics.active_work.fetch_add(1, Ordering::AcqRel);
    if let Some(interaction) = &request.interaction {
        interaction.queue_wait(
            "speech",
            request.enqueued_at.elapsed(),
            request.pending_depth_before,
            queue_metrics.active_work.load(Ordering::Acquire) as usize,
        );
        interaction.in_flight("speech", 1);
    }
    if request_is_stale(&request, generation) {
        finish(
            event_sender,
            request,
            SpeechOutcome::Cancelled,
            queue_metrics.active_work.as_ref(),
        );
        return None;
    }

    let synthesis = request
        .interaction
        .as_ref()
        .map(|interaction| interaction.stage("piper.synthesis", telemetry::Fields::new()));
    info!(
        target: "speech",
        event = "synthesis_started",
        utterance_id = request.utterance_id.value(),
        owner = ?request.owner,
        "speech"
    );
    // Every utterance is synthesized by Piper. There are no fixed status cues
    // any more, so there is nothing worth caching and no outcome is ever
    // reported as a cache hit.
    let audio = synthesizer.synthesize(
        &SynthesisRequest {
            utterance_id: request.utterance_id.value(),
            generation: request.generation,
            text: request.text.clone(),
        },
        generation,
    );

    let audio = match audio {
        Ok(SynthesisResult::Audio(audio)) => {
            if let Some(stage) = synthesis {
                stage.finish(
                    "completed",
                    telemetry::fields([
                        ("generated_audio_bytes", serde_json::json!(audio.len())),
                        (
                            "generated_audio_duration_ms",
                            serde_json::json!(
                                super::playback::wav_duration(&audio)
                                    .and_then(|duration| u64::try_from(duration.as_millis()).ok())
                            ),
                        ),
                    ]),
                );
            }
            info!(
                target: "speech",
                event = "synthesis_completed",
                utterance_id = request.utterance_id.value(),
                bytes = audio.len(),
                owner = ?request.owner,
                "speech"
            );
            audio
        }
        Ok(SynthesisResult::Cancelled) => {
            if let Some(stage) = synthesis {
                stage.finish("cancelled", telemetry::Fields::new());
            }
            info!(
                target: "speech",
                event = "synthesis_cancelled",
                utterance_id = request.utterance_id.value(),
                owner = ?request.owner,
                "speech"
            );
            finish(
                event_sender,
                request,
                SpeechOutcome::Cancelled,
                queue_metrics.active_work.as_ref(),
            );
            return None;
        }
        Err(_error) => {
            if let Some(stage) = synthesis {
                stage.finish(
                    "failed",
                    telemetry::fields([("failure_class", serde_json::json!("subprocess_failure"))]),
                );
            }
            warn!(
                target: "speech",
                event = "synthesis_failed",
                utterance_id = request.utterance_id.value(),
                failure_class = "synthesis_failure",
                owner = ?request.owner,
                "speech"
            );
            finish(
                event_sender,
                request,
                SpeechOutcome::SynthesisFailed,
                queue_metrics.active_work.as_ref(),
            );
            return None;
        }
    };

    Some((request, audio))
}

/// Plays one synthesized utterance and reports its outcome.
fn play_request<P>(
    request: SpeechRequest,
    audio: Vec<u8>,
    event_sender: &tokio_mpsc::Sender<SpeechEvent>,
    playback: &mut P,
    generation: &AtomicU64,
    queue_metrics: &WorkerQueueMetrics,
) where
    P: Playback,
{
    if request_is_stale(&request, generation) {
        info!(
            target: "speech",
            event = "synthesis_result_discarded",
            utterance_id = request.utterance_id.value(),
            owner = ?request.owner,
            "speech"
        );
        finish(
            event_sender,
            request,
            SpeechOutcome::Cancelled,
            queue_metrics.active_work.as_ref(),
        );
        return;
    }

    info!(
        target: "speech",
        event = "playback_started",
        utterance_id = request.utterance_id.value(),
        owner = ?request.owner,
        "speech"
    );
    if let Some(interaction) = &request.interaction {
        // This is the point immediately before the worker asks the playback
        // backend to start. It measures software dispatch, not sound at ears.
        interaction.mark("speech.playback_started");
        if matches!(request.owner, SpeechOwner::Turn(_)) {
            // A streamed answer is several utterances; "first audio" is the
            // first sentence's playback.
            interaction.mark_first("final_answer.playback_started");
        }
    }
    let playback_stage = request
        .interaction
        .as_ref()
        .map(|interaction| interaction.stage("speech.playback", telemetry::Fields::new()));
    match playback.play(
        PlaybackRequest {
            utterance_id: request.utterance_id.value(),
            generation: request.generation,
            wav: audio,
        },
        generation,
    ) {
        Ok(PlaybackResult::Completed) => {
            if let Some(stage) = playback_stage {
                stage.finish("completed", telemetry::Fields::new());
            }
            info!(
                target: "speech",
                event = "playback_completed",
                utterance_id = request.utterance_id.value(),
                owner = ?request.owner,
                "speech"
            );
            finish(
                event_sender,
                request,
                SpeechOutcome::Played,
                queue_metrics.active_work.as_ref(),
            );
        }
        Ok(PlaybackResult::Cancelled) => {
            if let Some(stage) = playback_stage {
                stage.finish("cancelled", telemetry::Fields::new());
            }
            info!(
                target: "speech",
                event = "playback_cancelled",
                utterance_id = request.utterance_id.value(),
                owner = ?request.owner,
                "speech"
            );
            finish(
                event_sender,
                request,
                SpeechOutcome::Cancelled,
                queue_metrics.active_work.as_ref(),
            );
        }
        Err(_error) => {
            if let Some(stage) = playback_stage {
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
                event = "playback_failed",
                utterance_id = request.utterance_id.value(),
                failure_class = "output_device_failure",
                owner = ?request.owner,
                "terminal dialogue remains available"
            );
            finish(
                event_sender,
                request,
                SpeechOutcome::PlaybackFailed,
                queue_metrics.active_work.as_ref(),
            );
        }
    }
}

fn request_is_stale(request: &SpeechRequest, generation: &AtomicU64) -> bool {
    generation.load(Ordering::Acquire) != request.generation
}

fn finish(
    event_sender: &tokio_mpsc::Sender<SpeechEvent>,
    request: SpeechRequest,
    outcome: SpeechOutcome,
    active_work: &AtomicU64,
) {
    active_work.fetch_sub(1, Ordering::AcqRel);
    if let Some(interaction) = &request.interaction {
        interaction.in_flight("speech", -1);
        interaction.event(
            "speech.utterance",
            speech_outcome_label(outcome),
            telemetry::fields([
                (
                    "utterance_id",
                    serde_json::json!(request.utterance_id.value()),
                ),
                (
                    "owner",
                    serde_json::json!(speech_owner_label(request.owner)),
                ),
            ]),
        );
    }
    let event = SpeechEvent {
        utterance_id: request.utterance_id,
        owner: request.owner,
        outcome,
    };
    if event_sender.blocking_send(event).is_err() {
        warn!(
            target: "speech",
            event = "event_receiver_closed",
            utterance_id = request.utterance_id.value(),
            "speech"
        );
    }
}

fn speech_owner_label(owner: SpeechOwner) -> &'static str {
    match owner {
        SpeechOwner::Turn(_) => "turn",
        SpeechOwner::System => "system",
    }
}

fn speech_outcome_label(outcome: SpeechOutcome) -> &'static str {
    match outcome {
        SpeechOutcome::Played => "played",
        SpeechOutcome::Cancelled => "cancelled",
        SpeechOutcome::SynthesisFailed => "synthesis_failed",
        SpeechOutcome::PlaybackFailed => "playback_failed",
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
            mpsc,
        },
        thread,
        time::Instant,
    };

    use tokio::sync::mpsc as tokio_mpsc;

    use super::{
        SpeechCommand, SpeechEvent, SpeechOutcome, SpeechOwner, SpeechRequest, worker_loop,
    };
    use crate::speak::{
        playback::{Playback, PlaybackError, PlaybackRequest, PlaybackResult},
        tts::{SynthesisError, SynthesisRequest, SynthesisResult, Synthesizer},
    };

    struct FakeSynthesizer {
        calls: Arc<Mutex<Vec<u64>>>,
        results: VecDeque<Result<SynthesisResult, SynthesisError>>,
    }

    impl Synthesizer for FakeSynthesizer {
        fn synthesize(
            &mut self,
            request: &SynthesisRequest,
            _current_generation: &AtomicU64,
        ) -> Result<SynthesisResult, SynthesisError> {
            self.calls.lock().unwrap().push(request.utterance_id);
            self.results
                .pop_front()
                .unwrap_or_else(|| Ok(SynthesisResult::Audio(vec![1, 2, 3])))
        }
    }

    struct FakePlayback {
        played: Arc<Mutex<Vec<u64>>>,
    }

    impl Playback for FakePlayback {
        fn play(
            &mut self,
            request: PlaybackRequest,
            _current_generation: &AtomicU64,
        ) -> Result<PlaybackResult, PlaybackError> {
            self.played.lock().unwrap().push(request.utterance_id);
            Ok(PlaybackResult::Completed)
        }
    }

    /// Playback that stays "playing" until the next utterance has been
    /// synthesized (or a timeout), recording whether that overlap happened.
    struct OverlapProbePlayback {
        synth_calls: Arc<Mutex<Vec<u64>>>,
        played: Arc<Mutex<Vec<u64>>>,
        overlapped: Arc<Mutex<Vec<u64>>>,
    }

    impl Playback for OverlapProbePlayback {
        fn play(
            &mut self,
            request: PlaybackRequest,
            _current_generation: &AtomicU64,
        ) -> Result<PlaybackResult, PlaybackError> {
            let next = request.utterance_id + 1;
            let deadline = Instant::now() + std::time::Duration::from_millis(500);
            while Instant::now() < deadline {
                if self.synth_calls.lock().unwrap().contains(&next) {
                    self.overlapped.lock().unwrap().push(request.utterance_id);
                    break;
                }
                thread::sleep(std::time::Duration::from_millis(5));
            }
            self.played.lock().unwrap().push(request.utterance_id);
            Ok(PlaybackResult::Completed)
        }
    }

    #[tokio::test]
    async fn the_next_sentence_is_synthesized_while_the_current_one_plays() {
        let (sender, receiver) = mpsc::sync_channel(8);
        let (event_sender, mut event_receiver) = tokio_mpsc::channel(8);
        let generation = Arc::new(AtomicU64::new(1));
        let synth_calls = Arc::new(Mutex::new(Vec::new()));
        let played = Arc::new(Mutex::new(Vec::new()));
        let overlapped = Arc::new(Mutex::new(Vec::new()));
        for id in 1..=3 {
            sender
                .send(SpeechCommand::Speak(request(id, 1, "A sentence.")))
                .unwrap();
        }
        sender.send(SpeechCommand::Shutdown).unwrap();
        let worker = {
            let synth_calls = synth_calls.clone();
            let played = played.clone();
            let overlapped = overlapped.clone();
            thread::spawn(move || {
                worker_loop(
                    receiver,
                    event_sender,
                    FakeSynthesizer {
                        calls: synth_calls.clone(),
                        results: VecDeque::new(),
                    },
                    OverlapProbePlayback {
                        synth_calls,
                        played,
                        overlapped,
                    },
                    generation,
                );
            })
        };
        for _ in 0..3 {
            assert_eq!(
                event_receiver.recv().await.unwrap().outcome,
                SpeechOutcome::Played
            );
        }
        worker.join().unwrap();

        assert_eq!(
            played.lock().unwrap().as_slice(),
            &[1, 2, 3],
            "order is kept"
        );
        assert_eq!(synth_calls.lock().unwrap().as_slice(), &[1, 2, 3]);
        assert_eq!(
            overlapped.lock().unwrap().as_slice(),
            &[1, 2],
            "sentences 2 and 3 were synthesized during the previous playback"
        );
    }

    struct FailingPlayback;

    impl Playback for FailingPlayback {
        fn play(
            &mut self,
            _request: PlaybackRequest,
            _current_generation: &AtomicU64,
        ) -> Result<PlaybackResult, PlaybackError> {
            Err(PlaybackError::NoDefaultDevice)
        }
    }

    struct HeldPlayback {
        started: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    impl Playback for HeldPlayback {
        fn play(
            &mut self,
            request: PlaybackRequest,
            current_generation: &AtomicU64,
        ) -> Result<PlaybackResult, PlaybackError> {
            self.started.send(()).unwrap();
            self.release.recv().unwrap();
            if current_generation.load(Ordering::Acquire) != request.generation {
                Ok(PlaybackResult::Cancelled)
            } else {
                Ok(PlaybackResult::Completed)
            }
        }
    }

    fn request(id: u64, generation: u64, text: &str) -> SpeechRequest {
        SpeechRequest {
            utterance_id: super::UtteranceId(id),
            owner: SpeechOwner::Turn(super::TurnId::new(id)),
            generation,
            text: text.into(),
            interaction: None,
            enqueued_at: Instant::now(),
            pending_depth_before: 0,
        }
    }

    #[tokio::test]
    async fn worker_plays_requests_in_queue_order() {
        let (sender, receiver) = mpsc::sync_channel(4);
        let (event_sender, mut event_receiver) = tokio_mpsc::channel(4);
        let generation = Arc::new(AtomicU64::new(1));
        let synth_calls = Arc::new(Mutex::new(Vec::new()));
        let played = Arc::new(Mutex::new(Vec::new()));
        let worker_generation = generation.clone();
        let worker_synth_calls = synth_calls.clone();
        let worker_played = played.clone();

        let worker = thread::spawn(move || {
            worker_loop(
                receiver,
                event_sender,
                FakeSynthesizer {
                    calls: worker_synth_calls,
                    results: VecDeque::new(),
                },
                FakePlayback {
                    played: worker_played,
                },
                worker_generation,
            );
        });
        sender
            .send(SpeechCommand::Speak(request(1, 1, "first")))
            .unwrap();
        sender
            .send(SpeechCommand::Speak(request(2, 1, "second")))
            .unwrap();
        sender.send(SpeechCommand::Shutdown).unwrap();

        let first = event_receiver.recv().await.unwrap();
        let second = event_receiver.recv().await.unwrap();
        worker.join().unwrap();

        assert_eq!(synth_calls.lock().unwrap().as_slice(), &[1, 2]);
        assert_eq!(played.lock().unwrap().as_slice(), &[1, 2]);
        assert_eq!(first.outcome, SpeechOutcome::Played);
        assert_eq!(second.outcome, SpeechOutcome::Played);
    }

    #[tokio::test]
    async fn stale_request_never_reaches_synthesis_or_playback() {
        let (sender, receiver) = mpsc::sync_channel(2);
        let (event_sender, mut event_receiver) = tokio_mpsc::channel(2);
        let generation = Arc::new(AtomicU64::new(2));
        let synth_calls = Arc::new(Mutex::new(Vec::new()));
        let played = Arc::new(Mutex::new(Vec::new()));
        let worker_generation = generation.clone();
        let worker_synth_calls = synth_calls.clone();
        let worker_played = played.clone();
        let worker = thread::spawn(move || {
            worker_loop(
                receiver,
                event_sender,
                FakeSynthesizer {
                    calls: worker_synth_calls,
                    results: VecDeque::new(),
                },
                FakePlayback {
                    played: worker_played,
                },
                worker_generation,
            );
        });
        sender
            .send(SpeechCommand::Speak(request(1, 1, "obsolete")))
            .unwrap();
        sender.send(SpeechCommand::Shutdown).unwrap();

        let event = event_receiver.recv().await.unwrap();
        worker.join().unwrap();

        assert_eq!(event.outcome, SpeechOutcome::Cancelled);
        assert!(synth_calls.lock().unwrap().is_empty());
        assert!(played.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn repeated_text_is_synthesized_every_time_without_a_cache() {
        let (sender, receiver) = mpsc::sync_channel(4);
        let (event_sender, mut event_receiver) = tokio_mpsc::channel(4);
        let generation = Arc::new(AtomicU64::new(1));
        let synth_calls = Arc::new(Mutex::new(Vec::new()));
        let played = Arc::new(Mutex::new(Vec::new()));
        let worker_synth_calls = synth_calls.clone();
        let worker_played = played.clone();
        let worker = thread::spawn(move || {
            worker_loop(
                receiver,
                event_sender,
                FakeSynthesizer {
                    calls: worker_synth_calls,
                    results: VecDeque::new(),
                },
                FakePlayback {
                    played: worker_played,
                },
                generation,
            );
        });
        sender
            .send(SpeechCommand::Speak(request(1, 1, "i'm thinking")))
            .unwrap();
        sender
            .send(SpeechCommand::Speak(request(2, 1, "i'm thinking")))
            .unwrap();
        sender.send(SpeechCommand::Shutdown).unwrap();

        let _ = event_receiver.recv().await.unwrap();
        let _ = event_receiver.recv().await.unwrap();
        worker.join().unwrap();

        assert_eq!(synth_calls.lock().unwrap().as_slice(), &[1, 2]);
        assert_eq!(played.lock().unwrap().as_slice(), &[1, 2]);
    }

    #[tokio::test]
    async fn playback_failure_is_reported_without_panicking_the_worker() {
        let (sender, receiver) = mpsc::sync_channel(2);
        let (event_sender, mut event_receiver) = tokio_mpsc::channel(2);
        let generation = Arc::new(AtomicU64::new(1));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let worker_calls = calls.clone();
        let worker_generation = generation.clone();
        let worker = thread::spawn(move || {
            worker_loop(
                receiver,
                event_sender,
                FakeSynthesizer {
                    calls: worker_calls,
                    results: VecDeque::new(),
                },
                FailingPlayback,
                worker_generation,
            );
        });
        sender
            .send(SpeechCommand::Speak(request(1, 1, "answer")))
            .unwrap();
        sender.send(SpeechCommand::Shutdown).unwrap();

        let event = event_receiver.recv().await.unwrap();
        worker.join().unwrap();

        assert_eq!(event.outcome, SpeechOutcome::PlaybackFailed);
        assert_eq!(calls.lock().unwrap().as_slice(), &[1]);
    }

    #[tokio::test]
    async fn cancellation_during_playback_prevents_a_late_completed_event() {
        let (sender, receiver) = mpsc::sync_channel(2);
        let (event_sender, mut event_receiver) = tokio_mpsc::channel(2);
        let generation = Arc::new(AtomicU64::new(1));
        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let worker_generation = generation.clone();
        let worker = thread::spawn(move || {
            worker_loop(
                receiver,
                event_sender,
                FakeSynthesizer {
                    calls: Arc::new(Mutex::new(Vec::new())),
                    results: VecDeque::new(),
                },
                HeldPlayback {
                    started: started_sender,
                    release: release_receiver,
                },
                worker_generation,
            );
        });
        sender
            .send(SpeechCommand::Speak(request(1, 1, "answer")))
            .unwrap();
        started_receiver.recv().unwrap();
        generation.fetch_add(1, Ordering::AcqRel);
        release_sender.send(()).unwrap();
        sender.send(SpeechCommand::Shutdown).unwrap();

        let event = event_receiver.recv().await.unwrap();
        worker.join().unwrap();

        assert_eq!(event.outcome, SpeechOutcome::Cancelled);
    }

    #[test]
    fn speech_event_carries_existing_owner_identity() {
        let event = SpeechEvent {
            utterance_id: super::UtteranceId(12),
            owner: SpeechOwner::System,
            outcome: SpeechOutcome::PlaybackFailed,
        };
        assert_eq!(event.utterance_id.value(), 12);
        assert_eq!(event.outcome, SpeechOutcome::PlaybackFailed);
    }

    #[test]
    fn generation_change_is_visible_to_a_worker() {
        let generation = AtomicU64::new(3);
        generation.fetch_add(1, Ordering::AcqRel);
        assert_eq!(generation.load(Ordering::Acquire), 4);
    }
}
