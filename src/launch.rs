//! Small, explicit command-line selection for Independence runtime modes.
//!
//! This is intentionally not a generic configuration framework.  It exists
//! because a directly attached keyboard must choose a different input adapter
//! from the laptop terminal, and that choice must be validated before either
//! adapter starts owning operating-system input.

use std::{ffi::OsString, path::PathBuf};

use crate::security::OperatingMode;

/// Hardware profiles understood by the application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HardwareProfile {
    RaspberryPi,
    /// A Raspberry Pi with a local Hailo VLM bridge. This keeps the ordinary
    /// Pi physical defaults while selecting the explicitly supported local
    /// inference adapter for that attached accelerator.
    RaspberryPiHailo,
}

impl HardwareProfile {
    pub fn label(self) -> &'static str {
        match self {
            Self::RaspberryPi => "raspberry-pi",
            Self::RaspberryPiHailo => "raspberry-pi-hailo",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "raspberry-pi" => Ok(Self::RaspberryPi),
            "raspberry-pi-hailo" => Ok(Self::RaspberryPiHailo),
            _ => Err(format!(
                "unknown profile `{value}`; use `raspberry-pi` or `raspberry-pi-hailo`"
            )),
        }
    }
}

/// Which device-independent control adapter should own interaction input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceInputMode {
    Terminal,
    Buttons,
}

/// The processing path selected after a completed hold-to-talk recording.
/// Hardware profiles intentionally do not choose this: a Pi and laptop can
/// use either pipeline when its required local backend is available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoicePipeline {
    TranscribeAndSpeak,
    AudioToAudio,
}

impl VoicePipeline {
    pub const fn label(self) -> &'static str {
        match self {
            Self::TranscribeAndSpeak => "transcribe-and-speak",
            Self::AudioToAudio => "audio-to-audio",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "transcribe-and-speak" => Ok(Self::TranscribeAndSpeak),
            "audio-to-audio" => Ok(Self::AudioToAudio),
            _ => Err(format!(
                "unknown pipeline `{value}`; use `transcribe-and-speak` or `audio-to-audio`"
            )),
        }
    }
}

/// The runtime responsible for serving the selected model.
///
/// `ollama` is the ordinary local model runtime. `hailo-vlm` is Independence's
/// loopback-only Hailo VLM bridge, which deliberately implements the narrow
/// Ollama HTTP request/response contract Independence already uses. `fixture`
/// is deliberately model-less: it is a deterministic direct-audio plumbing
/// check, not an alternate conversational model. Keeping this distinct from
/// `VoicePipeline` lets a backend select its documented operations without
/// changing hardware setup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelBackend {
    Ollama,
    HailoVlm,
    Fixture,
}

impl ModelBackend {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::HailoVlm => "hailo-vlm",
            Self::Fixture => "fixture",
        }
    }

    /// Both native Ollama and the local Hailo bridge use the same deliberately
    /// narrow, loopback-only HTTP contract. The transport still verifies the
    /// advertised capabilities at runtime instead of trusting this selection.
    pub(crate) const fn uses_ollama_compatible_http(self) -> bool {
        matches!(self, Self::Ollama | Self::HailoVlm)
    }

    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "ollama" => Ok(Self::Ollama),
            "hailo-vlm" => Ok(Self::HailoVlm),
            "fixture" => Ok(Self::Fixture),
            _ => Err(format!(
                "unknown backend `{value}`; use `ollama`, `hailo-vlm`, or `fixture`"
            )),
        }
    }
}

impl VoiceInputMode {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "terminal" => Ok(Self::Terminal),
            "buttons" => Ok(Self::Buttons),
            _ => Err(format!(
                "unknown input mode `{value}`; use `terminal` or `buttons`"
            )),
        }
    }
}

/// CLI-only selections for the hold-to-talk voice experience.
///
/// `input` stays optional until profile defaults are resolved by the runtime,
/// allowing an explicit `--input terminal` to override a Pi profile default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VoiceLaunchOptions {
    pub profile: Option<HardwareProfile>,
    pub input: Option<VoiceInputMode>,
    pub input_device: Option<PathBuf>,
    /// Exact CPAL input-device identifier or name. This stays a launch choice
    /// rather than mutating the process environment, so an attached USB
    /// microphone can be selected reproducibly alongside the button keyboard.
    pub audio_input: Option<String>,
    pub audio_output: Option<String>,
    /// Explicit model selection wins over `INDEPENDENCE_MODEL`, the legacy
    /// `OLLAMA_MODEL`, and profile defaults.
    pub model: Option<String>,
    pub pipeline: Option<VoicePipeline>,
    pub backend: Option<ModelBackend>,
    /// Defaults to Ephemeral Mode. Private Mode is parsed explicitly so the
    /// SecurityManager can reject unauthenticated use before hardware starts.
    pub mode: Option<OperatingMode>,
}

/// The top-level action selected from argv.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchCommand {
    Interactive { mode: OperatingMode },
    Dictate { audio_input: Option<String> },
    Voice(VoiceLaunchOptions),
    ListAudioOutputs,
    ListAudioInputs,
    ListInputDevices,
    SpeakTest { audio_output: Option<String> },
}

/// Parses application arguments without touching hardware or model files.
pub fn parse_arguments(arguments: &[OsString]) -> Result<LaunchCommand, String> {
    let values = arguments
        .iter()
        .map(|argument| {
            argument
                .to_str()
                .ok_or_else(|| "arguments must be valid UTF-8".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;

    match values.as_slice() {
        [] => Ok(LaunchCommand::Interactive {
            mode: OperatingMode::Ephemeral,
        }),
        ["--mode", mode] => Ok(LaunchCommand::Interactive {
            mode: OperatingMode::parse(mode)?,
        }),
        ["--dictate", remaining @ ..] => parse_dictate_options(remaining)
            .map(|audio_input| LaunchCommand::Dictate { audio_input }),
        ["--audio-devices"] => Ok(LaunchCommand::ListAudioOutputs),
        ["--audio-input-devices"] => Ok(LaunchCommand::ListAudioInputs),
        ["--input-devices"] => Ok(LaunchCommand::ListInputDevices),
        ["--voice", remaining @ ..] => parse_voice_options(remaining).map(LaunchCommand::Voice),
        ["--speak-test", remaining @ ..] => parse_speak_test_options(remaining)
            .map(|audio_output| LaunchCommand::SpeakTest { audio_output }),
        _ => Err("unrecognized command-line arguments".into()),
    }
}

fn parse_voice_options(values: &[&str]) -> Result<VoiceLaunchOptions, String> {
    let mut options = VoiceLaunchOptions::default();
    let mut index = 0;
    while index < values.len() {
        let flag = values[index];
        let value = values
            .get(index + 1)
            .ok_or_else(|| format!("{flag} requires a value"))?;
        match flag {
            "--profile" => set_once(
                &mut options.profile,
                HardwareProfile::parse(value)?,
                "--profile",
            )?,
            "--input" => set_once(&mut options.input, VoiceInputMode::parse(value)?, "--input")?,
            "--input-device" => set_once(
                &mut options.input_device,
                PathBuf::from(value),
                "--input-device",
            )?,
            "--audio-input" => set_once(
                &mut options.audio_input,
                (*value).to_owned(),
                "--audio-input",
            )?,
            "--audio-output" => set_once(
                &mut options.audio_output,
                (*value).to_owned(),
                "--audio-output",
            )?,
            "--model" => set_once(&mut options.model, (*value).to_owned(), "--model")?,
            "--pipeline" => set_once(
                &mut options.pipeline,
                VoicePipeline::parse(value)?,
                "--pipeline",
            )?,
            "--backend" => set_once(
                &mut options.backend,
                ModelBackend::parse(value)?,
                "--backend",
            )?,
            "--mode" => set_once(&mut options.mode, OperatingMode::parse(value)?, "--mode")?,
            "--audio-backend" => {
                return Err("`--audio-backend` was renamed to `--backend`".into());
            }
            _ => return Err(format!("unknown --voice option `{flag}`")),
        }
        index += 2;
    }
    Ok(options)
}

fn parse_dictate_options(values: &[&str]) -> Result<Option<String>, String> {
    match values {
        [] => Ok(None),
        ["--audio-input", value] => Ok(Some((*value).to_owned())),
        _ => Err("--dictate accepts only --audio-input <exact device id>".into()),
    }
}

fn parse_speak_test_options(values: &[&str]) -> Result<Option<String>, String> {
    match values {
        [] => Ok(None),
        ["--audio-output", value] => Ok(Some((*value).to_owned())),
        _ => Err("--speak-test accepts only --audio-output <exact device id>".into()),
    }
}

fn set_once<T>(slot: &mut Option<T>, value: T, flag: &str) -> Result<(), String> {
    if slot.replace(value).is_some() {
        return Err(format!("{flag} may be provided only once"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{
        HardwareProfile, LaunchCommand, ModelBackend, VoiceInputMode, VoiceLaunchOptions,
        VoicePipeline, parse_arguments,
    };
    use crate::security::OperatingMode;

    fn parse(values: &[&str]) -> Result<LaunchCommand, String> {
        parse_arguments(&values.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn voice_options_are_order_independent_and_literal() {
        assert_eq!(
            parse(&[
                "--voice",
                "--audio-output",
                "alsa:usb-speaker",
                "--audio-input",
                "alsa:usb-microphone",
                "--input-device",
                "/dev/input/by-id/usb-controller-event-kbd",
                "--input",
                "buttons",
                "--profile",
                "raspberry-pi",
            ]),
            Ok(LaunchCommand::Voice(VoiceLaunchOptions {
                profile: Some(HardwareProfile::RaspberryPi),
                input: Some(VoiceInputMode::Buttons),
                input_device: Some("/dev/input/by-id/usb-controller-event-kbd".into()),
                audio_input: Some("alsa:usb-microphone".into()),
                audio_output: Some("alsa:usb-speaker".into()),
                model: None,
                pipeline: None,
                backend: None,
                mode: None,
            }))
        );
    }

    #[test]
    fn input_device_needs_an_explicit_input_mode_later_but_is_parsed_literally() {
        let options = match parse(&["--voice", "--input-device", "/dev/input/event9"]).unwrap() {
            LaunchCommand::Voice(options) => options,
            _ => unreachable!(),
        };
        assert_eq!(
            options.input_device.unwrap(),
            std::path::PathBuf::from("/dev/input/event9")
        );
    }

    #[test]
    fn dictate_accepts_one_explicit_audio_input() {
        assert_eq!(
            parse(&["--dictate"]),
            Ok(LaunchCommand::Dictate { audio_input: None })
        );
        assert_eq!(
            parse(&["--dictate", "--audio-input", "alsa:usb-microphone"]),
            Ok(LaunchCommand::Dictate {
                audio_input: Some("alsa:usb-microphone".into())
            })
        );
    }

    #[test]
    fn rejects_unknown_or_duplicate_options() {
        assert!(parse(&["--voice", "--input", "touch"]).is_err());
        assert!(parse(&["--voice", "--input", "terminal", "--input", "buttons"]).is_err());
        assert!(parse(&["--dictate", "--audio-output", "alsa:usb-speaker"]).is_err());
        assert!(
            parse(&[
                "--dictate",
                "--audio-input",
                "first",
                "--audio-input",
                "second"
            ])
            .is_err()
        );
        assert!(parse(&["--input-devices", "--voice"]).is_err());
    }

    #[test]
    fn pipeline_and_model_are_explicit_voice_choices() {
        let options = match parse(&[
            "--voice",
            "--pipeline",
            "audio-to-audio",
            "--backend",
            "fixture",
            "--model",
            "local-audio-model",
        ])
        .unwrap()
        {
            LaunchCommand::Voice(options) => options,
            _ => unreachable!(),
        };
        assert_eq!(options.pipeline, Some(VoicePipeline::AudioToAudio));
        assert_eq!(options.backend, Some(ModelBackend::Fixture));
        assert_eq!(options.model.as_deref(), Some("local-audio-model"));
        assert_eq!(options.mode, None);
    }

    #[test]
    fn backend_is_a_voice_choice_and_old_audio_name_explains_the_rename() {
        let options =
            match parse(&["--voice", "--backend", "ollama", "--model", "local-model"]).unwrap() {
                LaunchCommand::Voice(options) => options,
                _ => unreachable!(),
            };
        assert_eq!(options.backend, Some(ModelBackend::Ollama));
        assert_eq!(options.model.as_deref(), Some("local-model"));

        let error = parse(&[
            "--voice",
            "--pipeline",
            "audio-to-audio",
            "--audio-backend",
            "fixture",
        ])
        .unwrap_err();
        assert!(error.contains("renamed to `--backend`"));
    }

    #[test]
    fn hailo_profile_and_backend_are_explicit_choices() {
        let options = match parse(&[
            "--voice",
            "--profile",
            "raspberry-pi-hailo",
            "--backend",
            "hailo-vlm",
        ])
        .unwrap()
        {
            LaunchCommand::Voice(options) => options,
            _ => unreachable!(),
        };
        assert_eq!(options.profile, Some(HardwareProfile::RaspberryPiHailo));
        assert_eq!(options.backend, Some(ModelBackend::HailoVlm));
        assert!(ModelBackend::HailoVlm.uses_ollama_compatible_http());
    }

    #[test]
    fn operating_mode_defaults_to_ephemeral_and_private_is_explicit() {
        assert_eq!(
            parse(&[]),
            Ok(LaunchCommand::Interactive {
                mode: OperatingMode::Ephemeral
            })
        );
        assert_eq!(
            parse(&["--mode", "private"]),
            Ok(LaunchCommand::Interactive {
                mode: OperatingMode::Private
            })
        );
        let options = match parse(&["--voice", "--mode", "ephemeral"]).unwrap() {
            LaunchCommand::Voice(options) => options,
            _ => unreachable!(),
        };
        assert_eq!(options.mode, Some(OperatingMode::Ephemeral));
    }
}
