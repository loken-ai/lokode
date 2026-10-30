//! The documentation names what the program offers: every flag of `--help` and every tool the
//! model can call appears in the README or in a guide.

use std::fs;
use std::path::Path;
use std::process::Command;

/// The README and every guide, as one text.
fn documentation() -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut text = fs::read_to_string(root.join("README.md")).expect("the README");
    let mut guides: Vec<_> = fs::read_dir(root.join("docs/guides"))
        .expect("the guides")
        .map(|e| e.expect("a guide").path())
        .collect();
    guides.sort();
    for guide in guides {
        text.push_str(&fs::read_to_string(guide).expect("a guide"));
    }
    text
}

#[test]
fn every_flag_is_documented() {
    let out = Command::new(env!("CARGO_BIN_EXE_lokode"))
        .arg("--help")
        .output()
        .expect("lokode --help");
    let help = String::from_utf8(out.stdout).expect("utf-8 help");
    let docs = documentation();
    let mut missing = Vec::new();
    for word in help.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')) {
        let flag = word.trim_end_matches('-');
        if flag.starts_with("--")
            && flag.len() > 2
            && !["--help", "--version"].contains(&flag)
            && !docs.contains(flag)
        {
            missing.push(flag.to_string());
        }
    }
    missing.sort();
    missing.dedup();
    assert!(
        missing.is_empty(),
        "flags the documentation does not name: {missing:?}"
    );
}

#[test]
fn every_tool_is_documented() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let docs = documentation();
    let mut missing = Vec::new();
    for entry in fs::read_dir(root.join("src/tools")).expect("the tools") {
        let source = fs::read_to_string(entry.expect("a file").path()).expect("a source");
        let lines: Vec<&str> = source.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if !line.trim_start().starts_with("fn name(&self) -> &str") {
                continue;
            }
            let name = lines
                .get(i + 1)
                .map(|l| l.trim().trim_matches('"'))
                .unwrap_or("");
            let literal = lines[i + 1].trim().starts_with('"');
            if literal && !docs.contains(&format!("`{name}`")) {
                missing.push(name.to_string());
            }
        }
    }
    missing.sort();
    assert!(
        missing.is_empty(),
        "tools the documentation does not name: {missing:?}"
    );
}
