use std::io::{BufRead, BufReader};
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

pub enum Event {
    Started(String),
    /// Build scripts finish before the package's real lib/bin, so they get their own
    /// completion event instead of an `Artifact` for the "building" indicator to clear on.
    ScriptExecuted,
    Artifact {
        id: String,
        name: String,
        fresh: bool,
        real: bool,
    },
    Warning(Warning),
    Error(String),
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

pub fn build(tx: &Emitter<Event>, names: &HashMap<String, String>, extra_args: &[String]) {
    let mut child = Command::new("cargo")
        .args(["build", "--message-format=json", "--color=always"])
        .args(extra_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn cargo");

    // cargo writes "Compiling"/"Finished" straight to stderr regardless of --message-format;
    // it's the only place we learn a crate *started*, so a second thread parses it for that.
    let stderr = child.stderr.take().unwrap();
    let stderr_tx = tx.clone();
    let stderr_thread = std::thread::spawn(move || {
        let mut full = String::new();
        for line in BufReader::new(stderr)
            .lines()
            .map_while(std::io::Result::ok)
        {
            let clean = strip_ansi(&line);
            if let Some(rest) = clean.trim_start().strip_prefix("Compiling ")
                && let Some(name) = rest.split_whitespace().next()
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

    for message in Message::parse_stream(reader).flatten() {
        match message {
            Message::CompilerArtifact(artifact) => {
                let fresh = artifact.fresh;
                let real = !artifact
                    .target
                    .is_kind(cargo_metadata::TargetKind::CustomBuild);
                let id = artifact.package_id.repr.clone();
                let name = names
                    .get(id.as_str())
                    .cloned()
                    .unwrap_or(artifact.target.name);
                tx.send(Event::Artifact {
                    id,
                    name,
                    fresh,
                    real,
                });
            }
            Message::BuildScriptExecuted(_) => tx.send(Event::ScriptExecuted),
            Message::CompilerMessage(msg) => match msg.message.level {
                DiagnosticLevel::Error => {
                    if let Some(rendered) = msg.message.rendered {
                        tx.send(Event::Error(rendered));
                    }
                }
                DiagnosticLevel::Warning => tx.send(Event::Warning(to_warning(&msg.message))),
                _ => {}
            },
            Message::BuildFinished(finished) => ok = finished.success,
            _ => {}
        }
    }

    let _ = child.wait();
    let stderr_text = stderr_thread.join().unwrap_or_default();
    if !ok && !stderr_text.trim().is_empty() {
        tx.send(Event::Error(stderr_text));
    }
    tx.send(Event::Done(ok));
}
