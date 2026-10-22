//! Explicit one-shot V4L2 camera capture for the `see` capability.
//!
//! Device discovery opens V4L2 nodes only long enough to inspect their
//! capture capabilities; it never starts a stream. A capture uses a bounded
//! V4L2 poll window, copies one final frame into memory, then drops the stream
//! and device before the caller can start inference.

use std::{
    fmt, io,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use image::{RgbImage, codecs::jpeg::JpegEncoder};
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use v4l::{
    Device, Format, FourCC,
    buffer::Type,
    capability::Flags as CapabilityFlags,
    context,
    io::{mmap::Stream as MmapStream, traits::CaptureStream},
    video::Capture,
};

use super::{Photo, PhotoError};
use crate::telemetry::{self, Interaction};

/// The largest resolution accepted from a camera before allocating capture
/// buffers. It is intentionally smaller than the general file-photo limit.
pub const MAX_CAMERA_WIDTH: u32 = 1_280;
pub const MAX_CAMERA_HEIGHT: u32 = 720;
const MAX_CAMERA_PIXELS: u32 = MAX_CAMERA_WIDTH * MAX_CAMERA_HEIGHT;
const MAX_CAMERA_BUFFER_BYTES: usize = 4 * 1024 * 1024;
const CAPTURE_BUFFER_COUNT: u32 = 3;
const WARM_UP_FRAMES: usize = 2;
const CAPTURE_WINDOW: Duration = Duration::from_secs(3);
const MAX_FRAME_WAIT: Duration = Duration::from_secs(1);
const JPEG_QUALITY: u8 = 85;

/// The numeric portion of a `/dev/video<ID>` V4L2 node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CameraId(u32);

impl CameraId {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        value.parse().ok().map(Self)
    }

    pub(crate) const fn new(value: u32) -> Self {
        Self(value)
    }

    fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for CameraId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// A V4L2 device that this prototype can open and capture from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraDescriptor {
    id: CameraId,
    name: String,
}

impl CameraDescriptor {
    pub(crate) fn new(id: CameraId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
        }
    }

    pub fn id(&self) -> CameraId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for CameraDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} (camera {}, /dev/video{})",
            self.name, self.id, self.id
        )
    }
}

/// Metadata about a camera image after the camera handle has been released.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraPhotoSource {
    camera: CameraDescriptor,
    width: u32,
    height: u32,
    input_format: &'static str,
}

impl CameraPhotoSource {
    pub(crate) fn new(
        camera: CameraDescriptor,
        width: u32,
        height: u32,
        input_format: &'static str,
    ) -> Self {
        Self {
            camera,
            width,
            height,
            input_format,
        }
    }

    pub fn camera(&self) -> &CameraDescriptor {
        &self.camera
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn input_format(&self) -> &'static str {
        self.input_format
    }
}

/// Hardware access used by the runtime and replaced by controlled fakes in
/// tests. This is a real test seam: unit tests must never open a camera.
pub(crate) trait CameraBackend: Send + Sync {
    fn list(&self) -> Result<Vec<CameraDescriptor>, CameraError>;
    fn capture(&self, camera: &CameraDescriptor) -> Result<Photo, CameraError>;
}

#[derive(Default)]
struct V4lCameraBackend;

impl CameraBackend for V4lCameraBackend {
    fn list(&self) -> Result<Vec<CameraDescriptor>, CameraError> {
        let mut cameras = Vec::new();

        for node in context::enum_devices() {
            let Ok(index) = u32::try_from(node.index()) else {
                continue;
            };
            let id = CameraId::new(index);
            let name = node
                .name()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| format!("V4L2 device {id}"));

            // This is a capability query only. It does not create a capture
            // stream or acquire any frames.
            let Ok(device) = Device::with_path(node.path()) else {
                continue;
            };
            let Ok(capabilities) = device.query_caps() else {
                continue;
            };
            if !capabilities
                .capabilities
                .contains(CapabilityFlags::VIDEO_CAPTURE)
                || !capabilities
                    .capabilities
                    .contains(CapabilityFlags::STREAMING)
            {
                continue;
            }

            let Ok(formats) = device.enum_formats() else {
                continue;
            };
            if !formats
                .iter()
                .any(|format| is_supported_fourcc(format.fourcc))
            {
                continue;
            }

            cameras.push(CameraDescriptor::new(id, name));
        }

        cameras.sort_by_key(CameraDescriptor::id);
        Ok(cameras)
    }

    fn capture(&self, camera: &CameraDescriptor) -> Result<Photo, CameraError> {
        let device = Device::new(camera.id.index()).map_err(|source| CameraError::Open {
            camera: camera.clone(),
            source,
        })?;
        let original_format = device
            .format()
            .map_err(|source| CameraError::InspectFormat {
                camera: camera.clone(),
                source,
            })?;

        let capture_result = capture_raw_frame(&device, camera);
        // A format change can affect other local camera consumers. Restore the
        // format after the stream has dropped, even if capture itself failed.
        let restore_result = device.set_format(&original_format);
        drop(device);

        let captured = match (capture_result, restore_result) {
            (Ok(captured), Ok(_)) => captured,
            (Err(capture), Ok(_)) => return Err(capture),
            (Ok(_), Err(source)) => {
                return Err(CameraError::RestoreFormat {
                    camera: camera.clone(),
                    source,
                });
            }
            (Err(capture), Err(restore)) => {
                return Err(CameraError::CaptureAndRestore {
                    capture: capture.to_string(),
                    restore: restore.to_string(),
                });
            }
        };

        let jpeg = match captured.pixel_format {
            CameraPixelFormat::Mjpeg => captured.bytes,
            CameraPixelFormat::Yuyv => encode_yuyv_as_jpeg(&captured, camera)?,
        };
        let source = CameraPhotoSource::new(
            camera.clone(),
            captured.format.width,
            captured.format.height,
            captured.pixel_format.name(),
        );
        Photo::from_camera_jpeg(source, jpeg).map_err(|source| CameraError::Validate {
            camera: camera.clone(),
            source: Box::new(source),
        })
    }
}

/// Coordinates bounded capture work. The semaphore remains owned by the
/// blocking closure, so cancelling an async caller cannot accidentally start
/// a second task against a camera that is still closing.
#[derive(Clone)]
pub(crate) struct CameraService {
    backend: Arc<dyn CameraBackend>,
    capture_gate: Arc<Semaphore>,
    waiting_captures: Arc<AtomicUsize>,
    active_captures: Arc<AtomicUsize>,
}

impl CameraService {
    pub(crate) fn v4l() -> Self {
        Self {
            backend: Arc::new(V4lCameraBackend),
            capture_gate: Arc::new(Semaphore::new(1)),
            waiting_captures: Arc::new(AtomicUsize::new(0)),
            active_captures: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_backend(backend: Arc<dyn CameraBackend>) -> Self {
        Self {
            backend,
            capture_gate: Arc::new(Semaphore::new(1)),
            waiting_captures: Arc::new(AtomicUsize::new(0)),
            active_captures: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Enumerates prototype-usable V4L2 cameras without starting a stream.
    pub(crate) async fn list(&self) -> Result<Vec<CameraDescriptor>, CameraError> {
        let backend = self.backend.clone();
        tokio::task::spawn_blocking(move || backend.list())
            .await
            .map_err(|error| CameraError::Worker {
                operation: "device discovery",
                detail: error.to_string(),
            })?
    }

    /// Waits for exclusive hardware access, then captures one bounded still.
    ///
    /// Once the blocking capture has begun it is deliberately not aborted:
    /// Tokio cannot stop a running `spawn_blocking` task. The V4L2 backend's
    /// frame polls are bounded by `CAPTURE_WINDOW`, and the closure owns the
    /// semaphore until it has dropped its stream and device handle.
    #[cfg(test)]
    pub(crate) async fn capture(
        &self,
        camera: CameraDescriptor,
        cancellation: CancellationToken,
    ) -> Result<Photo, CameraRunError> {
        self.capture_with_telemetry(camera, cancellation, None)
            .await
    }

    /// Captures through the existing exclusive hardware gate while optionally
    /// recording its real queue wait and blocking execution time. Cancelling
    /// this future still does not abort the V4L2 blocking closure.
    pub(crate) async fn capture_with_telemetry(
        &self,
        camera: CameraDescriptor,
        cancellation: CancellationToken,
        interaction: Option<Interaction>,
    ) -> Result<Photo, CameraRunError> {
        let wait_started = Instant::now();
        let pending_before = self.waiting_captures.fetch_add(1, Ordering::AcqRel);
        let capture_gate = self.capture_gate.clone();
        let permit = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                self.waiting_captures.fetch_sub(1, Ordering::AcqRel);
                if let Some(interaction) = &interaction {
                    interaction.queue_wait(
                        "camera",
                        wait_started.elapsed(),
                        pending_before,
                        self.active_captures.load(Ordering::Acquire),
                    );
                    interaction.event("camera.capture.gate", "cancelled", telemetry::Fields::new());
                }
                return Err(CameraRunError::Cancelled);
            },
            permit = capture_gate.acquire_owned() => permit.map_err(|_| CameraRunError::Failed(
                CameraError::Worker {
                    operation: "capture scheduling",
                    detail: "camera capture service shut down".into(),
                },
            ))?,
        };
        self.waiting_captures.fetch_sub(1, Ordering::AcqRel);
        if let Some(interaction) = &interaction {
            interaction.queue_wait(
                "camera",
                wait_started.elapsed(),
                pending_before,
                self.active_captures.load(Ordering::Acquire),
            );
        }

        if cancellation.is_cancelled() {
            if let Some(interaction) = &interaction {
                interaction.event("camera.capture.gate", "cancelled", telemetry::Fields::new());
            }
            return Err(CameraRunError::Cancelled);
        }

        let backend = self.backend.clone();
        let active_captures = self.active_captures.clone();
        let worker_interaction = interaction.clone();
        let camera_for_metrics = camera.clone();
        let mut task = tokio::task::spawn_blocking(move || {
            // Keep this permit until `capture` has returned and all V4L2
            // objects in that call have been dropped.
            let _permit = permit;
            let _in_flight = worker_interaction
                .as_ref()
                .map(|interaction| interaction.in_flight_guard("camera"));
            active_captures.fetch_add(1, Ordering::AcqRel);
            let stage = worker_interaction.as_ref().map(|interaction| {
                interaction.stage(
                    "camera.capture_and_validate",
                    telemetry::fields([(
                        "camera_id",
                        serde_json::json!(camera_for_metrics.id().to_string()),
                    )]),
                )
            });
            let result = backend.capture(&camera);
            if let Some(stage) = stage {
                let (outcome, fields) = match &result {
                    Ok(photo) => (
                        "completed",
                        telemetry::fields([(
                            "encoded_image_bytes",
                            serde_json::json!(photo.bytes().len()),
                        )]),
                    ),
                    Err(_) => (
                        "failed",
                        telemetry::fields([("failure_class", serde_json::json!("device_failure"))]),
                    ),
                };
                stage.finish(outcome, fields);
            }
            active_captures.fetch_sub(1, Ordering::AcqRel);
            result
        });

        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                // Do not abort this task. A running blocking task cannot be
                // forcibly stopped safely; it will finish its bounded V4L2
                // window and release `_permit` before another capture starts.
                Err(CameraRunError::Cancelled)
            }
            result = &mut task => match result {
                Ok(Ok(photo)) => Ok(photo),
                Ok(Err(error)) => Err(CameraRunError::Failed(error)),
                Err(error) => Err(CameraRunError::Failed(CameraError::Worker {
                    operation: "capture",
                    detail: error.to_string(),
                })),
            },
        }
    }

    /// Waits until a previously cancelled blocking capture has released its
    /// V4L2 resources. Runtime shutdown uses this; ordinary `/stop` does not,
    /// so the terminal remains responsive while the bounded cleanup finishes.
    pub(crate) async fn wait_for_idle(&self) {
        if let Ok(permit) = self.capture_gate.clone().acquire_owned().await {
            drop(permit);
        }
    }
}

/// Applies the user-facing camera-selection rules after a no-stream device
/// inventory has completed.
pub(crate) fn select_camera(
    cameras: &[CameraDescriptor],
    requested: Option<CameraId>,
) -> Result<CameraDescriptor, CameraError> {
    match (requested, cameras) {
        (_, []) => Err(CameraError::NoCameras),
        (Some(id), cameras) => cameras
            .iter()
            .find(|camera| camera.id == id)
            .cloned()
            .ok_or(CameraError::UnknownCamera { id }),
        (None, [camera]) => Ok(camera.clone()),
        (None, _) => Err(CameraError::SelectionRequired),
    }
}

#[derive(Debug)]
pub(crate) enum CameraRunError {
    Cancelled,
    Failed(CameraError),
}

#[derive(Debug, Error)]
pub(crate) enum CameraError {
    #[error(
        "no usable V4L2 camera was found; connect one, check its permissions, then run /cameras"
    )]
    NoCameras,

    #[error(
        "more than one usable camera is available; run /cameras and use /camera <id> | <question>"
    )]
    SelectionRequired,

    #[error("camera {id} is not available; run /cameras to choose a listed identifier")]
    UnknownCamera { id: CameraId },

    #[error("could not open {camera}: {source}")]
    Open {
        camera: CameraDescriptor,
        #[source]
        source: io::Error,
    },

    #[error("could not inspect the active format for {camera}: {source}")]
    InspectFormat {
        camera: CameraDescriptor,
        #[source]
        source: io::Error,
    },

    #[error("{camera} does not support a safe MJPEG or YUYV capture format")]
    UnsupportedFormat { camera: CameraDescriptor },

    #[error("{camera} negotiated unsupported dimensions {width}x{height}")]
    Dimensions {
        camera: CameraDescriptor,
        width: u32,
        height: u32,
    },

    #[error(
        "{camera} negotiated a {size}-byte frame buffer, above the {MAX_CAMERA_BUFFER_BYTES}-byte limit"
    )]
    BufferTooLarge {
        camera: CameraDescriptor,
        size: usize,
    },

    #[error("could not configure a capture stream for {camera}: {source}")]
    Stream {
        camera: CameraDescriptor,
        #[source]
        source: io::Error,
    },

    #[error("timed out waiting for a frame from {camera} within the three-second capture window")]
    FrameTimeout { camera: CameraDescriptor },

    #[error("could not read a frame from {camera}: {source}")]
    Frame {
        camera: CameraDescriptor,
        #[source]
        source: io::Error,
    },

    #[error("{camera} reported {used} bytes for a {available}-byte capture buffer")]
    InvalidFrameLength {
        camera: CameraDescriptor,
        used: usize,
        available: usize,
    },

    #[error(
        "{camera} produced a {size}-byte frame, above the {MAX_CAMERA_BUFFER_BYTES}-byte limit"
    )]
    FrameTooLarge {
        camera: CameraDescriptor,
        size: usize,
    },

    #[error("{camera} returned malformed YUYV frame data")]
    MalformedYuyv { camera: CameraDescriptor },

    #[error("could not encode a JPEG from {camera}: {source}")]
    Encode {
        camera: CameraDescriptor,
        #[source]
        source: image::ImageError,
    },

    #[error("could not validate the image captured from {camera}: {source}")]
    Validate {
        camera: CameraDescriptor,
        #[source]
        source: Box<PhotoError>,
    },

    #[error("could not restore the prior format for {camera}: {source}")]
    RestoreFormat {
        camera: CameraDescriptor,
        #[source]
        source: io::Error,
    },

    #[error(
        "camera capture failed ({capture}) and restoring the prior format also failed ({restore})"
    )]
    CaptureAndRestore { capture: String, restore: String },

    #[error("{operation} worker ended unexpectedly: {detail}")]
    Worker {
        operation: &'static str,
        detail: String,
    },
}

#[derive(Clone, Copy)]
enum CameraPixelFormat {
    Mjpeg,
    Yuyv,
}

impl CameraPixelFormat {
    fn from_fourcc(fourcc: FourCC) -> Option<Self> {
        if fourcc == FourCC::new(b"MJPG") {
            Some(Self::Mjpeg)
        } else if fourcc == FourCC::new(b"YUYV") {
            Some(Self::Yuyv)
        } else {
            None
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Mjpeg => "MJPEG",
            Self::Yuyv => "YUYV",
        }
    }
}

struct CapturedFrame {
    bytes: Vec<u8>,
    format: Format,
    pixel_format: CameraPixelFormat,
}

fn capture_raw_frame(
    device: &Device,
    camera: &CameraDescriptor,
) -> Result<CapturedFrame, CameraError> {
    let format = configure_format(device, camera)?;
    let pixel_format = CameraPixelFormat::from_fourcc(format.fourcc).ok_or_else(|| {
        CameraError::UnsupportedFormat {
            camera: camera.clone(),
        }
    })?;

    let bytes = {
        let mut stream = MmapStream::with_buffers(device, Type::VideoCapture, CAPTURE_BUFFER_COUNT)
            .map_err(|source| CameraError::Stream {
                camera: camera.clone(),
                source,
            })?;
        let deadline = Instant::now() + CAPTURE_WINDOW;

        // Some sensors need a couple of frames to settle after stream-on.
        // Those frames are discarded; the third frame is the still image.
        for _ in 0..WARM_UP_FRAMES {
            let _ = read_frame(&mut stream, deadline, camera)?;
        }
        read_frame(&mut stream, deadline, camera)?
        // `stream` drops here, releasing its MMAP buffers before format
        // restoration and before the caller can contact Ollama.
    };

    Ok(CapturedFrame {
        bytes,
        format,
        pixel_format,
    })
}

fn configure_format(device: &Device, camera: &CameraDescriptor) -> Result<Format, CameraError> {
    let formats = device
        .enum_formats()
        .map_err(|source| CameraError::Stream {
            camera: camera.clone(),
            source,
        })?;

    for fourcc in [FourCC::new(b"MJPG"), FourCC::new(b"YUYV")] {
        if !formats.iter().any(|format| format.fourcc == fourcc) {
            continue;
        }

        for (width, height) in [(MAX_CAMERA_WIDTH, MAX_CAMERA_HEIGHT), (640, 480)] {
            let requested = Format::new(width, height, fourcc);
            let Ok(actual) = device.set_format(&requested) else {
                continue;
            };
            if validate_negotiated_format(&actual, camera).is_some() {
                // V4L2 intentionally applies settings on a best-effort basis.
                // Try the next conservative request, then restore the original
                // device setting in the caller if none were acceptable.
                continue;
            }
            return Ok(actual);
        }
    }

    Err(CameraError::UnsupportedFormat {
        camera: camera.clone(),
    })
}

fn validate_negotiated_format(format: &Format, camera: &CameraDescriptor) -> Option<CameraError> {
    if CameraPixelFormat::from_fourcc(format.fourcc).is_none() {
        return Some(CameraError::UnsupportedFormat {
            camera: camera.clone(),
        });
    }
    if format.width == 0
        || format.height == 0
        || format.width > MAX_CAMERA_WIDTH
        || format.height > MAX_CAMERA_HEIGHT
        || format
            .width
            .checked_mul(format.height)
            .is_none_or(|pixels| pixels > MAX_CAMERA_PIXELS)
    {
        return Some(CameraError::Dimensions {
            camera: camera.clone(),
            width: format.width,
            height: format.height,
        });
    }
    let Ok(size) = usize::try_from(format.size) else {
        return Some(CameraError::BufferTooLarge {
            camera: camera.clone(),
            size: usize::MAX,
        });
    };
    if size == 0 || size > MAX_CAMERA_BUFFER_BYTES {
        return Some(CameraError::BufferTooLarge {
            camera: camera.clone(),
            size,
        });
    }

    None
}

fn read_frame(
    stream: &mut MmapStream<'_>,
    deadline: Instant,
    camera: &CameraDescriptor,
) -> Result<Vec<u8>, CameraError> {
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return Err(CameraError::FrameTimeout {
            camera: camera.clone(),
        });
    };
    stream.set_timeout(remaining.min(MAX_FRAME_WAIT));

    let (buffer, metadata) = stream.next().map_err(|source| {
        if source.kind() == io::ErrorKind::TimedOut {
            CameraError::FrameTimeout {
                camera: camera.clone(),
            }
        } else {
            CameraError::Frame {
                camera: camera.clone(),
                source,
            }
        }
    })?;
    let used = usize::try_from(metadata.bytesused).map_err(|_| CameraError::FrameTooLarge {
        camera: camera.clone(),
        size: usize::MAX,
    })?;
    if used > buffer.len() {
        return Err(CameraError::InvalidFrameLength {
            camera: camera.clone(),
            used,
            available: buffer.len(),
        });
    }
    if used > MAX_CAMERA_BUFFER_BYTES {
        return Err(CameraError::FrameTooLarge {
            camera: camera.clone(),
            size: used,
        });
    }

    Ok(buffer[..used].to_vec())
}

fn encode_yuyv_as_jpeg(
    captured: &CapturedFrame,
    camera: &CameraDescriptor,
) -> Result<Vec<u8>, CameraError> {
    let width = usize::try_from(captured.format.width).map_err(|_| CameraError::MalformedYuyv {
        camera: camera.clone(),
    })?;
    let height =
        usize::try_from(captured.format.height).map_err(|_| CameraError::MalformedYuyv {
            camera: camera.clone(),
        })?;
    if width == 0 || height == 0 || width % 2 != 0 {
        return Err(CameraError::MalformedYuyv {
            camera: camera.clone(),
        });
    }

    let minimum_stride = width
        .checked_mul(2)
        .ok_or_else(|| CameraError::MalformedYuyv {
            camera: camera.clone(),
        })?;
    let stride = if captured.format.stride == 0 {
        minimum_stride
    } else {
        usize::try_from(captured.format.stride).map_err(|_| CameraError::MalformedYuyv {
            camera: camera.clone(),
        })?
    };
    let required = stride
        .checked_mul(height)
        .ok_or_else(|| CameraError::MalformedYuyv {
            camera: camera.clone(),
        })?;
    let rgb_len = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| CameraError::MalformedYuyv {
            camera: camera.clone(),
        })?;
    if stride < minimum_stride
        || required > captured.bytes.len()
        || rgb_len > MAX_CAMERA_BUFFER_BYTES
    {
        return Err(CameraError::MalformedYuyv {
            camera: camera.clone(),
        });
    }

    let mut rgb = vec![0_u8; rgb_len];
    for row in 0..height {
        let input = &captured.bytes[row * stride..row * stride + minimum_stride];
        let output = &mut rgb[row * width * 3..(row + 1) * width * 3];
        for pair in 0..(width / 2) {
            let offset = pair * 4;
            let (y0, u, y1, v) = (
                input[offset],
                input[offset + 1],
                input[offset + 2],
                input[offset + 3],
            );
            let output_offset = pair * 6;
            output[output_offset..output_offset + 3].copy_from_slice(&yuv_to_rgb(y0, u, v));
            output[output_offset + 3..output_offset + 6].copy_from_slice(&yuv_to_rgb(y1, u, v));
        }
    }

    let image = RgbImage::from_raw(captured.format.width, captured.format.height, rgb).ok_or_else(
        || CameraError::MalformedYuyv {
            camera: camera.clone(),
        },
    )?;
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY)
        .encode_image(&image)
        .map_err(|source| CameraError::Encode {
            camera: camera.clone(),
            source,
        })?;
    Ok(jpeg)
}

fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let y = i32::from(y).saturating_sub(16).max(0);
    let u = i32::from(u) - 128;
    let v = i32::from(v) - 128;
    let red = (298 * y + 409 * v + 128) >> 8;
    let green = (298 * y - 100 * u - 208 * v + 128) >> 8;
    let blue = (298 * y + 516 * u + 128) >> 8;
    [clamp_to_u8(red), clamp_to_u8(green), clamp_to_u8(blue)]
}

fn clamp_to_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

fn is_supported_fourcc(fourcc: FourCC) -> bool {
    CameraPixelFormat::from_fourcc(fourcc).is_some()
}

#[cfg(test)]
mod tests {
    use std::{
        env,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        time::Duration,
    };

    use image::{RgbImage, codecs::jpeg::JpegEncoder};
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;
    use v4l::{Format, FourCC};

    use super::{
        CameraBackend, CameraDescriptor, CameraError, CameraId, CameraPhotoSource,
        CameraPixelFormat, CameraRunError, CameraService, CapturedFrame, V4lCameraBackend,
        encode_yuyv_as_jpeg, select_camera,
    };
    use crate::see::{Photo, PhotoSource};

    const TEST_TIMEOUT: Duration = Duration::from_secs(2);

    fn camera(id: u32) -> CameraDescriptor {
        CameraDescriptor::new(CameraId::new(id), format!("Test Camera {id}"))
    }

    fn tiny_jpeg() -> Vec<u8> {
        let image = RgbImage::new(2, 2);
        let mut bytes = Vec::new();
        JpegEncoder::new_with_quality(&mut bytes, 85)
            .encode_image(&image)
            .unwrap();
        bytes
    }

    fn test_photo(camera: &CameraDescriptor) -> Photo {
        Photo::from_camera_jpeg(
            CameraPhotoSource::new(camera.clone(), 2, 2, "MJPEG"),
            tiny_jpeg(),
        )
        .unwrap()
    }

    #[test]
    fn selection_requires_an_explicit_identifier_when_multiple_cameras_exist() {
        let first = camera(0);
        let second = camera(3);

        assert!(matches!(
            select_camera(&[], None),
            Err(CameraError::NoCameras)
        ));
        assert_eq!(
            select_camera(std::slice::from_ref(&first), None).unwrap(),
            first
        );
        assert!(matches!(
            select_camera(&[first.clone(), second.clone()], None),
            Err(CameraError::SelectionRequired)
        ));
        assert_eq!(
            select_camera(&[first.clone(), second.clone()], Some(CameraId::new(3))).unwrap(),
            second
        );
        assert!(matches!(
            select_camera(&[first, second], Some(CameraId::new(99))),
            Err(CameraError::UnknownCamera { id }) if id == CameraId::new(99)
        ));
    }

    #[test]
    fn yuyv_conversion_produces_a_valid_jpeg_without_hardware() {
        let camera = camera(0);
        let captured = CapturedFrame {
            // Two grey pixels in YUYV order: Y0 U Y1 V.
            bytes: vec![128, 128, 128, 128],
            format: Format::new(2, 1, FourCC::new(b"YUYV")),
            pixel_format: CameraPixelFormat::Yuyv,
        };

        let jpeg = encode_yuyv_as_jpeg(&captured, &camera).unwrap();
        let photo =
            Photo::from_camera_jpeg(CameraPhotoSource::new(camera.clone(), 2, 1, "YUYV"), jpeg)
                .unwrap();

        assert!(matches!(
            photo.source(),
            PhotoSource::Camera(source)
                if source.camera() == &camera
                    && source.width() == 2
                    && source.height() == 1
                    && source.input_format() == "YUYV"
        ));
    }

    struct ImmediateBackend {
        cameras: Vec<CameraDescriptor>,
        photo: Photo,
        fail: bool,
        calls: AtomicUsize,
        active: AtomicUsize,
    }

    impl CameraBackend for ImmediateBackend {
        fn list(&self) -> Result<Vec<CameraDescriptor>, CameraError> {
            Ok(self.cameras.clone())
        }

        fn capture(&self, _camera: &CameraDescriptor) -> Result<Photo, CameraError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.active.fetch_add(1, Ordering::SeqCst);
            struct Active<'a>(&'a AtomicUsize);
            impl Drop for Active<'_> {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::SeqCst);
                }
            }
            let _active = Active(&self.active);

            if self.fail {
                Err(CameraError::NoCameras)
            } else {
                Ok(self.photo.clone())
            }
        }
    }

    #[tokio::test]
    async fn camera_service_releases_its_worker_after_success_and_failure() {
        let descriptor = camera(0);
        let success_backend = Arc::new(ImmediateBackend {
            cameras: vec![descriptor.clone()],
            photo: test_photo(&descriptor),
            fail: false,
            calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
        });
        let success_service = CameraService::with_backend(success_backend.clone());
        let photo = success_service
            .capture(descriptor.clone(), CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(photo.source(), PhotoSource::Camera(_)));
        assert_eq!(success_backend.calls.load(Ordering::SeqCst), 1);
        assert_eq!(success_backend.active.load(Ordering::SeqCst), 0);

        let failure_backend = Arc::new(ImmediateBackend {
            cameras: vec![descriptor.clone()],
            photo: test_photo(&descriptor),
            fail: true,
            calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
        });
        let failure_service = CameraService::with_backend(failure_backend.clone());
        let result = failure_service
            .capture(descriptor, CancellationToken::new())
            .await;
        assert!(matches!(
            result,
            Err(CameraRunError::Failed(CameraError::NoCameras))
        ));
        assert_eq!(failure_backend.calls.load(Ordering::SeqCst), 1);
        assert_eq!(failure_backend.active.load(Ordering::SeqCst), 0);
    }

    struct BlockingBackend {
        camera: CameraDescriptor,
        photo: Photo,
        started: mpsc::Sender<usize>,
        release: Mutex<mpsc::Receiver<()>>,
        calls: AtomicUsize,
        active: AtomicUsize,
        maximum_active: AtomicUsize,
        completed: AtomicUsize,
    }

    impl BlockingBackend {
        fn record_maximum(&self, current: usize) {
            let mut maximum = self.maximum_active.load(Ordering::SeqCst);
            while current > maximum {
                match self.maximum_active.compare_exchange(
                    maximum,
                    current,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => return,
                    Err(observed) => maximum = observed,
                }
            }
        }
    }

    impl CameraBackend for BlockingBackend {
        fn list(&self) -> Result<Vec<CameraDescriptor>, CameraError> {
            Ok(vec![self.camera.clone()])
        }

        fn capture(&self, _camera: &CameraDescriptor) -> Result<Photo, CameraError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.record_maximum(active);
            struct Active<'a> {
                active: &'a AtomicUsize,
                completed: &'a AtomicUsize,
            }
            impl Drop for Active<'_> {
                fn drop(&mut self) {
                    self.active.fetch_sub(1, Ordering::SeqCst);
                    self.completed.fetch_add(1, Ordering::SeqCst);
                }
            }
            let _active = Active {
                active: &self.active,
                completed: &self.completed,
            };

            self.started
                .send(call)
                .map_err(|error| CameraError::Worker {
                    operation: "test capture start notification",
                    detail: error.to_string(),
                })?;
            self.release
                .lock()
                .expect("test release receiver lock should not be poisoned")
                .recv_timeout(TEST_TIMEOUT)
                .map_err(|error| CameraError::Worker {
                    operation: "test capture release wait",
                    detail: error.to_string(),
                })?;
            Ok(self.photo.clone())
        }
    }

    async fn receive_start(receiver: Arc<Mutex<mpsc::Receiver<usize>>>) -> usize {
        timeout(
            TEST_TIMEOUT,
            tokio::task::spawn_blocking(move || {
                receiver
                    .lock()
                    .expect("test start receiver lock should not be poisoned")
                    .recv_timeout(TEST_TIMEOUT)
            }),
        )
        .await
        .expect("camera worker did not begin in time")
        .expect("start receiver task failed")
        .expect("camera worker did not report its start")
    }

    #[tokio::test]
    async fn cancellation_keeps_one_bounded_worker_and_allows_a_later_capture() {
        let descriptor = camera(0);
        let (started_sender, started_receiver) = mpsc::channel();
        let started_receiver = Arc::new(Mutex::new(started_receiver));
        let (release_sender, release_receiver) = mpsc::channel();
        let backend = Arc::new(BlockingBackend {
            camera: descriptor.clone(),
            photo: test_photo(&descriptor),
            started: started_sender,
            release: Mutex::new(release_receiver),
            calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            maximum_active: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
        });
        let service = CameraService::with_backend(backend.clone());
        let first_cancellation = CancellationToken::new();
        let first = tokio::spawn({
            let service = service.clone();
            let camera = descriptor.clone();
            let cancellation = first_cancellation.clone();
            async move { service.capture(camera, cancellation).await }
        });

        assert_eq!(receive_start(started_receiver.clone()).await, 1);
        first_cancellation.cancel();
        let first_result = timeout(TEST_TIMEOUT, first)
            .await
            .expect("cancelling the async caller should be prompt")
            .expect("first capture task should not panic");
        assert!(matches!(first_result, Err(CameraRunError::Cancelled)));
        assert_eq!(backend.active.load(Ordering::SeqCst), 1);

        let second = tokio::spawn({
            let service = service.clone();
            let camera = descriptor.clone();
            async move { service.capture(camera, CancellationToken::new()).await }
        });
        tokio::task::yield_now().await;
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        assert_eq!(backend.maximum_active.load(Ordering::SeqCst), 1);

        release_sender.send(()).unwrap();
        assert_eq!(receive_start(started_receiver).await, 2);
        release_sender.send(()).unwrap();
        let second_result = timeout(TEST_TIMEOUT, second)
            .await
            .expect("second capture should finish")
            .expect("second capture task should not panic");
        assert!(second_result.is_ok());
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        assert_eq!(backend.maximum_active.load(Ordering::SeqCst), 1);
        assert_eq!(backend.active.load(Ordering::SeqCst), 0);
        assert_eq!(backend.completed.load(Ordering::SeqCst), 2);
    }

    #[test]
    #[ignore = "requires a permitted physical V4L2 camera; choose its ID explicitly"]
    fn manual_v4l_capture_without_ollama() {
        let id = env::var("INDEPENDENCE_CAMERA_ID")
            .ok()
            .and_then(|value| CameraId::parse(&value))
            .expect("set INDEPENDENCE_CAMERA_ID to an ID shown by /cameras");
        let backend = V4lCameraBackend;
        let cameras = backend.list().expect("could not enumerate V4L2 cameras");
        let camera = select_camera(&cameras, Some(id)).expect("requested camera is not usable");
        let photo = backend.capture(&camera).expect("camera capture failed");
        let PhotoSource::Camera(source) = photo.source() else {
            panic!("V4L2 backend returned a non-camera source");
        };
        eprintln!(
            "captured camera {} ({}) at {}x{} from {} as validated JPEG",
            source.camera().id(),
            source.camera().name(),
            source.width(),
            source.height(),
            source.input_format(),
        );
        assert!(!photo.bytes().is_empty());
    }
}
