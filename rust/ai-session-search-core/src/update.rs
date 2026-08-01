// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::cli::ReportOutputFormat;
use crate::config::Config;
use crate::durable_fs::{atomic_write_file, AtomicWriteMode};
use crate::util::{executable_candidates, is_executable_file, render_posix_shell_command};

const PACKAGE_NAME: &str = "ai-session-search";
#[cfg(feature = "release-check")]
const LATEST_STABLE_RELEASE_API_URL: &str =
    "https://api.github.com/repos/ahundt/ai-session-search/releases/latest";
#[cfg(feature = "release-check")]
const RELEASE_LIST_API_URL: &str =
    "https://api.github.com/repos/ahundt/ai-session-search/releases?per_page=100";
const MAX_RELEASE_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;
const RELEASE_CACHE_FILE_NAME: &str = "release-check.json";
const REQUESTED_RELEASE_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CARGO_INSTALL_METADATA_BYTES: u64 = 1024 * 1024;
const CRATES_IO_CARGO_SOURCE: &str = "(registry+https://github.com/rust-lang/crates.io-index)";
const RELEASE_CACHE_SCHEMA_VERSION: u32 = 3;
const SECONDS_PER_HOUR: u64 = 60 * 60;
const CLOCK_SKEW_TOLERANCE_SECONDS: u64 = 5 * 60;
const PASSIVE_FAILURE_RETRY_INTERVAL_HOURS: u64 = 1;

const ENV_INSTALLER: &str = "AI_SESSION_SEARCH_PYTHON_INSTALLER";
const ENV_INVOKED_EXECUTABLE: &str = "AI_SESSION_SEARCH_INVOKED_EXECUTABLE";
const ENV_PYTHON: &str = "AI_SESSION_SEARCH_PYTHON_EXECUTABLE";
const ENV_PREFIX: &str = "AI_SESSION_SEARCH_PYTHON_PREFIX";
const ENV_UV_RECEIPT: &str = "AI_SESSION_SEARCH_UV_TOOL_RECEIPT";
const ENV_PIPX_METADATA: &str = "AI_SESSION_SEARCH_PIPX_METADATA";
const ENV_DIRECT_URL: &str = "AI_SESSION_SEARCH_DIRECT_URL";
const ENV_SKIP_RELEASE_NOTIFICATION: &str = "AI_SESSION_SEARCH_SKIP_RELEASE_NOTIFICATION";

#[derive(Debug, Clone, PartialEq, Eq)]
struct InstallEvidence {
    pub executable: PathBuf,
    pub invoked_executable: Option<PathBuf>,
    pub python_installer: Option<String>,
    pub python_executable: Option<PathBuf>,
    pub python_prefix: Option<PathBuf>,
    pub uv_tool_receipt: Option<PathBuf>,
    pub pipx_metadata: Option<PathBuf>,
    pub direct_url: Option<String>,
}

impl InstallEvidence {
    fn capture() -> Result<Self> {
        Ok(Self {
            executable: std::env::current_exe()
                .context("could not resolve the running executable")?,
            invoked_executable: nonempty_env_path(ENV_INVOKED_EXECUTABLE),
            python_installer: nonempty_env(ENV_INSTALLER),
            python_executable: nonempty_env_path(ENV_PYTHON),
            python_prefix: nonempty_env_path(ENV_PREFIX),
            uv_tool_receipt: nonempty_env_path(ENV_UV_RECEIPT),
            pipx_metadata: nonempty_env_path(ENV_PIPX_METADATA),
            direct_url: nonempty_env(ENV_DIRECT_URL),
        })
    }
}

/// Resolve the same command surface for a detached child across native and Python installations.
///
/// A native `aise` process can re-execute `current_exe()`. Under a PyO3 console script,
/// `current_exe()` is the Python interpreter, so the entrypoint publishes both that runtime and the
/// exact invoked `aise` script. We trust the override only when the published Python path identifies
/// this process and the invoked path is an absolute executable file; otherwise native execution
/// retains its zero-discovery path. Time and retained memory are `O(1)`.
pub(crate) fn background_child_executable() -> Result<PathBuf> {
    background_child_executable_from_evidence(&InstallEvidence::capture()?)
}

fn background_child_executable_from_evidence(evidence: &InstallEvidence) -> Result<PathBuf> {
    let Some(python_runtime) = evidence
        .python_executable
        .as_deref()
        .filter(|python_runtime| paths_identify_same_file(&evidence.executable, python_runtime))
    else {
        return Ok(evidence.executable.clone());
    };
    let invoked = evidence.invoked_executable.as_ref().with_context(|| {
        format!(
            "Python runtime {} did not identify the invoked aise console script; run the installed \
             `aise` command instead of calling its Python entrypoint directly",
            python_runtime.display()
        )
    })?;
    if !invoked.is_absolute() || !is_executable_file(invoked) {
        bail!(
            "Python runtime {} reported an invalid invoked aise executable at {}; reinstall the \
             console script with its owning package manager",
            python_runtime.display(),
            invoked.display()
        );
    }
    Ok(invoked.clone())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum ExecutableOwner {
    UvTool,
    UvPip,
    Pip,
    Pipx,
    Cargo,
    Homebrew,
    DirectSource,
    Unknown,
}

impl ExecutableOwner {
    fn display_name(self) -> &'static str {
        match self {
            Self::UvTool => "uv tool",
            Self::UvPip => "uv pip",
            Self::Pip => "pip",
            Self::Pipx => "pipx",
            Self::Cargo => "Cargo",
            Self::Homebrew => "Homebrew",
            Self::DirectSource => "direct source",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum ExecutableUpdateAction {
    InvokePackageManager {
        argv: Vec<String>,
        environment: BTreeMap<String, String>,
    },
    Guidance {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ExecutableUpdatePlan {
    owner: ExecutableOwner,
    ownership_evidence: String,
    action: ExecutableUpdateAction,
}

#[cfg(any(feature = "release-check", test))]
#[derive(Debug, Clone, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    draft: bool,
    prerelease: bool,
}

#[derive(Debug, Deserialize)]
struct UvReceipt {
    tool: UvReceiptTool,
}

#[derive(Debug, Deserialize)]
struct UvReceiptTool {
    #[serde(default)]
    requirements: Vec<UvReceiptRequirement>,
    #[serde(default)]
    entrypoints: Vec<UvReceiptEntrypoint>,
}

#[derive(Debug, Deserialize)]
struct UvReceiptRequirement {
    name: String,
}

#[derive(Debug, Deserialize)]
struct UvReceiptEntrypoint {
    name: String,
    from: String,
    #[serde(rename = "install-path")]
    install_path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct PipxMetadata {
    main_package: PipxMainPackage,
}

#[derive(Debug, Deserialize)]
struct PipxMainPackage {
    package: String,
    #[serde(default)]
    apps: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CargoInstallMetadata {
    installs: std::collections::BTreeMap<String, CargoInstallRecord>,
}

#[derive(Debug, Deserialize)]
struct CargoInstallRecord {
    bins: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CargoInstallSource {
    Registry,
    DirectSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReleaseCache {
    schema_version: u32,
    checked_at_unix_seconds: u64,
    latest_version: Option<String>,
    release_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleaseStatus {
    pub current: Version,
    pub latest: Version,
    pub release_url: String,
}

#[derive(Debug, Serialize)]
struct PackageStatusReport {
    runtime_process_executable: PathBuf,
    invoked_command_executable: Option<PathBuf>,
    first_aise_on_path: Option<PathBuf>,
    all_aise_on_path: Vec<PathBuf>,
    installation_owner: ExecutableOwner,
    ownership_evidence: String,
    update_action: ExecutableUpdateAction,
    automatic_apply_supported_on_this_platform: bool,
}

#[derive(Debug, Serialize)]
struct PackageCheckReport {
    package: PackageStatusReport,
    current_version: String,
    latest_release_version: String,
    release_url: String,
    newer_release_available: bool,
    current_build_is_newer_than_latest_release: bool,
}

impl ReleaseStatus {
    fn update_available(&self) -> bool {
        self.latest > self.current
    }
}

fn detect_executable_owner(evidence: &InstallEvidence) -> ExecutableOwner {
    let python_runtime_is_bound = python_executable_belongs_to_prefix(evidence);
    if evidence.direct_url.is_some() && python_runtime_is_bound {
        return ExecutableOwner::DirectSource;
    }

    match evidence.python_installer.as_deref() {
        Some("uv")
            if python_runtime_is_bound
                && evidence
                    .uv_tool_receipt
                    .as_deref()
                    .is_some_and(is_regular_file_without_symlink)
                && uv_receipt_belongs_to_python_prefix(evidence) =>
        {
            return ExecutableOwner::UvTool;
        }
        Some("uv") if evidence.uv_tool_receipt.is_some() => {}
        Some("uv") if python_runtime_is_bound => {
            return ExecutableOwner::UvPip;
        }
        Some("pip")
            if python_runtime_is_bound
                && evidence
                    .pipx_metadata
                    .as_deref()
                    .is_some_and(is_regular_file_without_symlink)
                && pipx_metadata_belongs_to_python_prefix(evidence) =>
        {
            return ExecutableOwner::Pipx;
        }
        Some("pip") if evidence.pipx_metadata.is_some() => {}
        Some("pip") if python_runtime_is_bound => {
            return ExecutableOwner::Pip;
        }
        _ => {}
    }

    if is_homebrew_executable(&evidence.executable) {
        return ExecutableOwner::Homebrew;
    }
    match cargo_install_source(&evidence.executable) {
        Some(CargoInstallSource::Registry) => return ExecutableOwner::Cargo,
        Some(CargoInstallSource::DirectSource) => return ExecutableOwner::DirectSource,
        None => {}
    }
    ExecutableOwner::Unknown
}

fn is_regular_file_without_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

fn plan_package_manager_update(evidence: &InstallEvidence) -> Result<ExecutableUpdatePlan> {
    let owner = detect_executable_owner(evidence);
    let (description, action) = match owner {
        ExecutableOwner::UvTool => (
            format!(
                "{} identifies uv and {} is a uv tool receipt",
                ENV_INSTALLER,
                evidence
                    .uv_tool_receipt
                    .as_deref()
                    .unwrap_or_else(|| Path::new("<missing>"))
                    .display()
            ),
            manager_action_with_environment(
                strings(["uv", "tool", "upgrade", PACKAGE_NAME]),
                uv_tool_update_environment(evidence),
            ),
        ),
        ExecutableOwner::UvPip => {
            let python = evidence
                .python_executable
                .as_deref()
                .context("uv pip ownership requires the exact Python executable")?
                .to_string_lossy()
                .into_owned();
            (
                format!("{ENV_INSTALLER} identifies uv without a uv tool receipt"),
                manager_action(vec![
                        "uv".into(),
                        "pip".into(),
                        "install".into(),
                        "--python".into(),
                        python,
                        "--upgrade".into(),
                        PACKAGE_NAME.into(),
                    ]),
            )
        }
        ExecutableOwner::Pip => {
            let python = evidence
                .python_executable
                .as_deref()
                .context("pip ownership requires the exact Python executable")?
                .to_string_lossy()
                .into_owned();
            (
                format!("{ENV_INSTALLER} identifies pip"),
                manager_action(vec![
                        python,
                        "-m".into(),
                        "pip".into(),
                        "install".into(),
                        "--upgrade".into(),
                        PACKAGE_NAME.into(),
                    ]),
            )
        }
        ExecutableOwner::Pipx => (
            "pip installation metadata includes an environment-bound pipx metadata file".into(),
            manager_action_with_environment(
                strings(["pipx", "upgrade", PACKAGE_NAME]),
                pipx_update_environment(evidence),
            ),
        ),
        ExecutableOwner::Cargo => (
            "the executable is in a Cargo installation root tracked by .crates2.json".into(),
            manager_action(cargo_update_argv(evidence)),
        ),
        ExecutableOwner::Homebrew => (
            "the resolved executable is inside a Homebrew Cellar".into(),
            manager_action(strings([
                &homebrew_executable(evidence)
                    .unwrap_or_else(|| PathBuf::from("brew"))
                    .to_string_lossy(),
                "upgrade",
                PACKAGE_NAME,
            ])),
        ),
        ExecutableOwner::DirectSource => (
            if evidence.direct_url.is_some() {
                "Python installation metadata records a direct URL or local source".into()
            } else {
                "Cargo installation metadata records a path or Git source".into()
            },
            ExecutableUpdateAction::Guidance {
                message: "This is a developer/source-managed installation. Update its checkout or recorded source, then reinstall with the same tool; aise will not replace it with a registry package."
                    .into(),
            },
        ),
        ExecutableOwner::Unknown => (
            "no authoritative package-manager or executable-bound native receipt was found".into(),
            ExecutableUpdateAction::Guidance {
                message: "Use the displayed runtime process, any invoked command, and PATH candidates to identify the installer, then update with that manager. Unknown executables are never overwritten."
                    .into(),
            },
        ),
    };
    Ok(ExecutableUpdatePlan {
        owner,
        ownership_evidence: description,
        action,
    })
}

pub(crate) fn print_package_status(format: ReportOutputFormat) -> Result<()> {
    let report = package_status_report()?;
    print_package_report(&report, format)
}

pub(crate) fn run_package_check(config: &Config, format: ReportOutputFormat) -> Result<()> {
    let report = package_check_report(config)?;
    match format {
        ReportOutputFormat::Table => print_package_check_report(&report),
        ReportOutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
    }
}

pub(crate) fn run_package_update(config: &Config, skip_confirmation: bool) -> Result<()> {
    let report = package_check_report(config)?;
    print_package_check_report(&report)?;
    if !report.newer_release_available {
        return Ok(());
    }
    let ExecutableUpdateAction::InvokePackageManager { argv, environment } =
        &report.package.update_action
    else {
        return Ok(());
    };
    if !report.package.automatic_apply_supported_on_this_platform {
        println!("{}", manual_update_guidance(argv, environment)?);
        return Ok(());
    }
    if !skip_confirmation
        && !interactive_confirmation_supported(
            io::stdin().is_terminal(),
            io::stderr().is_terminal(),
        )
    {
        bail!(
            "interactive confirmation requires terminal stdin and stderr; rerun in a terminal or pass `aise package update --yes`"
        );
    }
    if !skip_confirmation && !prompt_package_manager_update()? {
        println!("update cancelled");
        return Ok(());
    }
    execute_update_command(argv, environment)
}

fn package_check_report(config: &Config) -> Result<PackageCheckReport> {
    let mut package = package_status_report()?;
    let status = fetch_latest_release(REQUESTED_RELEASE_CHECK_TIMEOUT)
        .context("failed to check the applicable release channel")?;
    let newer_release_available = status.update_available();
    let current_build_is_newer_than_latest_release = status.current > status.latest;
    if newer_release_available {
        package.update_action = update_action_for_release(
            package.installation_owner,
            package.update_action,
            &status.current,
            &status.latest,
        )?;
        package.automatic_apply_supported_on_this_platform =
            automatic_package_manager_apply_supported(&package.update_action, cfg!(windows));
    }
    if let Ok(cache) = cache_from_status(&status) {
        let _ = write_cache(config, &cache);
    }
    Ok(PackageCheckReport {
        package,
        current_version: status.current.to_string(),
        latest_release_version: status.latest.to_string(),
        release_url: status.release_url,
        newer_release_available,
        current_build_is_newer_than_latest_release,
    })
}

fn update_action_for_release(
    owner: ExecutableOwner,
    action: ExecutableUpdateAction,
    current: &Version,
    latest: &Version,
) -> Result<ExecutableUpdateAction> {
    if current.pre.is_empty() {
        return Ok(action);
    }
    let python_spec = format!("{PACKAGE_NAME}=={}", python_version(latest)?);
    match (owner, action) {
        (
            ExecutableOwner::UvTool,
            ExecutableUpdateAction::InvokePackageManager {
                environment, ..
            },
        ) => Ok(manager_action_with_environment(
            strings(["uv", "tool", "upgrade", &python_spec]),
            environment,
        )),
        (
            ExecutableOwner::UvPip | ExecutableOwner::Pip,
            ExecutableUpdateAction::InvokePackageManager {
                mut argv,
                environment,
            },
        ) => {
            let package = argv
                .last_mut()
                .context("Python manager command is missing its package requirement")?;
            *package = python_spec;
            Ok(manager_action_with_environment(argv, environment))
        }
        (
            ExecutableOwner::Cargo,
            ExecutableUpdateAction::InvokePackageManager {
                mut argv,
                environment,
            },
        ) => {
            argv.extend(["--version".into(), latest.to_string()]);
            Ok(manager_action_with_environment(argv, environment))
        }
        (ExecutableOwner::Pipx, _) => Ok(ExecutableUpdateAction::Guidance {
            message: format!(
                "pipx preserves recorded environment settings, so aise will not replace an RC constraint automatically. Review `pipx install {python_spec} --force`, then run `aise package status`."
            ),
        }),
        (ExecutableOwner::Homebrew, _) => Ok(ExecutableUpdateAction::Guidance {
            message:
                "Homebrew formulae are stable-channel packages. Install this prerelease through uv, pip, Cargo, or its verified native archive, or wait for the stable formula."
                    .into(),
        }),
        (_, action) => Ok(action),
    }
}

fn python_version(version: &Version) -> Result<String> {
    if version.pre.is_empty() {
        return Ok(format!(
            "{}.{}.{}",
            version.major, version.minor, version.patch
        ));
    }
    let prerelease = version.pre.as_str();
    let (phase, number) = prerelease
        .split_once('.')
        .context("release prerelease must use a named phase and numeric component")?;
    let phase = match phase {
        "alpha" => "a",
        "beta" => "b",
        "rc" => "rc",
        _ => bail!("unsupported release prerelease phase {phase:?}"),
    };
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("release prerelease number is invalid");
    }
    Ok(format!(
        "{}.{}.{}{phase}{number}",
        version.major, version.minor, version.patch
    ))
}

fn release_relation_summary(
    newer_release_available: bool,
    current_build_is_newer_than_latest_release: bool,
) -> Option<&'static str> {
    if newer_release_available {
        None
    } else if current_build_is_newer_than_latest_release {
        Some("This build is newer than the latest applicable release.")
    } else {
        Some("This build matches the latest applicable release.")
    }
}

pub(crate) fn notify_if_new_stable_release_available_after_cli_output(
    config: &Config,
    skip_for_invocation: bool,
) {
    if skip_for_invocation
        || !config.release_notifications.enabled
        || env_truthy(ENV_SKIP_RELEASE_NOTIFICATION)
        || !io::stderr().is_terminal()
        || !io::stdout().is_terminal()
    {
        return;
    }
    let now = unix_seconds().ok();
    let cached = read_cache(config).ok().flatten();
    if let (Some(now), Some(cache)) = (now, cached.as_ref()) {
        let retry_interval_hours = if cache.latest_version.is_none() {
            config
                .release_notifications
                .minimum_check_interval_hours
                .min(PASSIVE_FAILURE_RETRY_INTERVAL_HOURS)
        } else {
            config.release_notifications.minimum_check_interval_hours
        };
        if release_cache_is_fresh(cache, now, retry_interval_hours) {
            print_cached_notice(cache);
            return;
        }
    }

    let timeout = Duration::from_millis(config.release_notifications.request_timeout_ms);
    let cache = match fetch_latest_release(timeout) {
        Ok(status) => cache_from_status(&status),
        // Cache an unavailable passive attempt too, so offline users do not pay the
        // configured timeout on every command. Explicit `package check` bypasses this cache.
        Err(_) => cache_without_release(),
    };
    if let Ok(cache) = cache {
        let _ = write_cache(config, &cache);
        print_cached_notice(&cache);
    }
}

fn package_status_report() -> Result<PackageStatusReport> {
    let evidence = InstallEvidence::capture()?;
    let runtime_process_executable = evidence.executable.clone();
    let invoked_command_executable = evidence
        .invoked_executable
        .clone()
        .filter(|path| path != &runtime_process_executable);
    let plan = plan_package_manager_update(&evidence)?;
    let all_aise_on_path = executable_candidates("aise");
    Ok(PackageStatusReport {
        runtime_process_executable,
        invoked_command_executable,
        first_aise_on_path: all_aise_on_path.first().cloned(),
        all_aise_on_path,
        installation_owner: plan.owner,
        ownership_evidence: plan.ownership_evidence,
        automatic_apply_supported_on_this_platform: automatic_package_manager_apply_supported(
            &plan.action,
            cfg!(windows),
        ),
        update_action: plan.action,
    })
}

fn automatic_package_manager_apply_supported(
    action: &ExecutableUpdateAction,
    running_executable_is_locked_by_platform: bool,
) -> bool {
    matches!(action, ExecutableUpdateAction::InvokePackageManager { .. })
        && !running_executable_is_locked_by_platform
}

fn print_package_report(report: &PackageStatusReport, format: ReportOutputFormat) -> Result<()> {
    if format == ReportOutputFormat::Json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }
    println!(
        "Runtime process executable: {}",
        report.runtime_process_executable.display()
    );
    if let Some(path) = &report.invoked_command_executable {
        println!("Invoked command executable: {}", path.display());
    }
    println!(
        "First aise on PATH: {}",
        report
            .first_aise_on_path
            .as_deref()
            .map_or_else(|| "not found".to_owned(), |path| path.display().to_string())
    );
    println!(
        "All aise on PATH: {}",
        if report.all_aise_on_path.is_empty() {
            "not found".to_owned()
        } else {
            report
                .all_aise_on_path
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    if report.all_aise_on_path.len() > 1 {
        println!(
            "Warning: multiple aise executables are on PATH; the first candidate wins. Keep one global package owner or remove stale candidates."
        );
    }
    println!(
        "Installation owner: {}",
        report.installation_owner.display_name()
    );
    println!("Ownership evidence: {}", report.ownership_evidence);
    match &report.update_action {
        ExecutableUpdateAction::InvokePackageManager { argv, environment } => {
            println!(
                "Manager update command: {}",
                render_update_command(argv, environment)?
            );
        }
        ExecutableUpdateAction::Guidance { message } => {
            println!("Update guidance: {message}");
        }
    }
    Ok(())
}

fn print_package_check_report(report: &PackageCheckReport) -> Result<()> {
    println!("Current version: {}", report.current_version);
    println!(
        "Latest applicable version: {}",
        report.latest_release_version
    );
    println!("Release: {}", report.release_url);
    print_package_report(&report.package, ReportOutputFormat::Table)?;
    if let Some(summary) = release_relation_summary(
        report.newer_release_available,
        report.current_build_is_newer_than_latest_release,
    ) {
        println!("{summary}");
    }
    Ok(())
}

fn prompt_package_manager_update() -> Result<bool> {
    eprint!("Apply this update with the detected package manager? [Y/n]: ");
    io::stderr().flush()?;
    let mut input = String::new();
    if io::stdin().read_line(&mut input)? == 0 {
        return Ok(false);
    }
    Ok(confirmation_answer_applies(Some(&input)))
}

fn interactive_confirmation_supported(
    stdin_is_terminal: bool,
    prompt_stream_is_terminal: bool,
) -> bool {
    stdin_is_terminal && prompt_stream_is_terminal
}

fn confirmation_answer_applies(answer: Option<&str>) -> bool {
    let Some(answer) = answer else {
        return false;
    };
    let answer = answer.trim().to_ascii_lowercase();
    answer.is_empty() || matches!(answer.as_str(), "y" | "yes")
}

fn execute_update_command(argv: &[String], environment: &BTreeMap<String, String>) -> Result<()> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow!("update command must not be empty"))?;
    let rendered = render_command(argv)?;
    let status = Command::new(program)
        .args(args)
        .envs(environment)
        .status()
        .with_context(|| format!("could not execute update command `{rendered}`"))?;
    if !status.success() {
        bail!(
            "update command `{rendered}` exited with {status}; the detected package manager remains the owner, so fix its reported error and rerun the same command"
        );
    }
    println!("Update command completed. Run `aise --version` to verify the active executable.");
    Ok(())
}

fn fetch_latest_release(request_timeout: Duration) -> Result<ReleaseStatus> {
    #[cfg(not(feature = "release-check"))]
    {
        let _ = request_timeout;
        bail!(
            "this build excludes release network checks; rebuild with the \
             `release-check` Cargo feature or update through the installation owner"
        );
    }

    #[cfg(feature = "release-check")]
    {
        use ureq::tls::{RootCerts, TlsConfig};

        let agent = ureq::Agent::config_builder()
            .https_only(true)
            .timeout_global(Some(request_timeout))
            .user_agent(format!("aise/{}", env!("CARGO_PKG_VERSION")))
            .tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent();
        let current = Version::parse(env!("CARGO_PKG_VERSION"))
            .context("the compiled package version is not valid Cargo SemVer")?;
        let endpoint = if current.pre.is_empty() {
            LATEST_STABLE_RELEASE_API_URL
        } else {
            RELEASE_LIST_API_URL
        };
        let mut response = agent
            .get(endpoint)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .call()
            .context("GitHub release request failed")?;
        if current.pre.is_empty() {
            let release: GitHubRelease = response
                .body_mut()
                .with_config()
                .limit(MAX_RELEASE_RESPONSE_BYTES)
                .read_json()
                .with_context(|| {
                    format!(
                        "GitHub stable-release response was invalid JSON or exceeded {} bytes",
                        MAX_RELEASE_RESPONSE_BYTES
                    )
                })?;
            stable_release_status_from_response(current, release)
        } else {
            let releases: Vec<GitHubRelease> = response
                .body_mut()
                .with_config()
                .limit(MAX_RELEASE_RESPONSE_BYTES)
                .read_json()
                .with_context(|| {
                    format!(
                        "GitHub release-list response was invalid JSON or exceeded {} bytes",
                        MAX_RELEASE_RESPONSE_BYTES
                    )
                })?;
            applicable_release_status_from_responses(current, releases)
        }
    }
}

#[cfg(test)]
fn release_status_from_response(release: GitHubRelease) -> Result<ReleaseStatus> {
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .context("the compiled package version is not valid Cargo SemVer")?;
    stable_release_status_from_response(current, release)
}

#[cfg(any(feature = "release-check", test))]
fn stable_release_status_from_response(
    current: Version,
    release: GitHubRelease,
) -> Result<ReleaseStatus> {
    if release.draft || release.prerelease {
        bail!("GitHub latest release response was not a completed stable release");
    }
    let latest = parse_release_tag(&release.tag_name)?;
    validate_release_url(&release)?;
    Ok(ReleaseStatus {
        current,
        latest,
        release_url: release.html_url,
    })
}

#[cfg(any(feature = "release-check", test))]
fn applicable_release_status_from_responses(
    current: Version,
    releases: Vec<GitHubRelease>,
) -> Result<ReleaseStatus> {
    let current_is_prerelease = !current.pre.is_empty();
    let selected = releases
        .into_iter()
        .filter(|release| !release.draft)
        .filter_map(|release| {
            let version = parse_release_tag(&release.tag_name).ok()?;
            let version_is_prerelease = !version.pre.is_empty();
            if release.prerelease != version_is_prerelease {
                return None;
            }
            if version_is_prerelease && python_version(&version).is_err() {
                return None;
            }
            let same_release_train = version.major == current.major
                && version.minor == current.minor
                && version.patch == current.patch;
            (!version_is_prerelease || (current_is_prerelease && same_release_train))
                .then_some((version, release))
        })
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .context("GitHub returned no stable release or same-train prerelease")?;
    let (latest, release) = selected;
    validate_release_url(&release)?;
    Ok(ReleaseStatus {
        current,
        latest,
        release_url: release.html_url,
    })
}

#[cfg(any(feature = "release-check", test))]
fn validate_release_url(release: &GitHubRelease) -> Result<()> {
    let expected_release_url = format!(
        "https://github.com/ahundt/ai-session-search/releases/tag/{}",
        release.tag_name
    );
    if release.html_url != expected_release_url {
        bail!("GitHub latest release response contained an unexpected release URL");
    }
    Ok(())
}

#[cfg(any(feature = "release-check", test))]
fn parse_release_tag(tag: &str) -> Result<Version> {
    let value = tag.strip_prefix('v').unwrap_or(tag);
    if let Ok(version) = Version::parse(value) {
        return Ok(version);
    }
    let phase_start = value
        .char_indices()
        .find_map(|(index, character)| matches!(character, 'a' | 'b' | 'r').then_some(index))
        .context("release tag is neither Cargo SemVer nor supported Python release spelling")?;
    let (release, suffix) = value.split_at(phase_start);
    let (phase, number) = if let Some(number) = suffix.strip_prefix("rc") {
        ("rc", number)
    } else if let Some(number) = suffix.strip_prefix('a') {
        ("alpha", number)
    } else if let Some(number) = suffix.strip_prefix('b') {
        ("beta", number)
    } else {
        bail!("unsupported Python release phase in tag {tag:?}");
    };
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("release tag {tag:?} has an invalid pre-release number");
    }
    Version::parse(&format!("{release}-{phase}.{number}"))
        .with_context(|| format!("release tag {tag:?} is not a supported version"))
}

fn cache_from_status(status: &ReleaseStatus) -> Result<ReleaseCache> {
    Ok(ReleaseCache {
        schema_version: RELEASE_CACHE_SCHEMA_VERSION,
        checked_at_unix_seconds: unix_seconds()?,
        latest_version: Some(status.latest.to_string()),
        release_url: Some(status.release_url.clone()),
    })
}

fn cache_without_release() -> Result<ReleaseCache> {
    Ok(ReleaseCache {
        schema_version: RELEASE_CACHE_SCHEMA_VERSION,
        checked_at_unix_seconds: unix_seconds()?,
        latest_version: None,
        release_url: None,
    })
}

fn cache_path(config: &Config) -> PathBuf {
    config.cache_dir().join(RELEASE_CACHE_FILE_NAME)
}

fn read_cache(config: &Config) -> Result<Option<ReleaseCache>> {
    let path = cache_path(config);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Ok(None)
        }
        Ok(metadata) if metadata.len() > MAX_RELEASE_RESPONSE_BYTES => return Ok(None),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect {}", path.display()))
        }
    }
    let bytes = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let cache: ReleaseCache = match serde_json::from_slice(&bytes) {
        Ok(cache) => cache,
        Err(_) => return Ok(None),
    };
    let release_is_valid = match (&cache.latest_version, &cache.release_url) {
        (None, None) => true,
        (Some(version), Some(url)) => {
            Version::parse(version).is_ok()
                && url.starts_with("https://github.com/ahundt/ai-session-search/releases/tag/")
        }
        _ => false,
    };
    if cache.schema_version != RELEASE_CACHE_SCHEMA_VERSION || !release_is_valid {
        return Ok(None);
    }
    Ok(Some(cache))
}

fn write_cache(config: &Config, cache: &ReleaseCache) -> Result<()> {
    let bytes = serde_json::to_vec(cache)?;
    atomic_write_file(&cache_path(config), &bytes, AtomicWriteMode::Replace)
}

fn release_cache_is_fresh(
    cache: &ReleaseCache,
    now_unix_seconds: u64,
    minimum_check_interval_hours: u64,
) -> bool {
    let interval_seconds = u128::from(minimum_check_interval_hours) * u128::from(SECONDS_PER_HOUR);
    cache.checked_at_unix_seconds <= now_unix_seconds.saturating_add(CLOCK_SKEW_TOLERANCE_SECONDS)
        && u128::from(now_unix_seconds.saturating_sub(cache.checked_at_unix_seconds))
            < interval_seconds
}

fn print_cached_notice(cache: &ReleaseCache) {
    let Ok(current) = Version::parse(env!("CARGO_PKG_VERSION")) else {
        return;
    };
    let (Some(latest_version), Some(release_url)) = (
        cache.latest_version.as_deref(),
        cache.release_url.as_deref(),
    ) else {
        return;
    };
    let Ok(latest) = Version::parse(latest_version) else {
        return;
    };
    if latest > current {
        eprintln!(
            "aise: release {latest} is available; run `aise package update` ({})",
            release_url
        );
    }
}

fn cargo_install_source(executable: &Path) -> Option<CargoInstallSource> {
    let bin_dir = executable.parent()?;
    if bin_dir.file_name().is_none_or(|name| name != "bin") {
        return None;
    }
    let metadata_path = bin_dir
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(".crates2.json");
    let Ok(file_metadata) = fs::symlink_metadata(&metadata_path) else {
        return None;
    };
    if file_metadata.file_type().is_symlink()
        || !file_metadata.is_file()
        || file_metadata.len() > MAX_CARGO_INSTALL_METADATA_BYTES
    {
        return None;
    }
    let Ok(file) = File::open(metadata_path) else {
        return None;
    };
    let Ok(metadata) = serde_json::from_reader::<_, CargoInstallMetadata>(
        file.take(MAX_CARGO_INSTALL_METADATA_BYTES),
    ) else {
        return None;
    };
    metadata.installs.iter().find_map(|(package_id, record)| {
        let matches_package = package_id
            .split_ascii_whitespace()
            .next()
            .is_some_and(|package_name| package_name == PACKAGE_NAME)
            && record.bins.iter().any(|binary_name| binary_name == "aise");
        if !matches_package {
            return None;
        }
        if !package_id.contains('(') || package_id.contains(CRATES_IO_CARGO_SOURCE) {
            Some(CargoInstallSource::Registry)
        } else {
            Some(CargoInstallSource::DirectSource)
        }
    })
}

fn is_homebrew_executable(executable: &Path) -> bool {
    executable.ancestors().any(|version_directory| {
        let Some(formula_directory) = version_directory.parent() else {
            return false;
        };
        let Some(cellar_directory) = formula_directory.parent() else {
            return false;
        };
        if formula_directory
            .file_name()
            .is_none_or(|name| name != PACKAGE_NAME)
            || cellar_directory
                .file_name()
                .is_none_or(|name| name != "Cellar")
        {
            return false;
        }
        let receipt = version_directory.join("INSTALL_RECEIPT.json");
        fs::symlink_metadata(receipt)
            .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
    })
}

fn uv_receipt_belongs_to_python_prefix(evidence: &InstallEvidence) -> bool {
    let Some(python_prefix) = evidence.python_prefix.as_deref() else {
        return false;
    };
    let Some(receipt_path) = evidence.uv_tool_receipt.as_deref() else {
        return false;
    };
    if receipt_path != python_prefix.join("uv-receipt.toml") {
        return false;
    }
    let Ok(bytes) = read_bounded_regular_file(receipt_path, MAX_CARGO_INSTALL_METADATA_BYTES)
    else {
        return false;
    };
    let Ok(receipt) = toml::from_slice::<UvReceipt>(&bytes) else {
        return false;
    };
    receipt
        .tool
        .requirements
        .iter()
        .any(|requirement| package_names_match(&requirement.name, PACKAGE_NAME))
        && receipt.tool.entrypoints.iter().any(|entrypoint| {
            entrypoint.name == "aise"
                && package_names_match(&entrypoint.from, PACKAGE_NAME)
                && evidence
                    .invoked_executable
                    .as_deref()
                    .is_none_or(|invoked| {
                        paths_identify_same_file(invoked, &entrypoint.install_path)
                    })
        })
}

fn pipx_metadata_belongs_to_python_prefix(evidence: &InstallEvidence) -> bool {
    let Some(python_prefix) = evidence.python_prefix.as_deref() else {
        return false;
    };
    let Some(metadata_path) = evidence.pipx_metadata.as_deref() else {
        return false;
    };
    if metadata_path != python_prefix.join("pipx_metadata.json") {
        return false;
    }
    let Ok(bytes) = read_bounded_regular_file(metadata_path, MAX_CARGO_INSTALL_METADATA_BYTES)
    else {
        return false;
    };
    serde_json::from_slice::<PipxMetadata>(&bytes).is_ok_and(|metadata| {
        package_names_match(&metadata.main_package.package, PACKAGE_NAME)
            && metadata.main_package.apps.iter().any(|app| app == "aise")
    })
}

fn read_bounded_regular_file(path: &Path, maximum_bytes: u64) -> Result<Vec<u8>> {
    if !is_regular_file_without_symlink(path) {
        bail!("{} is not a regular non-symlink file", path.display());
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(maximum_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > maximum_bytes {
        bail!("{} exceeds the supported metadata size", path.display());
    }
    Ok(bytes)
}

fn package_names_match(left: &str, right: &str) -> bool {
    fn normalized(name: &str) -> String {
        let mut output = String::with_capacity(name.len());
        let mut previous_was_separator = false;
        for character in name.chars() {
            if matches!(character, '-' | '_' | '.') {
                if !previous_was_separator {
                    output.push('-');
                }
                previous_was_separator = true;
            } else {
                output.extend(character.to_lowercase());
                previous_was_separator = false;
            }
        }
        output
    }
    normalized(left) == normalized(right)
}

fn python_executable_belongs_to_prefix(evidence: &InstallEvidence) -> bool {
    evidence
        .python_executable
        .as_deref()
        .zip(evidence.python_prefix.as_deref())
        .is_some_and(|(python_executable, python_prefix)| {
            python_executable.starts_with(python_prefix)
                && paths_identify_same_file(&evidence.executable, python_executable)
        })
}

fn paths_identify_same_file(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (file_id::get_file_id(left), file_id::get_file_id(right)) {
        (Ok(left_id), Ok(right_id)) => left_id == right_id,
        _ => false,
    }
}

fn manager_action(argv: Vec<String>) -> ExecutableUpdateAction {
    manager_action_with_environment(argv, BTreeMap::new())
}

fn manager_action_with_environment(
    argv: Vec<String>,
    environment: BTreeMap<String, String>,
) -> ExecutableUpdateAction {
    ExecutableUpdateAction::InvokePackageManager { argv, environment }
}

fn cargo_update_argv(evidence: &InstallEvidence) -> Vec<String> {
    let mut argv = strings(["cargo", "install"]);
    if let Some(root) = evidence
        .executable
        .parent()
        .filter(|directory| directory.file_name().is_some_and(|name| name == "bin"))
        .and_then(Path::parent)
    {
        argv.extend(["--root".into(), root.to_string_lossy().into_owned()]);
    }
    argv.extend([PACKAGE_NAME.into(), "--locked".into()]);
    argv
}

fn uv_tool_update_environment(evidence: &InstallEvidence) -> BTreeMap<String, String> {
    evidence
        .python_prefix
        .as_deref()
        .and_then(Path::parent)
        .map(|tool_directory| {
            BTreeMap::from([(
                "UV_TOOL_DIR".into(),
                tool_directory.to_string_lossy().into_owned(),
            )])
        })
        .unwrap_or_default()
}

fn pipx_update_environment(evidence: &InstallEvidence) -> BTreeMap<String, String> {
    evidence
        .python_prefix
        .as_deref()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(|pipx_home| {
            BTreeMap::from([("PIPX_HOME".into(), pipx_home.to_string_lossy().into_owned())])
        })
        .unwrap_or_default()
}

fn homebrew_executable(evidence: &InstallEvidence) -> Option<PathBuf> {
    evidence.executable.ancestors().find_map(|directory| {
        (directory.file_name().is_some_and(|name| name == "Cellar"))
            .then(|| directory.parent().map(|prefix| prefix.join("bin/brew")))
            .flatten()
    })
}

fn render_update_command(
    argv: &[String],
    environment: &BTreeMap<String, String>,
) -> Result<String> {
    #[cfg(unix)]
    {
        let mut command = Vec::with_capacity(1 + environment.len() + argv.len());
        if !environment.is_empty() {
            command.push("env".into());
            command.extend(
                environment
                    .iter()
                    .map(|(name, value)| format!("{name}={value}")),
            );
        }
        command.extend_from_slice(argv);
        render_command(&command)
    }
    #[cfg(not(unix))]
    {
        Ok(format!(
            "environment={} argv={}",
            serde_json::to_string(environment)?,
            serde_json::to_string(argv)?
        ))
    }
}

fn manual_update_guidance(
    argv: &[String],
    environment: &BTreeMap<String, String>,
) -> Result<String> {
    #[cfg(unix)]
    {
        Ok(format!(
            "Automatic apply is unavailable on this platform while aise is running. Exit aise, then run: {}",
            render_update_command(argv, environment)?
        ))
    }
    #[cfg(not(unix))]
    {
        Ok(format!(
            "Automatic apply is unavailable while aise is running. Exit aise, then invoke the detected package manager with these structured values: {}",
            render_update_command(argv, environment)?
        ))
    }
}

fn render_command(argv: &[String]) -> Result<String> {
    #[cfg(unix)]
    {
        render_posix_shell_command(argv)
    }
    #[cfg(not(unix))]
    {
        Ok(argv
            .iter()
            .map(|argument| format!("{argument:?}"))
            .collect::<Vec<_>>()
            .join(" "))
    }
}

fn strings<const N: usize>(values: [&str; N]) -> Vec<String> {
    values.into_iter().map(str::to_owned).collect()
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn nonempty_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn env_truthy(name: &str) -> bool {
    nonempty_env(name).is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn unix_seconds() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is earlier than the Unix epoch")?
        .as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn evidence(executable: PathBuf) -> InstallEvidence {
        InstallEvidence {
            executable,
            invoked_executable: None,
            python_installer: None,
            python_executable: None,
            python_prefix: None,
            uv_tool_receipt: None,
            pipx_metadata: None,
            direct_url: None,
        }
    }

    fn write_executable(path: &Path) {
        fs::write(path, b"executable").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn native_background_child_reexecutes_the_running_binary() {
        let evidence = evidence(PathBuf::from("/opt/aise/bin/aise"));

        assert_eq!(
            background_child_executable_from_evidence(&evidence).unwrap(),
            evidence.executable
        );
    }

    #[test]
    fn python_background_child_uses_the_published_console_script() {
        let temp = TempDir::new().unwrap();
        let python = temp.path().join("python");
        let invoked = temp.path().join("aise");
        write_executable(&python);
        write_executable(&invoked);
        let mut evidence = evidence(python.clone());
        evidence.python_executable = Some(python);
        evidence.invoked_executable = Some(invoked.clone());

        assert_eq!(
            background_child_executable_from_evidence(&evidence).unwrap(),
            invoked
        );
    }

    #[test]
    fn python_background_child_rejects_missing_or_invalid_console_scripts() {
        let temp = TempDir::new().unwrap();
        let python = temp.path().join("python");
        write_executable(&python);
        let mut evidence = evidence(python.clone());
        evidence.python_executable = Some(python);

        let missing = background_child_executable_from_evidence(&evidence)
            .unwrap_err()
            .to_string();
        assert!(missing.contains("did not identify the invoked aise console script"));

        evidence.invoked_executable = Some(PathBuf::from("relative/aise"));
        let invalid = background_child_executable_from_evidence(&evidence)
            .unwrap_err()
            .to_string();
        assert!(invalid.contains("reported an invalid invoked aise executable"));
    }

    #[test]
    fn developer_direct_source_never_becomes_a_registry_update() {
        let mut evidence = evidence(PathBuf::from("/tmp/tool/bin/python"));
        evidence.python_installer = Some("uv".into());
        evidence.python_executable = Some(PathBuf::from("/tmp/tool/bin/python"));
        evidence.python_prefix = Some(PathBuf::from("/tmp/tool"));
        evidence.direct_url =
            Some(r#"{"url":"file:///workspace/ai-session-search","dir_info":{}}"#.into());
        let plan = plan_package_manager_update(&evidence).unwrap();
        assert_eq!(plan.owner, ExecutableOwner::DirectSource);
        assert!(matches!(
            plan.action,
            ExecutableUpdateAction::Guidance { .. }
        ));
    }

    #[test]
    fn uv_tool_requires_an_existing_receipt() {
        let temp = TempDir::new().unwrap();
        let receipt = temp.path().join("uv-receipt.toml");
        let invoked = temp.path().join("bin/aise");
        fs::create_dir_all(invoked.parent().unwrap()).unwrap();
        fs::write(&invoked, b"aise").unwrap();
        fs::write(
            &receipt,
            format!(
                "[tool]\nrequirements = [{{ name = \"ai-session-search\" }}]\n\
                 entrypoints = [{{ name = \"aise\", from = \"ai-session-search\", install-path = {:?} }}]\n",
                invoked
            ),
        )
        .unwrap();
        let mut evidence = evidence(temp.path().join("bin/python"));
        evidence.invoked_executable = Some(invoked);
        evidence.python_installer = Some("uv".into());
        evidence.python_executable = Some(temp.path().join("bin/python"));
        evidence.python_prefix = Some(temp.path().to_path_buf());
        evidence.uv_tool_receipt = Some(receipt);
        let plan = plan_package_manager_update(&evidence).unwrap();
        assert_eq!(plan.owner, ExecutableOwner::UvTool);
        let ExecutableUpdateAction::InvokePackageManager { argv, environment } = plan.action else {
            panic!("expected command");
        };
        assert_eq!(argv, strings(["uv", "tool", "upgrade", PACKAGE_NAME]));
        assert_eq!(
            environment.get("UV_TOOL_DIR").map(String::as_str),
            temp.path()
                .parent()
                .map(|path| path.to_string_lossy())
                .as_deref()
        );
    }

    #[test]
    fn uv_receipt_must_belong_to_the_reported_python_prefix() {
        let temp = TempDir::new().unwrap();
        let unrelated = TempDir::new().unwrap();
        let receipt = unrelated.path().join("uv-receipt.toml");
        fs::write(
            &receipt,
            "[tool]\nrequirements = [{ name = \"ai-session-search\" }]\n\
             entrypoints = [{ name = \"aise\", from = \"ai-session-search\", install-path = \"/tmp/aise\" }]\n",
        )
        .unwrap();
        let mut evidence = evidence(temp.path().join("bin/python"));
        evidence.python_installer = Some("uv".into());
        evidence.python_executable = Some(temp.path().join("bin/python"));
        evidence.python_prefix = Some(temp.path().to_path_buf());
        evidence.uv_tool_receipt = Some(receipt);

        assert_eq!(detect_executable_owner(&evidence), ExecutableOwner::Unknown);
    }

    #[test]
    fn pipx_requires_an_existing_environment_bound_metadata_file() {
        let temp = TempDir::new().unwrap();
        let metadata = temp.path().join("pipx_metadata.json");
        fs::write(
            &metadata,
            r#"{"main_package":{"package":"ai-session-search","apps":["aise"]}}"#,
        )
        .unwrap();
        let mut evidence = evidence(temp.path().join("bin/python"));
        evidence.python_installer = Some("pip".into());
        evidence.python_executable = Some(temp.path().join("bin/python"));
        evidence.python_prefix = Some(temp.path().to_path_buf());
        evidence.pipx_metadata = Some(metadata);

        let plan = plan_package_manager_update(&evidence).unwrap();

        assert_eq!(plan.owner, ExecutableOwner::Pipx);
        let ExecutableUpdateAction::InvokePackageManager { argv, environment } = plan.action else {
            panic!("expected command");
        };
        assert_eq!(argv, strings(["pipx", "upgrade", PACKAGE_NAME]));
        assert!(environment.contains_key("PIPX_HOME"));
    }

    #[test]
    fn receipts_for_other_packages_do_not_claim_aise() {
        let temp = TempDir::new().unwrap();
        let executable = temp.path().join("bin/python");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, b"python").unwrap();

        let uv_receipt = temp.path().join("uv-receipt.toml");
        fs::write(
            &uv_receipt,
            "[tool]\nrequirements = [{ name = \"other-tool\" }]\n\
             entrypoints = [{ name = \"aise\", from = \"other-tool\", install-path = \"/tmp/aise\" }]\n",
        )
        .unwrap();
        let mut uv_evidence = evidence(executable.clone());
        uv_evidence.python_installer = Some("uv".into());
        uv_evidence.python_executable = Some(executable.clone());
        uv_evidence.python_prefix = Some(temp.path().to_path_buf());
        uv_evidence.uv_tool_receipt = Some(uv_receipt);
        assert_eq!(
            detect_executable_owner(&uv_evidence),
            ExecutableOwner::Unknown
        );

        let pipx_metadata = temp.path().join("pipx_metadata.json");
        fs::write(
            &pipx_metadata,
            r#"{"main_package":{"package":"other-tool","apps":["aise"]}}"#,
        )
        .unwrap();
        let mut pipx_evidence = evidence(executable.clone());
        pipx_evidence.python_installer = Some("pip".into());
        pipx_evidence.python_executable = Some(executable);
        pipx_evidence.python_prefix = Some(temp.path().to_path_buf());
        pipx_evidence.pipx_metadata = Some(pipx_metadata);
        assert_eq!(
            detect_executable_owner(&pipx_evidence),
            ExecutableOwner::Unknown
        );
    }

    #[test]
    fn uv_pip_and_pip_target_the_exact_python_environment() {
        for (installer, owner, expected_prefix) in [
            ("uv", ExecutableOwner::UvPip, vec!["uv", "pip", "install"]),
            (
                "pip",
                ExecutableOwner::Pip,
                vec!["/tmp/venv/bin/python", "-m", "pip"],
            ),
        ] {
            let mut evidence = evidence(PathBuf::from("/tmp/venv/bin/python"));
            evidence.python_installer = Some(installer.into());
            evidence.python_executable = Some(PathBuf::from("/tmp/venv/bin/python"));
            evidence.python_prefix = Some(PathBuf::from("/tmp/venv"));
            let plan = plan_package_manager_update(&evidence).unwrap();
            assert_eq!(plan.owner, owner);
            let ExecutableUpdateAction::InvokePackageManager { argv, .. } = plan.action else {
                panic!("expected command");
            };
            assert_eq!(
                &argv[..3],
                &expected_prefix
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            );
            assert!(argv.iter().any(|part| part == "/tmp/venv/bin/python"));
        }
    }

    #[test]
    fn python_manager_evidence_must_target_the_reported_prefix() {
        for installer in ["uv", "pip"] {
            let mut evidence = evidence(PathBuf::from("/tmp/other/bin/python"));
            evidence.python_installer = Some(installer.into());
            evidence.python_executable = Some(PathBuf::from("/tmp/other/bin/python"));
            evidence.python_prefix = Some(PathBuf::from("/tmp/intended"));

            assert_eq!(detect_executable_owner(&evidence), ExecutableOwner::Unknown);
        }
    }

    #[test]
    fn python_manager_evidence_must_identify_the_running_executable() {
        let temp = TempDir::new().unwrap();
        let uv_prefix = temp.path().join("uv-tool");
        fs::create_dir_all(uv_prefix.join("bin")).unwrap();
        fs::write(
            uv_prefix.join("uv-receipt.toml"),
            "[tool]\nrequirements = [{ name = \"ai-session-search\" }]\n\
             entrypoints = [{ name = \"aise\", from = \"ai-session-search\", install-path = \"/tmp/aise\" }]\n",
        )
        .unwrap();

        let mut uv_evidence = evidence(temp.path().join("native/aise"));
        uv_evidence.python_installer = Some("uv".into());
        uv_evidence.python_executable = Some(uv_prefix.join("bin/python"));
        uv_evidence.python_prefix = Some(uv_prefix.clone());
        uv_evidence.uv_tool_receipt = Some(uv_prefix.join("uv-receipt.toml"));
        uv_evidence.direct_url =
            Some(r#"{"url":"file:///workspace/ai-session-search","dir_info":{}}"#.into());
        assert_eq!(
            detect_executable_owner(&uv_evidence),
            ExecutableOwner::Unknown
        );

        let pipx_prefix = temp.path().join("pipx");
        fs::create_dir_all(pipx_prefix.join("bin")).unwrap();
        fs::write(
            pipx_prefix.join("pipx_metadata.json"),
            r#"{"main_package":{"package":"ai-session-search","apps":["aise"]}}"#,
        )
        .unwrap();
        let mut pipx_evidence = evidence(temp.path().join("native/aise"));
        pipx_evidence.python_installer = Some("pip".into());
        pipx_evidence.python_executable = Some(pipx_prefix.join("bin/python"));
        pipx_evidence.python_prefix = Some(pipx_prefix.clone());
        pipx_evidence.pipx_metadata = Some(pipx_prefix.join("pipx_metadata.json"));
        assert_eq!(
            detect_executable_owner(&pipx_evidence),
            ExecutableOwner::Unknown
        );
    }

    #[cfg(unix)]
    #[test]
    fn python_manager_receipts_reject_symbolic_links() {
        use std::os::unix::fs::symlink;

        for (installer, receipt_name) in [("uv", "uv-receipt.toml"), ("pip", "pipx_metadata.json")]
        {
            let temp = TempDir::new().unwrap();
            let executable = temp.path().join("bin/python");
            fs::create_dir_all(executable.parent().unwrap()).unwrap();
            fs::write(&executable, b"python").unwrap();
            let target = temp.path().join("receipt-target");
            fs::write(
                &target,
                if installer == "uv" {
                    "[tool]\nrequirements = [{ name = \"ai-session-search\" }]\n\
                     entrypoints = [{ name = \"aise\", from = \"ai-session-search\", install-path = \"/tmp/aise\" }]\n"
                } else {
                    r#"{"main_package":{"package":"ai-session-search","apps":["aise"]}}"#
                },
            )
            .unwrap();
            let receipt = temp.path().join(receipt_name);
            symlink(&target, &receipt).unwrap();

            let mut evidence = evidence(executable.clone());
            evidence.python_installer = Some(installer.into());
            evidence.python_executable = Some(executable);
            evidence.python_prefix = Some(temp.path().to_path_buf());
            if installer == "uv" {
                evidence.uv_tool_receipt = Some(receipt);
            } else {
                evidence.pipx_metadata = Some(receipt);
            }

            assert_eq!(detect_executable_owner(&evidence), ExecutableOwner::Unknown);
        }
    }

    #[test]
    fn cargo_detection_requires_tracking_metadata_and_never_uses_force() {
        let temp = TempDir::new().unwrap();
        let bin = temp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let executable = bin.join("aise");
        fs::write(&executable, b"binary").unwrap();
        // This synthetic Cargo receipt is test-only and matches the RC being prepared.
        fs::write(
            temp.path().join(".crates2.json"),
            r#"{"installs":{"ai-session-search 1.0.0-rc.1":{"bins":["aise"]}}}"#,
        )
        .unwrap();
        let plan = plan_package_manager_update(&evidence(executable)).unwrap();
        assert_eq!(plan.owner, ExecutableOwner::Cargo);
        let ExecutableUpdateAction::InvokePackageManager { argv, .. } = plan.action else {
            panic!("expected command");
        };
        assert_eq!(
            argv,
            vec![
                "cargo",
                "install",
                "--root",
                temp.path().to_string_lossy().as_ref(),
                PACKAGE_NAME,
                "--locked",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
        );
        assert!(!argv.iter().any(|part| part == "--force"));
    }

    #[test]
    fn cargo_path_and_git_installs_preserve_the_recorded_source() {
        let temp = TempDir::new().unwrap();
        let bin = temp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let executable = bin.join("aise");
        fs::write(&executable, b"binary").unwrap();

        for source in [
            "path+file:///workspace/ai-session-search",
            "git+https://github.com/ahundt/ai-session-search#01234567",
            "registry+https://packages.example.test/index",
        ] {
            fs::write(
                temp.path().join(".crates2.json"),
                format!(
                    r#"{{"installs":{{"ai-session-search 1.0.0-rc.1 ({source})":{{"bins":["aise"]}}}}}}"#
                ),
            )
            .unwrap();
            let plan = plan_package_manager_update(&evidence(executable.clone())).unwrap();
            assert_eq!(plan.owner, ExecutableOwner::DirectSource);
            assert!(plan
                .ownership_evidence
                .contains("Cargo installation metadata"));
            assert!(matches!(
                plan.action,
                ExecutableUpdateAction::Guidance { .. }
            ));
        }
    }

    #[test]
    fn cargo_detection_rejects_unparsed_substring_decoys() {
        let temp = TempDir::new().unwrap();
        let bin = temp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let executable = bin.join("aise");
        fs::write(&executable, b"binary").unwrap();
        fs::write(
            temp.path().join(".crates2.json"),
            r#"{"comment":"ai-session-search and \"aise\" are not an install record"}"#,
        )
        .unwrap();

        assert_eq!(
            detect_executable_owner(&evidence(executable)),
            ExecutableOwner::Unknown
        );
    }

    #[test]
    fn homebrew_detection_requires_formula_and_version_receipt() {
        let temp = TempDir::new().unwrap();
        let version = temp.path().join("Cellar").join(PACKAGE_NAME).join("1.2.3");
        let executable = version.join("bin/aise");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, b"binary").unwrap();
        assert_eq!(
            detect_executable_owner(&evidence(executable.clone())),
            ExecutableOwner::Unknown
        );

        fs::write(version.join("INSTALL_RECEIPT.json"), "{}").unwrap();
        assert_eq!(
            detect_executable_owner(&evidence(executable)),
            ExecutableOwner::Homebrew
        );
    }

    #[test]
    fn untracked_executable_fails_closed() {
        let plan =
            plan_package_manager_update(&evidence(PathBuf::from("/opt/custom/bin/aise"))).unwrap();
        assert_eq!(plan.owner, ExecutableOwner::Unknown);
        assert!(matches!(
            plan.action,
            ExecutableUpdateAction::Guidance { .. }
        ));
    }

    #[test]
    fn automatic_apply_requires_a_manager_command_and_an_unlocked_executable() {
        let manager_action = manager_action(strings(["uv", "tool", "upgrade", PACKAGE_NAME]));
        let guidance = ExecutableUpdateAction::Guidance {
            message: "reinstall from the recorded source".into(),
        };

        assert!(automatic_package_manager_apply_supported(
            &manager_action,
            false
        ));
        assert!(!automatic_package_manager_apply_supported(
            &manager_action,
            true
        ));
        assert!(!automatic_package_manager_apply_supported(&guidance, false));
    }

    #[test]
    fn confirmation_requires_two_terminal_streams_and_treats_eof_as_cancel() {
        assert!(interactive_confirmation_supported(true, true));
        assert!(!interactive_confirmation_supported(false, true));
        assert!(!interactive_confirmation_supported(true, false));

        assert!(confirmation_answer_applies(Some("\n")));
        assert!(confirmation_answer_applies(Some("YES\n")));
        assert!(!confirmation_answer_applies(Some("no\n")));
        assert!(!confirmation_answer_applies(None));
    }

    #[test]
    fn release_tags_share_python_and_cargo_version_identity() {
        assert_eq!(parse_release_tag("v1.2.3").unwrap(), Version::new(1, 2, 3));
        assert_eq!(
            parse_release_tag("v1.2.3rc4").unwrap(),
            Version::parse("1.2.3-rc.4").unwrap()
        );
        assert_eq!(
            parse_release_tag("1.2.3a4").unwrap(),
            Version::parse("1.2.3-alpha.4").unwrap()
        );
        assert!(parse_release_tag("latest").is_err());
        assert!(parse_release_tag("v1.2.3rc").is_err());
        assert_eq!(
            python_version(&Version::parse("1.2.3-rc.4").unwrap()).unwrap(),
            "1.2.3rc4"
        );
    }

    #[test]
    fn prerelease_updates_target_the_selected_version_without_shells_or_force() {
        let current = Version::parse("1.0.0-rc.1").unwrap();
        let latest = Version::parse("1.0.0-rc.2").unwrap();
        let environment = BTreeMap::from([("UV_TOOL_DIR".into(), "/tmp/tools".into())]);
        let uv = update_action_for_release(
            ExecutableOwner::UvTool,
            manager_action_with_environment(
                strings(["uv", "tool", "upgrade", PACKAGE_NAME]),
                environment.clone(),
            ),
            &current,
            &latest,
        )
        .unwrap();
        assert_eq!(
            uv,
            manager_action_with_environment(
                strings(["uv", "tool", "upgrade", "ai-session-search==1.0.0rc2"]),
                environment
            )
        );

        let cargo = update_action_for_release(
            ExecutableOwner::Cargo,
            manager_action(strings(["cargo", "install", PACKAGE_NAME, "--locked"])),
            &current,
            &latest,
        )
        .unwrap();
        let ExecutableUpdateAction::InvokePackageManager { argv, .. } = cargo else {
            panic!("expected Cargo command");
        };
        assert!(argv.ends_with(&["--version".into(), "1.0.0-rc.2".into()]));
        assert!(!argv.iter().any(|argument| argument == "--force"));
    }

    #[test]
    fn stable_builds_keep_manager_native_upgrade_semantics() {
        let action = manager_action(strings(["brew", "upgrade", PACKAGE_NAME]));
        assert_eq!(
            update_action_for_release(
                ExecutableOwner::Homebrew,
                action.clone(),
                &Version::new(1, 0, 0),
                &Version::new(1, 0, 1),
            )
            .unwrap(),
            action
        );
    }

    #[test]
    fn draft_and_prerelease_responses_are_rejected() {
        for (draft, prerelease) in [(true, false), (false, true)] {
            let error = release_status_from_response(GitHubRelease {
                tag_name: "v9.0.0".into(),
                html_url: "https://github.com/ahundt/ai-session-search/releases/tag/v9.0.0".into(),
                draft,
                prerelease,
            })
            .unwrap_err();
            assert!(error.to_string().contains("completed stable release"));
        }
    }

    #[test]
    fn prerelease_channel_selects_same_train_rc_or_completed_stable_release() {
        fn release(tag: &str, prerelease: bool) -> GitHubRelease {
            GitHubRelease {
                tag_name: tag.into(),
                html_url: format!("https://github.com/ahundt/ai-session-search/releases/tag/{tag}"),
                draft: false,
                prerelease,
            }
        }

        let current = Version::parse("1.0.0-rc.1").unwrap();
        let status = applicable_release_status_from_responses(
            current.clone(),
            vec![
                release("v1.1.0rc1", true),
                release("v1.0.0rc2", true),
                release("v0.9.0", false),
            ],
        )
        .unwrap();
        assert_eq!(status.latest, Version::parse("1.0.0-rc.2").unwrap());

        let status = applicable_release_status_from_responses(
            current,
            vec![
                release("v1.1.0rc1", true),
                release("v1.0.0rc2", true),
                release("v1.0.0", false),
            ],
        )
        .unwrap();
        assert_eq!(status.latest, Version::new(1, 0, 0));
    }

    #[test]
    fn release_list_rejects_drafts_and_phase_flag_mismatches() {
        let current = Version::parse("1.0.0-rc.1").unwrap();
        let error = applicable_release_status_from_responses(
            current,
            vec![
                GitHubRelease {
                    tag_name: "v1.0.0rc2".into(),
                    html_url: "https://github.com/ahundt/ai-session-search/releases/tag/v1.0.0rc2"
                        .into(),
                    draft: true,
                    prerelease: true,
                },
                GitHubRelease {
                    tag_name: "v1.0.0rc3".into(),
                    html_url: "https://github.com/ahundt/ai-session-search/releases/tag/v1.0.0rc3"
                        .into(),
                    draft: false,
                    prerelease: false,
                },
            ],
        )
        .unwrap_err();
        assert!(error.to_string().contains("no stable release"));
    }

    #[test]
    fn release_response_rejects_an_unexpected_html_url() {
        let error = release_status_from_response(GitHubRelease {
            tag_name: "v9.0.0".into(),
            html_url: "https://example.test/releases/tag/v9.0.0".into(),
            draft: false,
            prerelease: false,
        })
        .unwrap_err();
        assert!(error.to_string().contains("unexpected release URL"));
    }

    #[test]
    fn release_summary_distinguishes_equal_and_unreleased_newer_builds() {
        // These historical 0.x versions are test-only comparison fixtures, not release metadata.
        let mut status = ReleaseStatus {
            current: Version::parse("0.3.1").unwrap(),
            latest: Version::parse("0.3.0").unwrap(),
            release_url: "https://github.com/ahundt/ai-session-search/releases/tag/v0.3.0".into(),
        };
        assert_eq!(
            release_relation_summary(false, status.current > status.latest),
            Some("This build is newer than the latest applicable release.")
        );
        status.latest = status.current.clone();
        assert_eq!(
            release_relation_summary(false, status.current > status.latest),
            Some("This build matches the latest applicable release.")
        );
        status.latest = Version::parse("0.4.0").unwrap();
        assert_eq!(
            release_relation_summary(status.update_available(), false),
            None
        );
    }

    #[test]
    fn corrupt_future_and_stale_cache_records_are_safe() {
        let mut config = Config::default();
        let temp = TempDir::new().unwrap();
        config.index.cache_dir = Some(temp.path().to_string_lossy().into_owned());
        let path = cache_path(&config);
        fs::write(&path, b"{").unwrap();
        assert!(read_cache(&config).unwrap().is_none());
        fs::write(&path, vec![b'x'; MAX_RELEASE_RESPONSE_BYTES as usize + 1]).unwrap();
        assert!(read_cache(&config).unwrap().is_none());

        // A valid historical release keeps this cache-integrity test independent of RC metadata.
        let cache = ReleaseCache {
            schema_version: RELEASE_CACHE_SCHEMA_VERSION,
            checked_at_unix_seconds: 10_000,
            latest_version: Some("0.3.1".into()),
            release_url: Some(
                "https://github.com/ahundt/ai-session-search/releases/tag/v0.3.1".into(),
            ),
        };
        assert!(!release_cache_is_fresh(&cache, 1_000, 24));
        assert!(!release_cache_is_fresh(
            &cache,
            10_000 + 24 * SECONDS_PER_HOUR,
            24
        ));
        assert!(release_cache_is_fresh(&cache, 10_001, 24));
        let mut ancient_cache = cache.clone();
        ancient_cache.checked_at_unix_seconds = 0;
        assert!(release_cache_is_fresh(&ancient_cache, u64::MAX, u64::MAX));
    }

    #[test]
    fn cache_write_is_atomic_and_round_trips() {
        let mut config = Config::default();
        let temp = TempDir::new().unwrap();
        config.index.cache_dir = Some(temp.path().to_string_lossy().into_owned());
        // A valid historical release is sufficient to test serialization and atomic replacement.
        let cache = ReleaseCache {
            schema_version: RELEASE_CACHE_SCHEMA_VERSION,
            checked_at_unix_seconds: 123,
            latest_version: Some("0.3.1".into()),
            release_url: Some(
                "https://github.com/ahundt/ai-session-search/releases/tag/v0.3.1".into(),
            ),
        };
        write_cache(&config, &cache).unwrap();
        let restored = read_cache(&config).unwrap().unwrap();
        assert_eq!(restored.latest_version.as_deref(), Some("0.3.1"));
        assert_eq!(restored.checked_at_unix_seconds, 123);

        // `None` is a successful retry marker for a failed passive check, not a release version.
        let unavailable = ReleaseCache {
            schema_version: RELEASE_CACHE_SCHEMA_VERSION,
            checked_at_unix_seconds: 456,
            latest_version: None,
            release_url: None,
        };
        write_cache(&config, &unavailable).unwrap();
        let restored = read_cache(&config).unwrap().unwrap();
        assert_eq!(restored.latest_version, None);
        assert_eq!(restored.release_url, None);
        assert!(release_cache_is_fresh(&restored, 457, 24));
        assert!(release_cache_is_fresh(
            &restored,
            456 + PASSIVE_FAILURE_RETRY_INTERVAL_HOURS * SECONDS_PER_HOUR - 1,
            PASSIVE_FAILURE_RETRY_INTERVAL_HOURS,
        ));
        assert!(!release_cache_is_fresh(
            &restored,
            456 + PASSIVE_FAILURE_RETRY_INTERVAL_HOURS * SECONDS_PER_HOUR,
            PASSIVE_FAILURE_RETRY_INTERVAL_HOURS,
        ));
    }
}
