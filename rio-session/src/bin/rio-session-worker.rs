fn main() {
    if let Err(error) = rio_session::worker::run() {
        eprintln!("rio-session-worker: {error}");
        std::process::exit(1);
    }
}
