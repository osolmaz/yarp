fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(yarp_cli::run_cli(&arguments));
}
