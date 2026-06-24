fn main() {
    let config = localityd::DaemonConfig::default();
    let daemon = localityd::Daemon::new(config);

    if let Err(error) = daemon.run_foreground() {
        eprintln!("localityd failed: {error}");
        std::process::exit(1);
    }
}
