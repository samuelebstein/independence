//! Focused hardware-profile resolution for the existing voice application.
//!
//! This is configuration data, not a plugin system: it selects among the two
//! input adapters and two camera backends already compiled into Independence.
//! Explicit command-line selection takes precedence over profile defaults;
//! documented environment variables remain the next override layer.

use std::{env, ffi::OsString, path::PathBuf};

use thiserror::Error;

use crate::{
    launch::{HardwareProfile, VoiceInputMode, VoiceLaunchOptions},
    speak::tts::{DEFAULT_HAL_MODEL, DEFAULT_PIPER_COMMAND, PiperConfig},
    think::{ImageInputSupport, OllamaConfig, ThinkError},
};

const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";
const LAPTOP_MODEL: &str = "qwen3-vl:4b-instruct";
const PI_TEXT_MODEL: &str = "gemma3:1b";
const DEFAULT_WHISPER_MODEL: &str = "local-data/whisper/ggml-base.en.bin";

/// Camera backends that the one existing camera service can select.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CameraBackendSelection {
    V4l,
    RpiCamStill,
}

impl CameraBackendSelection {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::V4l => "v4l2",
            Self::RpiCamStill => "rpicam-still",
        }
    }

    fn parse(value: &str) -> Result<Self, ProfileError> {
        match value {
            "v4l" | "v4l2" => Ok(Self::V4l),
            "rpicam" | "rpicam-still" => Ok(Self::RpiCamStill),
            _ => Err(ProfileError::InvalidCameraBackend(value.to_owned())),
        }
    }
}

/// The resolved input source.  Unlike raw CLI options, this always has one
/// complete choice and has rejected a missing device path for button mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResolvedVoiceInput {
    Terminal,
    Buttons(PathBuf),
}

impl ResolvedVoiceInput {
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::Terminal => "terminal raw-key controls",
            Self::Buttons(_) => "exclusive Linux evdev button controller",
        }
    }
}

/// All values that must agree for one `--voice` launch.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedVoiceProfile {
    pub profile: Option<HardwareProfile>,
    pub input: ResolvedVoiceInput,
    pub microphone: Option<String>,
    pub audio_output: Option<String>,
    pub whisper_model: PathBuf,
    pub piper: PiperConfig,
    pub ollama: OllamaConfig,
    pub camera_backend: CameraBackendSelection,
}

#[derive(Clone, Debug, Default)]
struct EnvironmentOverrides {
    microphone: Option<String>,
    audio_output: Option<String>,
    whisper_model: Option<PathBuf>,
    tts_command: Option<OsString>,
    tts_model: Option<PathBuf>,
    tts_config: Option<PathBuf>,
    ollama_url: Option<String>,
    ollama_model: Option<String>,
    image_support: Option<String>,
    camera_backend: Option<String>,
}

impl EnvironmentOverrides {
    fn from_process() -> Self {
        Self {
            microphone: nonempty_var("INDEPENDENCE_MICROPHONE"),
            audio_output: nonempty_var("INDEPENDENCE_AUDIO_OUTPUT"),
            whisper_model: env::var_os("INDEPENDENCE_WHISPER_MODEL").map(PathBuf::from),
            tts_command: env::var_os("INDEPENDENCE_PIPER_COMMAND"),
            tts_model: env::var_os("INDEPENDENCE_TTS_MODEL").map(PathBuf::from),
            tts_config: env::var_os("INDEPENDENCE_TTS_CONFIG").map(PathBuf::from),
            ollama_url: nonempty_var("OLLAMA_URL"),
            ollama_model: nonempty_var("OLLAMA_MODEL"),
            image_support: nonempty_var("INDEPENDENCE_IMAGE_MODEL_SUPPORT"),
            camera_backend: nonempty_var("INDEPENDENCE_CAMERA_BACKEND"),
        }
    }
}

fn nonempty_var(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.trim().is_empty())
}

#[derive(Clone, Copy)]
struct ProfileDefaults {
    input: VoiceInputMode,
    model: &'static str,
    image_support: ImageInputSupport,
    camera_backend: CameraBackendSelection,
}

fn defaults_for(profile: Option<HardwareProfile>) -> ProfileDefaults {
    match profile {
        Some(HardwareProfile::RaspberryPi) => ProfileDefaults {
            input: VoiceInputMode::Buttons,
            model: PI_TEXT_MODEL,
            // gemma3:1b is deliberately treated as text-only until a person
            // explicitly configures a verified vision model/support pairing.
            image_support: ImageInputSupport::TextOnly,
            camera_backend: CameraBackendSelection::RpiCamStill,
        },
        None => ProfileDefaults {
            input: VoiceInputMode::Terminal,
            model: LAPTOP_MODEL,
            image_support: ImageInputSupport::Vision,
            camera_backend: CameraBackendSelection::V4l,
        },
    }
}

/// Resolves profile defaults, existing environment overrides, and explicit
/// command-line selections. The precedence is:
///
/// `--input` / `--input-device` / `--audio-input` / `--audio-output` >
/// environment > profile defaults > ordinary laptop defaults.
pub(crate) fn resolve_voice_profile(
    options: &VoiceLaunchOptions,
) -> Result<ResolvedVoiceProfile, ProfileError> {
    resolve_with_environment(options, EnvironmentOverrides::from_process())
}

fn resolve_with_environment(
    options: &VoiceLaunchOptions,
    environment: EnvironmentOverrides,
) -> Result<ResolvedVoiceProfile, ProfileError> {
    let defaults = defaults_for(options.profile);
    let input_mode = options.input.unwrap_or(defaults.input);
    let input = match (input_mode, options.input_device.as_ref()) {
        (VoiceInputMode::Terminal, None) => ResolvedVoiceInput::Terminal,
        (VoiceInputMode::Terminal, Some(_)) => return Err(ProfileError::TerminalDevicePath),
        (VoiceInputMode::Buttons, Some(path)) => ResolvedVoiceInput::Buttons(path.clone()),
        (VoiceInputMode::Buttons, None) => return Err(ProfileError::MissingButtonDevice),
    };

    let model = environment
        .ollama_model
        .unwrap_or_else(|| defaults.model.to_owned());
    let image_support = match environment.image_support {
        Some(value) => match value.as_str() {
            "vision" => ImageInputSupport::Vision,
            "text" | "text-only" => ImageInputSupport::TextOnly,
            _ => return Err(ProfileError::InvalidImageSupport(value)),
        },
        None => defaults.image_support,
    };
    let endpoint = environment
        .ollama_url
        .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_owned());
    let ollama = OllamaConfig::new_with_image_support(&endpoint, model, image_support)
        .map_err(ProfileError::Ollama)?;

    let whisper_model = environment
        .whisper_model
        .unwrap_or_else(|| PathBuf::from(DEFAULT_WHISPER_MODEL));
    let tts_model = environment
        .tts_model
        .unwrap_or_else(|| PathBuf::from(DEFAULT_HAL_MODEL));
    let tts_config = environment
        .tts_config
        .unwrap_or_else(|| PathBuf::from(format!("{}.json", tts_model.display())));
    let piper = PiperConfig::new(
        environment
            .tts_command
            .unwrap_or_else(|| OsString::from(DEFAULT_PIPER_COMMAND)),
        tts_model,
        tts_config,
    );
    let camera_backend = match environment.camera_backend {
        Some(value) => CameraBackendSelection::parse(&value)?,
        None => defaults.camera_backend,
    };

    Ok(ResolvedVoiceProfile {
        profile: options.profile,
        input,
        microphone: options.audio_input.clone().or(environment.microphone),
        audio_output: options.audio_output.clone().or(environment.audio_output),
        whisper_model,
        piper,
        ollama,
        camera_backend,
    })
}

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error(
        "--input buttons requires --input-device <selected /dev/input path>; run --input-devices first"
    )]
    MissingButtonDevice,
    #[error("--input-device is meaningful only with --input buttons")]
    TerminalDevicePath,
    #[error("INDEPENDENCE_IMAGE_MODEL_SUPPORT must be `vision`, `text`, or `text-only`, not `{0}`")]
    InvalidImageSupport(String),
    #[error("INDEPENDENCE_CAMERA_BACKEND must be `v4l2` or `rpicam-still`, not `{0}")]
    InvalidCameraBackend(String),
    #[error("invalid local Ollama configuration: {0}")]
    Ollama(#[source] ThinkError),
}

#[cfg(test)]
mod tests {
    use crate::{
        launch::{HardwareProfile, VoiceInputMode, VoiceLaunchOptions},
        think::ImageInputSupport,
    };

    use super::{
        CameraBackendSelection, EnvironmentOverrides, ResolvedVoiceInput, defaults_for,
        resolve_with_environment,
    };

    #[test]
    fn pi_defaults_are_explicit_and_conservative_about_images() {
        let defaults = defaults_for(Some(HardwareProfile::RaspberryPi));
        assert_eq!(defaults.input, VoiceInputMode::Buttons);
        assert_eq!(defaults.model, "gemma3:1b");
        assert_eq!(defaults.image_support, ImageInputSupport::TextOnly);
        assert_eq!(defaults.camera_backend, CameraBackendSelection::RpiCamStill);
    }

    #[test]
    fn explicit_audio_selections_beat_environment_and_profile_defaults() {
        let options = VoiceLaunchOptions {
            profile: Some(HardwareProfile::RaspberryPi),
            input: Some(VoiceInputMode::Buttons),
            input_device: Some("/dev/input/by-id/button-kbd".into()),
            audio_input: Some("alsa:usb-microphone".into()),
            audio_output: Some("alsa:usb-speaker".into()),
        };
        let resolved = resolve_with_environment(
            &options,
            EnvironmentOverrides {
                microphone: Some("alsa:default-microphone".into()),
                audio_output: Some("alsa:default".into()),
                ..EnvironmentOverrides::default()
            },
        )
        .unwrap();
        assert_eq!(resolved.audio_output.as_deref(), Some("alsa:usb-speaker"));
        assert_eq!(resolved.microphone.as_deref(), Some("alsa:usb-microphone"));
        assert!(matches!(resolved.input, ResolvedVoiceInput::Buttons(_)));
    }

    #[test]
    fn pi_profile_requires_explicit_button_device() {
        let error = resolve_with_environment(
            &VoiceLaunchOptions {
                profile: Some(HardwareProfile::RaspberryPi),
                ..VoiceLaunchOptions::default()
            },
            EnvironmentOverrides::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("--input-device"));
    }

    #[test]
    fn environment_can_explicitly_enable_a_verified_vision_pairing() {
        let options = VoiceLaunchOptions::default();
        let resolved = resolve_with_environment(
            &options,
            EnvironmentOverrides {
                ollama_model: Some("a-verified-vision-model".into()),
                image_support: Some("vision".into()),
                ..EnvironmentOverrides::default()
            },
        )
        .unwrap();
        assert_eq!(resolved.ollama.model(), "a-verified-vision-model");
        assert_eq!(
            resolved.ollama.image_input_support(),
            ImageInputSupport::Vision
        );
    }
}
