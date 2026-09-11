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
use crossterm::terminal;
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
/// Coarse jump size for PageUp/PageDown and for landing near the tail when a test is first
/// expanded. Not the actual display cap: the render step sizes that to what the real terminal
/// can show, so this only has to be a reasonable approximation for input handling.
const STDOUT_PAGE: usize = 20;
/// Right-hand breathing room for wrapped stdout lines, so captured output stops just short of
/// the terminal edge instead of spanning the full width.
const STDOUT_RIGHT_PADDING: usize = 4;

/// What's being built: identity, disk footprint, per-crate versions.
#[derive(Default)]
struct Project {
    name: String,
    target_dir: Option<PathBuf>,
    before_size: u64,
    versions: HashMap<String, String>,
    compiled: HashSet<String>,
    grew_by: u64,
}

/// The live progress bar / building-done lists.
#[derive(Default)]
struct Progress {
    total: usize,
    seen: usize,
    // (crate or test name, started, running its build.rs right now? always false for tests)
    building: Vec<(String, Instant, bool)>,
    done: Vec<(String, f32)>,
    // Flips once the compile phase hands off to running the compiled tests, at which point
    // "Compiling"/"Compiled" become "Running"/"Tests" and the progress bar restarts at 0.
    testing: bool,
}

/// The warning panel shown after a build with warnings settles.
#[derive(Default)]
struct Warnings {
    items: Vec<Warning>,
    selected: usize,
    inspecting: bool,
}

/// The failed-test accordion: every failure stays listed, ↑↓/Enter pick and expand one.
#[derive(Default)]
struct FailedTests {
    items: Vec<FailedTest>,
    selected: usize,
    expanded: Vec<bool>,
    // How many lines of the selected test's stdout are scrolled past, so a panic's full
    // output stays reachable via PageUp/PageDown instead of being clipped by the terminal.
    stdout_scroll: usize,
}

/// Approximate scroll position that lands near the tail (usually where the panic message is).
/// The render step re-clamps this to the terminal's actual available rows.
fn tail_scroll(stdout: &str) -> usize {
    stdout.lines().count().saturating_sub(STDOUT_PAGE)
}

/// Terminal rows currently available, or a sane guess when the query fails (e.g. not a tty).
fn terminal_rows() -> usize {
    terminal::size().map(|(_, rows)| rows).unwrap_or(24) as usize
}

/// Terminal columns currently available, or a sane guess when the query fails (e.g. not a tty).
fn terminal_cols() -> usize {
    terminal::size().map(|(cols, _)| cols).unwrap_or(80) as usize
}

/// Hard-wraps `line` to `width` chars. norimel clips instead of wrapping, so this has to.
/// ponytail: char count, not display width. Fine for ASCII debug output.
fn wrap_line(line: &str, width: usize) -> Vec<&str> {
    if width == 0 {
        return vec![line];
    }
    let mut out = Vec::new();
    let mut start = 0;
    let mut count = 0;
    for (i, _) in line.char_indices() {
        if count == width {
            out.push(&line[start..i]);
            start = i;
            count = 0;
        }
        count += 1;
    }
    out.push(&line[start..]);
    out
}

/// How a stdout line should stand out. Panic message and assert diff get styled, the rest
/// stays `Plain`.
#[derive(Clone, Copy, PartialEq, Debug)]
enum StdoutStyle {
    Plain,
    Message,
    Left,
    Right,
}

/// Tags the line after `panicked at ...:` as the message, and libtest's `  left: `/` right: `
/// pair after it, when present, as the diff.
fn classify_stdout(stdout: &str) -> Vec<(StdoutStyle, &str)> {
    let mut out = Vec::new();
    let mut lines = stdout.lines().peekable();
    while let Some(line) = lines.next() {
        out.push((StdoutStyle::Plain, line));
        if !line.contains("panicked at ") {
            continue;
        }
        let Some(message) = lines.next() else { continue };
        out.push((StdoutStyle::Message, message));
        if let Some(left) = lines.peek().copied()
            && left.starts_with("  left: ")
        {
            out.push((StdoutStyle::Left, left));
            lines.next();
            if let Some(right) = lines.peek().copied()
                && right.starts_with(" right: ")
            {
                out.push((StdoutStyle::Right, right));
                lines.next();
            }
        }
    }
    out
}

/// Everything the render loop accumulates across frames.
#[derive(Default)]
struct State {
    project: Project,
    progress: Progress,
    warnings: Warnings,
    failed: FailedTests,
    errors: Vec<String>,
    build_ok: Option<bool>,
    run_target: Option<PathBuf>,
    // Frozen the instant `Done` fires, so the clock stops for real instead of ticking away
    // while the user browses failed tests afterward.
    final_elapsed: Option<f32>,
}

fn main() -> Result<()> {
    let mut extra_args: Vec<String> = std::env::args().skip(1).collect();
    // When cargo dispatches `cargo pretty ...`, it prepends the subcommand name to argv,
    // so it'd otherwise get forwarded into the wrapped cargo command as if the user had typed it.
    if let Some(subcommand) = env!("CARGO_BIN_NAME").strip_prefix("cargo-")
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
    let mut s = State::default();

    let run = Inline::run(20, |cx| {
        inbox.drain(|event| match event {
            Event::Ready {
                project: p,
                target_dir: dir,
                total: t,
                versions: v,
            } => {
                s.project.name = p;
                s.project.before_size = dir_size(&dir);
                s.project.target_dir = Some(dir);
                s.progress.total = t;
                s.project.versions = v;
            }
            Event::Total(t) => s.progress.total = t,
            Event::Started(name) => {
                if !s.progress.building.iter().any(|(n, _, _)| n == &name) {
                    s.progress.building.push((name, Instant::now(), false));
                }
            }
            Event::ScriptRunning(name) => {
                if let Some(entry) = s.progress.building.iter_mut().find(|(n, _, _)| n == &name) {
                    entry.2 = true;
                }
            }
            Event::ScriptExecuted(name) => {
                s.progress.seen += 1;
                if let Some(entry) = s.progress.building.iter_mut().find(|(n, _, _)| n == &name) {
                    entry.2 = false;
                }
            }
            Event::Artifact {
                id,
                name,
                fresh,
                real,
            } => {
                s.progress.seen += 1;
                if !fresh {
                    s.project.compiled.insert(id);
                    if real {
                        let secs = match s.progress.building.iter().position(|(n, _, _)| n == &name)
                        {
                            Some(pos) => s.progress.building.remove(pos).1.elapsed().as_secs_f32(),
                            None => 0.0,
                        };
                        s.progress.done.push((name, secs));
                    }
                }
            }
            Event::Warning(w) => s.warnings.items.push(w),
            Event::Error(msg) => s.errors.push(msg),
            Event::Executables(paths) => s.run_target = paths.into_iter().next(),
            Event::SuiteStarted { total: t } => {
                if !s.progress.testing {
                    s.progress.testing = true;
                    s.progress.seen = 0;
                    s.progress.total = 0;
                    s.progress.done.clear();
                    s.progress.building.clear();
                }
                s.progress.total += t;
            }
            Event::TestStarted(name) => {
                if !s.progress.building.iter().any(|(n, _, _)| n == &name) {
                    s.progress.building.push((name, Instant::now(), false));
                }
            }
            Event::TestFinished {
                name,
                secs,
                outcome,
            } => {
                s.progress.seen += 1;
                if let Some(pos) = s.progress.building.iter().position(|(n, _, _)| n == &name) {
                    s.progress.building.remove(pos);
                }
                match outcome {
                    Outcome::Passed => s.progress.done.push((name, secs)),
                    Outcome::Ignored => {}
                    Outcome::Failed { location, stdout } => {
                        s.failed.items.push(FailedTest {
                            name,
                            secs,
                            location,
                            stdout,
                        });
                        s.failed.expanded.push(false);
                    }
                }
            }
            Event::Done(ok) => {
                s.build_ok = Some(ok);
                s.final_elapsed = Some(started.elapsed().as_secs_f32());
                s.project.grew_by = s
                    .project
                    .target_dir
                    .as_deref()
                    .map(|dir| dir_size(dir).saturating_sub(s.project.before_size))
                    .unwrap_or(0);
            }
        });

        let is_test = matches!(verb, Verb::Test { .. });
        let elapsed = s
            .final_elapsed
            .unwrap_or_else(|| started.elapsed().as_secs_f32());

        if dismissing.get() {
            cx.render(summary_block(
                &s.project.name,
                s.build_ok.unwrap_or(false),
                elapsed,
                s.warnings.items.len(),
                s.project.compiled.len(),
                s.project.grew_by,
            ));
            quit();
            return;
        }

        let mut settled = false;
        if s.build_ok.is_some() {
            let t = settle_t.update(|t| {
                *t = (*t + SETTLE_STEP).min(1.0);
                *t
            });
            settled = t >= 1.0;

            // `build`/`run` still replace the whole view with a compact summary once settled.
            // `test` instead falls through to the normal table below and keeps it on screen:
            // the last few tests run are exactly what you want to still see once it's done.
            if settled && !is_test {
                if !s.warnings.items.is_empty() {
                    if let Some(k) = cx.key() {
                        match k.code {
                            KeyCode::Up => {
                                s.warnings.selected = s.warnings.selected.saturating_sub(1);
                                s.warnings.inspecting = false;
                            }
                            KeyCode::Down => {
                                s.warnings.selected =
                                    (s.warnings.selected + 1).min(s.warnings.items.len() - 1);
                                s.warnings.inspecting = false;
                            }
                            KeyCode::Enter => s.warnings.inspecting = !s.warnings.inspecting,
                            KeyCode::Esc => {
                                dismissing.set(true);
                                cx.render(rimel::text(""));
                                return;
                            }
                            _ => {}
                        }
                    }
                    cx.render(warning_panel(
                        &s.warnings.items,
                        s.warnings.selected,
                        s.warnings.inspecting,
                    ));
                    return;
                }

                cx.render(summary_block(
                    &s.project.name,
                    s.build_ok.unwrap_or(false),
                    elapsed,
                    s.warnings.items.len(),
                    s.project.compiled.len(),
                    s.project.grew_by,
                ));
                quit();
                return;
            }
        }

        // Accordion over every failed test, not a one-at-a-time viewer: all rows stay visible.
        // While a row is collapsed, ↑↓ move which row is highlighted. Once Enter expands it,
        // ↑↓ scroll its stdout instead (that's the content the arrows are on now); PageUp/Down
        // always scroll regardless, and the render step is what actually clamps to the true
        // end, so these never need to know the real max themselves.
        if is_test
            && settled
            && !s.failed.items.is_empty()
            && let Some(k) = cx.key()
        {
            let expanded = s
                .failed
                .expanded
                .get(s.failed.selected)
                .copied()
                .unwrap_or(false);
            match k.code {
                KeyCode::Up if expanded => {
                    s.failed.stdout_scroll = s.failed.stdout_scroll.saturating_sub(1);
                }
                KeyCode::Down if expanded => {
                    s.failed.stdout_scroll = s.failed.stdout_scroll.saturating_add(1);
                }
                KeyCode::Up => {
                    s.failed.selected = s.failed.selected.saturating_sub(1);
                    s.failed.stdout_scroll = tail_scroll(&s.failed.items[s.failed.selected].stdout);
                }
                KeyCode::Down => {
                    s.failed.selected = (s.failed.selected + 1).min(s.failed.items.len() - 1);
                    s.failed.stdout_scroll = tail_scroll(&s.failed.items[s.failed.selected].stdout);
                }
                KeyCode::Enter => {
                    if let Some(expanded) = s.failed.expanded.get_mut(s.failed.selected) {
                        *expanded = !*expanded;
                    }
                    s.failed.stdout_scroll = tail_scroll(&s.failed.items[s.failed.selected].stdout);
                }
                KeyCode::PageUp => {
                    s.failed.stdout_scroll = s.failed.stdout_scroll.saturating_sub(STDOUT_PAGE);
                }
                KeyCode::PageDown => {
                    s.failed.stdout_scroll = s.failed.stdout_scroll.saturating_add(STDOUT_PAGE);
                }
                KeyCode::Esc => quit(),
                _ => {}
            }
        }

        let ratio = (s.progress.seen as f32 / s.progress.total.max(1) as f32).min(1.0);
        let filled = (f32::from(BAR_WIDTH) * ratio).round() as u16;
        let (live_label, done_label, bar_label) = if s.progress.testing {
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
        let building_count = s.progress.building.len().min(BUILDING_ROWS);
        for i in 0..BUILDING_ROWS {
            match s.progress.building.get(i) {
                Some((name, start, running_script)) => {
                    let alpha = (i + 1) as f32 / building_count.max(1) as f32;
                    let name_fg = if name == &s.project.name {
                        rimel::palette::SAPPHIRE
                    } else {
                        rimel::palette::TEXT
                    };
                    let label = if s.progress.testing {
                        name.clone()
                    } else if *running_script {
                        let version = s
                            .project
                            .versions
                            .get(name)
                            .map(String::as_str)
                            .unwrap_or("");
                        format!("build({name}) {version}")
                    } else {
                        let version = s
                            .project
                            .versions
                            .get(name)
                            .map(String::as_str)
                            .unwrap_or("");
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
                None if i == 0 && s.progress.building.is_empty() => {
                    lines.push(rimel::text("  …").dim())
                }
                None => lines.push(rimel::text("")),
            }
        }

        lines.push(rimel::separator(40).dim());
        lines.push(
            rimel::text(format!(
                "{done_label} ({}/{})",
                s.progress.seen, s.progress.total
            ))
            .dim(),
        );
        let recent: Vec<&(String, f32)> = s.progress.done.iter().rev().take(DONE_ROWS).collect();
        let recent_count = recent.len();
        for _ in 0..DONE_ROWS - recent.len() {
            lines.push(rimel::text(""));
        }
        for (i, (name, secs)) in recent.into_iter().rev().enumerate() {
            let alpha = (i + 1) as f32 / recent_count.max(1) as f32;
            let label = if s.progress.testing {
                name.clone()
            } else {
                let version = s
                    .project
                    .versions
                    .get(name)
                    .map(String::as_str)
                    .unwrap_or("");
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
            let color = if s.failed.items.is_empty() {
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
        if !s.failed.items.is_empty() {
            let header = if settled {
                format!(
                    "Failed ({}/{})",
                    s.failed.selected + 1,
                    s.failed.items.len()
                )
            } else {
                format!("Failed ({})", s.failed.items.len())
            };
            lines.push(rimel::text(""));
            lines.push(rimel::text(header).fg(rimel::palette::RED).bold());
            if settled {
                lines.push(rimel::text(""));
                lines.push(
                    rimel::text(
                        "↑↓ select / scroll when expanded   Enter expand/collapse   PgUp/PgDn page",
                    )
                    .dim(),
                );
            }
            // Scrolls to keep the selected row in view instead of dumping every failure on
            // screen at once, which is unreadable once there are more than a handful.
            let window_start = if s.failed.items.len() > FAILED_ROWS {
                s.failed
                    .selected
                    .saturating_sub(FAILED_ROWS - 1)
                    .min(s.failed.items.len() - FAILED_ROWS)
            } else {
                0
            };
            let window_end = (window_start + FAILED_ROWS).min(s.failed.items.len());
            let window = s.failed.items[window_start..window_end]
                .iter()
                .enumerate()
                .map(|(j, f)| (window_start + j, f));
            for (i, f) in window {
                let expanded = s.failed.expanded.get(i).copied().unwrap_or(false);
                let marker = if expanded { "▾ " } else { "▸ " };
                let name =
                    rimel::text(format!("{:<NAME_WIDTH$} ", f.name)).fg(rimel::palette::TEXT);
                let name = if settled && i == s.failed.selected {
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
                    // Unbounded stdout can grow the view taller than the terminal, which
                    // ratatui's inline viewport then silently clips instead of scrolling. Sizing
                    // the window to what's actually left on screen shows the whole thing
                    // whenever it fits (the common case), only scrolling when it truly can't.
                    let indent_width = terminal_cols()
                        .saturating_sub(2 + STDOUT_RIGHT_PADDING)
                        .max(1);
                    let stdout_lines: Vec<(StdoutStyle, &str)> = classify_stdout(&f.stdout)
                        .into_iter()
                        .flat_map(|(style, line)| {
                            wrap_line(line, indent_width)
                                .into_iter()
                                .map(move |chunk| (style, chunk))
                        })
                        .collect();
                    let budget = terminal_rows()
                        .saturating_sub(lines.len())
                        .saturating_sub(2)
                        .max(3);
                    let max_scroll = stdout_lines.len().saturating_sub(budget);
                    let scroll = s.failed.stdout_scroll.min(max_scroll);
                    let end = (scroll + budget).min(stdout_lines.len());
                    if scroll > 0 {
                        lines.push(rimel::text(format!("  ↑ {scroll} more lines (PageUp)")).dim());
                    }
                    for (style, line) in &stdout_lines[scroll..end] {
                        let text = rimel::text(format!("  {line}"));
                        lines.push(match style {
                            StdoutStyle::Plain => text,
                            StdoutStyle::Message => text.bold(),
                            StdoutStyle::Left => text.fg(rimel::palette::RED),
                            StdoutStyle::Right => text.fg(rimel::palette::GREEN),
                        });
                    }
                    let below = stdout_lines.len() - end;
                    if below > 0 {
                        lines.push(rimel::text(format!("  ↓ {below} more lines (PageDown)")).dim());
                    }
                }
            }
        }

        cx.render(rimel::col(lines));
        if is_test && settled && s.failed.items.is_empty() {
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

    if s.build_ok == Some(false) {
        for err in &s.errors {
            eprint!("{err}");
        }
        std::process::exit(1);
    }

    // The TUI has already handed the terminal back at this point, so the child inherits it
    // cleanly instead of racing the animated view for the same screen region.
    if let Verb::Run { program_args } = verb {
        let Some(path) = s.run_target else {
            eprintln!("cargo pretty: build succeeded but no runnable target was found");
            std::process::exit(1);
        };
        let name = path
            .file_name()
            .map_or_else(|| path.to_string_lossy(), std::ffi::OsStr::to_string_lossy);

        let (name, _) = name.split_once('.').unwrap_or((&name, ""));

        println!("{}", "─".repeat(40).dim());
        println!("{} {}", " Executing".dim(), name.bold().underlined());
        let status = std::process::Command::new(path)
            .args(program_args)
            .status()?;
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{StdoutStyle, classify_stdout, wrap_line};

    #[test]
    fn wraps_long_lines_and_leaves_short_ones_alone() {
        assert_eq!(wrap_line("short", 10), vec!["short"]);
        assert_eq!(wrap_line("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap_line("", 4), vec![""]);
    }

    #[test]
    fn classifies_the_panic_message_and_assert_eq_diff() {
        let stdout = "thread 'x' panicked at src/lib.rs:1:1:\n\
            assertion `left == right` failed\n  left: `1`\n right: `2`\n\
            note: run with `RUST_BACKTRACE=1`";
        assert_eq!(
            classify_stdout(stdout),
            vec![
                (
                    StdoutStyle::Plain,
                    "thread 'x' panicked at src/lib.rs:1:1:"
                ),
                (StdoutStyle::Message, "assertion `left == right` failed"),
                (StdoutStyle::Left, "  left: `1`"),
                (StdoutStyle::Right, " right: `2`"),
                (StdoutStyle::Plain, "note: run with `RUST_BACKTRACE=1`"),
            ]
        );
    }

    #[test]
    fn leaves_a_plain_panic_message_uncolored_without_a_diff() {
        let stdout = "thread 'x' panicked at src/lib.rs:1:1:\n\
                       called `Option::unwrap()` on a `None` value";
        assert_eq!(
            classify_stdout(stdout),
            vec![
                (
                    StdoutStyle::Plain,
                    "thread 'x' panicked at src/lib.rs:1:1:"
                ),
                (
                    StdoutStyle::Message,
                    "called `Option::unwrap()` on a `None` value"
                ),
            ]
        );
    }
}
