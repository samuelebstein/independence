//! Microphone acquisition and audio preparation for local dictation.
//!
//! CPAL owns the platform input stream. The callback does only format
//! conversion, mono downmixing, and bounded buffering; it never transcribes or
//! touches application state. Once capture ends, the stream is dropped before
//! Rubato resamples the retained in-memory samples for Whisper.

use std::{
    env,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
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

/// CPAL implementation selected by the optional `INDEPENDENCE_MICROPHONE`
/// exact device-name environment variable.
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
                "microphone `{requested_name}` was not found; unset INDEPENDENCE_MICROPHONE or choose an exact ID/name from --audio-input-devices"
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
            let outcome_label = match &outcome {
                Ok(CaptureOutcome::Audio(_)) => "completed",
                Ok(CaptureOutcome::Cancelled) => "cancelled",
                Err(_) => "device_failure",
            };
            interaction.event(
                "microphone.capture.lifecycle",
                outcome_label,
                telemetry::fields([(
                    "capture_lifecycle_duration_ms",
                    serde_json::json!(
                        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
                    ),
                )]),
            );
        }
        let event = match outcome {
            Ok(CaptureOutcome::Audio(audio)) => DictationEvent::RecordingFinished {
                recording_id,
                audio,
            },
            Ok(CaptureOutcome::Cancelled) => DictationEvent::RecordingCancelled { recording_id },
            Err(message) => DictationEvent::RecordingFailed {
                recording_id,
                message,
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
    ) -> Result<CaptureOutcome, String> {
        if command_requests_cancel(commands) {
            return Ok(CaptureOutcome::Cancelled);
        }

        let device = self.select_device()?;
        let supported_config = device
            .default_input_config()
            .map_err(|error| format!("could not query microphone format: {error}"))?;
        let channels = usize::from(supported_config.channels());
        let sample_rate = supported_config.sample_rate();
        if channels == 0 || sample_rate == 0 {
            return Err("microphone reported an unusable zero-channel or zero-rate format".into());
        }

        let sample_limit = bounded_raw_sample_limit(sample_rate)?;
        let captured = Arc::new(CaptureBuffer::new(sample_limit)?);
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
            captured.clone(),
            stream_error.clone(),
        )?;

        if command_requests_cancel(commands) {
            drop(stream);
            return Ok(CaptureOutcome::Cancelled);
        }

        stream
            .play()
            .map_err(|error| format!("could not start microphone capture: {error}"))?;
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
            captured.as_ref(),
            sample_rate,
            started,
        );

        // Dropping CPAL's stream ends its callback and releases the device
        // before we inspect samples, resample, or invoke whisper.cpp.
        drop(stream);
        let recording_duration = started.elapsed();
        if let Some(interaction) = interaction {
            interaction.mark("microphone.released");
            let outcome = match &finish_reason {
                Ok(FinishReason::Finished) => "completed",
                Ok(FinishReason::Cancelled) => "cancelled",
                Err(_) => "device_failure",
            };
            interaction.event(
                "microphone.recording",
                outcome,
                telemetry::fields([(
                    "recording_duration_ms",
                    serde_json::json!(
                        u64::try_from(recording_duration.as_millis()).unwrap_or(u64::MAX)
                    ),
                )]),
            );
        }

        let finish_reason = finish_reason?;

        if finish_reason == FinishReason::Cancelled {
            return Ok(CaptureOutcome::Cancelled);
        }

        let samples = captured.take()?;
        let input_samples = samples.len();
        let preparation = interaction.map(|interaction| {
            interaction.stage(
                "audio.prepare_resample",
                telemetry::fields([
                    ("source_sample_rate_hz", serde_json::json!(sample_rate)),
                    ("input_samples", serde_json::json!(input_samples)),
                ]),
            )
        });
        let audio = match prepare_for_whisper(samples, sample_rate) {
            Ok(audio) => audio,
            Err(error) => {
                if let Some(stage) = preparation {
                    stage.finish("failed", telemetry::Fields::new());
                }
                return Err(error);
            }
        };
        if let Some(stage) = preparation {
            stage.finish(
                "completed",
                telemetry::fields([
                    (
                        "whisper_sample_rate_hz",
                        serde_json::json!(WHISPER_SAMPLE_RATE),
                    ),
                    ("output_samples", serde_json::json!(audio.samples().len())),
                    (
                        "audio_duration_ms",
                        serde_json::json!(
                            (audio.samples().len() as u64).saturating_mul(1_000)
                                / u64::from(WHISPER_SAMPLE_RATE)
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
}

fn wait_for_finish(
    recording_id: RecordingId,
    event_sender: &EventSender<DictationEvent>,
    commands: &Receiver<CaptureCommand>,
    stream_error: &Mutex<Option<String>>,
    captured: &CaptureBuffer,
    sample_rate: u32,
    started: Instant,
) -> Result<FinishReason, String> {
    let mut last_reported_second = 0;
    loop {
        if let Some(error) = stream_error
            .lock()
            .map_err(|_| "microphone error state lock was poisoned".to_owned())?
            .take()
        {
            return Err(format!("microphone stream failed: {error}"));
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
                let event = match captured
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

struct CaptureBuffer {
    samples: Mutex<Vec<f32>>,
    sample_limit: usize,
    overflowed: AtomicBool,
    dropped_frames: AtomicUsize,
}

impl CaptureBuffer {
    fn new(sample_limit: usize) -> Result<Self, String> {
        let mut samples = Vec::new();
        samples
            .try_reserve_exact(sample_limit)
            .map_err(|error| format!("could not reserve bounded microphone buffer: {error}"))?;
        Ok(Self {
            samples: Mutex::new(samples),
            sample_limit,
            overflowed: AtomicBool::new(false),
            dropped_frames: AtomicUsize::new(0),
        })
    }

    fn append<T>(&self, data: &[T], channels: usize)
    where
        T: Sample,
        f32: FromSample<T>,
    {
        let frames = data.len() / channels;
        let Ok(mut samples) = self.samples.try_lock() else {
            // Blocking the high-priority audio callback would be worse than
            // rejecting one unreliable recording after it has stopped.
            self.dropped_frames.fetch_add(frames, Ordering::Relaxed);
            return;
        };

        for frame in data.chunks_exact(channels) {
            if samples.len() == self.sample_limit {
                self.overflowed.store(true, Ordering::Relaxed);
                break;
            }
            let mono = frame
                .iter()
                .map(|sample| f32::from_sample(*sample))
                .sum::<f32>()
                / channels as f32;
            samples.push(mono);
        }
    }

    fn take(&self) -> Result<Vec<f32>, String> {
        if self.overflowed.load(Ordering::Relaxed) {
            return Err("recording exceeded the bounded in-memory audio limit".into());
        }
        if self.dropped_frames.load(Ordering::Relaxed) != 0 {
            return Err("recording lost audio frames while buffering; try again".into());
        }
        let mut samples = self
            .samples
            .lock()
            .map_err(|_| "microphone sample buffer lock was poisoned".to_owned())?;
        Ok(std::mem::take(&mut *samples))
    }

    fn snapshot(&self) -> Result<Vec<f32>, String> {
        if self.overflowed.load(Ordering::Relaxed) {
            return Err("recording exceeded the bounded in-memory audio limit".into());
        }
        if self.dropped_frames.load(Ordering::Relaxed) != 0 {
            return Err("recording lost audio frames while buffering; try again".into());
        }
        self.samples
            .lock()
            .map_err(|_| "microphone sample buffer lock was poisoned".to_owned())
            .map(|samples| samples.clone())
    }
}

fn build_input_stream(
    device: &Device,
    config: &cpal::StreamConfig,
    sample_format: SampleFormat,
    channels: usize,
    captured: Arc<CaptureBuffer>,
    stream_error: Arc<Mutex<Option<String>>>,
) -> Result<cpal::Stream, String> {
    macro_rules! build {
        ($sample:ty) => {
            build_stream_for_format::<$sample>(device, config, channels, captured, stream_error)
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
    captured: Arc<CaptureBuffer>,
    stream_error: Arc<Mutex<Option<String>>>,
) -> Result<cpal::Stream, String>
where
    T: Sample + SizedSample,
    f32: FromSample<T>,
{
    let input = captured.clone();
    let error_slot = stream_error.clone();
    device
        .build_input_stream(
            *config,
            move |data: &[T], _| input.append(data, channels),
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

/// Converts an in-memory mono clip to exactly what whisper.cpp expects.
/// This happens after the CPAL stream has been dropped.
pub(crate) fn prepare_for_whisper(
    samples: Vec<f32>,
    source_sample_rate: u32,
) -> Result<CapturedAudio, String> {
    if samples.is_empty() {
        return Err("recording contained no audio samples".into());
    }
    if source_sample_rate == 0 {
        return Err("recording reported a zero sample rate".into());
    }

    let resampled = if source_sample_rate == WHISPER_SAMPLE_RATE {
        samples
    } else {
        resample_mono(samples, source_sample_rate)?
    };

    if resampled.len() > MAX_WHISPER_SAMPLES {
        return Err("recording exceeded the 30-second transcription limit after resampling".into());
    }
    if !resampled
        .iter()
        .any(|sample| sample.is_finite() && sample.abs() >= MINIMUM_AUDIBLE_PEAK)
    {
        return Err("recording was silent or too quiet to transcribe".into());
    }

    Ok(CapturedAudio::new(resampled))
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
    use super::{
        WHISPER_SAMPLE_RATE, bounded_raw_sample_limit, downmix_interleaved, prepare_for_whisper,
    };

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
