use std::io::{self, Read};

const MAX_FUZZ_INPUT_BYTES: u64 = 300 * 1024;

fn main() -> Result<(), String> {
    let mut bytes = Vec::new();
    io::stdin()
        .take(MAX_FUZZ_INPUT_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read fuzz input: {error}"))?;
    let source = String::from_utf8_lossy(&bytes).into_owned();
    let original = source.clone();
    let _ = yarp_cli::shell::inspect_syntax(&source);
    let _ = yarp_cli::rewrite::select_result_plan(&source);
    let _ = yarp_cli::rewrite::rewrite(&source);
    if source != original {
        return Err("shell classification changed its input".to_owned());
    }
    Ok(())
}
