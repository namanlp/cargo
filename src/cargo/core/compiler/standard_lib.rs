//! Code for building the standard library.

use crate::core::compiler::unit_dependencies::IsArtifact;
use crate::core::compiler::UnitInterner;
use crate::core::compiler::{CompileKind, CompileMode, RustcTargetData, Unit};
use crate::core::profiles::{Profiles, UnitFor};
use crate::core::resolver::features::{CliFeatures, FeaturesFor, ResolvedFeatures};
use crate::core::resolver::HasDevUnits;
use crate::core::{PackageId, PackageSet, Resolve, Workspace};
use crate::ops::{self, Packages};
use crate::util::errors::CargoResult;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use super::BuildConfig;

fn std_crates<'a>(crates: &'a [String], units: Option<&[Unit]>) -> HashSet<&'a str> {
    let mut crates = HashSet::from_iter(crates.iter().map(|s| s.as_str()));
    // This is a temporary hack until there is a more principled way to
    // declare dependencies in Cargo.toml.
    if crates.is_empty() {
        crates.insert("std");
    }
    if crates.contains("std") {
        crates.insert("core");
        crates.insert("alloc");
        crates.insert("proc_macro");
        crates.insert("panic_unwind");
        crates.insert("compiler_builtins");
        // Only build libtest if it looks like it is needed (libtest depends on libstd)
        // If we know what units we're building, we can filter for libtest depending on the jobs.
        if let Some(units) = units {
            if units
                .iter()
                .any(|unit| unit.mode.is_rustc_test() && unit.target.harness())
            {
                crates.insert("test");
            }
        }
    } else if crates.contains("core") {
        crates.insert("compiler_builtins");
    }

    crates
}

/// Resolve the standard library dependencies.
pub fn resolve_std<'gctx>(
    ws: &Workspace<'gctx>,
    target_data: &mut RustcTargetData<'gctx>,
    build_config: &BuildConfig,
    crates: &[String],
) -> CargoResult<(PackageSet<'gctx>, Resolve, ResolvedFeatures)> {
    let crates = std_crates(crates, None);

    if build_config.build_plan {
        ws.gctx()
            .shell()
            .warn("-Zbuild-std does not currently fully support --build-plan")?;
    }

    // check that targets support building std
    if crates.contains("std") {
        let unsupported_targets = target_data.get_unsupported_std_targets();
        if !unsupported_targets.is_empty() {
            anyhow::bail!(
                "building std is not supported on the following targets: {}",
                unsupported_targets.join(", ")
            )
        }
    }

    let src_path = detect_sysroot_src_path(target_data)?;
    let std_ws_manifest_path = src_path.join("Cargo.toml");
    let gctx = ws.gctx();
    // TODO: Consider doing something to enforce --locked? Or to prevent the
    // lock file from being written, such as setting ephemeral.
    let mut std_ws = Workspace::new(&std_ws_manifest_path, gctx)?;
    // Don't require optional dependencies in this workspace, aka std's own
    // `[dev-dependencies]`. No need for us to generate a `Resolve` which has
    // those included because we'll never use them anyway.
    std_ws.set_require_optional_deps(false);
    // `sysroot` + the default feature set below should give us a good default
    // Resolve, which includes `libtest` as well.
    let specs = Packages::Packages(vec!["sysroot".into()]);
    let specs = specs.to_package_id_specs(&std_ws)?;
    let features = match &gctx.cli_unstable().build_std_features {
        Some(list) => list.clone(),
        None => vec![
            "panic-unwind".to_string(),
            "backtrace".to_string(),
            "default".to_string(),
        ],
    };
    let cli_features = CliFeatures::from_command_line(
        &features, /*all_features*/ false, /*uses_default_features*/ false,
    )?;
    let dry_run = false;
    let resolve = ops::resolve_ws_with_opts(
        &std_ws,
        target_data,
        &build_config.requested_kinds,
        &cli_features,
        &specs,
        HasDevUnits::No,
        crate::core::resolver::features::ForceAllTargets::No,
        dry_run,
    )?;
    Ok((
        resolve.pkg_set,
        resolve.targeted_resolve,
        resolve.resolved_features,
    ))
}

/// Generates a map of root units for the standard library for each kind requested.
///
/// * `crates` is the arg value from `-Zbuild-std`.
/// * `units` is the root units of the build.
pub fn generate_std_roots(
    crates: &[String],
    units: &[Unit],
    std_resolve: &Resolve,
    std_features: &ResolvedFeatures,
    kinds: &[CompileKind],
    package_set: &PackageSet<'_>,
    interner: &UnitInterner,
    profiles: &Profiles,
    target_data: &RustcTargetData<'_>,
) -> CargoResult<HashMap<CompileKind, Vec<Unit>>> {
    let std_ids = std_crates(crates, Some(units))
        .iter()
        .map(|crate_name| std_resolve.query(crate_name))
        .collect::<CargoResult<Vec<PackageId>>>()?;
    // Convert PackageId to Package.
    let std_pkgs = package_set.get_many(std_ids)?;
    // Generate a map of Units for each kind requested.
    let mut ret = HashMap::new();
    for pkg in std_pkgs {
        let lib = pkg
            .targets()
            .iter()
            .find(|t| t.is_lib())
            .expect("std has a lib");
        // I don't think we need to bother with Check here, the difference
        // in time is minimal, and the difference in caching is
        // significant.
        let mode = CompileMode::Build;
        let features = std_features.activated_features(pkg.package_id(), FeaturesFor::NormalOrDev);
        for kind in kinds {
            let list = ret.entry(*kind).or_insert_with(Vec::new);
            let unit_for = UnitFor::new_normal(*kind);
            let profile = profiles.get_profile(
                pkg.package_id(),
                /*is_member*/ false,
                /*is_local*/ false,
                unit_for,
                *kind,
            );
            list.push(interner.intern(
                pkg,
                lib,
                profile,
                *kind,
                mode,
                features.clone(),
                target_data.info(*kind).rustflags.clone(),
                target_data.info(*kind).rustdocflags.clone(),
                target_data.target_config(*kind).links_overrides.clone(),
                /*is_std*/ true,
                /*dep_hash*/ 0,
                IsArtifact::No,
                None,
            ));
        }
    }
    Ok(ret)
}

fn detect_sysroot_src_path(target_data: &RustcTargetData<'_>) -> CargoResult<PathBuf> {
    if let Some(s) = target_data.gctx.get_env_os("__CARGO_TESTS_ONLY_SRC_ROOT") {
        return Ok(s.into());
    }

    // NOTE: This is temporary until we figure out how to acquire the source.
    let src_path = target_data
        .info(CompileKind::Host)
        .sysroot
        .join("lib")
        .join("rustlib")
        .join("src")
        .join("rust")
        .join("library");
    let lock = src_path.join("Cargo.lock");
    if !lock.exists() {
        let msg = format!(
            "{:?} does not exist, unable to build with the standard \
             library, try:\n        rustup component add rust-src",
            lock
        );
        match target_data.gctx.get_env("RUSTUP_TOOLCHAIN") {
            Ok(rustup_toolchain) => {
                anyhow::bail!("{} --toolchain {}", msg, rustup_toolchain);
            }
            Err(_) => {
                anyhow::bail!(msg);
            }
        }
    }
    Ok(src_path)
}
