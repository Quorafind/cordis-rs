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

// ─── The acceptance milestone ────────────────────────────────────────────────────────────

/// The integration `docs/integration.md` proposes, applied to compose-runtime's
/// `src/resolve.mjs`: `cordis` and `@deepseek-ai/cordis` resolve to the host's builtin when
/// the host defines one (Verve with `cordis_verve::install`), and to the vendored bundle
/// otherwise (Node, in CI).
pub const RESOLVE_PATCH: [(&str, &str); 2] = [
    (
        "import { registerHooks } from 'node:module';",
        "import { isBuiltin, registerHooks } from 'node:module';",
    ),
    (
        "    resolve(specifier, context, next) {\n",
        "    resolve(specifier, context, next) {\n      // The native Cordis, when the host defines it (cordis-rs on Verve).\n      if ((specifier === 'cordis' || specifier === '@deepseek-ai/cordis') && isBuiltin('cordis')) {\n        return { url: 'node:cordis', shortCircuit: true };\n      }\n",
    ),
];

/// compose-runtime's suites on the native module, through [`RESOLVE_PATCH`], in a copy of
/// `runtimes/node` (the checkout is only read) whose vendored bundle is replaced by a module
/// that throws — so nothing can quietly fall back to it. Each test must end as it ends on the
/// vendored bundle. Ignored by default: it needs a Rebon checkout, `CORDIS_REBON_RUNTIMES`
/// (default `../reboncode/runtimes/node` beside this repository).
#[test]
#[ignore]
fn compose_runtime_suites_on_the_native_module() {
    let source = std::env::var_os("CORDIS_REBON_RUNTIMES")
        .map(PathBuf::from)
        .unwrap_or_else(|| conformance().join("../../reboncode/runtimes/node"));
    assert!(
        source.join("compose-runtime").is_dir(),
        "no runtimes at {}",
        source.display()
    );
    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join("rebon-runtimes");
    let _ = std::fs::remove_dir_all(&scratch);
    copy_tree(&source, &scratch.join("runtimes/node"));
    let compose = scratch.join("runtimes/node/compose-runtime");
    let resolve = compose.join("src/resolve.mjs");
    let mut text = std::fs::read_to_string(&resolve)
        .unwrap()
        .replace("\r\n", "\n");
    for (from, to) in RESOLVE_PATCH {
        assert!(text.contains(from), "resolve.mjs has changed: no {from:?}");
        text = text.replacen(from, to, 1);
    }
    std::fs::write(&resolve, text).unwrap();
    std::fs::write(
        compose.join("payload/vendor/cordis/index.js"),
        "throw new Error('the vendored cordis was loaded');\n",
    )
    .unwrap();
    // The bundle's run gets a copy of its own too: nothing runs inside the checkout.
    let reference = Path::new(env!("CARGO_TARGET_TMPDIR")).join("rebon-runtimes-vendored");
    let _ = std::fs::remove_dir_all(&reference);
    copy_tree(&source, &reference.join("runtimes/node"));
    let pristine = reference.join("runtimes/node/compose-runtime");
    let mut differences = Vec::new();
    for suite in [
        "composition",
        "agent-loop",
        "dsh-tools",
        "llm-turn",
        "own-schedule-invoke",
    ] {
        let file = format!("test/{suite}.test.mjs");
        let native = outcomes(&run_in(&compose, true, &file));
        let vendored = outcomes(&run_in(&pristine, false, &file));
        assert!(!native.is_empty(), "{suite} ran natively");
        if native != vendored {
            differences.push(format!(
                "{suite}:\n  vendored {vendored:?}\n  native   {native:?}"
            ));
        }
    }
    assert!(differences.is_empty(), "{}", differences.join("\n"));
}

fn run_in(dir: &Path, native: bool, file: &str) -> Run {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cordis-run"));
    command.current_dir(dir);
    if native {
        command.arg("--native");
    }
    let output = command
        .arg("--timeout")
        .arg("240")
        .arg(file)
        .output()
        .unwrap();
    Run {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// `ok`/`not ok` per test name, in order.
fn outcomes(run: &Run) -> Vec<String> {
    let _ = (run.code, &run.stderr);
    run.stdout
        .lines()
        .chain(run.stderr.lines())
        .filter_map(|line| {
            let line = line
                .strip_prefix("not ok - ")
                .map(|n| format!("FAIL {n}"))
                .or_else(|| {
                    line.strip_prefix("ok - ").map(|n| {
                        let name = n.rsplit_once(" (").map_or(n, |(name, _)| name);
                        format!("PASS {name}")
                    })
                })?;
            Some(line)
        })
        .collect()
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            if entry.file_name() != "node_modules" {
                copy_tree(&entry.path(), &target);
            }
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}
