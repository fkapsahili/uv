use std::fmt::Write;

use anstream::eprint;
use anyhow::Result;
use owo_colors::OwoColorize;
use tracing::debug;

use distribution_types::{IndexLocations, Resolution};
use install_wheel_rs::linker::LinkMode;
use uv_auth::store_credentials_from_url;
use uv_cache::Cache;
use uv_client::{BaseClientBuilder, Connectivity, FlatIndexClient, RegistryClientBuilder};
use uv_configuration::{
    BuildOptions, Concurrency, ConfigSettings, ExtrasSpecification, IndexStrategy, PreviewMode,
    Reinstall, SetupPyStrategy, Upgrade,
};
use uv_configuration::{KeyringProviderType, TargetTriple};
use uv_dispatch::BuildDispatch;
use uv_fs::Simplified;
use uv_git::GitResolver;
use uv_installer::SitePackages;
use uv_requirements::{RequirementsSource, RequirementsSpecification};
use uv_resolver::{
    DependencyMode, ExcludeNewer, FlatIndex, InMemoryIndex, OptionsBuilder, PreReleaseMode,
    PythonRequirement, ResolutionMode,
};
use uv_toolchain::{
    EnvironmentPreference, Prefix, PythonEnvironment, PythonVersion, Target, ToolchainRequest,
};
use uv_types::{BuildIsolation, HashStrategy, InFlight};

use crate::commands::pip::operations::Modifications;
use crate::commands::pip::{operations, resolution_environment};
use crate::commands::ExitStatus;
use crate::printer::Printer;

/// Install a set of locked requirements into the current Python environment.
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
pub(crate) async fn pip_sync(
    requirements: &[RequirementsSource],
    constraints: &[RequirementsSource],
    reinstall: Reinstall,
    link_mode: LinkMode,
    compile: bool,
    require_hashes: bool,
    index_locations: IndexLocations,
    index_strategy: IndexStrategy,
    keyring_provider: KeyringProviderType,
    setup_py: SetupPyStrategy,
    connectivity: Connectivity,
    config_settings: &ConfigSettings,
    no_build_isolation: bool,
    build_options: BuildOptions,
    python_version: Option<PythonVersion>,
    python_platform: Option<TargetTriple>,
    strict: bool,
    exclude_newer: Option<ExcludeNewer>,
    python: Option<String>,
    system: bool,
    break_system_packages: bool,
    target: Option<Target>,
    prefix: Option<Prefix>,
    concurrency: Concurrency,
    native_tls: bool,
    preview: PreviewMode,
    cache: Cache,
    dry_run: bool,
    printer: Printer,
) -> Result<ExitStatus> {
    let client_builder = BaseClientBuilder::new()
        .connectivity(connectivity)
        .native_tls(native_tls)
        .keyring(keyring_provider);

    // Initialize a few defaults.
    let overrides = &[];
    let extras = ExtrasSpecification::default();
    let upgrade = Upgrade::default();
    let resolution_mode = ResolutionMode::default();
    let prerelease_mode = PreReleaseMode::default();
    let dependency_mode = DependencyMode::Direct;

    // Read all requirements from the provided sources.
    let RequirementsSpecification {
        project,
        requirements,
        constraints,
        overrides,
        source_trees,
        index_url,
        extra_index_urls,
        no_index,
        find_links,
        no_binary,
        no_build,
        extras: _,
    } = operations::read_requirements(
        requirements,
        constraints,
        overrides,
        &extras,
        &client_builder,
    )
    .await?;

    // Validate that the requirements are non-empty.
    let num_requirements = requirements.len() + source_trees.len();
    if num_requirements == 0 {
        writeln!(printer.stderr(), "No requirements found")?;
        return Ok(ExitStatus::Success);
    }

    // Detect the current Python interpreter.
    let environment = PythonEnvironment::find(
        &python
            .as_deref()
            .map(ToolchainRequest::parse)
            .unwrap_or_default(),
        EnvironmentPreference::from_system_flag(system, true),
        &cache,
    )?;

    debug!(
        "Using Python {} environment at {}",
        environment.interpreter().python_version(),
        environment.python_executable().user_display().cyan()
    );

    // Apply any `--target` or `--prefix` directories.
    let environment = if let Some(target) = target {
        debug!(
            "Using `--target` directory at {}",
            target.root().user_display()
        );
        target.init()?;
        environment.with_target(target)
    } else if let Some(prefix) = prefix {
        debug!(
            "Using `--prefix` directory at {}",
            prefix.root().user_display()
        );
        prefix.init()?;
        environment.with_prefix(prefix)
    } else {
        environment
    };

    // If the environment is externally managed, abort.
    if let Some(externally_managed) = environment.interpreter().is_externally_managed() {
        if break_system_packages {
            debug!("Ignoring externally managed environment due to `--break-system-packages`");
        } else {
            return if let Some(error) = externally_managed.into_error() {
                Err(anyhow::anyhow!(
                    "The interpreter at {} is externally managed, and indicates the following:\n\n{}\n\nConsider creating a virtual environment with `uv venv`.",
                    environment.root().user_display().cyan(),
                    textwrap::indent(&error, "  ").green(),
                ))
            } else {
                Err(anyhow::anyhow!(
                    "The interpreter at {} is externally managed. Instead, create a virtual environment with `uv venv`.",
                    environment.root().user_display().cyan()
                ))
            };
        }
    }

    let _lock = environment.lock()?;

    let interpreter = environment.interpreter();

    // Determine the Python requirement, if the user requested a specific version.
    let python_requirement = if let Some(python_version) = python_version.as_ref() {
        PythonRequirement::from_python_version(interpreter, python_version)
    } else {
        PythonRequirement::from_interpreter(interpreter)
    };

    // Determine the environment for the resolution.
    let (tags, markers) = resolution_environment(python_version, python_platform, interpreter)?;

    // Collect the set of required hashes.
    let hasher = if require_hashes {
        HashStrategy::from_requirements(
            requirements
                .iter()
                .map(|entry| (&entry.requirement, entry.hashes.as_slice())),
            Some(&markers),
        )?
    } else {
        HashStrategy::None
    };

    // Incorporate any index locations from the provided sources.
    let index_locations =
        index_locations.combine(index_url, extra_index_urls, find_links, no_index);

    // Add all authenticated sources to the cache.
    for url in index_locations.urls() {
        store_credentials_from_url(url);
    }

    // Initialize the registry client.
    let client = RegistryClientBuilder::new(cache.clone())
        .native_tls(native_tls)
        .connectivity(connectivity)
        .index_urls(index_locations.index_urls())
        .index_strategy(index_strategy)
        .keyring(keyring_provider)
        .markers(&markers)
        .platform(interpreter.platform())
        .build();

    // Combine the `--no-binary` and `--no-build` flags from the requirements files.
    let build_options = build_options.combine(no_binary, no_build);

    // Resolve the flat indexes from `--find-links`.
    let flat_index = {
        let client = FlatIndexClient::new(&client, &cache);
        let entries = client.fetch(index_locations.flat_index()).await?;
        FlatIndex::from_entries(entries, Some(&tags), &hasher, &build_options)
    };

    // Determine whether to enable build isolation.
    let build_isolation = if no_build_isolation {
        BuildIsolation::Shared(&environment)
    } else {
        BuildIsolation::Isolated
    };

    // Create a shared in-memory index.
    let index = InMemoryIndex::default();

    // Track in-flight downloads, builds, etc., across resolutions.
    let in_flight = InFlight::default();

    // When resolving, don't take any external preferences into account.
    let preferences = Vec::default();
    let git = GitResolver::default();

    // Ignore development dependencies.
    let dev = Vec::default();

    // Create a build dispatch for resolution.
    let resolve_dispatch = BuildDispatch::new(
        &client,
        &cache,
        interpreter,
        &index_locations,
        &flat_index,
        &index,
        &git,
        &in_flight,
        index_strategy,
        setup_py,
        config_settings,
        build_isolation,
        link_mode,
        &build_options,
        exclude_newer,
        concurrency,
        preview,
    );

    // Determine the set of installed packages.
    let site_packages = SitePackages::from_environment(&environment)?;

    let options = OptionsBuilder::new()
        .resolution_mode(resolution_mode)
        .prerelease_mode(prerelease_mode)
        .dependency_mode(dependency_mode)
        .exclude_newer(exclude_newer)
        .index_strategy(index_strategy)
        .build();

    let resolution = match operations::resolve(
        requirements,
        constraints,
        overrides,
        dev,
        source_trees,
        project,
        &extras,
        preferences,
        site_packages.clone(),
        &hasher,
        &reinstall,
        &upgrade,
        Some(&tags),
        Some(&markers),
        python_requirement,
        &client,
        &flat_index,
        &index,
        &resolve_dispatch,
        concurrency,
        options,
        printer,
        preview,
    )
    .await
    {
        Ok(resolution) => Resolution::from(resolution),
        Err(operations::Error::Resolve(uv_resolver::ResolveError::NoSolution(err))) => {
            let report = miette::Report::msg(format!("{err}"))
                .context("No solution found when resolving dependencies:");
            eprint!("{report:?}");
            return Ok(ExitStatus::Failure);
        }
        Err(err) => return Err(err.into()),
    };

    // Re-initialize the in-flight map.
    let in_flight = InFlight::default();

    // If we're running with `--reinstall`, initialize a separate `BuildDispatch`, since we may
    // end up removing some distributions from the environment.
    let install_dispatch = if reinstall.is_none() {
        resolve_dispatch
    } else {
        BuildDispatch::new(
            &client,
            &cache,
            interpreter,
            &index_locations,
            &flat_index,
            &index,
            &git,
            &in_flight,
            index_strategy,
            setup_py,
            config_settings,
            build_isolation,
            link_mode,
            &build_options,
            exclude_newer,
            concurrency,
            preview,
        )
    };

    // Sync the environment.
    operations::install(
        &resolution,
        site_packages,
        Modifications::Exact,
        &reinstall,
        &build_options,
        link_mode,
        compile,
        &index_locations,
        &hasher,
        &tags,
        &client,
        &in_flight,
        concurrency,
        &install_dispatch,
        &cache,
        &environment,
        dry_run,
        printer,
        preview,
    )
    .await?;

    // Notify the user of any resolution diagnostics.
    operations::diagnose_resolution(resolution.diagnostics(), printer)?;

    // Notify the user of any environment diagnostics.
    if strict && !dry_run {
        operations::diagnose_environment(&resolution, &environment, printer)?;
    }

    Ok(ExitStatus::Success)
}
