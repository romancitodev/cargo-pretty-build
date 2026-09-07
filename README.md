# cargo-pretty-build

A `cargo build` wrapper with a live status view: which crates are compiling right now, how
long each one took, and a progress bar driven by cargo's own unit graph instead of a guess.

<video src="assets/cargo-pretty-itself.mp4" controls></video>

## Install

```bash
cargo install --git https://github.com/romancitodev/cargo-pretty-build
```

Then run it from any cargo project as a subcommand:

```bash
cargo pretty-build
cargo pretty-build --release
```

Anything after `pretty-build` is forwarded straight to `cargo build`.

## What you get

- Up to six crates building at once, each with its own timer — older entries fade out as
  newer ones start
- The last few finished crates, same fading treatment
- A progress bar reading cargo's real unit count (via its own `--unit-graph`), not an estimate
- Warnings collected into a browsable panel (`↑↓` to move, `Enter` to expand, `Esc` to
  dismiss) instead of scrolling past them
- A short summary on exit: build time, libs compiled, disk used

## Built on

The rendering runs on [nobubbles](https://github.com/romancitodev/nobubbles), a TUI library
from the same author.

## License

MIT — see [LICENSE](LICENSE).
