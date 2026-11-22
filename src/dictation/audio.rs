//! Microphone acquisition and audio preparation for local dictation.
//!
//! CPAL owns the platform input stream. The callback does only format
//! conversion, mono downmixing, and a wait-free push into a lock-free ring
//! buffer; it never blocks, transcribes, or touches application state. Once capture ends, the stream is dropped before
//! a selected consumer converts the retained in-memory samples. Whisper asks
//! for 16 kHz; a direct audio backend receives the actual capture rate.

use std::{
    env,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
    },
    time::{Duration, Instant},
};

use cpal::{
    Device, FromSample, Sample, SampleFormat, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use rubato::{Fft, FixedSync, Resampler, audioadapter_buffers::owned::InterleavedOwned};
use tokio::{sync::mpsc::Sender as EventSender, task::JoinHandle};

use crate::protocol::{CapturedAudio, DictationEvent, RecordingId};
use crate::telemetry::{self, Interaction};

use super::controller::MAX_RECORDING_DURATION;

/// whisper.cpp's required input layout: 16 kHz, mono, f32 PCM.
pub const WHISPER_SAMPLE_RATE: u32 = 16_000;
const MAX_RAW_SAMPLES: usize = 8_000_000;
const MAX_WHISPER_SAMPLES: usize = WHISPER_SAMPLE_RATE as usize * 30;
const STREAM_START_TIMEOUT: Duration = Duration::from_secs(2);
const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MINIMUM_AUDIBLE_PEAK: f32 = 0.001;
/// How far (in seconds of audio) the real-time callback may run ahead of the
/// capture thread before frames are lost. The capture thread drains every
/// `CONTROL_POLL_INTERVAL`, so this leaves a large margin for a busy CPU.
const CAPTURE_RING_SECONDS: usize = 2;
/// Recordings shorter than this are treated as a tap (or a sticky button), not
/// a question: Whisper is not run and the person is asked to hold longer.
pub(crate) const MIN_RECORDING_DURATION: Duration = Duration::from_millis(300);

/// A non-sensitive description of the configured input before a stream opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MicrophoneDescription {
    name: String,
    sample_rate: u32,
    channels: u16,
}

/// One CPAL input device available for an explicit microphone selection.
///
/// `id` is CPAL's host-qualified identifier and is the preferred selector;
/// `name` remains useful for a person reading the list. Listing does not open
/// a stream or begin capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputDevice {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// Lists OS input devices without starting microphone capture.
pub fn list_input_devices() -> Result<Vec<InputDevice>, String> {
    let host = cpal::default_host();
    let default_id = host
        .default_input_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    let mut devices = host
        .input_devices()
        .map_err(|error| format!("could not enumerate microphone devices: {error}"))?
        .map(|device| {
            let id = device
                .id()
                .map_err(|error| format!("could not read microphone identifier: {error}"))?
                .to_string();
            let name = device
                .description()
                .map_err(|error| format!("could not read microphone description: {error}"))?
                .name()
                .to_owned();
            Ok(InputDevice {
                is_default: default_id.as_deref() == Some(id.as_str()),
                id,
                name,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    devices.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(devices)
}

impl MicrophoneDescription {
    pub(crate) fn new(name: String, sample_rate: u32, channels: u16) -> Self {
        Self {
            name,
            sample_rate,
            channels,
        }
    }

    pub fn label(&self) -> String {
        format!(
            "{} ({} Hz, {} channel{})",
            self.name,
            self.sample_rate,
            self.channels,
            if self.channels == 1 { "" } else { "s" }
        )
    }
}

/// The actual hardware boundary. A fake implementation makes normal tests
/// independent of a microphone and lets the runtime's cleanup path be tested.
pub trait Recorder: Send + Sync {
    fn selected_microphone(&self) -> Result<MicrophoneDescription, String>;

    fn start(
        &self,
        recording_id: RecordingId,
        event_sender: EventSender<DictationEvent>,
        interaction: Option<Interaction>,
    ) -> Result<Box<dyn RecordingHandle>, String>;
}

/// Controls one already-requested recording. `wait` is separate from cancel:
/// cancelling a blocking backend asks it to clean up, but does not pretend to
/// kill a platform audio call immediately.
pub trait RecordingHandle: Send {
    fn finish(&self);
    fn cancel(&self);
    /// Requests a best-effort in-memory audio snapshot while capture remains
    /// active. Backends that cannot support it keep the default no-op.
    fn snapshot(&self) {}
    fn wait(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

/// CPAL implementation selected by an exact device identifier or name from the
/// launch boundary, with the existing environment fallback for compatibility.
#[derive(Clone, Debug, Default)]
pub struct CpalRecorder {
    requested_name: Option<String>,
}

impl CpalRecorder {
    pub fn from_environment() -> Self {
        let requested_name = env::var("INDEPENDENCE_MICROPHONE")
            .ok()
            .filter(|name| !name.trim().is_empty());
        Self::with_requested_name(requested_name)
    }

    /// Constructs a recorder with an exact CPAL device identifier or name.
    /// Command-line/profile resolution happens above this hardware boundary;
    /// the recorder itself never changes global audio settings.
    pub fn with_requested_name(requested_name: Option<String>) -> Self {
        Self { requested_name }
    }

    fn select_device(&self) -> Result<Device, String> {
        let host = cpal::default_host();
        if let Some(requested_name) = &self.requested_name {
            let devices = host
                .input_devices()
                .map_err(|error| format!("could not enumerate microphone devices: {error}"))?;
            for device in devices {
                let name = device
                    .description()
                    .map_err(|error| format!("could not read microphone description: {error}"))?
                    .name()
                    .to_owned();
                let id = device
                    .id()
                    .map_err(|error| format!("could not read microphone identifier: {error}"))?
                    .to_string();
                if &name == requested_name || &id == requested_name {
                    return Ok(device);
                }
            }

            return Err(format!(
                "microphone `{requested_name}` was not found; choose an exact ID/name from --audio-input-devices"
            ));
        }

        host.default_input_device().ok_or_else(|| {
            "no default microphone is available; connect or select an input device before recording"
                .into()
        })
    }

    fn description_for(&self, device: &Device) -> Result<MicrophoneDescription, String> {
        let name = device
            .description()
            .map_err(|error| format!("could not read microphone description: {error}"))?
            .name()
            .to_owned();
        let config = device
            .default_input_config()
            .map_err(|error| format!("could not query microphone format: {error}"))?;
        Ok(MicrophoneDescription::new(
            name,
            config.sample_rate(),
            config.channels(),
        ))
    }

    fn run_capture(
        &self,
        recording_id: RecordingId,
        event_sender: EventSender<DictationEvent>,
        commands: Receiver<CaptureCommand>,
        interaction: Option<Interaction>,
    ) {
        let started = Instant::now();
        let outcome = self.capture(recording_id, &event_sender, &commands, interaction.as_ref());
        if let Some(interaction) = &interaction {
            let mut fields = telemetry::fields([(
                "capture_lifecycle_duration_ms",
                serde_json::json!(duration_ms(started.elapsed())),
            )]);
            let outcome_label = match &outcome {
                Ok(CaptureOutcome::Audio(_)) => "completed",
                Ok(CaptureOutcome::Cancelled) => "cancelled",
                Ok(CaptureOutcome::TooShort { recorded }) => {
                    fields.insert(
                        "recorded_ms".into(),
                        serde_json::json!(duration_ms(*recorded)),
                    );
                    "too_short"
                }
                Err(failure) => {
                    failure.add_fields(&mut fields);
                    "failed"
                }
            };
            interaction.event("microphone.capture.lifecycle", outcome_label, fields);
        }
        let event = match outcome {
            Ok(CaptureOutcome::Audio(audio)) => DictationEvent::RecordingFinished {
                recording_id,
                audio,
            },
            Ok(CaptureOutcome::Cancelled) => DictationEvent::RecordingCancelled { recording_id },
            Ok(CaptureOutcome::TooShort { .. }) => {
                DictationEvent::RecordingTooShort { recording_id }
            }
            Err(failure) => DictationEvent::RecordingFailed {
                recording_id,
                message: failure.message,
            },
        };
        let _ = event_sender.blocking_send(event);
    }

    fn capture(
        &self,
        recording_id: RecordingId,
        event_sender: &EventSender<DictationEvent>,
        commands: &Receiver<CaptureCommand>,
        interaction: Option<&Interaction>,
    ) -> Result<CaptureOutcome, CaptureFailure> {
        if command_requests_cancel(commands) {
            return Ok(CaptureOutcome::Cancelled);
        }

        let device = self.select_device().map_err(CaptureFailure::device)?;
        let supported_config = device.default_input_config().map_err(|error| {
            CaptureFailure::device(format!("could not query microphone format: {error}"))
        })?;
        let channels = usize::from(supported_config.channels());
        let sample_rate = supported_config.sample_rate();
        if channels == 0 || sample_rate == 0 {
            return Err(CaptureFailure::device(
                "microphone reported an unusable zero-channel or zero-rate format",
            ));
        }

        let sample_limit = bounded_raw_sample_limit(sample_rate).map_err(CaptureFailure::device)?;
        let (producer, mut recording) =
            capture_channel(sample_rate, sample_limit).map_err(CaptureFailure::device)?;
        let stream_error = Arc::new(Mutex::new(None));
        let sample_format = supported_config.sample_format();
        let stream_config = supported_config.into();

        if command_requests_cancel(commands) {
            return Ok(CaptureOutcome::Cancelled);
        }

        let stream = build_input_stream(
            &device,
            &stream_config,
            sample_format,
            channels,
            producer,
            stream_error.clone(),
        )
        .map_err(CaptureFailure::device)?;

        if command_requests_cancel(commands) {
            drop(stream);
            return Ok(CaptureOutcome::Cancelled);
        }

        stream.play().map_err(|error| {
            CaptureFailure::device(format!("could not start microphone capture: {error}"))
        })?;
        if let Some(interaction) = interaction {
            interaction.mark("microphone.recording_started");
            interaction.event(
                "microphone.capture.format",
                "started",
                telemetry::fields([
                    ("sample_rate_hz", serde_json::json!(sample_rate)),
                    ("channels", serde_json::json!(channels)),
                    (
                        "sample_format",
                        serde_json::json!(sample_format.to_string()),
                    ),
                ]),
            );
        }
        if event_sender
            .blocking_send(DictationEvent::RecordingStarted { recording_id })
            .is_err()
        {
            drop(stream);
            return Ok(CaptureOutcome::Cancelled);
        }

        let started = Instant::now();
        let finish_reason = wait_for_finish(
            recording_id,
            event_sender,
            commands,
            &stream_error,
            &mut recording,
            started,
        );

        // Dropping CPAL's stream ends its callback and releases the device
        // before we inspect samples, convert them for a consumer, or invoke
        // any recognizer/backend.
        drop(stream);
        let recording_duration = started.elapsed();
        if let Some(interaction) = interaction {
            interaction.mark("microphone.released");
            let mut fields = telemetry::fields([(
                "recording_duration_ms",
                serde_json::json!(duration_ms(recording_duration)),
            )]);
            let outcome = match &finish_reason {
                Ok(FinishReason::Finished) => "completed",
                Ok(FinishReason::Cancelled) => "cancelled",
                Err(failure) => {
                    failure.add_fields(&mut fields);
                    "failed"
                }
            };
            interaction.event("microphone.recording", outcome, fields);
        }

        if finish_reason? == FinishReason::Cancelled {
            return Ok(CaptureOutcome::Cancelled);
        }

        // The callback has stopped; collect anything it wrote after the last
        // poll before judging the recording.
        let samples = match recording.finish() {
            RecordingResult::Samples(samples) => samples,
            RecordingResult::TooShort { recorded } => {
                return Ok(CaptureOutcome::TooShort { recorded });
            }
            RecordingResult::Failed(failure) => return Err(failure),
        };
        let input_samples = samples.len();
        let preparation = interaction.map(|interaction| {
            interaction.stage(
                "audio.capture.validate",
                telemetry::fields([
                    ("source_sample_rate_hz", serde_json::json!(sample_rate)),
                    ("input_samples", serde_json::json!(input_samples)),
                ]),
            )
        });
        let audio = match prepare_captured_audio(samples, sample_rate) {
            Ok(audio) => audio,
            Err(error) => {
                if let Some(stage) = preparation {
                    stage.finish(
                        "failed",
                        telemetry::fields([(
                            "failure_class",
                            serde_json::json!(CaptureFailureClass::InvalidAudio.label()),
                        )]),
                    );
                }
                return Err(CaptureFailure::new(
                    CaptureFailureClass::InvalidAudio,
                    error,
                ));
            }
        };
        if let Some(stage) = preparation {
            stage.finish(
                "completed",
                telemetry::fields([
                    ("output_samples", serde_json::json!(audio.samples().len())),
                    (
                        "audio_duration_ms",
                        serde_json::json!(
                            (audio.samples().len() as u64).saturating_mul(1_000)
                                / u64::from(audio.sample_rate_hz())
                        ),
                    ),
                ]),
            );
        }
        Ok(CaptureOutcome::Audio(audio))
    }
}

impl Recorder for CpalRecorder {
    fn selected_microphone(&self) -> Result<MicrophoneDescription, String> {
        let device = self.select_device()?;
        self.description_for(&device)
    }

    fn start(
        &self,
        recording_id: RecordingId,
        event_sender: EventSender<DictationEvent>,
        interaction: Option<Interaction>,
    ) -> Result<Box<dyn RecordingHandle>, String> {
        let (command_sender, command_receiver) = mpsc::channel();
        let recorder = self.clone();
        let task = tokio::task::spawn_blocking(move || {
            recorder.run_capture(recording_id, event_sender, command_receiver, interaction);
        });
        Ok(Box::new(CpalRecordingHandle {
            command_sender,
            task,
        }))
    }
}

struct CpalRecordingHandle {
    command_sender: Sender<CaptureCommand>,
    task: JoinHandle<()>,
}

impl RecordingHandle for CpalRecordingHandle {
    fn finish(&self) {
        let _ = self.command_sender.send(CaptureCommand::Finish);
    }

    fn cancel(&self) {
        let _ = self.command_sender.send(CaptureCommand::Cancel);
    }

    fn snapshot(&self) {
        let _ = self.command_sender.send(CaptureCommand::Snapshot);
    }

    fn wait(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let CpalRecordingHandle { task, .. } = *self;
        Box::pin(async move {
            let _ = task.await;
        })
    }
}

#[derive(Clone, Copy, Debug)]
enum CaptureCommand {
    Finish,
    Cancel,
    Snapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FinishReason {
    Finished,
    Cancelled,
}

enum CaptureOutcome {
    Audio(CapturedAudio),
    Cancelled,
    /// The person released almost immediately (a tap, or a sticky button).
    /// This is not a device or recognition failure, and Whisper is not run.
    TooShort {
        recorded: Duration,
    },
}

/// Why a microphone capture failed, as a small fixed vocabulary for
/// diagnostics. It never carries audio, text, or device serial numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureFailureClass {
    /// The device could not be found, configured, opened, or started.
    DeviceUnavailable,
    /// The platform audio stream reported an error while recording.
    StreamError,
    /// The capture thread fell more than the ring-buffer length behind the
    /// real-time callback, so some audio was lost.
    DroppedFrames,
    /// The recording exceeded the bounded in-memory limit.
    BufferOverflow,
    /// The audio was silent, empty, or contained invalid samples.
    InvalidAudio,
    /// A shared error slot was poisoned by a panicking thread.
    LockPoisoned,
}

impl CaptureFailureClass {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::DeviceUnavailable => "device_unavailable",
            Self::StreamError => "stream_error",
            Self::DroppedFrames => "dropped_frames",
            Self::BufferOverflow => "buffer_overflow",
            Self::InvalidAudio => "invalid_audio",
            Self::LockPoisoned => "lock_poisoned",
        }
    }
}

/// A classified capture failure. `message` is the existing bounded
/// explanation; only `class` and `dropped_frames` reach telemetry.
#[derive(Debug)]
struct CaptureFailure {
    class: CaptureFailureClass,
    message: String,
    dropped_frames: usize,
}

impl CaptureFailure {
    fn new(class: CaptureFailureClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            dropped_frames: 0,
        }
    }

    fn device(message: impl Into<String>) -> Self {
        Self::new(CaptureFailureClass::DeviceUnavailable, message)
    }

    fn add_fields(&self, fields: &mut telemetry::Fields) {
        fields.insert(
            "failure_class".into(),
            serde_json::json!(self.class.label()),
        );
        if self.dropped_frames != 0 {
            fields.insert(
                "dropped_frames".into(),
                serde_json::json!(self.dropped_frames),
            );
        }
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn wait_for_finish(
    recording_id: RecordingId,
    event_sender: &EventSender<DictationEvent>,
    commands: &Receiver<CaptureCommand>,
    stream_error: &Mutex<Option<String>>,
    recording: &mut CaptureRecording,
    started: Instant,
) -> Result<FinishReason, CaptureFailure> {
    let mut last_reported_second = 0;
    loop {
        // Move whatever the real-time callback produced into the recording
        // owned by this thread. This never blocks the callback.
        recording.drain();

        if let Some(error) = stream_error
            .lock()
            .map_err(|_| {
                CaptureFailure::new(
                    CaptureFailureClass::LockPoisoned,
                    "microphone error state lock was poisoned",
                )
            })?
            .take()
        {
            return Err(CaptureFailure::new(
                CaptureFailureClass::StreamError,
                format!("microphone stream failed: {error}"),
            ));
        }

        let elapsed = started.elapsed();
        if elapsed >= MAX_RECORDING_DURATION {
            // The cap is enforced by the owner of the stream, not just by UI.
            // We notify the controller before returning the final samples.
            let _ =
                event_sender.blocking_send(DictationEvent::RecordingLimitReached { recording_id });
            return Ok(FinishReason::Finished);
        }

        let remaining = MAX_RECORDING_DURATION.saturating_sub(elapsed);
        let wait = CONTROL_POLL_INTERVAL.min(remaining);
        match commands.recv_timeout(wait) {
            Ok(CaptureCommand::Finish) => return Ok(FinishReason::Finished),
            Ok(CaptureCommand::Cancel) | Err(RecvTimeoutError::Disconnected) => {
                return Ok(FinishReason::Cancelled);
            }
            Ok(CaptureCommand::Snapshot) => {
                // Partial recognition is opportunistic. In particular, an
                // early or quiet clip has no useful audio yet; leave final
                // capture validation authoritative and keep recording. Send
                // an acknowledgement either way so voice mode can retry once
                // speech begins instead of treating this request as pending.
                // This thread copies its own buffer, so a snapshot can never
                // make the real-time callback lose audio.
                let sample_rate = recording.sample_rate;
                let event = match recording
                    .snapshot()
                    .and_then(|samples| prepare_for_whisper(samples, sample_rate))
                {
                    Ok(audio) => DictationEvent::RecordingSnapshot {
                        recording_id,
                        audio,
                    },
                    Err(_) => DictationEvent::RecordingSnapshotUnavailable { recording_id },
                };
                let _ = event_sender.blocking_send(event);
            }
            Err(RecvTimeoutError::Timeout) => {
                let whole_seconds = started.elapsed().as_secs();
                if whole_seconds > last_reported_second {
                    last_reported_second = whole_seconds;
                    let _ = event_sender.blocking_send(DictationEvent::RecordingElapsed {
                        recording_id,
                        elapsed: Duration::from_secs(whole_seconds),
                    });
                }
            }
        }
    }
}

fn command_requests_cancel(commands: &Receiver<CaptureCommand>) -> bool {
    match commands.try_recv() {
        Ok(CaptureCommand::Cancel) | Err(TryRecvError::Disconnected) => true,
        Ok(CaptureCommand::Finish) | Ok(CaptureCommand::Snapshot) | Err(TryRecvError::Empty) => {
            false
        }
    }
}

/// Creates the two halves of one recording's audio path.
///
/// The CPAL callback receives the `CaptureProducer` and only ever pushes mono
/// samples into a lock-free single-producer/single-consumer ring buffer
/// (`rtrb`). The capture thread keeps the `CaptureRecording`, regularly drains
/// the ring into the recording it alone owns, and is the only code that copies
/// or inspects that recording. No lock is shared with the real-time callback,
/// so reading the recording can never make the callback drop audio.
fn capture_channel(
    sample_rate: u32,
    sample_limit: usize,
) -> Result<(CaptureProducer, CaptureRecording), String> {
    let ring_capacity = usize::try_from(sample_rate)
        .map_err(|_| "microphone sample rate does not fit this platform".to_owned())?
        .checked_mul(CAPTURE_RING_SECONDS)
        .ok_or_else(|| "microphone sample rate overflows the capture ring".to_owned())?
        .max(1);
    let mut samples = Vec::new();
    samples
        .try_reserve_exact(sample_limit)
        .map_err(|error| format!("could not reserve bounded microphone buffer: {error}"))?;
    let (producer, consumer) = rtrb::RingBuffer::new(ring_capacity);
    let dropped_frames = Arc::new(AtomicUsize::new(0));
    Ok((
        CaptureProducer {
            producer,
            dropped_frames: dropped_frames.clone(),
        },
        CaptureRecording {
            consumer,
            samples,
            sample_limit,
            sample_rate,
            overflowed: false,
            dropped_frames,
        },
    ))
}

/// The real-time half. Its only work is format conversion, mono downmixing,
/// and a wait-free push. If the ring is full, the missing frames are counted
/// (an atomic add) rather than blocking the audio thread.
struct CaptureProducer {
    producer: rtrb::Producer<f32>,
    dropped_frames: Arc<AtomicUsize>,
}

impl CaptureProducer {
    fn push_frames<T>(&mut self, data: &[T], channels: usize)
    where
        T: Sample,
        f32: FromSample<T>,
    {
        let frames = data.len() / channels;
        let writable = self.producer.slots().min(frames);
        if writable > 0
            && let Ok(chunk) = self.producer.write_chunk_uninit(writable)
        {
            chunk.fill_from_iter(data.chunks_exact(channels).take(writable).map(|frame| {
                frame
                    .iter()
                    .map(|sample| f32::from_sample(*sample))
                    .sum::<f32>()
                    / channels as f32
            }));
        }
        if writable < frames {
            self.dropped_frames
                .fetch_add(frames - writable, Ordering::Relaxed);
        }
    }
}

/// The capture-thread half: the only owner of the growing recording.
struct CaptureRecording {
    consumer: rtrb::Consumer<f32>,
    samples: Vec<f32>,
    sample_limit: usize,
    sample_rate: u32,
    overflowed: bool,
    dropped_frames: Arc<AtomicUsize>,
}

/// The authoritative end-of-recording judgement.
enum RecordingResult {
    Samples(Vec<f32>),
    TooShort { recorded: Duration },
    Failed(CaptureFailure),
}

impl CaptureRecording {
    /// Moves every sample currently in the ring into the owned recording.
    fn drain(&mut self) {
        let available = self.consumer.slots();
        if available == 0 {
            return;
        }
        let Ok(chunk) = self.consumer.read_chunk(available) else {
            return;
        };
        let (first, second) = chunk.as_slices();
        for sample in first.iter().chain(second) {
            if self.samples.len() == self.sample_limit {
                self.overflowed = true;
                break;
            }
            self.samples.push(*sample);
        }
        chunk.commit_all();
    }

    fn integrity_failure(&self) -> Option<CaptureFailure> {
        if self.overflowed {
            return Some(CaptureFailure::new(
                CaptureFailureClass::BufferOverflow,
                "recording exceeded the bounded in-memory audio limit",
            ));
        }
        let dropped_frames = self.dropped_frames.load(Ordering::Relaxed);
        if dropped_frames != 0 {
            let mut failure = CaptureFailure::new(
                CaptureFailureClass::DroppedFrames,
                "recording lost audio frames while buffering; try again",
            );
            failure.dropped_frames = dropped_frames;
            return Some(failure);
        }
        None
    }

    fn snapshot(&mut self) -> Result<Vec<f32>, String> {
        self.drain();
        if let Some(failure) = self.integrity_failure() {
            return Err(failure.message);
        }
        Ok(self.samples.clone())
    }

    /// Call only after the stream has been dropped, so no further samples can
    /// arrive. A too-short recording is reported before integrity problems:
    /// a tap is never worth a failure message.
    fn finish(mut self) -> RecordingResult {
        self.drain();
        let recorded = recorded_duration(self.samples.len(), self.sample_rate);
        if recorded < MIN_RECORDING_DURATION {
            return RecordingResult::TooShort { recorded };
        }
        if let Some(failure) = self.integrity_failure() {
            return RecordingResult::Failed(failure);
        }
        RecordingResult::Samples(std::mem::take(&mut self.samples))
    }
}

fn recorded_duration(samples: usize, sample_rate: u32) -> Duration {
    if sample_rate == 0 {
        return Duration::ZERO;
    }
    Duration::from_secs_f64(samples as f64 / f64::from(sample_rate))
}

fn build_input_stream(
    device: &Device,
    config: &cpal::StreamConfig,
    sample_format: SampleFormat,
    channels: usize,
    producer: CaptureProducer,
    stream_error: Arc<Mutex<Option<String>>>,
) -> Result<cpal::Stream, String> {
    macro_rules! build {
        ($sample:ty) => {
            build_stream_for_format::<$sample>(device, config, channels, producer, stream_error)
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
        // DSD is a one-bit bitstream rather than PCM samples. Treating it as
        // regular unsigned PCM would be an incorrect relabeling.
        _ => Err(format!(
            "microphone sample format `{sample_format}` is not supported for local dictation"
        )),
    }
}

fn build_stream_for_format<T>(
    device: &Device,
    config: &cpal::StreamConfig,
    channels: usize,
    mut producer: CaptureProducer,
    stream_error: Arc<Mutex<Option<String>>>,
) -> Result<cpal::Stream, String>
where
    T: Sample + SizedSample,
    f32: FromSample<T>,
{
    let error_slot = stream_error.clone();
    device
        .build_input_stream(
            *config,
            move |data: &[T], _| producer.push_frames(data, channels),
            move |error| {
                if let Ok(mut slot) = error_slot.lock() {
                    if slot.is_none() {
                        *slot = Some(error.to_string());
                    }
                }
            },
            Some(STREAM_START_TIMEOUT),
        )
        .map_err(|error| format!("could not open microphone input stream: {error}"))
}

fn bounded_raw_sample_limit(sample_rate: u32) -> Result<usize, String> {
    let required = usize::try_from(sample_rate)
        .map_err(|_| "microphone sample rate does not fit this platform".to_owned())?
        .checked_mul(MAX_RECORDING_DURATION.as_secs() as usize)
        .ok_or_else(|| "microphone sample rate overflows the recording limit".to_owned())?;
    if required > MAX_RAW_SAMPLES {
        return Err(format!(
            "microphone sample rate {sample_rate} Hz would exceed the {MAX_RAW_SAMPLES}-sample capture bound"
        ));
    }
    Ok(required)
}

/// Validates a completed mono f32 capture while retaining its actual sample
/// rate. This is shared by the text and direct-audio pipelines.
pub(crate) fn prepare_captured_audio(
    samples: Vec<f32>,
    source_sample_rate: u32,
) -> Result<CapturedAudio, String> {
    if samples.is_empty() {
        return Err("recording contained no audio samples".into());
    }
    if source_sample_rate == 0 {
        return Err("recording reported a zero sample rate".into());
    }
    if !samples.iter().all(|sample| sample.is_finite()) {
        return Err("recording contained non-finite audio samples".into());
    }
    if !samples
        .iter()
        .any(|sample| sample.abs() >= MINIMUM_AUDIBLE_PEAK)
    {
        return Err("recording was silent or too quiet to process".into());
    }

    Ok(CapturedAudio::with_sample_rate(samples, source_sample_rate))
}

/// Converts an in-memory mono clip to exactly what whisper.cpp expects.
/// This happens after the CPAL stream has been dropped, immediately before
/// the Whisper boundary rather than at microphone capture time.
pub(crate) fn prepare_for_whisper(
    samples: Vec<f32>,
    source_sample_rate: u32,
) -> Result<CapturedAudio, String> {
    let audio = prepare_captured_audio(samples, source_sample_rate)?;
    let (samples, source_sample_rate) = audio.into_parts();

    let resampled = if source_sample_rate == WHISPER_SAMPLE_RATE {
        samples
    } else {
        resample_mono(samples, source_sample_rate)?
    };

    if resampled.len() > MAX_WHISPER_SAMPLES {
        return Err("recording exceeded the 30-second transcription limit after resampling".into());
    }
    Ok(CapturedAudio::with_sample_rate(
        resampled,
        WHISPER_SAMPLE_RATE,
    ))
}

fn resample_mono(samples: Vec<f32>, source_sample_rate: u32) -> Result<Vec<f32>, String> {
    let input_frames = samples.len();
    let input = InterleavedOwned::new_from(samples, 1, input_frames)
        .map_err(|error| format!("could not prepare microphone samples for resampling: {error}"))?;
    let mut resampler = Fft::<f32>::new(
        source_sample_rate as usize,
        WHISPER_SAMPLE_RATE as usize,
        1_024,
        1,
        FixedSync::Both,
    )
    .map_err(|error| format!("could not configure audio resampling: {error}"))?;
    let output = resampler
        .process_all(&input, input_frames, None)
        .map_err(|error| format!("could not resample microphone audio: {error}"))?;
    Ok(output.take_data())
}

#[cfg(test)]
fn downmix_interleaved<T>(data: &[T], channels: usize) -> Vec<f32>
where
    T: Sample,
    f32: FromSample<T>,
{
    data.chunks_exact(channels)
        .map(|frame| {
            frame
                .iter()
                .map(|sample| f32::from_sample(*sample))
                .sum::<f32>()
                / channels as f32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        thread,
        time::Duration,
    };

    use super::{
        CaptureFailureClass, MIN_RECORDING_DURATION, RecordingResult, WHISPER_SAMPLE_RATE,
        bounded_raw_sample_limit, capture_channel, downmix_interleaved, prepare_for_whisper,
        recorded_duration,
    };

    const RATE: u32 = 16_000;

    fn expect_samples(result: RecordingResult) -> Vec<f32> {
        match result {
            RecordingResult::Samples(samples) => samples,
            RecordingResult::TooShort { recorded } => {
                panic!("unexpectedly too short: {recorded:?}")
            }
            RecordingResult::Failed(failure) => panic!("unexpected failure: {:?}", failure.class),
        }
    }

    #[test]
    fn concurrent_snapshots_never_make_the_callback_drop_audio() {
        // Reproduces the Pi failure shape: the "callback" keeps pushing while
        // the capture thread repeatedly copies the recording for live
        // transcription. With the old shared lock, contention dropped frames
        // and failed the whole recording.
        let (mut producer, mut recording) = capture_channel(RATE, RATE as usize * 30).unwrap();
        let total_frames = RATE as usize * 3;
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let callback_done = done.clone();
        let callback = thread::spawn(move || {
            let block: Vec<i16> = (0..160).map(|index| (index * 100) as i16).collect();
            for _ in 0..(total_frames / block.len()) {
                producer.push_frames(&block, 1);
                thread::sleep(Duration::from_micros(200));
            }
            callback_done.store(true, Ordering::Release);
        });
        let mut snapshots = 0;
        while !done.load(Ordering::Acquire) {
            let snapshot = recording.snapshot().expect("snapshot must not fail");
            assert!(snapshot.len() <= total_frames);
            snapshots += 1;
        }
        callback.join().unwrap();
        assert!(snapshots > 1);
        let samples = expect_samples(recording.finish());
        assert_eq!(samples.len(), total_frames, "every pushed frame is kept");
    }

    #[test]
    fn stereo_frames_are_downmixed_before_entering_the_ring() {
        let (mut producer, recording) = capture_channel(RATE, RATE as usize * 30).unwrap();
        let frames = RATE as usize; // one second, well above the minimum
        let stereo: Vec<f32> = (0..frames).flat_map(|_| [0.5_f32, -0.5]).collect();
        producer.push_frames(&stereo, 2);
        let samples = expect_samples(recording.finish());
        assert_eq!(samples.len(), frames);
        assert!(samples.iter().all(|sample| sample.abs() < 1e-6));
    }

    #[test]
    fn a_full_ring_counts_dropped_frames_instead_of_blocking() {
        let (mut producer, recording) = capture_channel(RATE, RATE as usize * 30).unwrap();
        // The ring holds two seconds. Never draining it while three seconds
        // arrive simulates a capture thread that stalled completely.
        let block = vec![0.25_f32; RATE as usize];
        for _ in 0..3 {
            producer.push_frames(&block, 1);
        }
        match recording.finish() {
            RecordingResult::Failed(failure) => {
                assert_eq!(failure.class, CaptureFailureClass::DroppedFrames);
                assert_eq!(failure.dropped_frames, RATE as usize);
            }
            _ => panic!("a stalled capture thread must be reported, not hidden"),
        }
    }

    #[test]
    fn exceeding_the_recording_limit_is_a_buffer_overflow() {
        let limit = RATE as usize; // one second
        let (mut producer, recording) = capture_channel(RATE, limit).unwrap();
        producer.push_frames(&vec![0.25_f32; limit + 10], 1);
        match recording.finish() {
            RecordingResult::Failed(failure) => {
                assert_eq!(failure.class, CaptureFailureClass::BufferOverflow);
            }
            _ => panic!("overflow must be reported"),
        }
    }

    #[test]
    fn recordings_under_the_minimum_are_too_short_not_failures() {
        for millis in [0_u64, 30, 100, 299] {
            let (mut producer, recording) = capture_channel(RATE, RATE as usize * 30).unwrap();
            let frames = (RATE as u64 * millis / 1_000) as usize;
            producer.push_frames(&vec![0.25_f32; frames], 1);
            assert!(
                matches!(recording.finish(), RecordingResult::TooShort { .. }),
                "{millis} ms should be too short"
            );
        }

        let (mut producer, recording) = capture_channel(RATE, RATE as usize * 30).unwrap();
        let frames = (RATE as u128 * MIN_RECORDING_DURATION.as_millis() / 1_000) as usize;
        producer.push_frames(&vec![0.25_f32; frames], 1);
        assert_eq!(expect_samples(recording.finish()).len(), frames);
    }

    #[test]
    fn a_tap_is_reported_as_too_short_even_if_frames_were_dropped() {
        let (mut producer, recording) = capture_channel(RATE, RATE as usize * 30).unwrap();
        producer.dropped_frames.store(5, Ordering::Relaxed);
        producer.push_frames(&vec![0.25_f32; 100], 1);
        assert!(matches!(
            recording.finish(),
            RecordingResult::TooShort { .. }
        ));
    }

    #[test]
    fn recorded_duration_uses_the_capture_rate() {
        assert_eq!(recorded_duration(48_000, 48_000), Duration::from_secs(1));
        assert_eq!(recorded_duration(4_800, 48_000), Duration::from_millis(100));
        assert_eq!(recorded_duration(10, 0), Duration::ZERO);
    }

    #[test]
    fn failure_classes_are_a_fixed_content_free_vocabulary() {
        let labels = [
            CaptureFailureClass::DeviceUnavailable,
            CaptureFailureClass::StreamError,
            CaptureFailureClass::DroppedFrames,
            CaptureFailureClass::BufferOverflow,
            CaptureFailureClass::InvalidAudio,
            CaptureFailureClass::LockPoisoned,
        ]
        .map(CaptureFailureClass::label);
        assert_eq!(
            labels,
            [
                "device_unavailable",
                "stream_error",
                "dropped_frames",
                "buffer_overflow",
                "invalid_audio",
                "lock_poisoned"
            ]
        );
    }

    #[test]
    fn integer_stereo_samples_are_converted_and_downmixed() {
        let samples = downmix_interleaved(&[i16::MAX, i16::MIN, 0, i16::MAX], 2);
        assert_eq!(samples.len(), 2);
        assert!(samples[0].abs() < 0.0001);
        assert!((samples[1] - 0.5).abs() < 0.01);
    }

    #[test]
    fn unsigned_samples_are_centered_before_downmixing() {
        let samples = downmix_interleaved(&[0_u16, u16::MAX], 2);
        assert_eq!(samples.len(), 1);
        assert!(samples[0].abs() < 0.0001);
    }

    #[test]
    fn resampling_uses_the_actual_source_rate() {
        let source_rate = 48_000;
        let input: Vec<f32> = (0..source_rate)
            .map(|index| {
                ((index as f32 / source_rate as f32) * 440.0 * std::f32::consts::TAU).sin()
            })
            .collect();
        let audio = prepare_for_whisper(input, source_rate).expect("synthetic tone should convert");
        assert!(
            (audio.sample_count() as isize - WHISPER_SAMPLE_RATE as isize).abs() <= 2,
            "actual output length was {}",
            audio.sample_count()
        );
    }

    #[test]
    fn silence_is_an_error_not_a_fabricated_transcript() {
        assert!(
            prepare_for_whisper(vec![0.0; 1_600], WHISPER_SAMPLE_RATE)
                .expect_err("silence must not be sent to recognition")
                .contains("silent")
        );
    }

    #[test]
    fn raw_capture_limit_is_bounded() {
        assert!(bounded_raw_sample_limit(48_000).is_ok());
        assert!(bounded_raw_sample_limit(384_000).is_err());
    }
}
