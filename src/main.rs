#[tokio::main]
async fn main() {
    independence::speak::init_logging();

    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let result = match arguments.as_slice() {
        [] => independence::app::run().await,
        [mode] if mode.to_str() == Some("--dictate") => independence::app::run_dictation().await,
        [mode] if mode.to_str() == Some("--voice") => independence::app::run_voice().await,
        [mode] if mode.to_str() == Some("--audio-devices") => {
            independence::app::list_audio_outputs()
        }
        [mode] if mode.to_str() == Some("--speak-test") => {
            independence::app::run_speech_test(None).await
        }
        [mode, flag, device]
            if mode.to_str() == Some("--voice") && flag.to_str() == Some("--audio-output") =>
        {
            let Some(device) = device.to_str() else {
                usage_and_exit();
            };
            independence::app::run_voice_with_output(Some(device.to_owned())).await
        }
        [mode, flag, device]
            if mode.to_str() == Some("--speak-test") && flag.to_str() == Some("--audio-output") =>
        {
            let Some(device) = device.to_str() else {
                usage_and_exit();
            };
            independence::app::run_speech_test(Some(device.to_owned())).await
        }
        _ => usage_and_exit(),
    };

    if let Err(error) = result {
        independence::speak::application_error(error);
        std::process::exit(1);
    }
}

fn usage_and_exit() -> ! {
    independence::speak::usage(
        "cargo run -- [--dictate|--voice [--audio-output <exact device id>]|--audio-devices|--speak-test [--audio-output <exact device id>]",
    );
    std::process::exit(2);
}
