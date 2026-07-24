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
use crate::util::{executable_candidates, render_posix_shell_command};

const PACKAGE_NAME: &str = "ai-session-search";
#[cfg(feature = "release-check")]
const LATEST_STABLE_RELEASE_API_URL: &str =
    "https://api.github.com/repos/ahundt/ai-session-search/releases/latest";
const MAX_RELEASE_RESPONSE_BYTES: u64 = 64 * 1024;
const STABLE_RELEASE_CACHE_FILE_NAME: &str = "stable-release-check.json";
const REQUESTED_RELEASE_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CARGO_INSTALL_METADATA_BYTES: u64 = 1024 * 1024;
const CRATES_IO_CARGO_SOURCE: &str = "(registry+https://github.com/rust-lang/crates.io-index)";
const RELEASE_CACHE_SCHEMA_VERSION: u32 = 2;
const SECONDS_PER_HOUR: u64 = 60 * 60;
const CLOCK_SKEW_TOLERANCE_SECONDS: u64 = 5 * 60;

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
    InvokePackageManager { argv: Vec<String> },
    Guidance { message: String },
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
struct StableReleaseStatus {
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
    latest_stable_version: String,
    release_url: String,
    newer_stable_release_available: bool,
    current_build_is_newer_than_latest_stable: bool,
}

impl StableReleaseStatus {
    fn update_available(&self) -> bool {
        self.latest > self.current
    }
}

fn detect_executable_owner(evidence: &InstallEvidence) -> ExecutableOwner {
    if evidence.direct_url.is_some() {
        return ExecutableOwner::DirectSource;
    }

    match evidence.python_installer.as_deref() {
        Some("uv")
            if evidence
                .uv_tool_receipt
                .as_deref()
                .is_some_and(Path::is_file)
                && uv_receipt_belongs_to_python_prefix(evidence) =>
        {
            return ExecutableOwner::UvTool;
        }
        Some("uv") if python_executable_belongs_to_prefix(evidence) => {
            return ExecutableOwner::UvPip;
        }
        Some("pip") if pipx_metadata_belongs_to_python_prefix(evidence) => {
            return ExecutableOwner::Pipx;
        }
        Some("pip") if python_executable_belongs_to_prefix(evidence) => {
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
            ExecutableUpdateAction::InvokePackageManager {
                argv: strings(["uv", "tool", "upgrade", PACKAGE_NAME]),
            },
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
                ExecutableUpdateAction::InvokePackageManager {
                    argv: vec![
                        "uv".into(),
                        "pip".into(),
                        "install".into(),
                        "--python".into(),
                        python,
                        "--upgrade".into(),
                        PACKAGE_NAME.into(),
                    ],
                },
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
                ExecutableUpdateAction::InvokePackageManager {
                    argv: vec![
                        python,
                        "-m".into(),
                        "pip".into(),
                        "install".into(),
                        "--upgrade".into(),
                        PACKAGE_NAME.into(),
                    ],
                },
            )
        }
        ExecutableOwner::Pipx => (
            "pip installation metadata includes an environment-bound pipx metadata file".into(),
            ExecutableUpdateAction::InvokePackageManager {
                argv: strings(["pipx", "upgrade", PACKAGE_NAME]),
            },
        ),
        ExecutableOwner::Cargo => (
            "the executable is in a Cargo installation root tracked by .crates2.json".into(),
            ExecutableUpdateAction::InvokePackageManager {
                argv: strings(["cargo", "install", PACKAGE_NAME, "--locked"]),
            },
        ),
        ExecutableOwner::Homebrew => (
            "the resolved executable is inside a Homebrew Cellar".into(),
            ExecutableUpdateAction::InvokePackageManager {
                argv: strings(["brew", "upgrade", PACKAGE_NAME]),
            },
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
    if !report.newer_stable_release_available {
        return Ok(());
    }
    let ExecutableUpdateAction::InvokePackageManager { argv } = &report.package.update_action
    else {
        return Ok(());
    };
    if !report.package.automatic_apply_supported_on_this_platform {
        println!(
            "Automatic apply is unavailable on this platform while aise is running. Exit aise, then run: {}",
            render_command(argv)?
        );
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
    execute_update_command(argv)
}

fn package_check_report(config: &Config) -> Result<PackageCheckReport> {
    let package = package_status_report()?;
    let status = fetch_latest_stable_release(REQUESTED_RELEASE_CHECK_TIMEOUT)
        .context("failed to check the latest completed stable release")?;
    let newer_stable_release_available = status.update_available();
    let current_build_is_newer_than_latest_stable = status.current > status.latest;
    if let Ok(cache) = cache_from_status(&status) {
        let _ = write_cache(config, &cache);
    }
    Ok(PackageCheckReport {
        package,
        current_version: status.current.to_string(),
        latest_stable_version: status.latest.to_string(),
        release_url: status.release_url,
        newer_stable_release_available,
        current_build_is_newer_than_latest_stable,
    })
}

fn stable_release_relation_summary(
    newer_stable_release_available: bool,
    current_build_is_newer_than_latest_stable: bool,
) -> Option<&'static str> {
    if newer_stable_release_available {
        None
    } else if current_build_is_newer_than_latest_stable {
        Some("This build is newer than the latest completed stable release.")
    } else {
        Some("This build matches the latest completed stable release.")
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
        if stable_release_cache_is_fresh(
            cache,
            now,
            config.release_notifications.minimum_check_interval_hours,
        ) {
            print_cached_notice(cache);
            return;
        }
    }

    let timeout = Duration::from_millis(config.release_notifications.request_timeout_ms);
    let cache = match fetch_latest_stable_release(timeout) {
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
        ExecutableUpdateAction::InvokePackageManager { argv } => {
            println!("Manager update command: {}", render_command(argv)?);
        }
        ExecutableUpdateAction::Guidance { message } => {
            println!("Update guidance: {message}");
        }
    }
    Ok(())
}

fn print_package_check_report(report: &PackageCheckReport) -> Result<()> {
    println!("Current version: {}", report.current_version);
    println!("Latest stable version: {}", report.latest_stable_version);
    println!("Release: {}", report.release_url);
    print_package_report(&report.package, ReportOutputFormat::Table)?;
    if let Some(summary) = stable_release_relation_summary(
        report.newer_stable_release_available,
        report.current_build_is_newer_than_latest_stable,
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

fn execute_update_command(argv: &[String]) -> Result<()> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow!("update command must not be empty"))?;
    let rendered = render_command(argv)?;
    let status = Command::new(program)
        .args(args)
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

fn fetch_latest_stable_release(request_timeout: Duration) -> Result<StableReleaseStatus> {
    #[cfg(not(feature = "release-check"))]
    {
        let _ = request_timeout;
        bail!(
            "this build excludes stable-release network checks; rebuild with the \
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
        let mut response = agent
            .get(LATEST_STABLE_RELEASE_API_URL)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .call()
            .context("GitHub release request failed")?;
        let release: GitHubRelease = response
            .body_mut()
            .with_config()
            .limit(MAX_RELEASE_RESPONSE_BYTES)
            .read_json()
            .context("GitHub release response was not valid bounded JSON")?;
        release_status_from_response(release)
    }
}

#[cfg(any(feature = "release-check", test))]
fn release_status_from_response(release: GitHubRelease) -> Result<StableReleaseStatus> {
    if release.draft || release.prerelease {
        bail!("GitHub latest release response was not a completed stable release");
    }
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .context("the compiled package version is not valid Cargo SemVer")?;
    let latest = parse_release_tag(&release.tag_name)?;
    let expected_release_url = format!(
        "https://github.com/ahundt/ai-session-search/releases/tag/{}",
        release.tag_name
    );
    if release.html_url != expected_release_url {
        bail!("GitHub latest release response contained an unexpected release URL");
    }
    Ok(StableReleaseStatus {
        current,
        latest,
        release_url: release.html_url,
    })
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

fn cache_from_status(status: &StableReleaseStatus) -> Result<ReleaseCache> {
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
    config.cache_dir().join(STABLE_RELEASE_CACHE_FILE_NAME)
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

fn stable_release_cache_is_fresh(
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
            "aise: stable release {latest} is available; run `aise package update` ({})",
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
    evidence
        .uv_tool_receipt
        .as_deref()
        .is_some_and(|receipt| receipt == python_prefix.join("uv-receipt.toml"))
}

fn pipx_metadata_belongs_to_python_prefix(evidence: &InstallEvidence) -> bool {
    let Some(python_prefix) = evidence.python_prefix.as_deref() else {
        return false;
    };
    evidence.pipx_metadata.as_deref().is_some_and(|metadata| {
        metadata == python_prefix.join("pipx_metadata.json") && metadata.is_file()
    })
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

    #[test]
    fn developer_direct_source_never_becomes_a_registry_update() {
        let mut evidence = evidence(PathBuf::from("/tmp/python"));
        evidence.python_installer = Some("uv".into());
        evidence.python_executable = Some(PathBuf::from("/tmp/tool/bin/python"));
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
        fs::write(&receipt, "[tool]\n").unwrap();
        let mut evidence = evidence(temp.path().join("bin/python"));
        evidence.python_installer = Some("uv".into());
        evidence.python_executable = Some(temp.path().join("bin/python"));
        evidence.python_prefix = Some(temp.path().to_path_buf());
        evidence.uv_tool_receipt = Some(receipt);
        let plan = plan_package_manager_update(&evidence).unwrap();
        assert_eq!(plan.owner, ExecutableOwner::UvTool);
        assert_eq!(
            plan.action,
            ExecutableUpdateAction::InvokePackageManager {
                argv: strings(["uv", "tool", "upgrade", PACKAGE_NAME])
            }
        );
    }

    #[test]
    fn uv_receipt_must_belong_to_the_reported_python_prefix() {
        let temp = TempDir::new().unwrap();
        let unrelated = TempDir::new().unwrap();
        let receipt = unrelated.path().join("uv-receipt.toml");
        fs::write(&receipt, "[tool]\n").unwrap();
        let mut evidence = evidence(temp.path().join("bin/python"));
        evidence.python_installer = Some("uv".into());
        evidence.python_executable = Some(temp.path().join("bin/python"));
        evidence.python_prefix = Some(temp.path().to_path_buf());
        evidence.uv_tool_receipt = Some(receipt);

        assert_eq!(detect_executable_owner(&evidence), ExecutableOwner::UvPip);
    }

    #[test]
    fn pipx_requires_an_existing_environment_bound_metadata_file() {
        let temp = TempDir::new().unwrap();
        let metadata = temp.path().join("pipx_metadata.json");
        fs::write(&metadata, "{}").unwrap();
        let mut evidence = evidence(temp.path().join("bin/python"));
        evidence.python_installer = Some("pip".into());
        evidence.python_executable = Some(temp.path().join("bin/python"));
        evidence.python_prefix = Some(temp.path().to_path_buf());
        evidence.pipx_metadata = Some(metadata);

        let plan = plan_package_manager_update(&evidence).unwrap();

        assert_eq!(plan.owner, ExecutableOwner::Pipx);
        assert_eq!(
            plan.action,
            ExecutableUpdateAction::InvokePackageManager {
                argv: strings(["pipx", "upgrade", PACKAGE_NAME])
            }
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
            let ExecutableUpdateAction::InvokePackageManager { argv } = plan.action else {
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
        let ExecutableUpdateAction::InvokePackageManager { argv } = plan.action else {
            panic!("expected command");
        };
        assert_eq!(
            argv,
            strings(["cargo", "install", PACKAGE_NAME, "--locked"])
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
        let manager_action = ExecutableUpdateAction::InvokePackageManager {
            argv: strings(["uv", "tool", "upgrade", PACKAGE_NAME]),
        };
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
        let mut status = StableReleaseStatus {
            current: Version::parse("0.3.1").unwrap(),
            latest: Version::parse("0.3.0").unwrap(),
            release_url: "https://github.com/ahundt/ai-session-search/releases/tag/v0.3.0".into(),
        };
        assert_eq!(
            stable_release_relation_summary(false, status.current > status.latest),
            Some("This build is newer than the latest completed stable release.")
        );
        status.latest = status.current.clone();
        assert_eq!(
            stable_release_relation_summary(false, status.current > status.latest),
            Some("This build matches the latest completed stable release.")
        );
        status.latest = Version::parse("0.4.0").unwrap();
        assert_eq!(
            stable_release_relation_summary(status.update_available(), false),
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
        assert!(!stable_release_cache_is_fresh(&cache, 1_000, 24));
        assert!(!stable_release_cache_is_fresh(
            &cache,
            10_000 + 24 * SECONDS_PER_HOUR,
            24
        ));
        assert!(stable_release_cache_is_fresh(&cache, 10_001, 24));
        let mut ancient_cache = cache.clone();
        ancient_cache.checked_at_unix_seconds = 0;
        assert!(stable_release_cache_is_fresh(
            &ancient_cache,
            u64::MAX,
            u64::MAX
        ));
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
        assert!(stable_release_cache_is_fresh(&restored, 457, 24));
    }
}
