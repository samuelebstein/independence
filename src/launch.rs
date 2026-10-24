//! Small, explicit command-line selection for Independence runtime modes.
//!
//! This is intentionally not a generic configuration framework.  It exists
//! because a directly attached keyboard must choose a different input adapter
//! from the laptop terminal, and that choice must be validated before either
//! adapter starts owning operating-system input.

use std::{ffi::OsString, path::PathBuf};

/// The one hardware profile currently understood by the application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HardwareProfile {
    RaspberryPi,
}

impl HardwareProfile {
    pub fn label(self) -> &'static str {
        match self {
            Self::RaspberryPi => "raspberry-pi",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "raspberry-pi" => Ok(Self::RaspberryPi),
            _ => Err(format!(
                "unknown profile `{value}`; the only supported profile is `raspberry-pi`"
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
}

/// The top-level action selected from argv.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchCommand {
    Interactive,
    Dictate,
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
        [] => Ok(LaunchCommand::Interactive),
        ["--dictate"] => Ok(LaunchCommand::Dictate),
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
            _ => return Err(format!("unknown --voice option `{flag}`")),
        }
        index += 2;
    }
    Ok(options)
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
        HardwareProfile, LaunchCommand, VoiceInputMode, VoiceLaunchOptions, parse_arguments,
    };

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
    fn rejects_unknown_or_duplicate_options() {
        assert!(parse(&["--voice", "--input", "touch"]).is_err());
        assert!(parse(&["--voice", "--input", "terminal", "--input", "buttons"]).is_err());
        assert!(parse(&["--input-devices", "--voice"]).is_err());
    }
}
