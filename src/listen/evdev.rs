//! Linux evdev adapter for a keyboard used as a physical button controller.
//!
//! It reads Linux key events directly.  It deliberately does not translate a
//! keyboard layout, assemble text, or touch Crossterm/stdin.  The selected
//! device is grabbed exclusively while this adapter is active, so the same
//! keys are not delivered to a local console as ordinary keyboard input.

use std::{
    collections::BTreeMap,
    fs, io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use evdev::{EventSummary, KeyCode, raw_stream::RawDevice};
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use thiserror::Error;
use tokio::sync::mpsc::Sender;

use super::controls::{VoiceControlEvent, VoiceInputEvent, VoiceInputSource};

const SOURCE_NAME: &str = "Linux button keyboard";
const RETRY_INTERVAL: Duration = Duration::from_millis(500);
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The button-related keys a Linux input node advertises.  Listing reports
/// capabilities instead of guessing that every `/dev/input/event*` node is a
/// keyboard.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ButtonCapabilities {
    pub enter: bool,
    pub photo: bool,
    pub cancel: bool,
}

impl ButtonCapabilities {
    fn from_device(device: &RawDevice) -> Self {
        let keys = device.supported_keys();
        Self {
            enter: keys.is_some_and(|keys| keys.contains(KeyCode::KEY_ENTER)),
            photo: keys.is_some_and(|keys| keys.contains(KeyCode::KEY_P)),
            cancel: keys.is_some_and(|keys| keys.contains(KeyCode::KEY_ESC)),
        }
    }

    fn supports_controller(self) -> bool {
        self.enter && self.photo && self.cancel
    }
}

/// Read-only inventory data for one Linux input node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputDeviceInfo {
    pub path: PathBuf,
    pub stable_path: Option<PathBuf>,
    pub name: Option<String>,
    pub physical_path: Option<String>,
    pub controls: Option<ButtonCapabilities>,
    pub access_error: Option<String>,
}

impl InputDeviceInfo {
    pub fn supports_controller(&self) -> bool {
        self.controls
            .is_some_and(ButtonCapabilities::supports_controller)
    }
}

/// Lists Linux event devices without grabbing one, reading events, opening a
/// microphone, or initializing models.  Inaccessible event nodes are shown as
/// such rather than silently treated as absent, which makes permission setup
/// diagnosable without running the application as root.
pub fn list_input_devices() -> Result<Vec<InputDeviceInfo>, InputDeviceError> {
    let mut stable_paths = stable_input_paths()?;
    let mut paths =
        fs::read_dir("/dev/input").map_err(|source| InputDeviceError::List { source })?;
    let mut devices = Vec::new();

    for entry in paths.by_ref() {
        let entry = entry.map_err(|source| InputDeviceError::List { source })?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if !file_name.starts_with("event") {
            continue;
        }

        let path = entry.path();
        let stable_path = stable_paths.remove(&canonical_or_self(&path));
        match RawDevice::open(&path) {
            Ok(device) => devices.push(InputDeviceInfo {
                path,
                stable_path,
                name: device.name().map(str::to_owned),
                physical_path: device.physical_path().map(str::to_owned),
                controls: Some(ButtonCapabilities::from_device(&device)),
                access_error: None,
            }),
            Err(error) => devices.push(InputDeviceInfo {
                path,
                stable_path,
                name: None,
                physical_path: None,
                controls: None,
                access_error: Some(error.to_string()),
            }),
        }
    }

    devices.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(devices)
}

fn stable_input_paths() -> Result<BTreeMap<PathBuf, PathBuf>, InputDeviceError> {
    let directory = Path::new("/dev/input/by-id");
    if !directory.exists() {
        return Ok(BTreeMap::new());
    }

    let entries =
        fs::read_dir(directory).map_err(|source| InputDeviceError::StablePaths { source })?;
    let mut result = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|source| InputDeviceError::StablePaths { source })?;
        let path = entry.path();
        let Ok(target) = fs::canonicalize(&path) else {
            continue;
        };
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.contains("event") {
            result.entry(target).or_insert(path);
        }
    }
    Ok(result)
}

fn canonical_or_self(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

/// Maps raw Linux evdev key values.  Values are specified by Linux input:
/// press is `1`, release is `0`, and autorepeat is `2`.  Autorepeat is
/// intentionally ignored so a held key cannot duplicate a talk/photo action.
pub(crate) fn classify_button_key(code: KeyCode, value: i32) -> Option<VoiceControlEvent> {
    match (code, value) {
        (KeyCode::KEY_ENTER, 1) => Some(VoiceControlEvent::TalkPressed),
        (KeyCode::KEY_ENTER, 0) => Some(VoiceControlEvent::TalkReleased),
        (KeyCode::KEY_P, 1) => Some(VoiceControlEvent::PhotoPressed),
        (KeyCode::KEY_ESC, 1) => Some(VoiceControlEvent::Cancel),
        _ => None,
    }
}

/// Owns one selected evdev device and its reader thread.
pub struct EvdevButtonInputSource {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl EvdevButtonInputSource {
    /// Opens and exclusively grabs the exact requested node before starting
    /// the worker.  This makes an incorrect path or permission problem a
    /// startup error; the application never falls back to terminal controls.
    pub fn open(path: PathBuf, sender: Sender<VoiceInputEvent>) -> Result<Self, InputDeviceError> {
        let device = open_grabbed(&path)?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let thread = thread::Builder::new()
            .name("independence-evdev-buttons".to_owned())
            .spawn(move || run_reader(path, device, sender, worker_stop))
            .map_err(InputDeviceError::Spawn)?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl VoiceInputSource for EvdevButtonInputSource {
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for EvdevButtonInputSource {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn open_grabbed(path: &Path) -> Result<RawDevice, InputDeviceError> {
    let mut device = RawDevice::open(path).map_err(|source| InputDeviceError::Open {
        path: path.to_owned(),
        source,
    })?;
    let controls = ButtonCapabilities::from_device(&device);
    if !controls.supports_controller() {
        return Err(InputDeviceError::UnsupportedControls {
            path: path.to_owned(),
            controls,
        });
    }
    set_nonblocking(&device).map_err(|source| InputDeviceError::Configure {
        path: path.to_owned(),
        source,
    })?;
    device.grab().map_err(|source| InputDeviceError::Grab {
        path: path.to_owned(),
        source,
    })?;
    Ok(device)
}

fn set_nonblocking(device: &RawDevice) -> io::Result<()> {
    let raw_fd = device.as_raw_fd();
    let current = fcntl(raw_fd, FcntlArg::F_GETFL).map_err(io::Error::from)?;
    let mut flags = OFlag::from_bits_retain(current);
    flags.insert(OFlag::O_NONBLOCK);
    fcntl(raw_fd, FcntlArg::F_SETFL(flags)).map_err(io::Error::from)?;
    Ok(())
}

fn run_reader(
    path: PathBuf,
    mut device: RawDevice,
    sender: Sender<VoiceInputEvent>,
    stop: Arc<AtomicBool>,
) {
    loop {
        match read_until_lost(&mut device, &sender, &stop) {
            Ok(()) if stop.load(Ordering::Acquire) => {
                let _ = device.ungrab();
                return;
            }
            Ok(()) => unreachable!("a live evdev reader only returns on shutdown or failure"),
            Err(message) => {
                let _ = device.ungrab();
                if sender
                    .blocking_send(VoiceInputEvent::SourceLost {
                        source: SOURCE_NAME,
                        message,
                    })
                    .is_err()
                {
                    return;
                }
            }
        }

        while !stop.load(Ordering::Acquire) {
            thread::sleep(RETRY_INTERVAL);
            if let Ok(next) = open_grabbed(&path) {
                device = next;
                if sender
                    .blocking_send(VoiceInputEvent::SourceRecovered {
                        source: SOURCE_NAME,
                    })
                    .is_err()
                {
                    return;
                }
                break;
            }
        }

        if stop.load(Ordering::Acquire) {
            let _ = device.ungrab();
            return;
        }
    }
}

fn read_until_lost(
    device: &mut RawDevice,
    sender: &Sender<VoiceInputEvent>,
    stop: &AtomicBool,
) -> Result<(), String> {
    while !stop.load(Ordering::Acquire) {
        match device.fetch_events() {
            Ok(events) => {
                for event in events {
                    match event.destructure() {
                        EventSummary::Synchronization(
                            _,
                            evdev::SynchronizationCode::SYN_DROPPED,
                            _,
                        ) => {
                            // We might have missed a release. Cancelling is
                            // safer than submitting audio from an uncertain
                            // press lifecycle. Reopen/re-grab before any new
                            // press can be trusted.
                            return Err("event synchronization was lost".into());
                        }
                        EventSummary::Key(_, code, value) => {
                            if let Some(control) = classify_button_key(code, value)
                                && sender
                                    .blocking_send(VoiceInputEvent::Control(control))
                                    .is_err()
                            {
                                return Err(
                                    "the application stopped receiving button events".into()
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(IDLE_POLL_INTERVAL);
            }
            Err(error) => return Err(format!("could not read {SOURCE_NAME}: {error}")),
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum InputDeviceError {
    #[error("could not inspect /dev/input: {source}")]
    List {
        #[source]
        source: io::Error,
    },
    #[error("could not inspect /dev/input/by-id: {source}")]
    StablePaths {
        #[source]
        source: io::Error,
    },
    #[error("could not open input device {}: {source}", path.display())]
    Open {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not configure input device {}: {source}", path.display())]
    Configure {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not exclusively grab input device {}: {source}", path.display())]
    Grab {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "input device {} cannot be the button controller: required keys enter={}, P={}, Escape={} are not all supported",
        path.display(), controls.enter, controls.photo, controls.cancel
    )]
    UnsupportedControls {
        path: PathBuf,
        controls: ButtonCapabilities,
    },
    #[error("could not start the input-device worker: {0}")]
    Spawn(#[source] io::Error),
}

#[cfg(test)]
mod tests {
    use evdev::KeyCode;

    use crate::listen::controls::VoiceControlEvent;

    use super::classify_button_key;

    #[test]
    fn maps_only_press_and_release_events_for_the_four_controls() {
        assert_eq!(
            classify_button_key(KeyCode::KEY_ENTER, 1),
            Some(VoiceControlEvent::TalkPressed)
        );
        assert_eq!(
            classify_button_key(KeyCode::KEY_ENTER, 0),
            Some(VoiceControlEvent::TalkReleased)
        );
        assert_eq!(
            classify_button_key(KeyCode::KEY_P, 1),
            Some(VoiceControlEvent::PhotoPressed)
        );
        assert_eq!(
            classify_button_key(KeyCode::KEY_ESC, 1),
            Some(VoiceControlEvent::Cancel)
        );
    }

    #[test]
    fn ignores_autorepeat_and_unrelated_keys() {
        assert_eq!(classify_button_key(KeyCode::KEY_ENTER, 2), None);
        assert_eq!(classify_button_key(KeyCode::KEY_P, 0), None);
        assert_eq!(classify_button_key(KeyCode::KEY_A, 1), None);
    }
}
