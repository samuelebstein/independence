#[tokio::main]
async fn main() {
    // Initialize providers before installing the tracing subscriber so every
    // later `tracing` span can receive a real OpenTelemetry context. Nothing
    // is exported unless the user configured an OTLP endpoint.
    let mut telemetry = independence::telemetry::initialize();
    independence::speak::init_logging();
    telemetry.report_startup();

    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let result = match independence::launch::parse_arguments(&arguments) {
        Ok(independence::launch::LaunchCommand::Interactive) => independence::app::run().await,
        Ok(independence::launch::LaunchCommand::Dictate) => {
            independence::app::run_dictation().await
        }
        Ok(independence::launch::LaunchCommand::Voice(options)) => {
            independence::app::run_voice_with_options(options).await
        }
        Ok(independence::launch::LaunchCommand::ListAudioOutputs) => {
            independence::app::list_audio_outputs()
        }
        Ok(independence::launch::LaunchCommand::ListAudioInputs) => {
            independence::app::list_audio_inputs()
        }
        Ok(independence::launch::LaunchCommand::ListInputDevices) => {
            independence::app::list_input_devices()
        }
        Ok(independence::launch::LaunchCommand::SpeakTest { audio_output }) => {
            independence::app::run_speech_test(audio_output).await
        }
        Err(message) => usage_and_exit(&message),
    };

    let exit_code = match result {
        Ok(()) => 0,
        Err(error) => {
            independence::speak::application_error(error);
            1
        }
    };
    telemetry.shutdown();
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

fn usage_and_exit(detail: &str) -> ! {
    let usage = format!(
        "{detail}. usage: cargo run -- [--dictate|--voice [--profile raspberry-pi] [--input terminal|buttons] [--input-device <stable /dev/input path>] [--audio-input <exact device id>] [--audio-output <exact device id>] [--model <model>] [--backend ollama|fixture] [--pipeline transcribe-and-speak|audio-to-audio]|--audio-devices|--audio-input-devices|--input-devices|--speak-test [--audio-output <exact device id>]"
    );
    independence::speak::usage(&usage);
    std::process::exit(2);
}
