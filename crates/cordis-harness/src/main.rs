//! `cordis-run`: runs JavaScript test files on Verve, as `node --test` and vitest run them.
//!
//! Verve has neither `node:test` nor vitest, and the suites this repository cares about are
//! written against them: compose-runtime's (`node:test`) and Cordis's own upstream specs
//! (vitest, transpiled to JavaScript under `conformance/`). This binary puts a small stand-in
//! for both on the runtime (`js/testkit.js`), imports each file, runs the tests it declared and
//! drives the event loop — timers, the network, workers — until they are done.
//!
//! ```text
//! cordis-run [--native] [--timeout <secs>] [--filter <text>] <file.mjs>...
//! ```
//!
//! `--native` defines the `cordis` module as the native one (`cordis-verve`); without it the
//! file resolves `cordis` however it would anyway (compose-runtime maps it to its vendored
//! bundle). The exit code is the number of failed tests, capped at 100, or 101 when a file
//! could not run at all.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use boa_engine::{JsValue, js_string};
use verve::boa_engine::builtins::promise::PromiseState;
use verve::{Level, Permissions, Runtime};

const STACK: usize = 256 * 1024 * 1024;
const TESTKIT: &str = include_str!("../js/testkit.js");

struct Options {
    native: bool,
    timeout: Duration,
    filter: Option<String>,
    files: Vec<PathBuf>,
}

fn parse() -> Result<Options, String> {
    let mut options = Options {
        native: false,
        timeout: Duration::from_secs(300),
        filter: None,
        files: Vec::new(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--native" => options.native = true,
            "--timeout" => {
                let secs = args.next().ok_or("--timeout needs seconds")?;
                options.timeout =
                    Duration::from_secs(secs.parse().map_err(|_| "--timeout needs seconds")?);
            }
            "--filter" => options.filter = Some(args.next().ok_or("--filter needs text")?),
            _ if arg.starts_with("--") => return Err(format!("unknown flag {arg}")),
            _ => options.files.push(PathBuf::from(arg)),
        }
    }
    if options.files.is_empty() {
        return Err("no test files given".into());
    }
    Ok(options)
}

fn main() -> ExitCode {
    let options = match parse() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("cordis-run: {message}");
            return ExitCode::from(2);
        }
    };
    let thread = std::thread::Builder::new()
        .name("cordis-run".into())
        .stack_size(STACK)
        .spawn(move || {
            verve::set_thread_stack(STACK);
            let mut failed = 0usize;
            for file in &options.files {
                failed += run_file(&options, file);
            }
            failed
        })
        .expect("a thread to run on");
    let failed = thread.join().unwrap_or(101);
    ExitCode::from(failed.min(101) as u8)
}

/// What wakes the loop: the network or a worker, from their own threads.
#[derive(Default)]
struct Signal {
    raised: Mutex<bool>,
    ready: Condvar,
}

impl Signal {
    fn raise(&self) {
        *self.raised.lock().unwrap() = true;
        self.ready.notify_one();
    }

    fn wait(&self, timeout: Duration) {
        let raised = self.raised.lock().unwrap();
        let (mut raised, _) = self
            .ready
            .wait_timeout_while(raised, timeout, |raised| !*raised)
            .unwrap();
        *raised = false;
    }
}

fn flush_console(runtime: &mut Runtime) {
    for line in runtime.take_console() {
        match line.level {
            Level::Error | Level::Warn => eprintln!("{}", line.text),
            _ => println!("{}", line.text),
        }
    }
}

/// Drives the loop until `done` says so or `deadline` passes; answers whether it finished.
fn drive(
    runtime: &mut Runtime,
    signal: &Signal,
    deadline: Instant,
    mut done: impl FnMut(&mut Runtime) -> bool,
) -> bool {
    loop {
        runtime.run_microtasks();
        flush_console(runtime);
        if done(runtime) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        runtime.run_due();
        runtime.run_workers();
        flush_console(runtime);
        if done(runtime) {
            return true;
        }
        let wait = runtime
            .next_due()
            .map(|due| ((due - runtime.now()) / 1000.).clamp(0., 0.05))
            .unwrap_or(0.05);
        if wait > 0. {
            signal.wait(Duration::from_secs_f64(wait));
        }
    }
}

fn settled(promise: &verve::boa_engine::object::builtins::JsPromise) -> bool {
    !matches!(promise.state(), PromiseState::Pending)
}

fn run_file(options: &Options, file: &std::path::Path) -> usize {
    let file = match std::fs::canonicalize(file) {
        Ok(file) => file,
        Err(error) => {
            eprintln!("cordis-run: {}: {error}", file.display());
            return 101;
        }
    };
    let display = file.display().to_string();
    let display = display.strip_prefix(r"\\?\").unwrap_or(&display).to_owned();
    println!("# {display}");
    let signal = Arc::new(Signal::default());
    let mut runtime = Runtime::new();
    runtime.set_permissions(Permissions::all());
    runtime.set_worker_permissions(Permissions::all());
    {
        let signal = signal.clone();
        runtime.set_waker(Some(Arc::new(move || signal.raise())));
    }
    let kit = match runtime.eval(TESTKIT) {
        Ok(kit) => kit,
        Err(error) => {
            eprintln!("cordis-run: the testkit failed: {error}");
            return 101;
        }
    };
    let kit = kit.as_object().expect("the testkit is an object").clone();
    let context = runtime.context();
    let node_test = kit.get(js_string!("nodeTest"), context).unwrap_or_default();
    let vitest = kit.get(js_string!("vitest"), context).unwrap_or_default();
    if let Some(filter) = &options.filter {
        let _ = kit.set(
            js_string!("filter"),
            JsValue::from(verve::boa_engine::JsString::from(filter.as_str())),
            false,
            context,
        );
    }
    runtime.define_builtin("test", node_test);
    runtime.define_builtin("vitest", vitest);
    if options.native {
        if let Err(error) = cordis_verve::install(&mut runtime) {
            eprintln!("cordis-run: the native cordis failed to install: {error}");
            return 101;
        }
    }
    let deadline = Instant::now() + options.timeout;
    let url = url_of(&file);
    let imported = match runtime.import_module(&url, None) {
        Ok(promise) => promise,
        Err(error) => {
            eprintln!("cordis-run: {display}: {error}");
            return 101;
        }
    };
    if !drive(&mut runtime, &signal, deadline, |_| settled(&imported)) {
        eprintln!("cordis-run: {display}: timed out loading");
        return 101;
    }
    if let PromiseState::Rejected(reason) = imported.state() {
        let error = verve::boa_engine::JsError::from_opaque(reason);
        let context = runtime.context();
        let message = verve::thrown_at(context, &error);
        eprintln!("cordis-run: {display} failed to load: {message}");
        return 101;
    }
    let run = kit
        .get(js_string!("run"), runtime.context())
        .unwrap_or_default();
    let result = match runtime.call(&run, &JsValue::from(kit.clone()), &[]) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("cordis-run: {display}: {error}");
            return 101;
        }
    };
    let promise = verve::boa_engine::object::builtins::JsPromise::from_object(
        result.as_object().expect("run answers a promise").clone(),
    )
    .expect("run answers a promise");
    if !drive(&mut runtime, &signal, deadline, |_| settled(&promise)) {
        eprintln!("cordis-run: {display}: timed out running");
        return 101;
    }
    match promise.state() {
        PromiseState::Fulfilled(value) => {
            let context = runtime.context();
            let failed = value
                .as_object()
                .and_then(|o| o.get(js_string!("failed"), context).ok())
                .and_then(|v| v.as_number())
                .unwrap_or(1.);
            failed as usize
        }
        PromiseState::Rejected(reason) => {
            let error = verve::boa_engine::JsError::from_opaque(reason);
            eprintln!(
                "cordis-run: {display}: the runner failed: {}",
                verve::describe(runtime.context(), &error)
            );
            101
        }
        PromiseState::Pending => 101,
    }
}

fn url_of(path: &std::path::Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = text.strip_prefix("//?/").unwrap_or(&text).to_owned();
    let mut url = String::from("file:///");
    for ch in text.trim_start_matches('/').chars() {
        match ch {
            ' ' => url.push_str("%20"),
            '%' => url.push_str("%25"),
            '#' => url.push_str("%23"),
            _ => url.push(ch),
        }
    }
    url
}
