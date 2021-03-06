//!
//! Cargo compile currently does the following steps:
//!
//! All configurations are already injected as environment variables via the
//! main cargo command
//!
//! 1. Read the manifest
//! 2. Shell out to `cargo-resolve` with a list of dependencies and sources as
//!    stdin
//!
//!    a. Shell out to `--do update` and `--do list` for each source
//!    b. Resolve dependencies and return a list of name/version/source
//!
//! 3. Shell out to `--do download` for each source
//! 4. Shell out to `--do get` for each source, and build up the list of paths
//!    to pass to rustc -L
//! 5. Call `cargo-rustc` with the results of the resolver zipped together with
//!    the results of the `get`
//!
//!    a. Topologically sort the dependencies
//!    b. Compile each dependency in order, passing in the -L's pointing at each
//!       previously compiled dependency
//!

use std::collections::HashMap;
use std::default::Default;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use core::registry::PackageRegistry;
use core::{Source, SourceId, PackageSet, Package, Target};
use core::{Profile, TargetKind, Profiles};
use core::resolver::{Method, Resolve};
use ops::{self, BuildOutput, ExecEngine};
use sources::PathSource;
use util::config::Config;
use util::{CargoResult, profile, human, ChainError};

/// Contains information about how a package should be compiled.
pub struct CompileOptions<'a> {
    pub config: &'a Config,
    /// Number of concurrent jobs to use.
    pub jobs: Option<u32>,
    /// The target platform to compile for (example: `i686-unknown-linux-gnu`).
    pub target: Option<&'a str>,
    /// Extra features to build for the root package
    pub features: &'a [String],
    /// Flag if the default feature should be built for the root package
    pub no_default_features: bool,
    /// Root package to build (if None it's the current one)
    pub spec: &'a [String],
    /// Filter to apply to the root package to select which targets will be
    /// built.
    pub filter: CompileFilter<'a>,
    /// Engine which drives compilation
    pub exec_engine: Option<Arc<Box<ExecEngine>>>,
    /// Whether this is a release build or not
    pub release: bool,
    /// Mode for this compile.
    pub mode: CompileMode,
    /// Extra arguments to be passed to rustdoc (for main crate and dependencies)
    pub target_rustdoc_args: Option<&'a [String]>,
    /// The specified target will be compiled with all the available arguments,
    /// note that this only accounts for the *final* invocation of rustc
    pub target_rustc_args: Option<&'a [String]>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum CompileMode {
    Test,
    Build,
    Bench,
    Doc { deps: bool },
}

pub enum CompileFilter<'a> {
    Everything,
    Only {
        lib: bool,
        bins: &'a [String],
        examples: &'a [String],
        tests: &'a [String],
        benches: &'a [String],
    }
}

pub fn compile<'a>(manifest_path: &Path,
                   options: &CompileOptions<'a>)
                   -> CargoResult<ops::Compilation<'a>> {
    debug!("compile; manifest-path={}", manifest_path.display());

    let package = try!(Package::for_path(manifest_path, options.config));
    debug!("loaded package; package={}", package);

    for key in package.manifest().warnings().iter() {
        try!(options.config.shell().warn(key))
    }
    compile_pkg(&package, None, options)
}

pub fn resolve_dependencies<'a>(root_package: &Package,
                                config: &'a Config,
                                source: Option<Box<Source + 'a>>,
                                features: Vec<String>,
                                no_default_features: bool)
                                -> CargoResult<(PackageSet<'a>, Resolve)> {

    let mut registry = PackageRegistry::new(config);

    if let Some(source) = source {
        registry.add_preloaded(root_package.package_id().source_id(), source);
    }

    // First, resolve the root_package's *listed* dependencies, as well as
    // downloading and updating all remotes and such.
    let resolve = try!(ops::resolve_pkg(&mut registry, root_package, config));

    // Second, resolve with precisely what we're doing. Filter out
    // transitive dependencies if necessary, specify features, handle
    // overrides, etc.
    let _p = profile::start("resolving w/ overrides...");

    try!(add_overrides(&mut registry, root_package.root(), config));

    let method = Method::Required{
        dev_deps: true, // TODO: remove this option?
        features: &features,
        uses_default_features: !no_default_features,
    };

    let resolved_with_overrides =
            try!(ops::resolve_with_previous(&mut registry, root_package,
                                            method, Some(&resolve), None));

    let packages = ops::get_resolved_packages(&resolved_with_overrides,
                                              registry);

    Ok((packages, resolved_with_overrides))
}

pub fn compile_pkg<'a>(root_package: &Package,
                       source: Option<Box<Source + 'a>>,
                       options: &CompileOptions<'a>)
                       -> CargoResult<ops::Compilation<'a>> {
    let CompileOptions { config, jobs, target, spec, features,
                         no_default_features, release, mode,
                         ref filter, ref exec_engine,
                         ref target_rustdoc_args,
                         ref target_rustc_args } = *options;

    let target = target.map(|s| s.to_string());
    let features = features.iter().flat_map(|s| {
        s.split(' ')
    }).map(|s| s.to_string()).collect::<Vec<String>>();

    if jobs == Some(0) {
        bail!("jobs must be at least 1")
    }

    let profiles = root_package.manifest().profiles();
    if spec.len() == 0 {
        try!(generate_targets(root_package, profiles, mode, filter, release));
    }

    let (packages, resolve_with_overrides) = {
        try!(resolve_dependencies(root_package, config, source, features,
                                  no_default_features))
    };

    let mut pkgids = Vec::new();
    if spec.len() > 0 {
        for p in spec {
            pkgids.push(try!(resolve_with_overrides.query(&p)));
        }
    } else {
        pkgids.push(root_package.package_id());
    };

    let to_builds = try!(pkgids.iter().map(|id| {
        packages.get(id)
    }).collect::<CargoResult<Vec<_>>>());

    let mut general_targets = Vec::new();
    let mut package_targets = Vec::new();

    match (*target_rustc_args, *target_rustdoc_args) {
        (Some(..), _) |
        (_, Some(..)) if to_builds.len() != 1 => {
            panic!("`rustc` and `rustdoc` should not accept multiple `-p` flags")
        }
        (Some(args), _) => {
            let targets = try!(generate_targets(to_builds[0], profiles,
                                                mode, filter, release));
            if targets.len() == 1 {
                let (target, profile) = targets[0];
                let mut profile = profile.clone();
                profile.rustc_args = Some(args.to_vec());
                general_targets.push((target, profile));
            } else {
                bail!("extra arguments to `rustc` can only be passed to one \
                       target, consider filtering\nthe package by passing \
                       e.g. `--lib` or `--bin NAME` to specify a single target")
            }
        }
        (None, Some(args)) => {
            let targets = try!(generate_targets(to_builds[0], profiles,
                                                mode, filter, release));
            if targets.len() == 1 {
                let (target, profile) = targets[0];
                let mut profile = profile.clone();
                profile.rustdoc_args = Some(args.to_vec());
                general_targets.push((target, profile));
            } else {
                bail!("extra arguments to `rustdoc` can only be passed to one \
                       target, consider filtering\nthe package by passing e.g. \
                       `--lib` or `--bin NAME` to specify a single target")
            }
        }
        (None, None) => {
            for &to_build in to_builds.iter() {
                let targets = try!(generate_targets(to_build, profiles, mode,
                                                    filter, release));
                package_targets.push((to_build, targets));
            }
        }
    };

    for &(target, ref profile) in &general_targets {
        for &to_build in to_builds.iter() {
            package_targets.push((to_build, vec![(target, profile)]));
        }
    }

    let mut ret = {
        let _p = profile::start("compiling");
        let mut build_config = try!(scrape_build_config(config, jobs, target));
        build_config.exec_engine = exec_engine.clone();
        build_config.release = release;
        build_config.test = mode == CompileMode::Test;
        if let CompileMode::Doc { deps } = mode {
            build_config.doc_all = deps;
        }

        try!(ops::compile_targets(&package_targets,
                                  &packages,
                                  &resolve_with_overrides,
                                  config,
                                  build_config,
                                  root_package.manifest().profiles(),
                                  ))
    };

    ret.to_doc_test = to_builds.iter().map(|&p| p.clone()).collect();

    Ok(ret)
}

impl<'a> CompileFilter<'a> {
    pub fn new(lib_only: bool,
               bins: &'a [String],
               tests: &'a [String],
               examples: &'a [String],
               benches: &'a [String]) -> CompileFilter<'a> {
        if lib_only || !bins.is_empty() || !tests.is_empty() ||
           !examples.is_empty() || !benches.is_empty() {
            CompileFilter::Only {
                lib: lib_only, bins: bins, examples: examples, benches: benches,
                tests: tests,
            }
        } else {
            CompileFilter::Everything
        }
    }

    pub fn matches(&self, target: &Target) -> bool {
        match *self {
            CompileFilter::Everything => true,
            CompileFilter::Only { lib, bins, examples, tests, benches } => {
                let list = match *target.kind() {
                    TargetKind::Bin => bins,
                    TargetKind::Test => tests,
                    TargetKind::Bench => benches,
                    TargetKind::Example => examples,
                    TargetKind::Lib(..) => return lib,
                    TargetKind::CustomBuild => return false,
                };
                list.iter().any(|x| *x == target.name())
            }
        }
    }
}

/// Given the configuration for a build, this function will generate all
/// target/profile combinations needed to be built.
fn generate_targets<'a>(pkg: &'a Package,
                        profiles: &'a Profiles,
                        mode: CompileMode,
                        filter: &CompileFilter,
                        release: bool)
                        -> CargoResult<Vec<(&'a Target, &'a Profile)>> {
    let build = if release {&profiles.release} else {&profiles.dev};
    let test = if release {&profiles.bench} else {&profiles.test};
    let profile = match mode {
        CompileMode::Test => test,
        CompileMode::Bench => &profiles.bench,
        CompileMode::Build => build,
        CompileMode::Doc { .. } => &profiles.doc,
    };
    match *filter {
        CompileFilter::Everything => {
            match mode {
                CompileMode::Bench => {
                    Ok(pkg.targets().iter().filter(|t| t.benched()).map(|t| {
                        (t, profile)
                    }).collect::<Vec<_>>())
                }
                CompileMode::Test => {
                    let mut base = pkg.targets().iter().filter(|t| {
                        t.tested()
                    }).map(|t| {
                        (t, if t.is_example() {build} else {profile})
                    }).collect::<Vec<_>>();

                    // Always compile the library if we're testing everything as
                    // it'll be needed for doctests
                    if let Some(t) = pkg.targets().iter().find(|t| t.is_lib()) {
                        if t.doctested() {
                            base.push((t, build));
                        }
                    }
                    Ok(base)
                }
                CompileMode::Build => {
                    Ok(pkg.targets().iter().filter(|t| {
                        t.is_bin() || t.is_lib()
                    }).map(|t| (t, profile)).collect())
                }
                CompileMode::Doc { .. } => {
                    Ok(pkg.targets().iter().filter(|t| t.documented())
                          .map(|t| (t, profile)).collect())
                }
            }
        }
        CompileFilter::Only { lib, bins, examples, tests, benches } => {
            let mut targets = Vec::new();

            if lib {
                if let Some(t) = pkg.targets().iter().find(|t| t.is_lib()) {
                    targets.push((t, profile));
                } else {
                    bail!("no library targets found")
                }
            }

            {
                let mut find = |names: &[String], desc, kind, profile| {
                    for name in names {
                        let target = pkg.targets().iter().find(|t| {
                            t.name() == *name && *t.kind() == kind
                        });
                        let t = match target {
                            Some(t) => t,
                            None => {
                                let suggestion = pkg.find_closest_target(name, kind);
                                match suggestion {
                                    Some(s) => {
                                        let suggested_name = s.name();
                                        bail!("no {} target named `{}`\n\nDid you mean `{}`?",
                                              desc, name, suggested_name)
                                    }
                                    None => bail!("no {} target named `{}`", desc, name),
                                }
                            }
                        };
                        debug!("found {} `{}`", desc, name);
                        targets.push((t, profile));
                    }
                    Ok(())
                };
                try!(find(bins, "bin", TargetKind::Bin, profile));
                try!(find(examples, "example", TargetKind::Example, build));
                try!(find(tests, "test", TargetKind::Test, test));
                try!(find(benches, "bench", TargetKind::Bench, &profiles.bench));
            }
            Ok(targets)
        }
    }
}

/// Read the `paths` configuration variable to discover all path overrides that
/// have been configured.
fn add_overrides<'a>(registry: &mut PackageRegistry<'a>,
                     cur_path: &Path,
                     config: &'a Config) -> CargoResult<()> {
    let paths = match try!(config.get_list("paths")) {
        Some(list) => list,
        None => return Ok(())
    };
    let paths = paths.val.iter().map(|&(ref s, ref p)| {
        // The path listed next to the string is the config file in which the
        // key was located, so we want to pop off the `.cargo/config` component
        // to get the directory containing the `.cargo` folder.
        (p.parent().unwrap().parent().unwrap().join(s), p)
    }).filter(|&(ref p, _)| {
        // Make sure we don't override the local package, even if it's in the
        // list of override paths.
        cur_path != &**p
    });

    for (path, definition) in paths {
        let id = try!(SourceId::for_path(&path));
        let mut source = PathSource::new_recursive(&path, &id, config);
        try!(source.update().chain_error(|| {
            human(format!("failed to update path override `{}` \
                           (defined in `{}`)", path.display(),
                          definition.display()))
        }));
        registry.add_override(&id, Box::new(source));
    }
    Ok(())
}

/// Parse all config files to learn about build configuration. Currently
/// configured options are:
///
/// * build.jobs
/// * build.target
/// * target.$target.ar
/// * target.$target.linker
/// * target.$target.libfoo.metadata
fn scrape_build_config(config: &Config,
                       jobs: Option<u32>,
                       target: Option<String>)
                       -> CargoResult<ops::BuildConfig> {
    let cfg_jobs = match try!(config.get_i64("build.jobs")) {
        Some(v) => {
            if v.val <= 0 {
                bail!("build.jobs must be positive, but found {} in {}",
                      v.val, v.definition)
            } else if v.val >= u32::max_value() as i64 {
                bail!("build.jobs is too large: found {} in {}", v.val,
                      v.definition)
            } else {
                Some(v.val as u32)
            }
        }
        None => None,
    };
    let jobs = jobs.or(cfg_jobs).unwrap_or(::num_cpus::get() as u32);
    let cfg_target = try!(config.get_string("build.target")).map(|s| s.val);
    let target = target.or(cfg_target);
    let mut base = ops::BuildConfig {
        jobs: jobs,
        requested_target: target.clone(),
        ..Default::default()
    };
    base.host = try!(scrape_target_config(config, &config.rustc_info().host));
    base.target = match target.as_ref() {
        Some(triple) => try!(scrape_target_config(config, &triple)),
        None => base.host.clone(),
    };
    Ok(base)
}

fn scrape_target_config(config: &Config, triple: &str)
                        -> CargoResult<ops::TargetConfig> {

    let key = format!("target.{}", triple);
    let mut ret = ops::TargetConfig {
        ar: try!(config.get_path(&format!("{}.ar", key))).map(|v| v.val),
        linker: try!(config.get_path(&format!("{}.linker", key))).map(|v| v.val),
        overrides: HashMap::new(),
    };
    let table = match try!(config.get_table(&key)) {
        Some(table) => table.val,
        None => return Ok(ret),
    };
    for (lib_name, _) in table.into_iter() {
        if lib_name == "ar" || lib_name == "linker" || lib_name == "rustflags" {
            continue
        }

        let mut output = BuildOutput {
            library_paths: Vec::new(),
            library_links: Vec::new(),
            cfgs: Vec::new(),
            metadata: Vec::new(),
            rerun_if_changed: Vec::new(),
        };
        let key = format!("{}.{}", key, lib_name);
        let table = try!(config.get_table(&key)).unwrap().val;
        for (k, _) in table.into_iter() {
            let key = format!("{}.{}", key, k);
            match &k[..] {
                "rustc-flags" => {
                    let flags = try!(config.get_string(&key)).unwrap();
                    let whence = format!("in `{}` (in {})", key,
                                         flags.definition);
                    let (paths, links) = try!(
                        BuildOutput::parse_rustc_flags(&flags.val, &whence)
                    );
                    output.library_paths.extend(paths.into_iter());
                    output.library_links.extend(links.into_iter());
                }
                "rustc-link-lib" => {
                    let list = try!(config.get_list(&key)).unwrap();
                    output.library_links.extend(list.val.into_iter()
                                                        .map(|v| v.0));
                }
                "rustc-link-search" => {
                    let list = try!(config.get_list(&key)).unwrap();
                    output.library_paths.extend(list.val.into_iter().map(|v| {
                        PathBuf::from(&v.0)
                    }));
                }
                "rustc-cfg" => {
                    let list = try!(config.get_list(&key)).unwrap();
                    output.cfgs.extend(list.val.into_iter().map(|v| v.0));
                }
                _ => {
                    let val = try!(config.get_string(&key)).unwrap();
                    output.metadata.push((k, val.val));
                }
            }
        }
        ret.overrides.insert(lib_name, output);
    }

    Ok(ret)
}
