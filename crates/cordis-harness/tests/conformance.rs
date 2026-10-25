//! The conformance suite: Cordis's upstream specs on both implementations, and the
//! differential scenarios, whose traces must be the same on both.
//!
//! "Vendored" is the published cordis@4.0.0-rc.8 bundle under `conformance/node_modules`;
//! "native" is this repository's engine through `cordis-verve` (`cordis-run --native`).

use std::path::{Path, PathBuf};
use std::process::Command;

fn conformance() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../conformance")
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run(native: bool, file: &Path) -> Run {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cordis-run"));
    command.current_dir(conformance());
    if native {
        command.arg("--native");
    }
    command.arg("--timeout").arg("120").arg(file);
    let output = command.output().expect("cordis-run runs");
    Run {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn specs() -> Vec<PathBuf> {
    let mut specs: Vec<PathBuf> = std::fs::read_dir(conformance().join("upstream"))
        .expect("the ported specs")
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with(".spec.mjs"))
        .collect();
    specs.sort();
    specs
}

fn check_specs(native: bool) {
    let mut failed = Vec::new();
    for spec in specs() {
        let result = run(native, &spec);
        if result.code != 0 {
            failed.push(format!(
                "{}:\n{}{}",
                spec.display(),
                result.stdout,
                result.stderr
            ));
        }
    }
    assert!(failed.is_empty(), "failing specs:\n{}", failed.join("\n"));
}

#[test]
fn upstream_specs_pass_on_the_vendored_bundle() {
    check_specs(false);
}

#[test]
fn upstream_specs_pass_on_the_native_module() {
    check_specs(true);
}

fn traces(run: &Run) -> Vec<String> {
    run.stdout
        .lines()
        .filter(|line| line.starts_with("TRACE "))
        .map(str::to_owned)
        .collect()
}

#[test]
fn the_native_module_behaves_as_the_vendored_bundle() {
    let file = conformance().join("differential/scenarios.mjs");
    let vendored = run(false, &file);
    let native = run(true, &file);
    assert_eq!(
        vendored.code, 0,
        "the scenarios run on the bundle:\n{}{}",
        vendored.stdout, vendored.stderr
    );
    assert_eq!(
        native.code, 0,
        "the scenarios run natively:\n{}{}",
        native.stdout, native.stderr
    );
    let (vendored, native) = (traces(&vendored), traces(&native));
    assert!(!vendored.is_empty());
    let mut differences = Vec::new();
    for (a, b) in vendored.iter().zip(&native) {
        if a != b {
            differences.push(format!("vendored: {a}\nnative:   {b}"));
        }
    }
    assert_eq!(vendored.len(), native.len(), "the same scenarios ran");
    assert!(differences.is_empty(), "{}", differences.join("\n\n"));
}
