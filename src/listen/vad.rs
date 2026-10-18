//! Continuous, diagnostic-only local speech-activity monitoring.
//!
//! This module deliberately stops at acoustic activity. It owns neither
//! Whisper transcription nor the application controller, Ollama, a camera, or
//! a turn-completion policy. A pause therefore means only that the VAD has
//! recently observed non-speech—not that a person has finished talking.

use std::{
    collections::VecDeque,
    env,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    time::{Duration, Instant},
};

use cpal::{
    Device, FromSample, Sample, SampleFormat, SizedSample,
    traits::{DeviceTrait, StreamTrait},
};
use rubato::{Fft, FixedSync, Resampler, audioadapter_buffers::owned::InterleavedOwned};
use tokio::{sync::mpsc::Sender as EventSender, task::JoinHandle};
use voice_activity_detector::VoiceActivityDetector;

use crate::{dictation::audio::CpalRecorder, protocol::DictationInput};

const VAD_SAMPLE_RATE: u32 = 16_000;
const VAD_FRAME_SAMPLES: usize = 512;
const MIN_CAPTURE_SAMPLE_RATE: u32 = 8_000;
const MAX_CAPTURE_SAMPLE_RATE: u32 = 192_000;
const CAPTURE_QUEUE_SECONDS: usize = 2;
const MAX_WORKER_DRAIN_SAMPLES: usize = 4_096;
const MAX_RESAMPLED_DRAIN_SAMPLES: usize = 10_240;
const MAX_RESAMPLER_PENDING_SAMPLES: usize = 8_192;
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(10);
const NO_AUDIO_NOTICE_AFTER: Duration = Duration::from_secs(2);
const DROPPED_AUDIO_REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// Tunable acoustic classification controls. These deliberately do not make
/// a conversational decision about when someone has completed a turn.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct VadSettings {
    threshold: f32,
    speech_frames: usize,
    silence_frames: usize,
    status_interval: Duration,
}

impl VadSettings {
    pub(crate) fn from_environment() -> Result<Self, String> {
        let threshold = parse_f32("INDEPENDENCE_VAD_THRESHOLD", 0.5)?;
        if !(0.0..=1.0).contains(&threshold) {
            return Err("INDEPENDENCE_VAD_THRESHOLD must be a number from 0.0 through 1.0".into());
        }

        let speech_frames = parse_usize("INDEPENDENCE_VAD_SPEECH_FRAMES", 2)?;
        let silence_frames = parse_usize("INDEPENDENCE_VAD_SILENCE_FRAMES", 8)?;
        if speech_frames == 0 || silence_frames == 0 {
            return Err(
                "INDEPENDENCE_VAD_SPEECH_FRAMES and INDEPENDENCE_VAD_SILENCE_FRAMES must both be at least 1"
                    .into(),
            );
        }

        let status_millis = parse_u64("INDEPENDENCE_VAD_STATUS_MS", 1_000)?;
        if status_millis == 0 {
            return Err("INDEPENDENCE_VAD_STATUS_MS must be at least 1".into());
        }

        Ok(Self {
            threshold,
            speech_frames,
            silence_frames,
            status_interval: Duration::from_millis(status_millis),
        })
    }

    pub(crate) fn summary(&self) -> String {
        format!(
            "Silero VAD: 16 kHz mono, 512-sample frames; threshold {:.2}; speech after {} frame(s), pause after {} frame(s).",
            self.threshold, self.speech_frames, self.silence_frames
        )
    }
}

fn parse_f32(name: &str, default: f32) -> Result<f32, String> {
    match env::var(name) {
        Ok(value) => value
            .parse::<f32>()
            .map_err(|_| format!("{name} must be a decimal number, not `{value}`")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(format!("could not read {name}: {error}")),
    }
}

fn parse_usize(name: &str, default: usize) -> Result<usize, String> {
    match env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .map_err(|_| format!("{name} must be a whole number, not `{value}`")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(format!("could not read {name}: {error}")),
    }
}

fn parse_u64(name: &str, default: u64) -> Result<u64, String> {
    match env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|_| format!("{name} must be a whole number, not `{value}`")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(format!("could not read {name}: {error}")),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CaptureGeneration(u64);

/// Raw-mode-safe, non-sensitive monitor output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VadMonitorOutput {
    MicrophoneOn { microphone: String, format: String },
    MicrophoneOff,
    SpeechStarted,
    SpeechResumed,
    SpeechPaused,
    NonspeechElapsed { elapsed: Duration },
    WaitingForAudio,
    AudioStalled,
    Diagnostic(String),
}

/// A controller-authorized monitor action. The UI decides how to render it;
/// the capture/worker code never writes directly to the terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VadMonitorAction {
    Display(VadMonitorOutput),
    Exit,
}

/// Facts delivered from CPAL ownership or the separate VAD worker.
#[derive(Debug)]
pub(crate) enum VadMonitorEvent {
    CaptureStarted {
        generation: CaptureGeneration,
        details: MonitorCaptureDetails,
    },
    CaptureFailed {
        generation: CaptureGeneration,
        message: String,
    },
    SpeechStarted {
        generation: CaptureGeneration,
    },
    SpeechResumed {
        generation: CaptureGeneration,
    },
    SpeechPaused {
        generation: CaptureGeneration,
    },
    NonspeechElapsed {
        generation: CaptureGeneration,
        elapsed: Duration,
    },
    WaitingForAudio {
        generation: CaptureGeneration,
    },
    AudioStalled {
        generation: CaptureGeneration,
    },
    AudioDropped {
        generation: CaptureGeneration,
        sample_frames: usize,
    },
    WorkerFailed {
        generation: CaptureGeneration,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MonitorCaptureDetails {
    microphone: String,
    format: String,
}

/// The diagnostic monitor's public owner. It is separate from
/// `DictationSession`: no accepted transcript can leave this module because it
/// never creates one.
pub(crate) struct VadMonitorSession<C: MonitorCapture = CpalMonitorCapture> {
    controller: MonitorController,
    runtime: MonitorRuntime<C>,
}

pub(crate) type VadMonitor = VadMonitorSession<CpalMonitorCapture>;

impl VadMonitorSession<CpalMonitorCapture> {
    pub(crate) fn from_environment(
        event_sender: EventSender<VadMonitorEvent>,
    ) -> Result<Self, String> {
        let settings = VadSettings::from_environment()?;
        let detector = Box::new(SileroDetector::new()?);
        Ok(Self::with_services(
            CpalMonitorCapture::from_environment(),
            detector,
            settings,
            event_sender,
        ))
    }
}

impl<C: MonitorCapture> VadMonitorSession<C> {
    fn with_services(
        capture: C,
        detector: Box<dyn SpeechDetector>,
        settings: VadSettings,
        event_sender: EventSender<VadMonitorEvent>,
    ) -> Self {
        Self {
            controller: MonitorController::new(),
            runtime: MonitorRuntime::new(capture, detector, settings, event_sender),
        }
    }

    pub(crate) fn microphone_label(&self) -> Result<String, String> {
        self.runtime.capture.microphone_label()
    }

    pub(crate) fn settings_summary(&self) -> String {
        self.runtime.settings.summary()
    }

    pub(crate) fn start(&mut self) -> Vec<VadMonitorAction> {
        self.dispatch(MonitorEvent::Start)
    }

    pub(crate) fn handle_key(&mut self, input: DictationInput) -> Vec<VadMonitorAction> {
        let event = match input {
            DictationInput::ToggleMute => MonitorEvent::ToggleMute,
            DictationInput::Escape | DictationInput::Interrupt | DictationInput::EndOfInput => {
                MonitorEvent::ExitRequested
            }
            // Enter has no meaning in the monitor. It must not queue any
            // recording or turn-taking behavior before that milestone exists.
            DictationInput::Enter => return Vec::new(),
        };
        self.dispatch(event)
    }

    pub(crate) fn handle_event(&mut self, event: VadMonitorEvent) -> Vec<VadMonitorAction> {
        self.dispatch(MonitorEvent::Runtime(event))
    }

    pub(crate) fn worker_channel_closed(&mut self) -> Vec<VadMonitorAction> {
        self.dispatch(MonitorEvent::WorkerChannelClosed)
    }

    pub(crate) fn poll(&mut self) -> Vec<VadMonitorAction> {
        let events = self.runtime.poll();
        let mut actions = Vec::new();
        for event in events {
            actions.extend(self.handle_event(event));
        }
        actions
    }

    pub(crate) async fn shutdown(&mut self) {
        self.runtime.shutdown().await;
    }

    fn dispatch(&mut self, event: MonitorEvent) -> Vec<VadMonitorAction> {
        let effects = self.controller.handle(event);
        let mut actions = Vec::new();
        for effect in effects {
            match effect {
                MonitorEffect::StartCapture { generation } => {
                    let event = match self.runtime.start_capture(generation) {
                        Ok(details) => VadMonitorEvent::CaptureStarted {
                            generation,
                            details,
                        },
                        Err(message) => VadMonitorEvent::CaptureFailed {
                            generation,
                            message,
                        },
                    };
                    actions.extend(self.handle_event(event));
                }
                MonitorEffect::StopCapture { generation } => {
                    self.runtime.stop_capture(generation);
                }
                MonitorEffect::Display(output) => actions.push(VadMonitorAction::Display(output)),
                MonitorEffect::Exit => actions.push(VadMonitorAction::Exit),
            }
        }
        actions
    }
}

#[derive(Debug)]
enum MonitorEvent {
    Start,
    ToggleMute,
    ExitRequested,
    WorkerChannelClosed,
    Runtime(VadMonitorEvent),
}

#[derive(Debug)]
enum MonitorEffect {
    StartCapture { generation: CaptureGeneration },
    StopCapture { generation: CaptureGeneration },
    Display(VadMonitorOutput),
    Exit,
}

#[derive(Clone, Copy, Debug)]
enum MonitorPhase {
    Idle,
    Starting { generation: CaptureGeneration },
    Listening { generation: CaptureGeneration },
    Muted,
    Failed,
    Exiting,
}

struct MonitorController {
    phase: MonitorPhase,
    next_generation: u64,
}

impl MonitorController {
    fn new() -> Self {
        Self {
            phase: MonitorPhase::Idle,
            next_generation: 1,
        }
    }

    fn handle(&mut self, event: MonitorEvent) -> Vec<MonitorEffect> {
        match event {
            MonitorEvent::Start => match self.phase {
                MonitorPhase::Idle => self.begin_capture(),
                _ => Vec::new(),
            },
            MonitorEvent::ToggleMute => self.toggle_mute(),
            MonitorEvent::ExitRequested => self.exit(),
            MonitorEvent::WorkerChannelClosed => self.worker_channel_closed(),
            MonitorEvent::Runtime(event) => self.handle_runtime(event),
        }
    }

    fn begin_capture(&mut self) -> Vec<MonitorEffect> {
        let generation = CaptureGeneration(self.next_generation);
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("VAD monitor capture generations exhausted");
        self.phase = MonitorPhase::Starting { generation };
        vec![MonitorEffect::StartCapture { generation }]
    }

    fn toggle_mute(&mut self) -> Vec<MonitorEffect> {
        match self.phase {
            MonitorPhase::Starting { generation } | MonitorPhase::Listening { generation } => {
                self.phase = MonitorPhase::Muted;
                vec![
                    MonitorEffect::StopCapture { generation },
                    MonitorEffect::Display(VadMonitorOutput::MicrophoneOff),
                ]
            }
            MonitorPhase::Muted => self.begin_capture(),
            MonitorPhase::Failed => vec![MonitorEffect::Display(VadMonitorOutput::Diagnostic(
                "the VAD worker failed; exit and restart the monitor before unmuting".into(),
            ))],
            MonitorPhase::Idle | MonitorPhase::Exiting => Vec::new(),
        }
    }

    fn exit(&mut self) -> Vec<MonitorEffect> {
        let active = match self.phase {
            MonitorPhase::Starting { generation } | MonitorPhase::Listening { generation } => {
                Some(generation)
            }
            MonitorPhase::Idle
            | MonitorPhase::Muted
            | MonitorPhase::Failed
            | MonitorPhase::Exiting => None,
        };
        self.phase = MonitorPhase::Exiting;
        active
            .into_iter()
            .map(|generation| MonitorEffect::StopCapture { generation })
            .chain([MonitorEffect::Exit])
            .collect()
    }

    fn worker_channel_closed(&mut self) -> Vec<MonitorEffect> {
        let active = match self.phase {
            MonitorPhase::Starting { generation } | MonitorPhase::Listening { generation } => {
                Some(generation)
            }
            MonitorPhase::Idle
            | MonitorPhase::Muted
            | MonitorPhase::Failed
            | MonitorPhase::Exiting => None,
        };
        let Some(generation) = active else {
            return Vec::new();
        };
        self.phase = MonitorPhase::Failed;
        vec![
            MonitorEffect::StopCapture { generation },
            MonitorEffect::Display(VadMonitorOutput::Diagnostic(
                "local VAD worker stopped unexpectedly; microphone released".into(),
            )),
            MonitorEffect::Display(VadMonitorOutput::MicrophoneOff),
        ]
    }

    fn handle_runtime(&mut self, event: VadMonitorEvent) -> Vec<MonitorEffect> {
        match event {
            VadMonitorEvent::CaptureStarted {
                generation,
                details,
            } if matches!(self.phase, MonitorPhase::Starting { generation: active } if active == generation) =>
            {
                self.phase = MonitorPhase::Listening { generation };
                vec![MonitorEffect::Display(VadMonitorOutput::MicrophoneOn {
                    microphone: details.microphone,
                    format: details.format,
                })]
            }
            VadMonitorEvent::CaptureFailed {
                generation,
                message,
            } if self.is_current(generation) => {
                self.phase = MonitorPhase::Muted;
                vec![
                    MonitorEffect::StopCapture { generation },
                    MonitorEffect::Display(VadMonitorOutput::Diagnostic(format!(
                        "microphone capture failed: {message}"
                    ))),
                    MonitorEffect::Display(VadMonitorOutput::MicrophoneOff),
                ]
            }
            VadMonitorEvent::WorkerFailed {
                generation,
                message,
            } if self.is_current(generation) => {
                self.phase = MonitorPhase::Failed;
                vec![
                    MonitorEffect::StopCapture { generation },
                    MonitorEffect::Display(VadMonitorOutput::Diagnostic(format!(
                        "local VAD worker failed: {message}"
                    ))),
                    MonitorEffect::Display(VadMonitorOutput::MicrophoneOff),
                ]
            }
            VadMonitorEvent::SpeechStarted { generation } if self.is_listening(generation) => {
                vec![MonitorEffect::Display(VadMonitorOutput::SpeechStarted)]
            }
            VadMonitorEvent::SpeechResumed { generation } if self.is_listening(generation) => {
                vec![MonitorEffect::Display(VadMonitorOutput::SpeechResumed)]
            }
            VadMonitorEvent::SpeechPaused { generation } if self.is_listening(generation) => {
                vec![MonitorEffect::Display(VadMonitorOutput::SpeechPaused)]
            }
            VadMonitorEvent::NonspeechElapsed {
                generation,
                elapsed,
            } if self.is_listening(generation) => {
                vec![MonitorEffect::Display(VadMonitorOutput::NonspeechElapsed {
                    elapsed,
                })]
            }
            VadMonitorEvent::WaitingForAudio { generation } if self.is_listening(generation) => {
                vec![MonitorEffect::Display(VadMonitorOutput::WaitingForAudio)]
            }
            VadMonitorEvent::AudioStalled { generation } if self.is_listening(generation) => {
                vec![MonitorEffect::Display(VadMonitorOutput::AudioStalled)]
            }
            VadMonitorEvent::AudioDropped {
                generation,
                sample_frames,
            } if self.is_listening(generation) => {
                vec![MonitorEffect::Display(VadMonitorOutput::Diagnostic(
                    format!(
                        "microphone dropped {sample_frames} audio sample frame(s); activity may be unreliable until capture catches up"
                    ),
                ))]
            }
            // Generation checks intentionally make late capture/worker events
            // harmless after mute, a fresh unmute, or exit.
            _ => Vec::new(),
        }
    }

    fn is_current(&self, generation: CaptureGeneration) -> bool {
        matches!(self.phase, MonitorPhase::Starting { generation: active } | MonitorPhase::Listening { generation: active } if active == generation)
    }

    fn is_listening(&self, generation: CaptureGeneration) -> bool {
        matches!(self.phase, MonitorPhase::Listening { generation: active } if active == generation)
    }
}

/// An actual hardware boundary, deliberately small so ordinary tests can use
/// a fake capture without a physical microphone.
pub(crate) trait MonitorCapture: Send {
    fn microphone_label(&self) -> Result<String, String>;
    fn start(
        &mut self,
        generation: CaptureGeneration,
        worker_commands: &Sender<WorkerCommand>,
    ) -> Result<MonitorCaptureDetails, String>;
    fn stop(&mut self, generation: CaptureGeneration);
    fn poll(&mut self) -> Vec<VadMonitorEvent>;
    fn shutdown(&mut self);
}

struct MonitorRuntime<C: MonitorCapture> {
    capture: C,
    settings: VadSettings,
    worker_commands: Sender<WorkerCommand>,
    worker: Option<JoinHandle<()>>,
}

impl<C: MonitorCapture> MonitorRuntime<C> {
    fn new(
        capture: C,
        detector: Box<dyn SpeechDetector>,
        settings: VadSettings,
        event_sender: EventSender<VadMonitorEvent>,
    ) -> Self {
        let (worker_commands, worker_receiver) = mpsc::channel();
        let worker_settings = settings.clone();
        let worker = tokio::task::spawn_blocking(move || {
            vad_worker_loop(detector, worker_receiver, worker_settings, event_sender);
        });
        Self {
            capture,
            settings,
            worker_commands,
            worker: Some(worker),
        }
    }

    fn start_capture(
        &mut self,
        generation: CaptureGeneration,
    ) -> Result<MonitorCaptureDetails, String> {
        self.capture.start(generation, &self.worker_commands)
    }

    fn stop_capture(&mut self, generation: CaptureGeneration) {
        self.capture.stop(generation);
    }

    fn poll(&mut self) -> Vec<VadMonitorEvent> {
        self.capture.poll()
    }

    async fn shutdown(&mut self) {
        self.capture.shutdown();
        let _ = self.worker_commands.send(WorkerCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.await;
        }
    }
}

pub(crate) struct CpalMonitorCapture {
    recorder: CpalRecorder,
    active: Option<RunningCapture>,
}

struct RunningCapture {
    generation: CaptureGeneration,
    stream: cpal::Stream,
    queue: Arc<SampleQueue>,
    stream_error: Arc<Mutex<Option<String>>>,
    last_drop_report: Instant,
    pending_dropped: usize,
    worker_commands: Sender<WorkerCommand>,
}

impl CpalMonitorCapture {
    fn from_environment() -> Self {
        Self {
            recorder: CpalRecorder::from_environment(),
            active: None,
        }
    }
}

impl MonitorCapture for CpalMonitorCapture {
    fn microphone_label(&self) -> Result<String, String> {
        let (_, _, microphone) = self.recorder.selected_input_configuration()?;
        Ok(microphone.label())
    }

    fn start(
        &mut self,
        generation: CaptureGeneration,
        worker_commands: &Sender<WorkerCommand>,
    ) -> Result<MonitorCaptureDetails, String> {
        self.shutdown();

        let (device, supported_config, microphone) =
            self.recorder.selected_input_configuration()?;
        let sample_rate = supported_config.sample_rate();
        let channels = usize::from(supported_config.channels());
        validate_capture_format(sample_rate, channels)?;

        let queue = Arc::new(SampleQueue::new(capture_queue_capacity(sample_rate)?)?);
        let stream_error = Arc::new(Mutex::new(None));
        let sample_format = supported_config.sample_format();
        let stream_config = supported_config.into();
        let stream = build_monitor_input_stream(
            &device,
            &stream_config,
            sample_format,
            channels,
            queue.clone(),
            stream_error.clone(),
        )?;

        worker_commands
            .send(WorkerCommand::Activate {
                generation,
                queue: queue.clone(),
                source_sample_rate: sample_rate,
            })
            .map_err(|_| {
                "local VAD worker stopped before microphone capture could start".to_owned()
            })?;

        if let Err(error) = stream.play() {
            let _ = worker_commands.send(WorkerCommand::Deactivate { generation });
            return Err(format!("could not start microphone capture: {error}"));
        }

        self.active = Some(RunningCapture {
            generation,
            stream,
            queue,
            stream_error,
            last_drop_report: Instant::now(),
            pending_dropped: 0,
            worker_commands: worker_commands.clone(),
        });

        Ok(MonitorCaptureDetails {
            microphone: microphone.label(),
            format: format!(
                "{} Hz, {} channel{}, {}",
                sample_rate,
                channels,
                if channels == 1 { "" } else { "s" },
                sample_format
            ),
        })
    }

    fn stop(&mut self, generation: CaptureGeneration) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        if active.generation != generation {
            return;
        }

        let RunningCapture {
            stream,
            queue,
            generation,
            worker_commands,
            ..
        } = self
            .active
            .take()
            .expect("active VAD capture was checked above");
        // Dropping CPAL's stream releases the input device before any queued
        // samples are discarded. A late worker event still carries this old
        // generation and is rejected by the controller.
        drop(stream);
        queue.clear();
        let _ = worker_commands.send(WorkerCommand::Deactivate { generation });
        // The worker is intentionally long-lived so mute/unmute cannot create
        // a collection of native inference workers. It resets only when this
        // generation is explicitly activated again.
    }

    fn poll(&mut self) -> Vec<VadMonitorEvent> {
        let Some(active) = self.active.as_mut() else {
            return Vec::new();
        };

        let stream_error = match active.stream_error.lock() {
            Ok(mut slot) => slot.take(),
            Err(_) => Some("microphone stream error-state lock was poisoned".into()),
        };
        if let Some(message) = stream_error {
            return vec![VadMonitorEvent::CaptureFailed {
                generation: active.generation,
                message,
            }];
        }

        active.pending_dropped = active
            .pending_dropped
            .saturating_add(active.queue.take_dropped());
        if active.pending_dropped != 0
            && active.last_drop_report.elapsed() >= DROPPED_AUDIO_REPORT_INTERVAL
        {
            active.last_drop_report = Instant::now();
            return vec![VadMonitorEvent::AudioDropped {
                generation: active.generation,
                sample_frames: std::mem::take(&mut active.pending_dropped),
            }];
        }

        Vec::new()
    }

    fn shutdown(&mut self) {
        if let Some(active) = self.active.take() {
            drop(active.stream);
            active.queue.clear();
            let _ = active.worker_commands.send(WorkerCommand::Deactivate {
                generation: active.generation,
            });
        }
    }
}

fn validate_capture_format(sample_rate: u32, channels: usize) -> Result<(), String> {
    if channels == 0 {
        return Err("microphone reported a zero-channel format".into());
    }
    if !(MIN_CAPTURE_SAMPLE_RATE..=MAX_CAPTURE_SAMPLE_RATE).contains(&sample_rate) {
        return Err(format!(
            "microphone sample rate {sample_rate} Hz is outside the supported {MIN_CAPTURE_SAMPLE_RATE}-{MAX_CAPTURE_SAMPLE_RATE} Hz monitor range"
        ));
    }
    Ok(())
}

fn capture_queue_capacity(sample_rate: u32) -> Result<usize, String> {
    usize::try_from(sample_rate)
        .map_err(|_| "microphone sample rate does not fit this platform".to_owned())?
        .checked_mul(CAPTURE_QUEUE_SECONDS)
        .ok_or_else(|| "microphone queue size overflowed".to_owned())
}

/// A fixed-capacity mono queue. CPAL's callback only downmixes into this
/// queue; it does not resample, run the VAD, print, or send through a channel.
pub(crate) struct SampleQueue {
    samples: Mutex<VecDeque<f32>>,
    capacity: usize,
    dropped: AtomicUsize,
}

impl SampleQueue {
    fn new(capacity: usize) -> Result<Self, String> {
        let mut samples = VecDeque::new();
        samples
            .try_reserve_exact(capacity)
            .map_err(|error| format!("could not reserve bounded VAD audio queue: {error}"))?;
        Ok(Self {
            samples: Mutex::new(samples),
            capacity,
            dropped: AtomicUsize::new(0),
        })
    }

    fn append<T>(&self, data: &[T], channels: usize)
    where
        T: Sample,
        f32: FromSample<T>,
    {
        if channels == 0 {
            return;
        }
        let frames = data.len() / channels;
        let Ok(mut samples) = self.samples.try_lock() else {
            self.dropped.fetch_add(frames, Ordering::Relaxed);
            return;
        };

        let available = self.capacity.saturating_sub(samples.len());
        let accepted = frames.min(available);
        for frame in data.chunks_exact(channels).take(accepted) {
            let mono = frame
                .iter()
                .map(|sample| f32::from_sample(*sample))
                .sum::<f32>()
                / channels as f32;
            samples.push_back(mono);
        }
        if accepted < frames {
            self.dropped.fetch_add(frames - accepted, Ordering::Relaxed);
        }
    }

    fn drain_into(&self, output: &mut Vec<f32>, maximum: usize) -> usize {
        output.clear();
        let Ok(mut samples) = self.samples.lock() else {
            return 0;
        };
        let count = samples.len().min(maximum);
        output.extend(samples.drain(..count));
        count
    }

    fn take_dropped(&self) -> usize {
        self.dropped.swap(0, Ordering::AcqRel)
    }

    fn clear(&self) {
        if let Ok(mut samples) = self.samples.lock() {
            samples.clear();
        }
        self.dropped.store(0, Ordering::Release);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.samples.lock().expect("test queue lock").len()
    }
}

fn build_monitor_input_stream(
    device: &Device,
    config: &cpal::StreamConfig,
    sample_format: SampleFormat,
    channels: usize,
    queue: Arc<SampleQueue>,
    stream_error: Arc<Mutex<Option<String>>>,
) -> Result<cpal::Stream, String> {
    macro_rules! build {
        ($sample:ty) => {
            build_monitor_stream_for_format::<$sample>(
                device,
                config,
                channels,
                queue,
                stream_error,
            )
        };
    }

    match sample_format {
        SampleFormat::F32 => build!(f32),
        SampleFormat::F64 => build!(f64),
        SampleFormat::I8 => build!(i8),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I24 => build!(cpal::I24),
        SampleFormat::I32 => build!(i32),
        SampleFormat::I64 => build!(i64),
        SampleFormat::U8 => build!(u8),
        SampleFormat::U16 => build!(u16),
        SampleFormat::U24 => build!(cpal::U24),
        SampleFormat::U32 => build!(u32),
        SampleFormat::U64 => build!(u64),
        _ => Err(format!(
            "microphone sample format `{sample_format}` is not supported for local VAD"
        )),
    }
}

fn build_monitor_stream_for_format<T>(
    device: &Device,
    config: &cpal::StreamConfig,
    channels: usize,
    queue: Arc<SampleQueue>,
    stream_error: Arc<Mutex<Option<String>>>,
) -> Result<cpal::Stream, String>
where
    T: Sample + SizedSample,
    f32: FromSample<T>,
{
    let input = queue.clone();
    let error_slot = stream_error.clone();
    device
        .build_input_stream(
            *config,
            move |data: &[T], _| input.append(data, channels),
            move |error| {
                if let Ok(mut slot) = error_slot.lock()
                    && slot.is_none()
                {
                    *slot = Some(error.to_string());
                }
            },
            Some(Duration::from_secs(2)),
        )
        .map_err(|error| format!("could not open microphone input stream: {error}"))
}

pub(crate) enum WorkerCommand {
    Activate {
        generation: CaptureGeneration,
        queue: Arc<SampleQueue>,
        source_sample_rate: u32,
    },
    Deactivate {
        generation: CaptureGeneration,
    },
    Shutdown,
}

trait SpeechDetector: Send {
    fn reset(&mut self);
    fn probability(&mut self, frame: &[f32]) -> Result<f32, String>;
}

struct SileroDetector {
    detector: VoiceActivityDetector,
}

impl SileroDetector {
    fn new() -> Result<Self, String> {
        let built = catch_unwind(AssertUnwindSafe(|| {
            VoiceActivityDetector::builder()
                .sample_rate(i64::from(VAD_SAMPLE_RATE))
                .chunk_size(VAD_FRAME_SAMPLES)
                .build()
        }));
        match built {
            Ok(Ok(detector)) => Ok(Self { detector }),
            Ok(Err(error)) => Err(format!(
                "could not initialize the embedded Silero VAD: {error}"
            )),
            Err(_) => Err(
                "the embedded Silero VAD panicked while initializing its ONNX Runtime session"
                    .into(),
            ),
        }
    }
}

impl SpeechDetector for SileroDetector {
    fn reset(&mut self) {
        self.detector.reset();
    }

    fn probability(&mut self, frame: &[f32]) -> Result<f32, String> {
        if frame.len() != VAD_FRAME_SAMPLES {
            return Err(format!(
                "internal VAD framing error: expected {VAD_FRAME_SAMPLES} samples, got {}",
                frame.len()
            ));
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            self.detector.predict(frame.iter().copied())
        }));
        match result {
            Ok(probability) if probability.is_finite() => Ok(probability),
            Ok(_) => Err("Silero VAD returned a non-finite speech probability".into()),
            Err(_) => Err("Silero VAD inference panicked".into()),
        }
    }
}

fn vad_worker_loop(
    mut detector: Box<dyn SpeechDetector>,
    commands: Receiver<WorkerCommand>,
    settings: VadSettings,
    event_sender: EventSender<VadMonitorEvent>,
) {
    let mut active: Option<WorkerGeneration> = None;
    let mut source_batch = Vec::with_capacity(MAX_WORKER_DRAIN_SAMPLES);

    loop {
        while let Ok(command) = commands.try_recv() {
            if !apply_worker_command(
                command,
                detector.as_mut(),
                &settings,
                &event_sender,
                &mut active,
            ) {
                return;
            }
        }

        let failure = if let Some(current) = active.as_mut() {
            process_worker_audio(current, detector.as_mut(), &event_sender, &mut source_batch)
        } else {
            None
        };
        if let Some((generation, message)) = failure {
            send_worker_event(
                &event_sender,
                VadMonitorEvent::WorkerFailed {
                    generation,
                    message,
                },
            );
            active = None;
        }

        match commands.recv_timeout(WORKER_POLL_INTERVAL) {
            Ok(command) => {
                if !apply_worker_command(
                    command,
                    detector.as_mut(),
                    &settings,
                    &event_sender,
                    &mut active,
                ) {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn apply_worker_command(
    command: WorkerCommand,
    detector: &mut dyn SpeechDetector,
    settings: &VadSettings,
    event_sender: &EventSender<VadMonitorEvent>,
    active: &mut Option<WorkerGeneration>,
) -> bool {
    match command {
        WorkerCommand::Activate {
            generation,
            queue,
            source_sample_rate,
        } => {
            detector.reset();
            match AudioProcessor::new(source_sample_rate, settings.clone()) {
                Ok(processor) => {
                    *active = Some(WorkerGeneration::new(generation, queue, processor));
                }
                Err(message) => {
                    *active = None;
                    send_worker_event(
                        event_sender,
                        VadMonitorEvent::WorkerFailed {
                            generation,
                            message,
                        },
                    );
                }
            }
            true
        }
        WorkerCommand::Deactivate { generation } => {
            if active.as_ref().map(|current| current.generation) == Some(generation) {
                *active = None;
            }
            true
        }
        WorkerCommand::Shutdown => false,
    }
}

struct WorkerGeneration {
    generation: CaptureGeneration,
    queue: Arc<SampleQueue>,
    processor: AudioProcessor,
    activated_at: Instant,
    last_audio_at: Option<Instant>,
    waiting_notice_sent: bool,
    stalled_notice_sent: bool,
    silence_since: Option<Instant>,
    last_silence_report: Option<Instant>,
}

impl WorkerGeneration {
    fn new(
        generation: CaptureGeneration,
        queue: Arc<SampleQueue>,
        processor: AudioProcessor,
    ) -> Self {
        Self {
            generation,
            queue,
            processor,
            activated_at: Instant::now(),
            last_audio_at: None,
            waiting_notice_sent: false,
            stalled_notice_sent: false,
            silence_since: None,
            last_silence_report: None,
        }
    }
}

fn process_worker_audio(
    active: &mut WorkerGeneration,
    detector: &mut dyn SpeechDetector,
    event_sender: &EventSender<VadMonitorEvent>,
    source_batch: &mut Vec<f32>,
) -> Option<(CaptureGeneration, String)> {
    let drained = active
        .queue
        .drain_into(source_batch, MAX_WORKER_DRAIN_SAMPLES);
    let now = Instant::now();
    if drained == 0 {
        report_missing_audio(active, now, event_sender);
        report_nonspeech_elapsed(active, now, event_sender);
        return None;
    }

    active.last_audio_at = Some(now);
    active.waiting_notice_sent = false;
    active.stalled_notice_sent = false;
    let result = match active.processor.push(source_batch, detector) {
        Ok(result) => result,
        Err(message) => return Some((active.generation, message)),
    };

    for transition in result.transitions {
        match transition {
            SpeechTransition::Started => {
                active.silence_since = None;
                active.last_silence_report = None;
                send_worker_event(
                    event_sender,
                    VadMonitorEvent::SpeechStarted {
                        generation: active.generation,
                    },
                );
            }
            SpeechTransition::Resumed => {
                active.silence_since = None;
                active.last_silence_report = None;
                send_worker_event(
                    event_sender,
                    VadMonitorEvent::SpeechResumed {
                        generation: active.generation,
                    },
                );
            }
            SpeechTransition::Paused => {
                active.silence_since = Some(now);
                active.last_silence_report = None;
                send_worker_event(
                    event_sender,
                    VadMonitorEvent::SpeechPaused {
                        generation: active.generation,
                    },
                );
            }
        }
    }

    if result.processed_frames != 0 && !active.processor.is_speaking() {
        active.silence_since.get_or_insert(now);
    }
    report_nonspeech_elapsed(active, now, event_sender);
    None
}

fn report_missing_audio(
    active: &mut WorkerGeneration,
    now: Instant,
    event_sender: &EventSender<VadMonitorEvent>,
) {
    match active.last_audio_at {
        None if !active.waiting_notice_sent
            && now.duration_since(active.activated_at) >= NO_AUDIO_NOTICE_AFTER =>
        {
            active.waiting_notice_sent = true;
            send_worker_event(
                event_sender,
                VadMonitorEvent::WaitingForAudio {
                    generation: active.generation,
                },
            );
        }
        Some(last_audio)
            if !active.stalled_notice_sent
                && now.duration_since(last_audio) >= NO_AUDIO_NOTICE_AFTER =>
        {
            active.stalled_notice_sent = true;
            send_worker_event(
                event_sender,
                VadMonitorEvent::AudioStalled {
                    generation: active.generation,
                },
            );
        }
        _ => {}
    }
}

fn report_nonspeech_elapsed(
    active: &mut WorkerGeneration,
    now: Instant,
    event_sender: &EventSender<VadMonitorEvent>,
) {
    let Some(silence_since) = active.silence_since else {
        return;
    };
    let Some(last_audio) = active.last_audio_at else {
        return;
    };
    // No callback is not evidence of silence. Only report elapsed non-speech
    // while fresh input samples continue to reach the worker.
    if now.duration_since(last_audio) >= NO_AUDIO_NOTICE_AFTER {
        return;
    }
    if active
        .last_silence_report
        .is_some_and(|last| now.duration_since(last) < active.processor.settings.status_interval)
    {
        return;
    }
    if now.duration_since(silence_since) < active.processor.settings.status_interval {
        return;
    }

    active.last_silence_report = Some(now);
    send_worker_event(
        event_sender,
        VadMonitorEvent::NonspeechElapsed {
            generation: active.generation,
            elapsed: now.duration_since(silence_since),
        },
    );
}

fn send_worker_event(event_sender: &EventSender<VadMonitorEvent>, event: VadMonitorEvent) {
    // This runs only on the dedicated blocking worker, never on CPAL's audio
    // callback. Bounded backpressure preserves critical failures instead of
    // silently leaving a broken worker beside an active microphone stream.
    // `run_vad_monitor` drops the receiver before awaiting shutdown, so a
    // worker cannot remain blocked on a final status during teardown.
    let _ = event_sender.blocking_send(event);
}

/// Streaming conversion from actual microphone rate to Silero's required
/// 16 kHz. The resampler and its pending source samples live for a whole
/// capture generation; they are never reset per CPAL callback.
enum StreamingResampler {
    Direct,
    Fft {
        // The FFT plan is substantial but created only once for an unmuted
        // generation. Boxing keeps the direct 16 kHz path compact without
        // moving any allocation into CPAL's callback.
        resampler: Box<Fft<f32>>,
        pending: VecDeque<f32>,
        input: Vec<f32>,
    },
}

impl StreamingResampler {
    fn new(source_sample_rate: u32) -> Result<Self, String> {
        validate_capture_format(source_sample_rate, 1)?;
        if source_sample_rate == VAD_SAMPLE_RATE {
            return Ok(Self::Direct);
        }
        let resampler = Box::new(
            Fft::<f32>::new(
                source_sample_rate as usize,
                VAD_SAMPLE_RATE as usize,
                1_024,
                1,
                FixedSync::Input,
            )
            .map_err(|error| {
                format!("could not configure continuous microphone resampling: {error}")
            })?,
        );
        Ok(Self::Fft {
            resampler,
            pending: VecDeque::with_capacity(MAX_RESAMPLER_PENDING_SAMPLES),
            input: Vec::with_capacity(MAX_RESAMPLER_PENDING_SAMPLES),
        })
    }

    fn push(&mut self, source: &[f32], output: &mut Vec<f32>) -> Result<(), String> {
        output.clear();
        match self {
            Self::Direct => {
                if source.len() > MAX_RESAMPLED_DRAIN_SAMPLES {
                    return Err("VAD direct audio batch exceeded its fixed processing bound".into());
                }
                output.extend_from_slice(source);
            }
            Self::Fft {
                resampler,
                pending,
                input,
            } => {
                let remaining = MAX_RESAMPLER_PENDING_SAMPLES.saturating_sub(pending.len());
                if source.len() > remaining {
                    return Err(
                        "continuous resampler backlog exceeded its fixed memory bound".into(),
                    );
                }
                pending.extend(source.iter().copied());
                while pending.len() >= resampler.input_frames_next() {
                    let frames = resampler.input_frames_next();
                    input.clear();
                    input.extend(pending.drain(..frames));
                    let buffer = InterleavedOwned::new_from(std::mem::take(input), 1, frames)
                        .map_err(|error| {
                            format!("could not prepare a microphone chunk for resampling: {error}")
                        })?;
                    let converted = resampler.process(&buffer, None).map_err(|error| {
                        format!("could not resample continuous microphone audio: {error}")
                    })?;
                    *input = buffer.take_data();
                    let converted = converted.take_data();
                    if output
                        .len()
                        .checked_add(converted.len())
                        .is_none_or(|length| length > MAX_RESAMPLED_DRAIN_SAMPLES)
                    {
                        return Err(
                            "resampled VAD batch exceeded its fixed processing bound".into()
                        );
                    }
                    output.extend(converted);
                }
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn input_frames_next(&self) -> usize {
        match self {
            Self::Direct => 1,
            Self::Fft { resampler, .. } => resampler.input_frames_next(),
        }
    }
}

struct AudioProcessor {
    settings: VadSettings,
    resampler: StreamingResampler,
    resampled: Vec<f32>,
    pending_frame: VecDeque<f32>,
    frame: [f32; VAD_FRAME_SAMPLES],
    smoother: SpeechSmoother,
}

impl AudioProcessor {
    fn new(source_sample_rate: u32, settings: VadSettings) -> Result<Self, String> {
        Ok(Self {
            resampler: StreamingResampler::new(source_sample_rate)?,
            resampled: Vec::with_capacity(MAX_RESAMPLED_DRAIN_SAMPLES),
            pending_frame: VecDeque::with_capacity(VAD_FRAME_SAMPLES * 2),
            frame: [0.0; VAD_FRAME_SAMPLES],
            smoother: SpeechSmoother::new(&settings),
            settings,
        })
    }

    fn push(
        &mut self,
        source: &[f32],
        detector: &mut dyn SpeechDetector,
    ) -> Result<ProcessResult, String> {
        self.resampler.push(source, &mut self.resampled)?;
        let mut result = ProcessResult::default();
        for sample in self.resampled.iter().copied() {
            self.pending_frame.push_back(sample);
            if self.pending_frame.len() != VAD_FRAME_SAMPLES {
                continue;
            }

            for slot in &mut self.frame {
                *slot = self
                    .pending_frame
                    .pop_front()
                    .expect("VAD frame length was checked before draining");
            }
            let probability = detector.probability(&self.frame)?;
            if !probability.is_finite() {
                return Err("speech detector returned a non-finite probability".into());
            }
            result.processed_frames += 1;
            if let Some(transition) = self.smoother.observe(probability) {
                result.transitions.push(transition);
            }
        }
        Ok(result)
    }

    fn is_speaking(&self) -> bool {
        self.smoother.is_speaking()
    }
}

#[derive(Default)]
struct ProcessResult {
    processed_frames: usize,
    transitions: Vec<SpeechTransition>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpeechTransition {
    Started,
    Resumed,
    Paused,
}

struct SpeechSmoother {
    threshold: f32,
    speech_frames_required: usize,
    silence_frames_required: usize,
    speech_frames: usize,
    silence_frames: usize,
    speaking: bool,
    has_spoken: bool,
}

impl SpeechSmoother {
    fn new(settings: &VadSettings) -> Self {
        Self {
            threshold: settings.threshold,
            speech_frames_required: settings.speech_frames,
            silence_frames_required: settings.silence_frames,
            speech_frames: 0,
            silence_frames: 0,
            speaking: false,
            has_spoken: false,
        }
    }

    fn observe(&mut self, probability: f32) -> Option<SpeechTransition> {
        if probability >= self.threshold {
            self.speech_frames = self.speech_frames.saturating_add(1);
            self.silence_frames = 0;
            if !self.speaking && self.speech_frames >= self.speech_frames_required {
                self.speaking = true;
                let transition = if self.has_spoken {
                    SpeechTransition::Resumed
                } else {
                    self.has_spoken = true;
                    SpeechTransition::Started
                };
                return Some(transition);
            }
        } else {
            self.silence_frames = self.silence_frames.saturating_add(1);
            self.speech_frames = 0;
            if self.speaking && self.silence_frames >= self.silence_frames_required {
                self.speaking = false;
                return Some(SpeechTransition::Paused);
            }
        }
        None
    }

    fn is_speaking(&self) -> bool {
        self.speaking
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use tokio::sync::mpsc;

    use crate::protocol::DictationInput;

    use super::{
        AudioProcessor, CaptureGeneration, MonitorCapture, MonitorCaptureDetails, MonitorEffect,
        MonitorEvent, MonitorPhase, SampleQueue, SileroDetector, SpeechDetector, SpeechSmoother,
        SpeechTransition, StreamingResampler, VAD_FRAME_SAMPLES, VadMonitorAction, VadMonitorEvent,
        VadMonitorOutput, VadMonitorSession, VadSettings, WorkerCommand,
    };

    fn settings() -> VadSettings {
        VadSettings {
            threshold: 0.5,
            speech_frames: 2,
            silence_frames: 3,
            status_interval: std::time::Duration::from_secs(1),
        }
    }

    struct FakeDetector {
        probabilities: VecDeque<Result<f32, String>>,
        resets: usize,
        frames: Vec<Vec<f32>>,
    }

    impl FakeDetector {
        fn with_probabilities(values: impl IntoIterator<Item = f32>) -> Self {
            Self {
                probabilities: values.into_iter().map(Ok).collect(),
                resets: 0,
                frames: Vec::new(),
            }
        }
    }

    impl SpeechDetector for FakeDetector {
        fn reset(&mut self) {
            self.resets += 1;
        }

        fn probability(&mut self, frame: &[f32]) -> Result<f32, String> {
            self.frames.push(frame.to_vec());
            self.probabilities.pop_front().unwrap_or(Ok(0.0))
        }
    }

    #[test]
    fn arbitrary_audio_chunks_are_framed_continuously() {
        let mut processor = AudioProcessor::new(16_000, settings()).expect("processor");
        let mut detector = FakeDetector::with_probabilities([0.2, 0.8, 0.9]);

        let first = processor
            .push(&vec![0.25; 511], &mut detector)
            .expect("partial frame");
        assert_eq!(first.processed_frames, 0);
        let second = processor
            .push(&vec![0.25; 1 + 512 + 512], &mut detector)
            .expect("continued frames");

        assert_eq!(second.processed_frames, 3);
        assert_eq!(detector.frames.len(), 3);
        assert!(detector.frames.iter().all(|frame| frame.len() == 512));
        assert_eq!(second.transitions, vec![SpeechTransition::Started]);
    }

    #[test]
    fn streaming_resampler_keeps_partial_callback_audio() {
        let mut resampler = StreamingResampler::new(48_000).expect("resampler");
        let needed = resampler.input_frames_next();
        let mut output = Vec::new();

        resampler
            .push(&vec![0.1; needed - 1], &mut output)
            .expect("partial source audio");
        assert!(output.is_empty());
        resampler
            .push(&[0.1], &mut output)
            .expect("completed source chunk");

        assert!(!output.is_empty());
        assert!(output.len() < needed);
    }

    #[test]
    fn speech_smoothing_prevents_single_frame_flicker() {
        let mut smoother = SpeechSmoother::new(&settings());
        assert_eq!(smoother.observe(0.9), None);
        assert_eq!(smoother.observe(0.9), Some(SpeechTransition::Started));
        assert_eq!(smoother.observe(0.1), None);
        assert_eq!(smoother.observe(0.9), None);
        assert_eq!(smoother.observe(0.1), None);
        assert_eq!(smoother.observe(0.1), None);
        assert_eq!(smoother.observe(0.1), Some(SpeechTransition::Paused));
        assert_eq!(smoother.observe(0.9), None);
        assert_eq!(smoother.observe(0.9), Some(SpeechTransition::Resumed));
    }

    #[test]
    fn sample_queue_is_bounded_and_can_be_cleared() {
        let queue = SampleQueue::new(3).expect("queue");
        queue.append(&[0.1_f32, 0.2, 0.3, 0.4], 1);
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.take_dropped(), 1);
        queue.clear();
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.take_dropped(), 0);
    }

    #[test]
    fn controller_ignores_stale_activity_after_mute_and_unmute() {
        let mut controller = super::MonitorController::new();
        let first = controller.handle(MonitorEvent::Start);
        let first_generation = match first.as_slice() {
            [MonitorEffect::StartCapture { generation }] => *generation,
            _ => panic!("expected initial capture"),
        };
        controller.handle(MonitorEvent::Runtime(VadMonitorEvent::CaptureStarted {
            generation: first_generation,
            details: MonitorCaptureDetails {
                microphone: "synthetic".into(),
                format: "16 kHz".into(),
            },
        }));
        let muted = controller.handle(MonitorEvent::ToggleMute);
        assert!(matches!(controller.phase, MonitorPhase::Muted));
        assert!(muted
            .iter()
            .any(|effect| matches!(effect, MonitorEffect::StopCapture { generation } if *generation == first_generation)));

        assert!(
            controller
                .handle(MonitorEvent::Runtime(VadMonitorEvent::SpeechStarted {
                    generation: first_generation,
                }))
                .is_empty()
        );

        let second = controller.handle(MonitorEvent::ToggleMute);
        let second_generation = match second.as_slice() {
            [MonitorEffect::StartCapture { generation }] => *generation,
            _ => panic!("expected resumed capture"),
        };
        assert_ne!(first_generation, second_generation);
    }

    #[derive(Default)]
    struct FakeCaptureState {
        starts: usize,
        stops: usize,
        active: Option<CaptureGeneration>,
    }

    struct FakeCapture {
        state: Arc<Mutex<FakeCaptureState>>,
    }

    impl MonitorCapture for FakeCapture {
        fn microphone_label(&self) -> Result<String, String> {
            Ok("synthetic microphone (16000 Hz, 1 channel)".into())
        }

        fn start(
            &mut self,
            generation: CaptureGeneration,
            _worker_commands: &std::sync::mpsc::Sender<WorkerCommand>,
        ) -> Result<MonitorCaptureDetails, String> {
            let mut state = self.state.lock().expect("fake capture lock");
            state.starts += 1;
            state.active = Some(generation);
            Ok(MonitorCaptureDetails {
                microphone: "synthetic microphone".into(),
                format: "16000 Hz, 1 channel, F32".into(),
            })
        }

        fn stop(&mut self, generation: CaptureGeneration) {
            let mut state = self.state.lock().expect("fake capture lock");
            if state.active == Some(generation) {
                state.stops += 1;
                state.active = None;
            }
        }

        fn poll(&mut self) -> Vec<VadMonitorEvent> {
            Vec::new()
        }

        fn shutdown(&mut self) {
            let mut state = self.state.lock().expect("fake capture lock");
            if state.active.take().is_some() {
                state.stops += 1;
            }
        }
    }

    #[tokio::test]
    async fn mute_unmute_and_shutdown_release_each_fake_capture_once() {
        let state = Arc::new(Mutex::new(FakeCaptureState::default()));
        let capture = FakeCapture {
            state: state.clone(),
        };
        let (events, _receiver) = mpsc::channel(8);
        let detector = Box::new(FakeDetector::with_probabilities([]));
        let mut session = VadMonitorSession::with_services(capture, detector, settings(), events);

        let start_actions = session.start();
        assert!(start_actions.iter().any(|action| {
            matches!(
                action,
                VadMonitorAction::Display(VadMonitorOutput::MicrophoneOn { .. })
            )
        }));
        session.handle_key(DictationInput::ToggleMute);
        session.handle_key(DictationInput::ToggleMute);
        session.shutdown().await;

        let state = state.lock().expect("fake capture lock");
        assert_eq!(state.starts, 2);
        assert_eq!(state.stops, 2);
        assert!(state.active.is_none());
    }

    #[tokio::test]
    async fn capture_failure_stops_the_current_fake_capture() {
        let state = Arc::new(Mutex::new(FakeCaptureState::default()));
        let capture = FakeCapture {
            state: state.clone(),
        };
        let (events, _receiver) = mpsc::channel(8);
        let detector = Box::new(FakeDetector::with_probabilities([]));
        let mut session = VadMonitorSession::with_services(capture, detector, settings(), events);
        session.start();
        let generation = state
            .lock()
            .expect("fake capture lock")
            .active
            .expect("active");

        let actions = session.handle_event(VadMonitorEvent::CaptureFailed {
            generation,
            message: "synthetic disconnect".into(),
        });
        assert!(actions.iter().any(|action| {
            matches!(action, VadMonitorAction::Display(VadMonitorOutput::Diagnostic(message)) if message.contains("disconnect"))
        }));
        assert_eq!(state.lock().expect("fake capture lock").stops, 1);
        session.shutdown().await;
    }

    #[tokio::test]
    async fn worker_failure_releases_capture_and_a_duplicate_event_is_stale() {
        let state = Arc::new(Mutex::new(FakeCaptureState::default()));
        let capture = FakeCapture {
            state: state.clone(),
        };
        let (events, _receiver) = mpsc::channel(8);
        let detector = Box::new(FakeDetector::with_probabilities([]));
        let mut session = VadMonitorSession::with_services(capture, detector, settings(), events);
        session.start();
        let generation = state
            .lock()
            .expect("fake capture lock")
            .active
            .expect("active");

        let first = session.handle_event(VadMonitorEvent::WorkerFailed {
            generation,
            message: "synthetic detector failure".into(),
        });
        assert!(first.iter().any(|action| {
            matches!(action, VadMonitorAction::Display(VadMonitorOutput::Diagnostic(message)) if message.contains("detector failure"))
        }));
        assert_eq!(state.lock().expect("fake capture lock").stops, 1);

        assert!(
            session
                .handle_event(VadMonitorEvent::WorkerFailed {
                    generation,
                    message: "late duplicate".into(),
                })
                .is_empty()
        );
        session.shutdown().await;
        assert_eq!(state.lock().expect("fake capture lock").stops, 1);
    }

    #[test]
    #[ignore = "requires the locally built ONNX Runtime bundle, but no microphone"]
    fn manual_silero_vad_smoke_without_microphone() {
        let mut detector = SileroDetector::new().expect("embedded Silero VAD should initialize");
        let probability = detector
            .probability(&[0.0; VAD_FRAME_SAMPLES])
            .expect("Silero VAD should process one silent frame");
        assert!(probability.is_finite());
    }
}
