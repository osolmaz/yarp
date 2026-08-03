use crate::config;

pub const HELP: &str = "Manage YARP configuration.\n\nUsage:\n  yarp config path\n  yarp config init\n  yarp config show [--json]\n  yarp config get <field>\n  yarp config set <field> [value ...]\n  yarp config unset <field>\n  yarp config check\n";

pub fn run(arguments: &[String]) -> i32 {
    match arguments {
        [] => {
            print!("{HELP}");
            0
        }
        [argument] if argument == "--help" || argument == "-h" => {
            print!("{HELP}");
            0
        }
        [command] if command == "path" => match config::path() {
            Ok(path) => {
                println!("{}", path.display());
                0
            }
            Err(error) => config_error(&error),
        },
        [command] if command == "init" => match config::init() {
            Ok(()) => {
                match config::path() {
                    Ok(path) => println!("created {}", path.display()),
                    Err(error) => return config_error(&error),
                }
                0
            }
            Err(error) => config_error(&error),
        },
        [command] if command == "show" => show(false),
        [command, option] if command == "show" && option == "--json" => show(true),
        [command, key] if command == "get" => {
            match config::load().and_then(|resolved| config::get(&resolved, key)) {
                Ok(value) => {
                    println!("{value}");
                    0
                }
                Err(error) => config_error(&error),
            }
        }
        [command, key, values @ ..] if command == "set" => {
            match config::set(key, values).and_then(|resolved| config::get(&resolved, key)) {
                Ok(value) => {
                    println!("{key} = {value}");
                    0
                }
                Err(error) => config_error(&error),
            }
        }
        [command, key] if command == "unset" => {
            match config::unset(key).and_then(|resolved| config::get(&resolved, key)) {
                Ok(value) => {
                    println!("{key} = {value}");
                    0
                }
                Err(error) => config_error(&error),
            }
        }
        [command] if command == "check" => match config::load() {
            Ok(_) => {
                println!("configuration: ok");
                0
            }
            Err(error) => config_error(&error),
        },
        _ => config_error("invalid config arguments"),
    }
}

fn show(json: bool) -> i32 {
    let rendered = config::load().and_then(|resolved| {
        if json {
            config::show_json(&resolved)
        } else {
            config::show_toml(&resolved)
        }
    });
    match rendered {
        Ok(text) => {
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
            0
        }
        Err(error) => config_error(&error),
    }
}

fn config_error(error: &str) -> i32 {
    eprintln!("yarp: {error}\n\n{HELP}");
    64
}
