#[tokio::main]
async fn main() {
    if let Err(error) = independence::app::run().await {
        eprintln!("independence: {error}");
        std::process::exit(1);
    }
}
