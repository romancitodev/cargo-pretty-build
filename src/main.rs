mod build;
mod metrics;
mod ui;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use cargo_metadata::MetadataCommand;
use crossterm::event::KeyCode;
use eyre::Result;
use nobubbles::app::{Cancelled, Inline};
use nobubbles::effects;
use nobubbles::rimel;
use nobubbles::signals::{quit, signal};

use build::{Event, Warning, build};
use metrics::{build_closure_size, dir_size, exact_unit_count, host_triple};
use ui::{fade, rust_ramp, summary_block, warning_panel};

const BAR_WIDTH: u16 = 28;
const NAME_WIDTH: usize = 32;
const BUILDING_ROWS: usize = 6;
const DONE_ROWS: usize = 6;
/// Writing this signal every frame is what keeps nobubbles' reactive loop ticking during the
/// post-build settle animation — a plain local (Instant, bool) wouldn't request the next frame.
const SETTLE_STEP: f32 = 0.125;

fn main() -> Result<()> {
    let mut extra_args: Vec<String> = std::env::args().skip(1).collect();
    // When cargo dispatches `cargo pretty-build ...`, it prepends the subcommand name to argv,
    // so it'd otherwise get forwarded into `cargo build` as if the user had typed it.
    if let Some(subcommand) = env!("CARGO_PKG_NAME").strip_prefix("cargo-")
        && extra_args.first().map(String::as_str) == Some(subcommand)
    {
        extra_args.remove(0);
    }
    let flag_line = if extra_args.is_empty() {
        "cargo build".to_string()
    } else {
        format!("cargo build {}", extra_args.join(" "))
    };

    // `cargo metadata` and the unit count both cost their own subprocess, so none of that runs
    // before the UI does — the loop below draws its first frame immediately, on placeholders,
    // while this background thread resolves the real project name, target dir and unit total
    // and only then hands off to the actual `cargo build`.
    let inbox = effects::inbox::<Event>();
    let setup_args = extra_args.clone();
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
        std::thread::spawn(move || {
            if let Some(total) = exact_unit_count(&unit_args) {
                unit_tx.send(Event::Total(total));
            }
        });

        build(&tx, &names, &setup_args);
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
    // (crate name, started, running its build.rs right now?)
    let mut building: Vec<(String, Instant, bool)> = Vec::new();
    let mut done: Vec<(String, f32)> = Vec::new();
    let mut warnings: Vec<Warning> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut build_ok: Option<bool> = None;
    let mut grew_by = 0u64;
    let mut selected = 0usize;
    let mut inspecting = false;

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
            Event::Done(ok) => {
                build_ok = Some(ok);
                grew_by = target_dir
                    .as_deref()
                    .map(|dir| dir_size(dir).saturating_sub(before_size))
                    .unwrap_or(0);
            }
        });

        let render_summary = |ok: bool| {
            summary_block(
                &project,
                ok,
                started.elapsed().as_secs_f32(),
                warnings.len(),
                compiled.len(),
                grew_by,
            )
        };

        if dismissing.get() {
            cx.render(render_summary(build_ok.unwrap_or(false)));
            quit();
            return;
        }

        if let Some(ok) = build_ok {
            let t = settle_t.update(|t| {
                *t = (*t + SETTLE_STEP).min(1.0);
                *t
            });

            if t >= 1.0 {
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
                    ok,
                    started.elapsed().as_secs_f32(),
                    warnings.len(),
                    compiled.len(),
                    grew_by,
                ));
                quit();
                return;
            }
        }

        let ratio = (seen as f32 / total.max(1) as f32).min(1.0);
        let filled = (f32::from(BAR_WIDTH) * ratio).round() as u16;

        let mut lines = vec![
            rimel::text(&project).bold(),
            rimel::text(&flag_line).dim(),
            rimel::separator(40).dim(),
            rimel::text("Compiling").dim(),
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
                    let version = versions.get(name).map(String::as_str).unwrap_or("");
                    let label = if *running_script {
                        format!("build({name}) {version}")
                    } else {
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
        lines.push(rimel::text(format!("Compiled ({seen}/{total})")).dim());
        let recent: Vec<&(String, f32)> = done.iter().rev().take(DONE_ROWS).collect();
        let recent_count = recent.len();
        for _ in 0..DONE_ROWS - recent.len() {
            lines.push(rimel::text(""));
        }
        for (i, (name, secs)) in recent.into_iter().rev().enumerate() {
            let alpha = (i + 1) as f32 / recent_count.max(1) as f32;
            let version = versions.get(name).map(String::as_str).unwrap_or("");
            let label = format!("{name} {version}");
            lines.push(fade(
                rimel::row([
                    rimel::text("✓ ").fg(rimel::palette::GREEN),
                    rimel::text(format!("{label:<NAME_WIDTH$}")).fg(rimel::palette::TEXT),
                    rimel::text(format!("{secs:05.2}s")).fg(rimel::palette::SUBTEXT0),
                ]),
                alpha,
            ));
        }
        lines.push(rimel::text(""));
        lines.push(rimel::row([
            rimel::text("Build         ").dim(),
            rimel::text("█".repeat(filled as usize)).animate(rust_ramp(), 0.6),
            rimel::text("░".repeat((BAR_WIDTH - filled) as usize)).fg(rimel::palette::SURFACE1),
            rimel::text(format!("  {:>3.0}%", ratio * 100.0)),
        ]));
        lines.push(rimel::row([
            rimel::text("Elapsed       ").dim(),
            rimel::text(format!("{:.2}s", started.elapsed().as_secs_f32())),
        ]));

        cx.render(rimel::col(lines));
    });

    match run {
        Ok(()) => {}
        // Ctrl+C: the run already left the terminal in a clean state, so there's nothing to
        // report — exit quietly with the shell's usual SIGINT-style code instead of letting
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

    Ok(())
}
