use std::fs;
use std::io;
use std::path::Path;

use skillflag::{Options, handle_skillflag};

const SKILL_ID: &str = "yarp";
const SKILL_MD: &str = include_str!("../skills/yarp/SKILL.md");

/// Handle a top-level `yarp --skill ...` invocation.
///
/// Returns `None` when the invocation belongs to the regular YARP CLI.
#[must_use]
pub fn maybe_run(arguments: &[String]) -> Option<i32> {
    if arguments.get(1).map(String::as_str) != Some("--skill") {
        return None;
    }
    Some(match materialize_and_run(arguments) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("yarp: could not prepare bundled skill: {error}");
            1
        }
    })
}

fn materialize_and_run(arguments: &[String]) -> io::Result<i32> {
    let temporary = tempfile::Builder::new().prefix("yarp-skill-").tempdir()?;
    let bundle_root = temporary.path().join("skills");
    let yarp_dir = bundle_root.join(SKILL_ID);
    fs::create_dir_all(&yarp_dir)?;
    let skill_path = yarp_dir.join("SKILL.md");
    fs::write(&skill_path, SKILL_MD)?;
    set_export_permissions(&bundle_root, &yarp_dir, &skill_path)?;

    let options = Options {
        skills_roots: vec![bundle_root],
        include_bundled_skill: false,
        ..Options::default()
    };
    Ok(handle_skillflag(arguments, &options))
}

#[cfg(unix)]
fn set_export_permissions(
    bundle_root: &Path,
    yarp_dir: &Path,
    skill_path: &Path,
) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(bundle_root, fs::Permissions::from_mode(0o755))?;
    fs::set_permissions(yarp_dir, fs::Permissions::from_mode(0o755))?;
    fs::set_permissions(skill_path, fs::Permissions::from_mode(0o644))
}

#[cfg(not(unix))]
fn set_export_permissions(
    _bundle_root: &Path,
    _yarp_dir: &Path,
    _skill_path: &Path,
) -> io::Result<()> {
    Ok(())
}
