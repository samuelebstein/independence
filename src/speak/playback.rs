//! Generic local audio-output selection and Rodio playback.
//!
//! This module knows operating-system audio devices and WAV decoding. It does
//! not know about dialogue, models, microphone capture, or controller state.

use std::{
    env,
    io::Cursor,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::Duration,
};

use rodio::cpal::traits::DeviceTrait;
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, cpal::traits::HostTrait};
use thiserror::Error;

/// One output device suitable for presenting to a person. The name is an OS
/// identifier, not an Independence-owned nickname.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputDevice {
    /// CPAL's device identifier. This is the only selector accepted by the
    /// command-line/environment override because friendly labels can repeat.
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// Lists every output-capable device exposed by Rodio's CPAL backend. It does
/// not open a playback stream or emit any sound.
pub fn list_output_devices() -> Result<Vec<OutputDevice>, PlaybackError> {
    let host = rodio::cpal::default_host();
    let default_id = host
        .default_output_device()
        .and_then(|device| device.id().ok())
        .map(|id| id.to_string());
    let mut devices = host
        .output_devices()
        .map_err(|error| PlaybackError::DeviceEnumeration(error.to_string()))?
        .map(|device| {
            let id = device_identifier(&device)?;
            Ok(OutputDevice {
                name: device_label(&device),
                is_default: default_id.as_deref() == Some(id.as_str()),
                id,
            })
        })
        .collect::<Result<Vec<_>, PlaybackError>>()?;
    devices.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(devices)
}

/// `INDEPENDENCE_AUDIO_OUTPUT` is an exact CPAL output-device identifier.
/// Leaving it
/// unset asks Rodio for the current system-default output device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputSelection {
    requested_id: Option<String>,
}

impl OutputSelection {
    pub fn from_environment() -> Self {
        Self::from_requested_id(
            env::var("INDEPENDENCE_AUDIO_OUTPUT")
                .ok()
                .filter(|name| !name.trim().is_empty()),
        )
    }

    pub fn from_requested_id(requested_id: Option<String>) -> Self {
        Self { requested_id }
    }

    #[cfg(test)]
    pub(crate) fn identified(id: impl Into<String>) -> Self {
        Self {
            requested_id: Some(id.into()),
        }
    }

    pub fn requested_id(&self) -> Option<&str> {
        self.requested_id.as_deref()
    }
}

/// Audio bytes and ownership data passed from synthesis to playback.
#[derive(Debug)]
pub struct PlaybackRequest {
    pub utterance_id: u64,
    pub generation: u64,
    pub wav: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackResult {
    Completed,
    Cancelled,
}

/// Small test seam for real Rodio and deterministic fake playback. It is the
/// point at which tests can verify ordering without an output device.
pub trait Playback {
    fn play(
        &mut self,
        request: PlaybackRequest,
        current_generation: &AtomicU64,
    ) -> Result<PlaybackResult, PlaybackError>;
}

/// A Rodio stream remains alive for the lifetime of this player. A fresh sink
/// represents each utterance, so cancellation can stop only that utterance
/// without mixing it with the next queued one.
pub struct RodioPlayback {
    stream: MixerDeviceSink,
    device_name: String,
    device_id: String,
}

impl RodioPlayback {
    pub fn open(selection: &OutputSelection) -> Result<Self, PlaybackError> {
        let host = rodio::cpal::default_host();
        let (device, device_name, device_id) = match selection.requested_id() {
            Some(requested) => {
                let device = host
                    .output_devices()
                    .map_err(|error| PlaybackError::DeviceEnumeration(error.to_string()))?
                    .map(|device| {
                        let id = device_identifier(&device)?;
                        Ok((device, id))
                    })
                    .collect::<Result<Vec<_>, PlaybackError>>()?
                    .into_iter()
                    .find(|(_, id)| id == requested)
                    .map(|(device, _)| device)
                    .ok_or_else(|| PlaybackError::RequestedDeviceNotFound(requested.to_owned()))?;
                let name = device_label(&device);
                let id = device_identifier(&device)?;
                (device, name, id)
            }
            None => {
                let device = host
                    .default_output_device()
                    .ok_or(PlaybackError::NoDefaultDevice)?;
                let name = device_label(&device);
                let id = device_identifier(&device)?;
                (device, name, id)
            }
        };

        let stream = DeviceSinkBuilder::from_device(device)
            .map_err(|error| PlaybackError::Open(error.to_string()))?
            .open_stream()
            .map_err(|error| PlaybackError::Open(error.to_string()))?;

        Ok(Self {
            stream,
            device_name,
            device_id,
        })
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }
}

fn device_identifier(device: &rodio::cpal::Device) -> Result<String, PlaybackError> {
    device
        .id()
        .map(|id| id.to_string())
        .map_err(|error| PlaybackError::DeviceIdentifier(error.to_string()))
}

fn device_label(device: &rodio::cpal::Device) -> String {
    device
        .description()
        .map(|description| description.to_string())
        .unwrap_or_else(|_| device.to_string())
}

impl Playback for RodioPlayback {
    fn play(
        &mut self,
        request: PlaybackRequest,
        current_generation: &AtomicU64,
    ) -> Result<PlaybackResult, PlaybackError> {
        if generation_changed(request.generation, current_generation) {
            return Ok(PlaybackResult::Cancelled);
        }

        let decoder = Decoder::try_from(Cursor::new(request.wav))
            .map_err(|error| PlaybackError::Decode(error.to_string()))?;
        let player = Player::connect_new(self.stream.mixer());
        player.append(decoder);

        loop {
            if generation_changed(request.generation, current_generation) {
                player.stop();
                return Ok(PlaybackResult::Cancelled);
            }
            if player.empty() {
                return Ok(PlaybackResult::Completed);
            }
            // This is a worker thread, never a microphone callback or the
            // controller event loop. It bounds cancellation latency without
            // pretending that an output driver stops at an exact instant.
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn generation_changed(expected: u64, current: &AtomicU64) -> bool {
    current.load(Ordering::Acquire) != expected
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PlaybackError {
    #[error("could not enumerate audio output devices: {0}")]
    DeviceEnumeration(String),

    #[error("could not read an audio output device identifier: {0}")]
    DeviceIdentifier(String),

    #[error("no system-default audio output device is available")]
    NoDefaultDevice,

    #[error("requested audio output device was not found: {0}")]
    RequestedDeviceNotFound(String),

    #[error("could not open the selected audio output device: {0}")]
    Open(String),

    #[error("could not decode synthesized WAV audio: {0}")]
    Decode(String),
}

#[cfg(test)]
mod tests {
    use super::OutputSelection;

    #[test]
    fn explicit_output_selection_preserves_an_exact_identifier() {
        let selection = OutputSelection::identified("alsa:usb-Headset-00.analog-stereo");
        assert_eq!(
            selection.requested_id(),
            Some("alsa:usb-Headset-00.analog-stereo")
        );
    }
}
