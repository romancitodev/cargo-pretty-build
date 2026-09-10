/// Verb requested via `cargo pretty <verb> ...`. The compile phase always goes through
/// [`crate::build::build`] (a plain `cargo build` for `build`/`run`, `cargo test --no-run` for
/// `test`); this only decides what happens once it succeeds.
#[derive(Clone)]
pub enum Verb {
    Build,
    /// Carries the argv to hand to the built binary, taken from after a `--` separator.
    Run { program_args: Vec<String> },
    /// Carries the libtest harness args (test name filter, `--no-fail-fast`, ...), also taken
    /// from after a `--` separator.
    Test { harness_args: Vec<String> },
}

impl Verb {
    /// Splits `cargo pretty`'s argv (subcommand token already stripped) into the requested
    /// verb and the args to pass to the compile step. No verb given defaults to `Build`,
    /// keeping bare `cargo pretty` equivalent to the old `cargo pretty-build`. An unrecognized
    /// verb is an error rather than a silent fallback to build, since that would run the wrong
    /// thing.
    pub fn parse(mut args: Vec<String>) -> Result<(Self, Vec<String>), String> {
        let Some(verb) = args.first().filter(|a| !a.starts_with('-')) else {
            return Ok((Verb::Build, args));
        };

        match verb.as_str() {
            "build" => {
                args.remove(0);
                Ok((Verb::Build, args))
            }
            "run" => {
                args.remove(0);
                Ok((
                    Verb::Run {
                        program_args: split_trailing_args(&mut args),
                    },
                    args,
                ))
            }
            "test" => {
                args.remove(0);
                Ok((
                    Verb::Test {
                        harness_args: split_trailing_args(&mut args),
                    },
                    args,
                ))
            }
            other => Err(format!(
                "cargo pretty: `{other}` isn't supported yet (try build, run or test)"
            )),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Verb::Build => "build",
            Verb::Run { .. } => "run",
            Verb::Test { .. } => "test",
        }
    }

    /// The cargo subcommand (plus any fixed flags) that compiles this verb's target.
    /// `run`'s own execution happens separately, after this build finishes, so it compiles
    /// exactly like `build`; `test` only needs the test binaries built, not run by cargo itself.
    pub fn compile_args(&self) -> &'static [&'static str] {
        match self {
            Verb::Build | Verb::Run { .. } => &["build"],
            Verb::Test { .. } => &["test", "--no-run"],
        }
    }
}

/// Pulls the args after a trailing `--` out of `args`, leaving `args` holding only what came
/// before it (with the `--` itself dropped too).
fn split_trailing_args(args: &mut Vec<String>) -> Vec<String> {
    match args.iter().position(|a| a == "--") {
        Some(i) => {
            let rest = args.split_off(i + 1);
            args.pop(); // drop the trailing "--" itself
            rest
        }
        None => Vec::new(),
    }
}
