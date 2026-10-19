#[tokio::main]
async fn main() {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let result = if arguments.is_empty() {
        independence::app::run().await
    } else if arguments.len() == 1 && arguments[0].to_str() == Some("--dictate") {
        independence::app::run_dictation().await
    } else if arguments.len() == 1 && arguments[0].to_str() == Some("--voice") {
        independence::app::run_voice().await
    } else {
        eprintln!("usage: cargo run -- [--dictate|--voice]");
        std::process::exit(2);
    };

    if let Err(error) = result {
        eprintln!("independence: {error}");
        std::process::exit(1);
    }
}
