//! Focused hardware-profile resolution for the existing voice application.
//!
//! This is configuration data, not a plugin system: it selects among the two
//! input adapters and two camera backends already compiled into Independence.
//! Explicit command-line selection takes precedence over profile defaults;
//! documented environment variables remain the next override layer.

use std::{env, ffi::OsString, path::PathBuf};

use thiserror::Error;

use crate::{
    launch::{HardwareProfile, ModelBackend, VoiceInputMode, VoiceLaunchOptions, VoicePipeline},
    security::OperatingMode,
    speak::tts::{DEFAULT_HAL_MODEL, DEFAULT_PIPER_COMMAND, PiperConfig},
    think::{OllamaConfig, ThinkError},
};

const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";
const DEFAULT_HAILO_VLM_URL: &str = "http://127.0.0.1:11435";
const LAPTOP_MODEL: &str = "qwen3-vl:4b-instruct";
const PI_TEXT_MODEL: &str = "gemma3:1b";
/// This is the local bridge's model alias, not an Ollama model tag. The bridge
/// maps it to the pre-downloaded Hailo Qwen2-VL HEF.
const HAILO_VLM_MODEL: &str = "qwen2-vl:2b";
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
    /// The selected model's name belongs to the backend choice, not to a
    /// hardware profile. The fixture backend deliberately leaves it unused.
    pub model: String,
    /// Strict loopback-only configuration for an Ollama-compatible endpoint.
    /// This is absent for the fixture plumbing backend so it cannot
    /// accidentally probe or contact a model server.
    pub ollama: Option<OllamaConfig>,
    pub camera_backend: CameraBackendSelection,
    pub pipeline: VoicePipeline,
    pub backend: ModelBackend,
    pub operating_mode: OperatingMode,
}

#[derive(Clone, Debug, Default)]
struct EnvironmentOverrides {
    microphone: Option<String>,
    audio_output: Option<String>,
    whisper_model: Option<PathBuf>,
    tts_command: Option<OsString>,
    tts_model: Option<PathBuf>,
    tts_config: Option<PathBuf>,
    backend: Option<String>,
    model: Option<String>,
    ollama_url: Option<String>,
    hailo_vlm_url: Option<String>,
    // Kept for compatibility with the existing typed/Ollama launch path.
    ollama_model: Option<String>,
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
            backend: nonempty_var("INDEPENDENCE_BACKEND"),
            model: nonempty_var("INDEPENDENCE_MODEL"),
            ollama_url: nonempty_var("OLLAMA_URL"),
            hailo_vlm_url: nonempty_var("INDEPENDENCE_HAILO_VLM_URL"),
            ollama_model: nonempty_var("OLLAMA_MODEL"),
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
    backend: ModelBackend,
    camera_backend: CameraBackendSelection,
}

fn defaults_for(profile: Option<HardwareProfile>) -> ProfileDefaults {
    match profile {
        Some(HardwareProfile::RaspberryPi) => ProfileDefaults {
            input: VoiceInputMode::Buttons,
            model: PI_TEXT_MODEL,
            backend: ModelBackend::Ollama,
            camera_backend: CameraBackendSelection::RpiCamStill,
        },
        Some(HardwareProfile::RaspberryPiHailo) => ProfileDefaults {
            input: VoiceInputMode::Buttons,
            model: HAILO_VLM_MODEL,
            backend: ModelBackend::HailoVlm,
            camera_backend: CameraBackendSelection::RpiCamStill,
        },
        None => ProfileDefaults {
            input: VoiceInputMode::Terminal,
            model: LAPTOP_MODEL,
            backend: ModelBackend::Ollama,
            camera_backend: CameraBackendSelection::V4l,
        },
    }
}

/// Resolves profile defaults, existing environment overrides, and explicit
/// command-line selections. The precedence is:
///
/// `--input` / `--input-device` / `--audio-input` / `--audio-output` /
/// `--model` / `--pipeline` / `--backend` > environment > profile defaults >
/// ordinary laptop defaults. The Hailo Pi profile also chooses its documented
/// local bridge backend; model capabilities are still discovered from that
/// backend at runtime rather than assumed from a profile name.
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

    let pipeline = options
        .pipeline
        .unwrap_or(VoicePipeline::TranscribeAndSpeak);
    let backend = match options.backend {
        Some(backend) => backend,
        None => match environment.backend.as_deref() {
            Some(value) => ModelBackend::parse(value).map_err(ProfileError::InvalidBackend)?,
            None => defaults.backend,
        },
    };
    let model = options
        .model
        .clone()
        .or(environment.model)
        .or(environment.ollama_model)
        .unwrap_or_else(|| {
            if backend == ModelBackend::HailoVlm {
                HAILO_VLM_MODEL.to_owned()
            } else {
                defaults.model.to_owned()
            }
        });
    if model.trim().is_empty() {
        return Err(ProfileError::EmptyModel);
    }

    // Profile resolution selects names and local endpoint settings only. It
    // deliberately does not declare model capabilities or pipeline support:
    // the selected backend decides those at runtime from its own metadata and
    // implemented request/response contract.
    let ollama = if backend.uses_ollama_compatible_http() {
        let endpoint = environment
            .ollama_url
            .or(if backend == ModelBackend::HailoVlm {
                environment.hailo_vlm_url
            } else {
                None
            })
            .unwrap_or_else(|| match backend {
                ModelBackend::HailoVlm => DEFAULT_HAILO_VLM_URL.to_owned(),
                ModelBackend::Ollama => DEFAULT_OLLAMA_URL.to_owned(),
                ModelBackend::Fixture => unreachable!("fixture has no HTTP endpoint"),
            });
        Some(OllamaConfig::new(&endpoint, model.clone()).map_err(ProfileError::Ollama)?)
    } else {
        None
    };

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
        model,
        ollama,
        camera_backend,
        pipeline,
        backend,
        operating_mode: options.mode.unwrap_or(OperatingMode::Ephemeral),
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
    #[error("INDEPENDENCE_CAMERA_BACKEND must be `v4l2` or `rpicam-still`, not `{0}")]
    InvalidCameraBackend(String),
    #[error("unknown backend `{0}`; use `ollama`, `hailo-vlm`, or `fixture`")]
    InvalidBackend(String),
    #[error("model name must not be empty")]
    EmptyModel,
    #[error("invalid local Ollama configuration: {0}")]
    Ollama(#[source] ThinkError),
}

#[cfg(test)]
mod tests {
    use crate::launch::{
        HardwareProfile, ModelBackend, VoiceInputMode, VoiceLaunchOptions, VoicePipeline,
    };
    use crate::security::OperatingMode;

    use super::{
        CameraBackendSelection, EnvironmentOverrides, ResolvedVoiceInput, defaults_for,
        resolve_with_environment,
    };

    #[test]
    fn pi_defaults_select_hardware_and_text_model_without_claiming_capabilities() {
        let defaults = defaults_for(Some(HardwareProfile::RaspberryPi));
        assert_eq!(defaults.input, VoiceInputMode::Buttons);
        assert_eq!(defaults.model, "gemma3:1b");
        assert_eq!(defaults.backend, ModelBackend::Ollama);
        assert_eq!(defaults.camera_backend, CameraBackendSelection::RpiCamStill);
    }

    #[test]
    fn hailo_pi_profile_selects_the_local_bridge_but_not_its_capabilities() {
        let defaults = defaults_for(Some(HardwareProfile::RaspberryPiHailo));
        assert_eq!(defaults.input, VoiceInputMode::Buttons);
        assert_eq!(defaults.model, "qwen2-vl:2b");
        assert_eq!(defaults.backend, ModelBackend::HailoVlm);
        assert_eq!(defaults.camera_backend, CameraBackendSelection::RpiCamStill);

        let resolved = resolve_with_environment(
            &VoiceLaunchOptions {
                profile: Some(HardwareProfile::RaspberryPiHailo),
                input_device: Some("/dev/input/by-id/hailo-buttons".into()),
                ..VoiceLaunchOptions::default()
            },
            EnvironmentOverrides::default(),
        )
        .unwrap();
        assert_eq!(resolved.backend, ModelBackend::HailoVlm);
        assert_eq!(resolved.model, "qwen2-vl:2b");
        assert_eq!(
            resolved.ollama,
            Some(crate::think::OllamaConfig::new("http://127.0.0.1:11435", "qwen2-vl:2b").unwrap())
        );
    }

    #[test]
    fn hailo_endpoint_override_is_specific_and_still_loopback_validated() {
        let options = VoiceLaunchOptions {
            profile: Some(HardwareProfile::RaspberryPiHailo),
            input_device: Some("/dev/input/by-id/hailo-buttons".into()),
            ..VoiceLaunchOptions::default()
        };
        let resolved = resolve_with_environment(
            &options,
            EnvironmentOverrides {
                hailo_vlm_url: Some("http://127.0.0.1:11500".into()),
                ..EnvironmentOverrides::default()
            },
        )
        .unwrap();
        assert_eq!(
            resolved.ollama,
            Some(crate::think::OllamaConfig::new("http://127.0.0.1:11500", "qwen2-vl:2b").unwrap())
        );

        let error = resolve_with_environment(
            &options,
            EnvironmentOverrides {
                hailo_vlm_url: Some("http://192.0.2.10:11500".into()),
                ..EnvironmentOverrides::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("loopback"));
    }

    #[test]
    fn explicit_audio_selections_beat_environment_and_profile_defaults() {
        let options = VoiceLaunchOptions {
            profile: Some(HardwareProfile::RaspberryPi),
            input: Some(VoiceInputMode::Buttons),
            input_device: Some("/dev/input/by-id/button-kbd".into()),
            audio_input: Some("alsa:usb-microphone".into()),
            audio_output: Some("alsa:usb-speaker".into()),
            ..VoiceLaunchOptions::default()
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
        assert_eq!(resolved.operating_mode, OperatingMode::Ephemeral);
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
    fn profile_resolves_a_model_without_declaring_its_capabilities() {
        let options = VoiceLaunchOptions::default();
        let resolved = resolve_with_environment(
            &options,
            EnvironmentOverrides {
                ollama_model: Some("a-verified-vision-model".into()),
                ..EnvironmentOverrides::default()
            },
        )
        .unwrap();
        assert_eq!(resolved.model, "a-verified-vision-model");
        assert!(resolved.ollama.is_some());
    }

    #[test]
    fn explicit_model_and_pipeline_are_not_part_of_the_hardware_profile() {
        let resolved = resolve_with_environment(
            &VoiceLaunchOptions {
                model: Some("my-local-model".into()),
                pipeline: Some(VoicePipeline::AudioToAudio),
                backend: Some(ModelBackend::Fixture),
                ..VoiceLaunchOptions::default()
            },
            EnvironmentOverrides::default(),
        )
        .unwrap();
        assert_eq!(resolved.model, "my-local-model");
        assert_eq!(resolved.pipeline, VoicePipeline::AudioToAudio);
        assert_eq!(resolved.backend, ModelBackend::Fixture);
        assert!(resolved.ollama.is_none());
    }

    #[test]
    fn backend_precedence_is_cli_then_environment_then_ollama_default() {
        let explicit = resolve_with_environment(
            &VoiceLaunchOptions {
                pipeline: Some(VoicePipeline::AudioToAudio),
                backend: Some(ModelBackend::Fixture),
                ..VoiceLaunchOptions::default()
            },
            EnvironmentOverrides {
                backend: Some("ollama".into()),
                ..EnvironmentOverrides::default()
            },
        )
        .unwrap();
        assert_eq!(explicit.backend, ModelBackend::Fixture);

        let from_environment = resolve_with_environment(
            &VoiceLaunchOptions {
                pipeline: Some(VoicePipeline::AudioToAudio),
                ..VoiceLaunchOptions::default()
            },
            EnvironmentOverrides {
                backend: Some("fixture".into()),
                ..EnvironmentOverrides::default()
            },
        )
        .unwrap();
        assert_eq!(from_environment.backend, ModelBackend::Fixture);

        let default_backend = resolve_with_environment(
            &VoiceLaunchOptions {
                pipeline: Some(VoicePipeline::AudioToAudio),
                ..VoiceLaunchOptions::default()
            },
            EnvironmentOverrides::default(),
        )
        .unwrap();
        assert_eq!(default_backend.backend, ModelBackend::Ollama);
        assert!(default_backend.ollama.is_some());

        let invalid = resolve_with_environment(
            &VoiceLaunchOptions::default(),
            EnvironmentOverrides {
                backend: Some("some-future-backend".into()),
                ..EnvironmentOverrides::default()
            },
        )
        .unwrap_err();
        assert!(invalid.to_string().contains("unknown backend"));
    }

    #[test]
    fn profile_defers_fixture_pipeline_support_to_runtime() {
        let resolved = resolve_with_environment(
            &VoiceLaunchOptions {
                backend: Some(ModelBackend::Fixture),
                ..VoiceLaunchOptions::default()
            },
            EnvironmentOverrides::default(),
        )
        .unwrap();
        assert_eq!(resolved.backend, ModelBackend::Fixture);
        assert_eq!(resolved.pipeline, VoicePipeline::TranscribeAndSpeak);
        assert!(resolved.ollama.is_none());
    }

    #[test]
    fn generic_model_environment_beats_legacy_ollama_model() {
        let resolved = resolve_with_environment(
            &VoiceLaunchOptions::default(),
            EnvironmentOverrides {
                model: Some("generic-selected-model".into()),
                ollama_model: Some("legacy-ollama-model".into()),
                ..EnvironmentOverrides::default()
            },
        )
        .unwrap();
        assert_eq!(resolved.model, "generic-selected-model");
        assert_eq!(
            resolved.ollama.as_ref().unwrap().model(),
            "generic-selected-model"
        );

        let explicit = resolve_with_environment(
            &VoiceLaunchOptions {
                model: Some("command-line-model".into()),
                ..VoiceLaunchOptions::default()
            },
            EnvironmentOverrides {
                model: Some("generic-selected-model".into()),
                ollama_model: Some("legacy-ollama-model".into()),
                ..EnvironmentOverrides::default()
            },
        )
        .unwrap();
        assert_eq!(explicit.model, "command-line-model");
    }
}
