use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use nobubbles::effects::Emitter;
use serde_json::Value;

use crate::build::{Event, Outcome};

/// Runs every test binary in turn, exactly like cargo's own test runner: sequential, one binary
/// at a time, stopping at the first failure unless `harness_args` carries `--no-fail-fast`.
/// Returns whether every test in every binary passed.
pub fn run(tx: &Emitter<Event>, binaries: &[PathBuf], harness_args: &[String]) -> bool {
    let no_fail_fast = harness_args.iter().any(|a| a == "--no-fail-fast");
    let mut all_ok = true;

    for path in binaries {
        let ok = run_one(tx, path, harness_args);
        all_ok &= ok;
        if !ok && !no_fail_fast {
            break;
        }
    }
    all_ok
}

/// `--format json` is libtest's own unstable flag (distinct from cargo's `--message-format`),
/// gated the same way cargo's own `-Z` flags are: `RUSTC_BOOTSTRAP=1` opts a stable toolchain
/// in. `--report-time` gets per-test `exec_time` in that JSON, matching the per-crate timers
/// already shown for compilation.
fn run_one(tx: &Emitter<Event>, path: &Path, harness_args: &[String]) -> bool {
    let mut child = Command::new(path)
        .args(["--format", "json", "-Z", "unstable-options", "--report-time"])
        .args(harness_args)
        .env("RUSTC_BOOTSTRAP", "1")
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn test binary");

    let reader = BufReader::new(child.stdout.take().unwrap());
    let mut ok = true;

    for line in reader.lines().map_while(Result::ok) {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match (msg["type"].as_str(), msg["event"].as_str()) {
            (Some("suite"), Some("started")) => {
                if let Some(total) = msg["test_count"].as_u64() {
                    tx.send(Event::SuiteStarted {
                        total: total as usize,
                    });
                }
            }
            (Some("test"), Some("started")) => {
                if let Some(name) = msg["name"].as_str() {
                    tx.send(Event::TestStarted(name.to_string()));
                }
            }
            (Some("test"), Some(event @ ("ok" | "failed" | "ignored"))) => {
                let name = msg["name"].as_str().unwrap_or_default().to_string();
                let secs = msg["exec_time"].as_f64().unwrap_or(0.0) as f32;
                let outcome = match event {
                    "ok" => Outcome::Passed,
                    "ignored" => Outcome::Ignored,
                    _ => {
                        ok = false;
                        let stdout = msg["stdout"].as_str().unwrap_or_default().to_string();
                        let location = panic_location(&stdout);
                        Outcome::Failed { location, stdout }
                    }
                };
                tx.send(Event::TestFinished { name, secs, outcome });
            }
            _ => {}
        }
    }

    let _ = child.wait();
    ok
}

/// Pulls `file:line` from the panic hook's `thread '...' panicked at file:line:col:` line.
/// libtest's JSON has no separate location field, only this text carries it.
fn panic_location(stdout: &str) -> Option<String> {
    let after = stdout.split_once("panicked at ")?.1;
    let line_end = after.find('\n').unwrap_or(after.len());
    let mut parts = after[..line_end].trim_end_matches(':').rsplitn(3, ':');
    let _col = parts.next()?;
    let line = parts.next()?;
    let file = parts.next()?;
    Some(format!("{file}:{line}"))
}
