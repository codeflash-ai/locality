use localityd::ipc::DaemonBuildInfo;

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args == ["--build-info"] {
        println!(
            "{}",
            serde_json::to_string(&DaemonBuildInfo::current())
                .expect("daemon build info serializes")
        );
        return;
    }
    if args == ["--version"] {
        let build = DaemonBuildInfo::current();
        println!("localityd {} {}", build.version, build.build_id);
        return;
    }
    if !args.is_empty() {
        eprintln!("usage: localityd [--build-info|--version]");
        std::process::exit(2);
    }

    let config = localityd::DaemonConfig::default();
    let daemon = localityd::Daemon::new(config);

    if let Err(error) = daemon.run_foreground() {
        eprintln!("localityd failed: {error}");
        std::process::exit(1);
    }
}
