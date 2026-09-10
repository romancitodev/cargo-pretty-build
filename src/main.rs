mod build;
mod commands;
mod metrics;
mod test;
mod ui;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use cargo_metadata::MetadataCommand;
use crossterm::event::KeyCode;
use crossterm::style::Stylize;
use eyre::Result;
use nobubbles::app::{Cancelled, Inline};
use nobubbles::effects;
use nobubbles::rimel;
use nobubbles::signals::{quit, signal};

use build::{Event, FailedTest, Outcome, Warning, build};
use commands::Verb;
use metrics::{build_closure_size, dir_size, exact_unit_count, host_triple};
use nobubbles::style::Color;
use ui::{fade, rust_ramp, summary_block, warning_panel};

const BAR_WIDTH: u16 = 28;
const NAME_WIDTH: usize = 32;
const BUILDING_ROWS: usize = 6;
const DONE_ROWS: usize = 6;
const FAILED_ROWS: usize = 6;
/// Writing this signal every frame is what keeps nobubbles' reactive loop ticking during the
/// post-build settle animation. A plain local (Instant, bool) wouldn't request the next frame.
const SETTLE_STEP: f32 = 0.125;

fn main() -> Result<()> {
    let mut extra_args: Vec<String> = std::env::args().skip(1).collect();
    // When cargo dispatches `cargo pretty ...`, it prepends the subcommand name to argv,
    // so it'd otherwise get forwarded into the wrapped cargo command as if the user had typed it.
    if let Some(subcommand) = env!("CARGO_PKG_NAME").strip_prefix("cargo-")
        && extra_args.first().map(String::as_str) == Some(subcommand)
    {
        extra_args.remove(0);
    }
    let (verb, extra_args) = match Verb::parse(extra_args) {
        Ok(parsed) => parsed,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(1);
        }
    };
    let flag_line = match &verb {
        Verb::Run { program_args } if !program_args.is_empty() => format!(
            "cargo run {} -- {}",
            extra_args.join(" "),
            program_args.join(" ")
        ),
        Verb::Test { harness_args } if !harness_args.is_empty() => format!(
            "cargo test {} -- {}",
            extra_args.join(" "),
            harness_args.join(" ")
        ),
        _ if extra_args.is_empty() => format!("cargo {}", verb.label()),
        _ => format!("cargo {} {}", verb.label(), extra_args.join(" ")),
    };

    // `cargo metadata` and the unit count both cost their own subprocess, so none of that runs
    // before the UI does. The loop below draws its first frame immediately, on placeholders,
    // while this background thread resolves the real project name, target dir and unit total
    // and only then hands off to the actual compile step.
    let inbox = effects::inbox::<Event>();
    let setup_args = extra_args.clone();
    let setup_verb = verb.clone();
    inbox.spawn(move |tx| {
        let mut metadata_cmd = MetadataCommand::new();
        if let Some(triple) = host_triple() {
            metadata_cmd.other_options(vec!["--filter-platform".to_string(), triple]);
        }
        let metadata = match metadata_cmd.exec() {
            Ok(m) => m,
            Err(e) => {
                tx.send(Event::Error(e.to_string()));
                tx.send(Event::Done(false));
                return;
            }
        };

        let project = metadata
            .root_package()
            .map(|p| p.name.to_string())
            .unwrap_or_else(|| "project".into());
        let names: HashMap<String, String> = metadata
            .packages
            .iter()
            .map(|p| (p.id.repr.clone(), p.name.to_string()))
            .collect();
        // Keyed by name rather than package id: that's what every event carries a crate's
        // identity as (cargo's own stderr/JSON output never mentions ids), so this is the
        // only key the render loop can actually look version up by.
        let versions: HashMap<String, String> = metadata
            .packages
            .iter()
            .map(|p| (p.name.to_string(), p.version.to_string()))
            .collect();
        let target_dir = metadata.target_directory.clone().into_std_path_buf();
        let total = build_closure_size(&metadata);

        // `cargo run` needs exactly one runnable target to know what to execute after the
        // build; replicate its own ambiguity check here since we build (and pick the binary
        // to run) ourselves instead of shelling out to `cargo run`.
        if matches!(setup_verb, Verb::Run { .. }) {
            let bin_names: Vec<&str> = metadata
                .root_package()
                .map(|p| {
                    p.targets
                        .iter()
                        .filter(|t| t.is_bin())
                        .map(|t| t.name.as_str())
                        .collect()
                })
                .unwrap_or_default();
            let target_given = setup_args.iter().any(|a| a == "--bin" || a == "--example");
            if bin_names.is_empty() && !target_given {
                tx.send(Event::Error(
                    "error: a bin target must be available for `cargo run`\n".into(),
                ));
                tx.send(Event::Done(false));
                return;
            }
            if bin_names.len() > 1 && !target_given {
                tx.send(Event::Error(format!(
                    "error: `cargo run` requires --bin or --example because multiple binaries are available: {}\n",
                    bin_names.join(", ")
                )));
                tx.send(Event::Done(false));
                return;
            }
        }

        tx.send(Event::Ready {
            project,
            target_dir,
            total,
            versions,
        });

        // A more exact count needs its own `cargo` invocation, so it runs alongside the real
        // build instead of delaying it; `total` above is close enough to draw a bar meanwhile.
        let unit_tx = tx.clone();
        let unit_args = setup_args.clone();
        let unit_cargo_args = setup_verb.compile_args();
        std::thread::spawn(move || {
            if let Some(total) = exact_unit_count(unit_cargo_args, &unit_args) {
                unit_tx.send(Event::Total(total));
            }
        });

        let (ok, executables) = build(&tx, &names, setup_verb.compile_args(), &setup_args);
        match setup_verb {
            Verb::Run { .. } => {
                if ok {
                    tx.send(Event::Executables(executables));
                }
                tx.send(Event::Done(ok));
            }
            Verb::Test { harness_args } => {
                let all_ok = ok && test::run(&tx, &executables, &harness_args);
                tx.send(Event::Done(all_ok));
            }
            Verb::Build => tx.send(Event::Done(ok)),
        }
    });

    let started = Instant::now();
    let settle_t = signal(0.0f32);
    let dismissing = signal(false);
    let mut project = String::new();
    let mut target_dir: Option<PathBuf> = None;
    let mut before_size = 0u64;
    let mut total = 0usize;
    let mut versions: HashMap<String, String> = HashMap::new();
    let mut compiled: HashSet<String> = HashSet::new();
    let mut seen = 0usize;
    // (crate or test name, started, running its build.rs right now? always false for tests)
    let mut building: Vec<(String, Instant, bool)> = Vec::new();
    let mut done: Vec<(String, f32)> = Vec::new();
    let mut failed: Vec<FailedTest> = Vec::new();
    let mut warnings: Vec<Warning> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut build_ok: Option<bool> = None;
    let mut grew_by = 0u64;
    let mut selected = 0usize;
    let mut inspecting = false;
    let mut failed_selected = 0usize;
    let mut failed_expanded: Vec<bool> = Vec::new();
    let mut run_target: Option<PathBuf> = None;
    // Frozen the instant `Done` fires, so the clock stops for real instead of ticking away
    // while the user browses failed tests afterward.
    let mut final_elapsed: Option<f32> = None;
    // Flips once the compile phase hands off to running the compiled tests, at which point
    // "Compiling"/"Compiled" become "Running"/"Tests" and the progress bar restarts at 0.
    let mut testing = false;

    let run = Inline::run(20, |cx| {
        inbox.drain(|event| match event {
            Event::Ready {
                project: p,
                target_dir: dir,
                total: t,
                versions: v,
            } => {
                project = p;
                before_size = dir_size(&dir);
                target_dir = Some(dir);
                total = t;
                versions = v;
            }
            Event::Total(t) => total = t,
            Event::Started(name) => {
                if !building.iter().any(|(n, _, _)| n == &name) {
                    building.push((name, Instant::now(), false));
                }
            }
            Event::ScriptRunning(name) => {
                if let Some(entry) = building.iter_mut().find(|(n, _, _)| n == &name) {
                    entry.2 = true;
                }
            }
            Event::ScriptExecuted(name) => {
                seen += 1;
                if let Some(entry) = building.iter_mut().find(|(n, _, _)| n == &name) {
                    entry.2 = false;
                }
            }
            Event::Artifact {
                id,
                name,
                fresh,
                real,
            } => {
                seen += 1;
                if !fresh {
                    compiled.insert(id);
                    if real {
                        let secs = match building.iter().position(|(n, _, _)| n == &name) {
                            Some(pos) => building.remove(pos).1.elapsed().as_secs_f32(),
                            None => 0.0,
                        };
                        done.push((name, secs));
                    }
                }
            }
            Event::Warning(w) => warnings.push(w),
            Event::Error(msg) => errors.push(msg),
            Event::Executables(paths) => run_target = paths.into_iter().next(),
            Event::SuiteStarted { total: t } => {
                if !testing {
                    testing = true;
                    seen = 0;
                    total = 0;
                    done.clear();
                    building.clear();
                }
                total += t;
            }
            Event::TestStarted(name) => {
                if !building.iter().any(|(n, _, _)| n == &name) {
                    building.push((name, Instant::now(), false));
                }
            }
            Event::TestFinished {
                name,
                secs,
                outcome,
            } => {
                seen += 1;
                if let Some(pos) = building.iter().position(|(n, _, _)| n == &name) {
                    building.remove(pos);
                }
                match outcome {
                    Outcome::Passed => done.push((name, secs)),
                    Outcome::Ignored => {}
                    Outcome::Failed { location, stdout } => {
                        failed.push(FailedTest {
                            name,
                            secs,
                            location,
                            stdout,
                        });
                        failed_expanded.push(false);
                    }
                }
            }
            Event::Done(ok) => {
                build_ok = Some(ok);
                final_elapsed = Some(started.elapsed().as_secs_f32());
                grew_by = target_dir
                    .as_deref()
                    .map(|dir| dir_size(dir).saturating_sub(before_size))
                    .unwrap_or(0);
            }
        });

        let is_test = matches!(verb, Verb::Test { .. });
        let elapsed = final_elapsed.unwrap_or_else(|| started.elapsed().as_secs_f32());

        if dismissing.get() {
            cx.render(summary_block(
                &project,
                build_ok.unwrap_or(false),
                elapsed,
                warnings.len(),
                compiled.len(),
                grew_by,
            ));
            quit();
            return;
        }

        let mut settled = false;
        if build_ok.is_some() {
            let t = settle_t.update(|t| {
                *t = (*t + SETTLE_STEP).min(1.0);
                *t
            });
            settled = t >= 1.0;

            // `build`/`run` still replace the whole view with a compact summary once settled.
            // `test` instead falls through to the normal table below and keeps it on screen:
            // the last few tests run are exactly what you want to still see once it's done.
            if settled && !is_test {
                if !warnings.is_empty() {
                    if let Some(k) = cx.key() {
                        match k.code {
                            KeyCode::Up => {
                                selected = selected.saturating_sub(1);
                                inspecting = false;
                            }
                            KeyCode::Down => {
                                selected = (selected + 1).min(warnings.len() - 1);
                                inspecting = false;
                            }
                            KeyCode::Enter => inspecting = !inspecting,
                            KeyCode::Esc => {
                                dismissing.set(true);
                                cx.render(rimel::text(""));
                                return;
                            }
                            _ => {}
                        }
                    }
                    cx.render(warning_panel(&warnings, selected, inspecting));
                    return;
                }

                cx.render(summary_block(
                    &project,
                    build_ok.unwrap_or(false),
                    elapsed,
                    warnings.len(),
                    compiled.len(),
                    grew_by,
                ));
                quit();
                return;
            }
        }

        // Accordion over every failed test, not a one-at-a-time viewer: all rows stay visible,
        // ↑↓ only move which row is highlighted, Enter expands/collapses that row in place.
        if is_test && settled && !failed.is_empty() && let Some(k) = cx.key() {
            match k.code {
                KeyCode::Up => failed_selected = failed_selected.saturating_sub(1),
                KeyCode::Down => {
                    failed_selected = (failed_selected + 1).min(failed.len() - 1);
                }
                KeyCode::Enter => {
                    if let Some(expanded) = failed_expanded.get_mut(failed_selected) {
                        *expanded = !*expanded;
                    }
                }
                KeyCode::Esc => quit(),
                _ => {}
            }
        }

        let ratio = (seen as f32 / total.max(1) as f32).min(1.0);
        let filled = (f32::from(BAR_WIDTH) * ratio).round() as u16;
        let (live_label, done_label, bar_label) = if testing {
            ("Running", "Tests", "Progress")
        } else {
            ("Compiling", "Compiled", "Build")
        };

        let mut lines = vec![
            rimel::text(&flag_line).dim(),
            rimel::separator(40).dim(),
            rimel::text(live_label).dim(),
        ];

        // Fixed row counts so nothing below reflows as jobs start/finish; blank rows pad instead.
        // Older rows fade toward the background so the freshest entry stands out.
        let building_count = building.len().min(BUILDING_ROWS);
        for i in 0..BUILDING_ROWS {
            match building.get(i) {
                Some((name, start, running_script)) => {
                    let alpha = (i + 1) as f32 / building_count.max(1) as f32;
                    let name_fg = if name == &project {
                        rimel::palette::SAPPHIRE
                    } else {
                        rimel::palette::TEXT
                    };
                    let label = if testing {
                        name.clone()
                    } else if *running_script {
                        let version = versions.get(name).map(String::as_str).unwrap_or("");
                        format!("build({name}) {version}")
                    } else {
                        let version = versions.get(name).map(String::as_str).unwrap_or("");
                        format!("{name} {version}")
                    };
                    lines.push(fade(
                        rimel::row([
                            rimel::text("● ").fg(rimel::palette::YELLOW),
                            rimel::text(format!("{label:<NAME_WIDTH$}")).fg(name_fg),
                            rimel::text(format!("{:05.2}s", start.elapsed().as_secs_f32()))
                                .fg(rimel::palette::SUBTEXT0),
                        ]),
                        alpha,
                    ))
                }
                None if i == 0 && building.is_empty() => lines.push(rimel::text("  …").dim()),
                None => lines.push(rimel::text("")),
            }
        }

        lines.push(rimel::separator(40).dim());
        lines.push(rimel::text(format!("{done_label} ({seen}/{total})")).dim());
        let recent: Vec<&(String, f32)> = done.iter().rev().take(DONE_ROWS).collect();
        let recent_count = recent.len();
        for _ in 0..DONE_ROWS - recent.len() {
            lines.push(rimel::text(""));
        }
        for (i, (name, secs)) in recent.into_iter().rev().enumerate() {
            let alpha = (i + 1) as f32 / recent_count.max(1) as f32;
            let label = if testing {
                name.clone()
            } else {
                let version = versions.get(name).map(String::as_str).unwrap_or("");
                format!("{name} {version}")
            };
            lines.push(fade(
                rimel::row([
                    rimel::text("✓ ").fg(rimel::palette::GREEN),
                    rimel::text(format!("{label:<NAME_WIDTH$}")).fg(rimel::palette::TEXT),
                    rimel::text(format!("{secs:05.2}s")).fg(rimel::palette::SUBTEXT0),
                ]),
                alpha,
            ));
        }
        // The gradient sweep is a "still working" cue. Once settled, nothing is, so the bar
        // gets a plain outcome color instead of an animation that would keep drifting forever.
        let filled_bar = if settled {
            let color = if failed.is_empty() {
                rimel::palette::GREEN
            } else {
                rimel::palette::RED
            };
            rimel::text("█".repeat(filled as usize)).fg(color)
        } else {
            rimel::text("█".repeat(filled as usize)).animate(rust_ramp(), 0.6)
        };
        lines.push(rimel::text(""));
        lines.push(rimel::row([
            rimel::text(format!("{bar_label:<14}")).dim(),
            filled_bar,
            rimel::text("░".repeat((BAR_WIDTH - filled) as usize)).fg(rimel::palette::SURFACE1),
            rimel::text(format!("  {:>3.0}%", ratio * 100.0)),
        ]));
        lines.push(rimel::row([
            rimel::text(format!("{:<14}", "Elapsed")).dim(),
            rimel::text(format!("{elapsed:.2}s")),
        ]));

        // Accordion: every failed test is always a row here, `settled` just turns on the
        // ↑↓ / Enter navigation above. Before that it's still shown, just not selectable yet,
        // so a failure is visible the instant it happens instead of hiding behind the run.
        if !failed.is_empty() {
            let header = if settled {
                format!("Failed ({}/{})", failed_selected + 1, failed.len())
            } else {
                format!("Failed ({})", failed.len())
            };
            lines.push(rimel::text(""));
            lines.push(rimel::text(header).fg(rimel::palette::RED).bold());
            if settled {
                lines.push(rimel::text(""));
                lines.push(rimel::text("↑↓ select   Enter expand/collapse").dim());
            }
            // Scrolls to keep the selected row in view instead of dumping every failure on
            // screen at once, which is unreadable once there are more than a handful.
            let window_start = if failed.len() > FAILED_ROWS {
                failed_selected
                    .saturating_sub(FAILED_ROWS - 1)
                    .min(failed.len() - FAILED_ROWS)
            } else {
                0
            };
            let window_end = (window_start + FAILED_ROWS).min(failed.len());
            let window = failed[window_start..window_end]
                .iter()
                .enumerate()
                .map(|(j, f)| (window_start + j, f));
            for (i, f) in window {
                let expanded = failed_expanded.get(i).copied().unwrap_or(false);
                let marker = if expanded { "▾ " } else { "▸ " };
                let name =
                    rimel::text(format!("{:<NAME_WIDTH$} ", f.name)).fg(rimel::palette::TEXT);
                let name = if settled && i == failed_selected {
                    name.bold().fg(Color::LightRed)
                } else {
                    name
                };
                lines.push(rimel::row([
                    rimel::text(marker).fg(rimel::palette::RED),
                    name,
                    rimel::text(format!("{:05.2}s", f.secs)).fg(rimel::palette::SUBTEXT0),
                ]));
                if expanded {
                    if let Some(loc) = &f.location {
                        lines.push(rimel::text(format!("  at {loc}")).dim());
                    }
                    lines.push(rimel::text(format!("  {}", f.stdout.clone())));
                }
            }
        }

        cx.render(rimel::col(lines));
        if is_test && settled && failed.is_empty() {
            quit();
        }
    });

    match run {
        Ok(()) => {}
        // Ctrl+C: the run already left the terminal in a clean state, so there's nothing to
        // report. Exit quietly with the shell's usual SIGINT-style code instead of letting
        // eyre print `Cancelled` as if it were a failure.
        Err(e) if e.downcast_ref::<Cancelled>().is_some() => std::process::exit(130),
        Err(e) => return Err(e),
    }

    if build_ok == Some(false) {
        for err in &errors {
            eprint!("{err}");
        }
        std::process::exit(1);
    }

    // The TUI has already handed the terminal back at this point, so the child inherits it
    // cleanly instead of racing the animated view for the same screen region.
    if let Verb::Run { program_args } = verb {
        let Some(path) = run_target else {
            eprintln!("cargo pretty: build succeeded but no runnable target was found");
            std::process::exit(1);
        };
        let name = path
            .file_name()
            .map_or_else(|| path.to_string_lossy(), std::ffi::OsStr::to_string_lossy);

        let (name, _) = name.split_once('.').unwrap_or((&name, ""));

        println!("{}", "─".repeat(40).dim());
        println!(
            "{} {}",
            " Executing".dim(),
            format!("{name}").bold().underlined()
        );
        let status = std::process::Command::new(path)
            .args(program_args)
            .status()?;
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}
