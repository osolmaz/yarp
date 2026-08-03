fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    if let Some(code) = yarp_cli::agent_skill::maybe_run(&arguments) {
        std::process::exit(code);
    }
    std::process::exit(yarp_cli::run_cli(&arguments[1..]));
}
