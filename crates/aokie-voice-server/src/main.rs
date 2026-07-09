fn main() {
    if let Err(e) = aokie_voice_server::run_from_env() {
        eprintln!("[aokie-voice-server] {e}");
        std::process::exit(1);
    }
}
