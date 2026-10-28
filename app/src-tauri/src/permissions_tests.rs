//! Every command the window can call needs a permission in `build.rs` and an
//! `allow-` entry in the main capability, or Tauri answers "not allowed" and the
//! screen that calls it stays blank. A stubbed UI preview can't catch that.

const LIB: &str = include_str!("lib.rs");
const BUILD: &str = include_str!("../build.rs");
const CAPABILITY: &str = include_str!("../capabilities/main.json");

/// The command names in `tauri::generate_handler![ ... ]`.
fn handlers() -> Vec<String> {
    let start = LIB.find("generate_handler![").expect("handler list") + "generate_handler![".len();
    let end = start + LIB[start..].find("])").expect("end of handler list");
    LIB[start..end]
        .split(',')
        .map(|s| {
            s.trim()
                .rsplit("::")
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

#[test]
fn every_registered_command_has_a_permission_and_a_capability_entry() {
    let names = handlers();
    assert!(names.len() > 50, "found only {} commands", names.len());
    let mut missing = Vec::new();
    for name in &names {
        let in_build = BUILD.contains(&format!("\"{name}\","));
        let allow = format!("\"allow-{}\"", name.replace('_', "-"));
        if !in_build {
            missing.push(format!("{name}: not in build.rs"));
        }
        if !CAPABILITY.contains(&allow) {
            missing.push(format!("{name}: no {allow} in capabilities/main.json"));
        }
    }
    assert!(missing.is_empty(), "{missing:#?}");
}

#[test]
fn the_check_sees_the_commands_it_should() {
    let names = handlers();
    for must in [
        "train_moss_defaults",
        "moss_add_lora",
        "train_quote",
        "moss_generate",
    ] {
        assert!(names.iter().any(|n| n == must), "{must}");
    }
}
