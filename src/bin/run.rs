use cargo::ops;
use cargo::util::{CliResult, CliError, Config, Human};
use cargo::util::important_paths::{find_root_manifest_for_wd};

#[derive(RustcDecodable)]
pub struct Options {
    flag_bin: Option<String>,
    flag_example: Option<String>,
    flag_jobs: Option<u32>,
    flag_features: Vec<String>,
    flag_no_default_features: bool,
    flag_target: Option<String>,
    flag_manifest_path: Option<String>,
    flag_verbose: Option<bool>,
    flag_quiet: Option<bool>,
    flag_color: Option<String>,
    flag_release: bool,
    arg_args: Vec<String>,
}

pub const USAGE: &'static str = "
Run the main binary of the local package (src/main.rs)

Usage:
    cargo run [options] [--] [<args>...]

Options:
    -h, --help              Print this message
    --bin NAME              Name of the bin target to run
    --example NAME          Name of the example target to run
    -j N, --jobs N          The number of jobs to run in parallel
    --release               Build artifacts in release mode, with optimizations
    --features FEATURES     Space-separated list of features to also build
    --no-default-features   Do not build the `default` feature
    --target TRIPLE         Build for the target triple
    --manifest-path PATH    Path to the manifest to execute
    -v, --verbose           Use verbose output
    -q, --quiet             No output printed to stdout
    --color WHEN            Coloring: auto, always, never

If neither `--bin` nor `--example` are given, then if the project only has one
bin target it will be run. Otherwise `--bin` specifies the bin target to run,
and `--example` specifies the example target to run. At most one of `--bin` or
`--example` can be provided.

All of the trailing arguments are passed to the binary to run. If you're passing
arguments to both Cargo and the binary, the ones after `--` go to the binary,
the ones before go to Cargo.
";

pub fn execute(options: Options, config: &Config) -> CliResult<Option<()>> {
    try!(config.configure_shell(options.flag_verbose,
                                options.flag_quiet,
                                &options.flag_color));

    let root = try!(find_root_manifest_for_wd(options.flag_manifest_path, config.cwd()));

    let (mut examples, mut bins) = (Vec::new(), Vec::new());
    if let Some(s) = options.flag_bin {
        bins.push(s);
    }
    if let Some(s) = options.flag_example {
        examples.push(s);
    }

    let compile_opts = ops::CompileOptions {
        config: config,
        jobs: options.flag_jobs,
        target: options.flag_target.as_ref().map(|t| &t[..]),
        features: &options.flag_features,
        no_default_features: options.flag_no_default_features,
        spec: &[],
        exec_engine: None,
        release: options.flag_release,
        mode: ops::CompileMode::Build,
        filter: if examples.is_empty() && bins.is_empty() {
            ops::CompileFilter::Everything
        } else {
            ops::CompileFilter::Only {
                lib: false, tests: &[], benches: &[],
                bins: &bins, examples: &examples,
            }
        },
        target_rustdoc_args: None,
        target_rustc_args: None,
    };

    match try!(ops::run(&root, &compile_opts, &options.arg_args)) {
        None => Ok(None),
        Some(err) => {
            // If we never actually spawned the process then that sounds pretty
            // bad and we always want to forward that up.
            let exit = match err.exit.clone() {
                Some(exit) => exit,
                None => return Err(CliError::new(Box::new(Human(err)), 101)),
            };

            // If `-q` was passed then we suppress extra error information about
            // a failed process, we assume the process itself printed out enough
            // information about why it failed so we don't do so as well
            let exit_code = exit.code().unwrap_or(101);
            Err(if options.flag_quiet == Some(true) {
                CliError::code(exit_code)
            } else {
                CliError::new(Box::new(Human(err)), exit_code)
            })
        }
    }
}
