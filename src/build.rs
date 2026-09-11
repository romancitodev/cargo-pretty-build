use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use std::collections::HashMap;

use cargo_metadata::Message;
use cargo_metadata::diagnostic::{Diagnostic, DiagnosticLevel};
use nobubbles::effects::Emitter;

pub struct Warning {
    pub message: String,
    pub location: Option<String>,
    pub help: Option<String>,
    pub rendered: String,
}

/// One failed test, collected for the post-run browsable list.
pub struct FailedTest {
    pub name: String,
    pub secs: f32,
    pub location: Option<String>,
    pub stdout: String,
}

/// Outcome of one finished test, carrying just enough to render inline and, on failure, to
/// inspect on demand.
pub enum Outcome {
    Passed,
    Ignored,
    Failed {
        /// `file:line` pulled out of the panic message, when there is one to point at.
        location: Option<String>,
        /// Captured stdout for the test, panic message included.
        stdout: String,
    },
}

pub enum Event {
    /// Sent once `cargo metadata` resolves, so the UI can swap its startup placeholders for
    /// the real project name and (a first estimate of) the unit total.
    Ready {
        project: String,
        target_dir: std::path::PathBuf,
        total: usize,
        versions: HashMap<String, String>,
        /// Package id to crate name, kept around so a later retry can rebuild without paying
        /// for another `cargo metadata` call just to resolve artifact names again.
        names: HashMap<String, String>,
    },
    /// A more exact unit count than `Ready`'s, from cargo's own unit graph. Arrives later
    /// because it costs its own `cargo` invocation, run only after the real build is under way.
    Total(usize),
    Started(String),
    /// The build script binary has been compiled and cargo is about to run it.
    ScriptRunning(String),
    /// Build scripts finish before the package's real lib/bin, so they get their own
    /// completion event instead of an `Artifact` for the "building" indicator to clear on.
    ScriptExecuted(String),
    Artifact {
        id: String,
        name: String,
        fresh: bool,
        real: bool,
    },
    Warning(Warning),
    Error(String),
    /// Sent once compilation finishes, only when the verb needs the produced binaries
    /// afterward: `run` execs the one it finds here; `test` runs every one of them itself
    /// before this ever gets sent, so it never needs this event at all.
    Executables(Vec<PathBuf>),
    /// One test binary is about to run; carries how many tests it holds, added to the total.
    SuiteStarted {
        total: usize,
    },
    TestStarted(String),
    TestFinished {
        name: String,
        secs: f32,
        outcome: Outcome,
    },
    /// A single failed test's `r`-triggered rerun came back.
    RetryFinished {
        name: String,
        secs: f32,
        outcome: Outcome,
    },
    Done(bool),
}

// cargo's own colored status lines have no escape-code parser in cargo_metadata's Message
// stream, so strip them by hand to pattern-match "Compiling <name>".
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_escape = false;
    for c in s.chars() {
        match c {
            '\u{1b}' => in_escape = true,
            c if in_escape => in_escape = !c.is_ascii_alphabetic(),
            c => out.push(c),
        }
    }
    out
}

fn to_warning(d: &Diagnostic) -> Warning {
    let location = d
        .spans
        .iter()
        .find(|s| s.is_primary)
        .map(|s| format!("{}:{}", s.file_name, s.line_start));
    let help = d
        .children
        .iter()
        .find(|c| c.level == DiagnosticLevel::Help)
        .map(|c| c.message.clone());

    Warning {
        message: d.message.clone(),
        location,
        help,
        rendered: d.rendered.clone().unwrap_or_default(),
    }
}

/// Compiles with `cargo <cargo_args> --message-format=json <extra_args>` and, when `tx` is
/// `Some`, streams progress as [`Event`]s. `None` is the quiet path a retry rebuilds with: no
/// progress events, since that would reanimate the build/progress block above an accordion the
/// user is mid-browse in. Either way, returns whether it succeeded, every runnable artifact it
/// produced (bins, examples, test binaries) in build order, and the compiler's error text (empty
/// on success).
pub fn build(
    tx: Option<&Emitter<Event>>,
    names: &HashMap<String, String>,
    cargo_args: &[&str],
    extra_args: &[String],
) -> (bool, Vec<PathBuf>, String) {
    let mut child = Command::new("cargo")
        .args(cargo_args)
        .args(["--message-format=json", "--color=always"])
        .args(extra_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn cargo");

    // cargo writes "Compiling"/"Finished" straight to stderr regardless of --message-format;
    // it's the only place we learn a crate *started*, so a second thread parses it for that.
    let stderr = child.stderr.take().unwrap();
    let stderr_tx = tx.cloned();
    let stderr_thread = std::thread::spawn(move || {
        let mut full = String::new();
        for line in BufReader::new(stderr)
            .lines()
            .map_while(std::io::Result::ok)
        {
            let clean = strip_ansi(&line);
            if let Some(rest) = clean.trim_start().strip_prefix("Compiling ")
                && let Some(name) = rest.split_whitespace().next()
                && let Some(stderr_tx) = &stderr_tx
            {
                stderr_tx.send(Event::Started(name.to_string()));
            }
            full.push_str(&line);
            full.push('\n');
        }
        full
    });

    let reader = BufReader::new(child.stdout.take().unwrap());
    let mut ok = true;
    let mut executables = Vec::new();
    let mut errors = String::new();

    for message in Message::parse_stream(reader).flatten() {
        match message {
            Message::CompilerArtifact(artifact) => {
                let fresh = artifact.fresh;
                if let Some(path) = artifact.executable {
                    executables.push(path.into_std_path_buf());
                }
                if let Some(tx) = tx {
                    let is_build_script = artifact
                        .target
                        .is_kind(cargo_metadata::TargetKind::CustomBuild);
                    let real = !is_build_script;
                    let id = artifact.package_id.repr.clone();
                    let name = names
                        .get(id.as_str())
                        .cloned()
                        .unwrap_or(artifact.target.name);
                    if is_build_script && !fresh {
                        tx.send(Event::ScriptRunning(name.clone()));
                    }
                    tx.send(Event::Artifact {
                        id,
                        name,
                        fresh,
                        real,
                    });
                }
            }
            Message::BuildScriptExecuted(script) => {
                if let Some(tx) = tx {
                    let name = names
                        .get(script.package_id.repr.as_str())
                        .cloned()
                        .unwrap_or(script.package_id.repr);
                    tx.send(Event::ScriptExecuted(name));
                }
            }
            Message::CompilerMessage(msg) => match msg.message.level {
                DiagnosticLevel::Error => {
                    if let Some(rendered) = msg.message.rendered {
                        if let Some(tx) = tx {
                            tx.send(Event::Error(rendered.clone()));
                        }
                        errors.push_str(&rendered);
                    }
                }
                DiagnosticLevel::Warning => {
                    if let Some(tx) = tx {
                        tx.send(Event::Warning(to_warning(&msg.message)));
                    }
                }
                _ => {}
            },
            Message::BuildFinished(finished) => ok = finished.success,
            _ => {}
        }
    }

    let _ = child.wait();
    let stderr_text = stderr_thread.join().unwrap_or_default();
    if !ok && !stderr_text.trim().is_empty() {
        if let Some(tx) = tx {
            tx.send(Event::Error(stderr_text.clone()));
        }
        errors.push_str(&stderr_text);
    }
    (ok, executables, errors)
}
