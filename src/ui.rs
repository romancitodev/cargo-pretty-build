use nobubbles::rimel::{self, Border, Ramp, palette};

use crate::build::Warning;
use crate::metrics::human_size;

pub fn rust_ramp() -> Ramp {
    let gradient = colorgrad::GradientBuilder::new()
        .html_colors(&[
            "#C1440E", "#E8834E", "#F6C9A0", "#FFFFFF", "#F6C9A0", "#E8834E", "#C1440E",
        ])
        .build::<colorgrad::LinearGradient>()
        .expect("static color list is valid");
    Ramp::new(gradient)
}

/// Fades a whole block toward the background by `alpha` (`0.0` invisible, `1.0` full color),
/// for a list where older rows recede and the newest stands out.
pub fn fade(block: rimel::Block, alpha: f32) -> rimel::Block {
    block.map_cells(move |_, _, style| {
        let fg = if style.fg == rimel::Color::Reset {
            palette::TEXT
        } else {
            style.fg
        };
        style.fg(rimel::blend(fg, palette::BASE, alpha))
    })
}

pub fn warning_panel(warnings: &[Warning], selected: usize, inspecting: bool) -> rimel::Block {
    let w = &warnings[selected];

    let body = if inspecting {
        rimel::text(w.rendered.clone())
    } else {
        let mut inner = vec![rimel::text(&w.message)];
        if let Some(loc) = &w.location {
            inner.push(rimel::text(loc).dim());
        }
        if let Some(help) = &w.help {
            inner.push(rimel::text(""));
            inner.push(rimel::text(format!("help: {help}")).dim());
        }
        rimel::col(inner)
    };
    let boxed = body
        .px(1)
        .border_with(Border::rounded())
        .border_color(palette::YELLOW);

    rimel::col([
        rimel::text("⚠  Build completed with warnings")
            .fg(palette::YELLOW)
            .bold(),
        rimel::text(""),
        rimel::text(format!(
            "{} / {} warning{}",
            selected + 1,
            warnings.len(),
            if warnings.len() == 1 { "" } else { "s" }
        ))
        .dim(),
        rimel::text(""),
        boxed,
        rimel::text(""),
        rimel::text("↑↓ navigate   Enter inspect   Esc dismiss").dim(),
    ])
}

pub fn summary_block(
    project: &str,
    ok: bool,
    secs: f32,
    warnings: usize,
    libs: usize,
    grew_by: u64,
) -> rimel::Block {
    let head = if ok && warnings == 0 {
        rimel::text(format!("✓ {project} built in {secs:.2}s"))
            .fg(palette::GREEN)
            .bold()
    } else if ok {
        rimel::text(format!(
            "⚠ {project} built in {secs:.2}s ({warnings} warning{})",
            if warnings == 1 { "" } else { "s" }
        ))
        .fg(palette::YELLOW)
        .bold()
    } else {
        rimel::text(format!("✗ {project} failed after {secs:.2}s"))
            .fg(palette::RED)
            .bold()
    };

    rimel::col([
        head,
        rimel::row([
            rimel::text("  libs compiled  ").dim(),
            rimel::text(format!("{libs}")),
        ]),
        rimel::row([
            rimel::text("  disk used      ").dim(),
            rimel::text(human_size(grew_by)),
        ]),
    ])
}
