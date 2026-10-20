//! Local Piper synthesis behind a narrow, in-memory boundary.
//!
//! Piper owns phonemization and ONNX inference. Independence only starts the
//! configured local command, gives it text through stdin, and keeps the WAV
//! bytes in memory for the playback capability. No shell is involved and no
//! generated audio is written to the repository or a recording directory.

use std::{
    env,
    ffi::OsString,
    io::{self, Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::Duration,
};

use thiserror::Error;

/// The local Piper model used unless the user chooses another explicit path.
pub const DEFAULT_HAL_MODEL: &str = "local-data/piper/hal/hal.onnx";
pub const DEFAULT_PIPER_COMMAND: &str = "piper";

/// Configuration for an installed local Piper CLI and an explicit voice pair.
///
/// `piper-tts` expects the ONNX model and its JSON configuration side by side.
/// The path is configurable so no model has to live in this repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PiperConfig {
    command: OsString,
    model_path: PathBuf,
    config_path: PathBuf,
}

impl PiperConfig {
    pub fn from_environment() -> Self {
        let model_path = env::var_os("INDEPENDENCE_TTS_MODEL")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_HAL_MODEL));
        let config_path = env::var_os("INDEPENDENCE_TTS_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("{}.json", model_path.display())));
        let command = env::var_os("INDEPENDENCE_PIPER_COMMAND")
            .unwrap_or_else(|| OsString::from(DEFAULT_PIPER_COMMAND));

        Self {
            command,
            model_path,
            config_path,
        }
    }

    #[cfg(test)]
    pub(crate) fn new(
        command: impl Into<OsString>,
        model_path: PathBuf,
        config_path: PathBuf,
    ) -> Self {
        Self {
            command: command.into(),
            model_path,
            config_path,
        }
    }

    pub fn command(&self) -> &std::ffi::OsStr {
        &self.command
    }

    pub fn model_path(&self) -> &std::path::Path {
        &self.model_path
    }

    pub fn config_path(&self) -> &std::path::Path {
        &self.config_path
    }
}

/// A generation-stamped request. The speech worker changes generation before
/// cancelling, so a late local process result cannot reach playback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SynthesisRequest {
    pub utterance_id: u64,
    pub generation: u64,
    pub text: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SynthesisResult {
    Audio(Vec<u8>),
    Cancelled,
}

/// Small test seam for real Piper and controllable fake synthesis. It exists
/// because automated tests must not require model weights or a local engine.
pub trait Synthesizer {
    fn synthesize(
        &mut self,
        request: &SynthesisRequest,
        current_generation: &AtomicU64,
    ) -> Result<SynthesisResult, SynthesisError>;
}

/// The established Piper CLI is deliberately launched without a shell. It
/// writes a WAV stream to stdout; the reader threads prevent a large answer
/// from filling an OS pipe while the child is still synthesizing.
pub struct PiperCli {
    config: PiperConfig,
}

impl PiperCli {
    pub fn new(config: PiperConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &PiperConfig {
        &self.config
    }
}

impl Synthesizer for PiperCli {
    fn synthesize(
        &mut self,
        request: &SynthesisRequest,
        current_generation: &AtomicU64,
    ) -> Result<SynthesisResult, SynthesisError> {
        if generation_changed(request.generation, current_generation) {
            return Ok(SynthesisResult::Cancelled);
        }
        if !self.config.model_path.is_file() {
            return Err(SynthesisError::MissingModel(self.config.model_path.clone()));
        }
        if !self.config.config_path.is_file() {
            return Err(SynthesisError::MissingConfig(
                self.config.config_path.clone(),
            ));
        }

        let mut child = Command::new(&self.config.command)
            .arg("--model")
            .arg(&self.config.model_path)
            .arg("--config")
            .arg(&self.config.config_path)
            .arg("--output-file")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| SynthesisError::Spawn {
                command: self.config.command.to_string_lossy().into_owned(),
                source,
            })?;

        let mut stdin = child.stdin.take().ok_or(SynthesisError::MissingStdin)?;
        stdin
            .write_all(request.text.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .map_err(SynthesisError::WriteInput)?;
        drop(stdin);

        let stdout = child.stdout.take().ok_or(SynthesisError::MissingStdout)?;
        let stderr = child.stderr.take().ok_or(SynthesisError::MissingStderr)?;
        let stdout_reader = read_all(stdout);
        let stderr_reader = read_all(stderr);

        let status = loop {
            if generation_changed(request.generation, current_generation) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Ok(SynthesisResult::Cancelled);
            }

            match child.try_wait().map_err(SynthesisError::Wait)? {
                Some(status) => break status,
                None => thread::sleep(Duration::from_millis(10)),
            }
        };

        let audio = join_reader(stdout_reader)?;
        // Piper diagnostics can include arbitrary engine details. They are not
        // logged because application logs intentionally avoid private text and
        // opaque subprocess output.
        let _ = join_reader(stderr_reader)?;

        if generation_changed(request.generation, current_generation) {
            return Ok(SynthesisResult::Cancelled);
        }
        if !status.success() {
            return Err(SynthesisError::CommandFailed {
                code: status.code(),
            });
        }
        if audio.is_empty() {
            return Err(SynthesisError::EmptyAudio);
        }

        Ok(SynthesisResult::Audio(audio))
    }
}

fn generation_changed(expected: u64, current: &AtomicU64) -> bool {
    current.load(Ordering::Acquire) != expected
}

fn read_all<R: Read + Send + 'static>(mut reader: R) -> thread::JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
}

fn join_reader(reader: thread::JoinHandle<io::Result<Vec<u8>>>) -> Result<Vec<u8>, SynthesisError> {
    reader
        .join()
        .map_err(|_| SynthesisError::ReaderPanicked)?
        .map_err(SynthesisError::ReadOutput)
}

#[derive(Debug, Error)]
pub enum SynthesisError {
    #[error("Piper model file is missing: {0}")]
    MissingModel(PathBuf),

    #[error("Piper model configuration file is missing: {0}")]
    MissingConfig(PathBuf),

    #[error("could not start local Piper command {command}: {source}")]
    Spawn {
        command: String,
        #[source]
        source: io::Error,
    },

    #[error("local Piper process did not provide stdin")]
    MissingStdin,

    #[error("local Piper process did not provide stdout")]
    MissingStdout,

    #[error("local Piper process did not provide stderr")]
    MissingStderr,

    #[error("could not send text to local Piper: {0}")]
    WriteInput(#[source] io::Error),

    #[error("could not wait for local Piper: {0}")]
    Wait(#[source] io::Error),

    #[error("could not read local Piper output: {0}")]
    ReadOutput(#[source] io::Error),

    #[error("local Piper output reader panicked")]
    ReaderPanicked,

    #[error("local Piper exited unsuccessfully{code}", code = exit_code(*code))]
    CommandFailed { code: Option<i32> },

    #[error("local Piper returned no audio")]
    EmptyAudio,
}

fn exit_code(code: Option<i32>) -> String {
    match code {
        Some(code) => format!(" with status {code}"),
        None => " after a signal".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{PiperConfig, SynthesisRequest, generation_changed};

    #[test]
    fn generation_check_rejects_an_obsolete_request() {
        let generation = AtomicU64::new(4);
        assert!(!generation_changed(4, &generation));
        generation.store(5, Ordering::Release);
        assert!(generation_changed(4, &generation));
    }

    #[test]
    fn explicit_config_keeps_command_and_paths_literal() {
        let config = PiperConfig::new(
            OsString::from("/opt/piper/bin/piper"),
            PathBuf::from("a model.onnx"),
            PathBuf::from("a model.onnx.json"),
        );
        assert_eq!(config.command(), "/opt/piper/bin/piper");
        assert_eq!(config.model_path(), PathBuf::from("a model.onnx"));
        assert_eq!(config.config_path(), PathBuf::from("a model.onnx.json"));
    }

    #[test]
    fn synthesis_request_retains_text_as_data() {
        let request = SynthesisRequest {
            utterance_id: 12,
            generation: 3,
            text: "$(not a shell command)".into(),
        };
        assert_eq!(request.text, "$(not a shell command)");
    }
}
