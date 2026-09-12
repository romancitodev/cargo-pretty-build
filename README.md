# `cargo-pretty`

![Check Format and Code Quality](https://github.com/romancitodev/cargo-pretty/actions/workflows/checks.yml/badge.svg)
![license](https://img.shields.io/badge/license-MIT-blue.svg)

<p align="center">
  <img src="/assets/building.gif" alt="demo">
</p>

Cargo's own build output does the job. This does it prettier: a live status view for
`build`, `run` and `test`, showing which crates (or tests) are running right now, how
long each one took, and a progress bar driven by cargo's real unit graph instead of a
guess.

> 💔 **Renamed from `cargo-pretty-build`.** One binary now, not one per verb. Had the
> old one installed? `cargo uninstall cargo-pretty-build`, install this, and swap
> `cargo pretty-build` for `cargo pretty build`. Bare `cargo pretty` still means build,
> so most of your muscle memory survives.

<p align="center">
  <video src="assets/cargo-pretty-itself.mp4" controls></video>
</p>

## Install

```bash
cargo install cargo-pretty-build      # from crates.io
cargo binstall cargo-pretty-build     # prebuilt, from GitHub releases
cargo install --git https://github.com/romancitodev/cargo-pretty  # straight from source
```

The crate is still called `cargo-pretty-build` (it was already published under that
name), only the binary it installs is `cargo-pretty`.

## Usage

```bash
cargo pretty                              # cargo build
cargo pretty build --release
cargo pretty run --bin server -- --port 8080
cargo pretty test
```

Everything after the verb goes straight to the matching cargo command. For `run`, args
after `--` land in your binary, not cargo. For `test`, args after `--` land in the test
harness (`--no-fail-fast`, a name filter, whatever you need).

## What you get

- 🛠️ Up to six crates (or tests) in flight at once, each with its own timer, older
  entries fading out as newer ones start
- 📊 A progress bar reading cargo's real unit count (via its own `--unit-graph`), not an
  estimate
- ⚠️ Warnings collected into a browsable panel (`↑↓` move, `Enter` expand, `Esc`
  dismiss) instead of scrolling past them
- ❌ Failed tests get the same treatment, always visible instead of vanishing once the
  run ends: `↑↓` to pick one, `Enter` to see the panic
- ✅ A short summary on exit: time elapsed, libs compiled or tests passed, disk used
- 💌 Feed it a verb it doesn't know and it takes it personally

## Testing
Testing was upgraded to provide a better output experience. Now, when a test fails, the output will include diff-coloring for `assert!` and `assert_eq!` panics, making it easier to identify what went wrong. Additionally, you can expand the information of a specific test by pressing `Enter`, and if you want to retry the test, simply press `r`. This makes debugging and iterating on tests much more efficient and user-friendly.

<p align="center">
  <img src="/assets/testing.gif" alt="testing">
</p>


## Star History

[![Star History Chart](https://api.star-history.com/chart?repos=romancitodev/cargo-pretty&type=date&logscale&legend=top-left)](https://www.star-history.com/?repos=romancitodev%2Fcargo-pretty&type=date&logscale=&legend=top-left)

## Built on

The rendering runs on [nobubbles](https://github.com/romancitodev/nobubbles), a TUI
library from the same author.

## License

MIT, see [LICENSE](LICENSE).
