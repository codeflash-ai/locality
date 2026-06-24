fn main() {
    let args = std::env::args().skip(1).collect();
    std::process::exit(loc_cli::run(args));
}
