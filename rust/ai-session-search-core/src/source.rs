// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! Canonical provider discovery and public source inventory.

use std::collections::HashSet;
use std::io::{BufReader, Read};
use std::sync::OnceLock;

use anyhow::{bail, Result};
use serde::Serialize;

use crate::config::Config;
use crate::hashing::FramedSha256;
use crate::models::{Provider, SourceFile};
use crate::providers::{
    aistudio::AiStudioAdapter, antigravity::AntigravityAdapter, claude::ClaudeAdapter,
    codex::CodexAdapter, cursor::CursorAdapter, gemini_cli::GeminiCliAdapter, pi::PiAdapter,
    ProviderDiscovery,
};
use crate::util::normalize_path;

/// Providers supported by discovery, in stable presentation order.
pub const PROVIDERS: [Provider; 9] = [
    Provider::Claude,
    Provider::ClaudeDesktop,
    Provider::Codex,
    Provider::Cursor,
    Provider::Antigravity,
    Provider::Pi,
    Provider::PrimeAgent,
    Provider::AiStudio,
    Provider::GeminiCli,
];

/// Stable fingerprint of every provider's parser contract.
///
/// The provider registry owns membership/order and `provider_parse_version` owns each version;
/// hashing those two canonical definitions avoids a second manually bumped generation. The first
/// call is `O(P + V)` for `P = 9` providers and `V` version bytes; `OnceLock` makes later calls
/// `O(1)`. Retained memory is one 32-byte value.
pub(crate) fn provider_parse_contract_fingerprint() -> [i64; 4] {
    static FINGERPRINT: OnceLock<[i64; 4]> = OnceLock::new();
    *FINGERPRINT.get_or_init(|| {
        provider_parse_contract_fingerprint_from(
            PROVIDERS
                .into_iter()
                .map(|provider| (provider, crate::util::provider_parse_version(provider))),
        )
    })
}

fn provider_parse_contract_fingerprint_from<'a>(
    contracts: impl IntoIterator<Item = (Provider, &'a str)>,
) -> [i64; 4] {
    let mut digest = FramedSha256::new(b"aise-provider-parse-contract-v1");
    for (provider, version) in contracts {
        digest.update_bytes(provider.as_str().as_bytes());
        digest.update_bytes(version.as_bytes());
    }
    digest.finish_i64_words()
}

/// Effective discovery configuration and current file count for one provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderSourceStatus {
    /// Provider represented by this status.
    pub provider: Provider,
    /// Whether discovery and indexing are enabled for this provider.
    pub enabled: bool,
    /// Effective normalized roots searched for session files.
    pub roots: Vec<String>,
    /// Number of files currently discoverable beneath `roots`.
    pub discovered_files: usize,
    /// Non-fatal discovery failures. Readable sources remain available when this is non-empty.
    pub warnings: Vec<ProviderDiscoveryWarning>,
}

/// One non-fatal filesystem or provider-sidecar discovery failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderDiscoveryWarning {
    pub provider: Provider,
    pub path: String,
    pub operation: String,
    pub message: String,
    pub readable_sources_preserved: bool,
    pub verification_command: String,
    pub guidance: String,
}

#[derive(Debug, Default)]
pub(crate) struct DiscoveryResult {
    pub(crate) sources: Vec<SourceFile>,
    pub(crate) warnings: Vec<ProviderDiscoveryWarning>,
}

pub(crate) struct SourceInventory {
    pub(crate) providers: Vec<ProviderSourceStatus>,
    pub(crate) discovered: HashSet<(Provider, String)>,
    pub(crate) warnings: Vec<ProviderDiscoveryWarning>,
}

pub(crate) struct ProviderSet {
    pub(crate) claude: ClaudeAdapter,
    claude_desktop: ClaudeAdapter,
    pub(crate) codex: CodexAdapter,
    pub(crate) cursor: CursorAdapter,
    pub(crate) antigravity: AntigravityAdapter,
    pub(crate) pi: PiAdapter,
    pub(crate) prime_agent: PiAdapter,
    pub(crate) aistudio: AiStudioAdapter,
    pub(crate) gemini_cli: GeminiCliAdapter,
}

impl ProviderSet {
    pub(crate) fn new(config: &Config) -> Self {
        Self {
            claude: ClaudeAdapter::new(provider_roots(config, Provider::Claude)),
            claude_desktop: ClaudeAdapter::new(provider_roots(config, Provider::ClaudeDesktop)),
            codex: CodexAdapter::new(provider_roots(config, Provider::Codex)),
            cursor: CursorAdapter::new(provider_roots(config, Provider::Cursor)),
            antigravity: AntigravityAdapter::new(provider_roots(config, Provider::Antigravity)),
            pi: PiAdapter::new(provider_roots(config, Provider::Pi)),
            prime_agent: PiAdapter::prime_agent(provider_roots(config, Provider::PrimeAgent)),
            aistudio: AiStudioAdapter::new(provider_roots(config, Provider::AiStudio)),
            gemini_cli: GeminiCliAdapter::new(provider_roots(config, Provider::GeminiCli)),
        }
    }

    /// Parse one discovered file with the adapter that owns its provider.
    ///
    /// One dispatch shared by the indexer and by `diagnostics::explain_unindexed`, so a
    /// diagnosis of why a file produced no session parses it exactly as indexing would. A
    /// second copy of this match would let the two disagree, which is precisely the failure a
    /// reconciliation diagnostic exists to rule out.
    pub(crate) fn parse(&self, source: &SourceFile) -> Result<crate::models::ParsedSession> {
        self.parse_until(source, &|| false)?
            .ok_or_else(|| anyhow::anyhow!("provider parse cancelled with a non-cancelling token"))
    }

    pub(crate) fn parse_until(
        &self,
        source: &SourceFile,
        should_cancel: &dyn Fn() -> bool,
    ) -> Result<Option<crate::models::ParsedSession>> {
        if should_cancel() {
            return Ok(None);
        }
        let parsed = match source.provider {
            Provider::Claude | Provider::ClaudeDesktop => {
                parse_jsonl_until(source, should_cancel, |reader| {
                    self.claude.parse_reader(reader, &source.path)
                })
            }
            Provider::Codex => parse_jsonl_until(source, should_cancel, |reader| {
                self.codex.parse_reader(reader, &source.path)
            }),
            Provider::Cursor => parse_jsonl_until(source, should_cancel, |reader| {
                self.cursor.parse_reader(reader, &source.path)
            }),
            Provider::Antigravity => parse_jsonl_until(source, should_cancel, |reader| {
                self.antigravity.parse_reader(reader, &source.path)
            }),
            Provider::Pi => parse_jsonl_until(source, should_cancel, |reader| {
                self.pi.parse_reader(reader, &source.path)
            }),
            Provider::PrimeAgent => parse_jsonl_until(source, should_cancel, |reader| {
                self.prime_agent.parse_reader(reader, &source.path)
            }),
            Provider::AiStudio => read_snapshot_until(source, should_cancel, |raw| {
                self.aistudio.parse_raw(&source.path, raw)
            }),
            Provider::GeminiCli => read_snapshot_until(source, should_cancel, |raw| {
                self.gemini_cli.parse_raw(&source.path, raw)
            }),
        };
        if should_cancel() {
            return Ok(None);
        }
        let parsed = parsed?;
        if let Some(reason) = total_parse_failure_reason(&parsed) {
            bail!("{reason}");
        }
        Ok(Some(parsed))
    }

    pub(crate) fn discover_enabled(&self, config: &Config) -> DiscoveryResult {
        let mut discovered = DiscoveryResult::default();
        if config.providers.claude.enabled {
            discovered.extend_provider(Provider::Claude, self.claude.discover_with_warnings());
        }
        if config.providers.claude_desktop.enabled {
            discovered.extend_provider(
                Provider::ClaudeDesktop,
                self.claude_desktop.discover_with_warnings(),
            );
        }
        if config.providers.codex.enabled {
            discovered.extend_provider(Provider::Codex, self.codex.discover_with_warnings());
        }
        if config.providers.cursor.enabled {
            discovered.extend_provider(Provider::Cursor, self.cursor.discover_with_warnings());
        }
        if config.providers.antigravity.enabled {
            discovered.extend_provider(
                Provider::Antigravity,
                self.antigravity.discover_with_warnings(),
            );
        }
        if config.providers.pi.enabled {
            discovered.extend_provider(Provider::Pi, self.pi.discover_with_warnings());
        }
        if config.providers.prime_agent.enabled {
            discovered.extend_provider(
                Provider::PrimeAgent,
                self.prime_agent.discover_with_warnings(),
            );
        }
        if config.providers.aistudio.enabled {
            discovered.extend_provider(Provider::AiStudio, self.aistudio.discover_with_warnings());
        }
        if config.providers.gemini_cli.enabled {
            discovered.extend_provider(
                Provider::GeminiCli,
                self.gemini_cli.discover_with_warnings(),
            );
        }
        deduplicate_warnings(&mut discovered.warnings);
        discovered
    }
}

struct CancellationReader<'a, R> {
    inner: R,
    should_cancel: &'a dyn Fn() -> bool,
}

impl<R: Read> Read for CancellationReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if (self.should_cancel)() {
            return Err(std::io::Error::other("session parse cancelled"));
        }
        self.inner.read(buffer)
    }
}

fn parse_jsonl_until(
    source: &SourceFile,
    should_cancel: &dyn Fn() -> bool,
    parse: impl FnOnce(
        BufReader<CancellationReader<'_, std::fs::File>>,
    ) -> Result<crate::models::ParsedSession>,
) -> Result<crate::models::ParsedSession> {
    let file = std::fs::File::open(&source.path)?;
    parse(BufReader::new(CancellationReader {
        inner: file,
        should_cancel,
    }))
}

fn read_snapshot_until(
    source: &SourceFile,
    should_cancel: &dyn Fn() -> bool,
    parse: impl FnOnce(String) -> Result<crate::models::ParsedSession>,
) -> Result<crate::models::ParsedSession> {
    let file = std::fs::File::open(&source.path)?;
    let mut reader = CancellationReader {
        inner: file,
        should_cancel,
    };
    let mut raw = String::new();
    reader.read_to_string(&mut raw)?;
    parse(raw)
}

fn total_parse_failure_reason(parsed: &crate::models::ParsedSession) -> Option<String> {
    if parsed.session.preview_text == "(parse failed)" {
        return Some(
            parsed
                .session
                .parse_warning
                .clone()
                .unwrap_or_else(|| "provider parser failed without a reason".to_string()),
        );
    }
    let metadata = parsed.session.raw_metadata_json.as_deref()?;
    let metadata: serde_json::Value = serde_json::from_str(metadata).ok()?;
    let malformed = metadata
        .get("malformed_line_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let valid_records = metadata
        .get("valid_record_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    (malformed > 0 && valid_records == 0).then(|| {
        parsed
            .session
            .parse_warning
            .clone()
            .unwrap_or_else(|| "session contained no parseable JSONL records".to_string())
    })
}

impl DiscoveryResult {
    fn extend_provider(&mut self, provider: Provider, discovered: ProviderDiscovery) {
        let readable_sources_preserved = !discovered.sources.is_empty();
        self.warnings
            .extend(discovered.warnings.into_iter().map(|warning| {
                let verification_command = "aise doctor --format json".to_string();
                ProviderDiscoveryWarning {
                    provider,
                    path: normalize_path(&warning.path),
                    operation: warning.operation.to_string(),
                    message: warning.message,
                    readable_sources_preserved,
                    guidance: discovery_warning_guidance(
                        readable_sources_preserved,
                        &verification_command,
                    ),
                    verification_command,
                }
            }));
        let mut seen = self
            .sources
            .iter()
            .map(source_identity)
            .collect::<HashSet<_>>();
        for source in discovered.sources {
            if seen.insert(source_identity(&source)) {
                self.sources.push(source);
            }
        }
    }
}

fn discovery_warning_guidance(preserved: bool, verification_command: &str) -> String {
    let preservation = if preserved {
        "Readable sources from this provider were preserved for indexing."
    } else {
        "No readable sources from this provider were discovered."
    };
    format!("{preservation} Run `{verification_command}` to verify discovery status.")
}

fn source_identity(source: &SourceFile) -> (Provider, String) {
    let path = std::fs::canonicalize(&source.path).unwrap_or_else(|_| source.path.clone());
    (source.provider, normalize_path(&path))
}

fn deduplicate_warnings(warnings: &mut Vec<ProviderDiscoveryWarning>) {
    let mut seen = HashSet::new();
    warnings.retain(|warning| {
        seen.insert((
            warning.provider,
            warning.path.clone(),
            warning.operation.clone(),
            warning.message.clone(),
        ))
    });
}

/// Discover enabled providers and report every provider's effective configuration.
pub fn inventory(config: &Config) -> Vec<ProviderSourceStatus> {
    inventory_snapshot(config).providers
}

pub(crate) fn inventory_snapshot(config: &Config) -> SourceInventory {
    let DiscoveryResult { sources, warnings } = ProviderSet::new(config).discover_enabled(config);
    let providers = PROVIDERS
        .into_iter()
        .map(|provider| ProviderSourceStatus {
            provider,
            enabled: provider_enabled(config, provider),
            roots: provider_roots(config, provider)
                .into_iter()
                .map(|path| normalize_path(&path))
                .collect(),
            discovered_files: sources
                .iter()
                .filter(|source| source.provider == provider)
                .count(),
            warnings: warnings
                .iter()
                .filter(|warning| warning.provider == provider)
                .cloned()
                .collect(),
        })
        .collect();
    let discovered = sources
        .into_iter()
        .map(|source| (source.provider, normalize_path(&source.path)))
        .collect();
    SourceInventory {
        providers,
        discovered,
        warnings,
    }
}

pub(crate) fn provider_enabled(config: &Config, provider: Provider) -> bool {
    match provider {
        Provider::Claude => config.providers.claude.enabled,
        Provider::ClaudeDesktop => config.providers.claude_desktop.enabled,
        Provider::Codex => config.providers.codex.enabled,
        Provider::Cursor => config.providers.cursor.enabled,
        Provider::Antigravity => config.providers.antigravity.enabled,
        Provider::Pi => config.providers.pi.enabled,
        Provider::PrimeAgent => config.providers.prime_agent.enabled,
        Provider::AiStudio => config.providers.aistudio.enabled,
        Provider::GeminiCli => config.providers.gemini_cli.enabled,
    }
}

pub(crate) fn provider_roots(config: &Config, provider: Provider) -> Vec<std::path::PathBuf> {
    let configured = match provider {
        Provider::Claude => config.claude_paths(),
        Provider::ClaudeDesktop => config.claude_desktop_paths(),
        Provider::Codex => config.codex_paths(),
        Provider::Cursor => config.cursor_paths(),
        Provider::Antigravity => config.antigravity_paths(),
        Provider::Pi => config.pi_paths(),
        Provider::PrimeAgent => config.prime_agent_paths(),
        Provider::AiStudio => config.aistudio_paths(),
        Provider::GeminiCli => config.gemini_cli_paths(),
    };
    canonical_unique_roots(configured)
}

fn canonical_unique_roots(paths: Vec<std::path::PathBuf>) -> Vec<std::path::PathBuf> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter_map(|path| {
            let identity = std::fs::canonicalize(&path).unwrap_or(path);
            seen.insert(identity.clone()).then_some(identity)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_parse_stops_inside_a_large_source_when_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.jsonl");
        let content = format!(
            "{{\"type\":\"user\",\"sessionId\":\"cancel\",\"message\":{{\"role\":\"user\",\"content\":{}}}}}\n",
            serde_json::to_string(&"x".repeat(128 * 1024)).unwrap()
        );
        std::fs::write(&path, content).unwrap();
        let source = SourceFile {
            provider: Provider::Claude,
            path: path.clone(),
            mtime_ns: 0,
            size_bytes: std::fs::metadata(&path).unwrap().len() as i64,
        };
        let config = Config::default();
        let providers = ProviderSet::new(&config);
        let checks = std::cell::Cell::new(0usize);

        let parsed = providers
            .parse_until(&source, &|| {
                let next = checks.get() + 1;
                checks.set(next);
                next >= 4
            })
            .unwrap();

        assert!(
            parsed.is_none(),
            "cancellation must not publish a partial parse"
        );
        assert!(checks.get() >= 4, "the token was checked during file reads");
    }

    #[test]
    fn snapshot_parse_stops_inside_a_large_source_read_when_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.md");
        std::fs::write(&path, "x".repeat(128 * 1024)).unwrap();
        let source = SourceFile {
            provider: Provider::AiStudio,
            path: path.clone(),
            mtime_ns: 0,
            size_bytes: std::fs::metadata(&path).unwrap().len() as i64,
        };
        let providers = ProviderSet::new(&Config::default());
        let checks = std::cell::Cell::new(0usize);

        let parsed = providers
            .parse_until(&source, &|| {
                let next = checks.get() + 1;
                checks.set(next);
                next >= 4
            })
            .unwrap();

        assert!(parsed.is_none());
        assert!(checks.get() >= 4);
    }

    #[test]
    fn valid_metadata_record_is_not_a_total_failure_when_another_line_is_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata-only.jsonl");
        std::fs::write(&path, "{\"type\":\"metadata\"}\n{partial\n").unwrap();
        let source = SourceFile {
            provider: Provider::Claude,
            path: path.clone(),
            mtime_ns: 0,
            size_bytes: std::fs::metadata(&path).unwrap().len() as i64,
        };

        let parsed = ProviderSet::new(&Config::default()).parse(&source).unwrap();

        assert!(parsed.messages.is_empty());
        assert_eq!(
            parsed.session.parse_warning.as_deref(),
            Some("skipped 1 malformed JSONL record")
        );
    }

    fn disable_all(config: &mut Config) {
        config.providers.claude.enabled = false;
        config.providers.claude_desktop.enabled = false;
        config.providers.codex.enabled = false;
        config.providers.cursor.enabled = false;
        config.providers.antigravity.enabled = false;
        config.providers.pi.enabled = false;
        config.providers.prime_agent.enabled = false;
        config.providers.aistudio.enabled = false;
        config.providers.gemini_cli.enabled = false;
    }

    #[test]
    fn parser_contract_fingerprint_changes_with_membership_order_or_version() {
        let baseline = provider_parse_contract_fingerprint_from([
            (Provider::Claude, "claude-v4"),
            (Provider::Codex, "codex-v5"),
        ]);
        assert_ne!(
            baseline,
            provider_parse_contract_fingerprint_from([
                (Provider::Claude, "claude-v5"),
                (Provider::Codex, "codex-v5"),
            ])
        );
        assert_ne!(
            baseline,
            provider_parse_contract_fingerprint_from([
                (Provider::Codex, "codex-v5"),
                (Provider::Claude, "claude-v4"),
            ])
        );
    }

    #[test]
    fn inventory_includes_all_disabled_providers_without_discovery() {
        let mut config = Config::default();
        disable_all(&mut config);

        let statuses = inventory(&config);

        assert_eq!(statuses.len(), PROVIDERS.len());
        assert!(statuses.iter().all(|status| !status.enabled));
        assert!(statuses.iter().all(|status| status.discovered_files == 0));
    }

    #[test]
    fn inventory_discovers_enabled_snapshot_providers() {
        let dir = tempfile::tempdir().unwrap();
        let aistudio = dir.path().join("aistudio");
        let gemini = dir.path().join("gemini");
        std::fs::create_dir_all(&aistudio).unwrap();
        std::fs::create_dir_all(gemini.join("project/chats")).unwrap();
        std::fs::write(aistudio.join("chat.json"), "{}").unwrap();
        std::fs::write(gemini.join("project/chats/session-one.json"), "{}").unwrap();
        let mut config = Config::default();
        disable_all(&mut config);
        config.providers.aistudio.enabled = true;
        config.providers.aistudio.paths = vec![aistudio.to_string_lossy().into_owned()];
        config.providers.gemini_cli.enabled = true;
        config.providers.gemini_cli.paths = vec![gemini.to_string_lossy().into_owned()];

        let statuses = inventory(&config);

        assert_eq!(
            statuses
                .iter()
                .find(|status| status.provider == Provider::AiStudio)
                .unwrap()
                .discovered_files,
            1
        );
        assert_eq!(
            statuses
                .iter()
                .find(|status| status.provider == Provider::GeminiCli)
                .unwrap()
                .discovered_files,
            1
        );
    }

    #[test]
    fn inventory_canonicalizes_alias_roots_before_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("claude");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("chat.json"), "{}").unwrap();
        let mut config = Config::default();
        disable_all(&mut config);
        config.providers.aistudio.enabled = true;
        config.providers.aistudio.paths = vec![
            root.to_string_lossy().into_owned(),
            root.join(".").to_string_lossy().into_owned(),
        ];

        let status = inventory(&config)
            .into_iter()
            .find(|status| status.provider == Provider::AiStudio)
            .unwrap();

        assert_eq!(status.roots.len(), 1);
        assert_eq!(status.discovered_files, 1);
    }

    #[test]
    fn discovery_deduplicates_alias_sources_in_first_encounter_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("aistudio");
        std::fs::create_dir_all(&root).unwrap();
        let first = root.join("first.json");
        let second = root.join("second.json");
        std::fs::write(&first, "{}").unwrap();
        std::fs::write(&second, "{}").unwrap();
        let first = std::fs::canonicalize(first).unwrap();
        let second = std::fs::canonicalize(second).unwrap();
        let mut config = Config::default();
        disable_all(&mut config);
        config.providers.aistudio.enabled = true;
        config.providers.aistudio.paths = vec![
            root.to_string_lossy().into_owned(),
            root.join(".").to_string_lossy().into_owned(),
        ];

        let discovered = ProviderSet::new(&config).discover_enabled(&config);

        assert_eq!(discovered.sources.len(), 2);
        assert_eq!(discovered.sources[0].path, first);
        assert_eq!(discovered.sources[1].path, second);
    }

    #[cfg(unix)]
    #[test]
    fn discovery_keeps_readable_sources_and_reports_denied_subtrees() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("aistudio");
        let denied = root.join("denied");
        std::fs::create_dir_all(&denied).unwrap();
        let readable = root.join("readable.jsonl");
        std::fs::write(&readable, "{}").unwrap();
        let readable = std::fs::canonicalize(readable).unwrap();
        std::fs::write(denied.join("hidden.jsonl"), "{}").unwrap();
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o000)).unwrap();
        let mut config = Config::default();
        disable_all(&mut config);
        config.providers.claude.enabled = true;
        config.providers.claude.paths = vec![root.to_string_lossy().into_owned()];

        let discovered = ProviderSet::new(&config).discover_enabled(&config);

        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(discovered
            .sources
            .iter()
            .any(|source| source.path == readable));
        assert!(discovered.warnings.iter().any(|warning| {
            warning.provider == Provider::Claude
                && warning.path.contains("denied")
                && warning.operation == "traverse"
                && warning.readable_sources_preserved
                && warning.verification_command == "aise doctor --format json"
                && warning.guidance.contains("preserved for indexing")
        }));
    }

    #[test]
    fn missing_discovery_roots_remain_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        disable_all(&mut config);
        config.providers.aistudio.enabled = true;
        config.providers.aistudio.paths = vec![dir.path().join("absent").display().to_string()];

        let discovered = ProviderSet::new(&config).discover_enabled(&config);

        assert!(discovered.sources.is_empty());
        assert!(discovered.warnings.is_empty());
    }

    fn provider_fixture_path(
        provider: Provider,
        root: &std::path::Path,
        label: &str,
    ) -> std::path::PathBuf {
        let path = match provider {
            Provider::Claude => root.join(format!("{label}.jsonl")),
            Provider::ClaudeDesktop => root
                .join("local-agent-mode-sessions")
                .join(format!("local_{label}"))
                .join("audit.jsonl"),
            Provider::Codex => root.join(format!(
                "rollout-2026-08-04T00-00-00-019efd97-d602-7922-89dd-46727210650{}.jsonl",
                if label == "first" { "5" } else { "6" }
            )),
            Provider::Cursor => root
                .join("agent-transcripts")
                .join(format!("{label}.jsonl")),
            Provider::Antigravity => root
                .join(label)
                .join(".system_generated/logs/transcript.jsonl"),
            // Named the way pi names a transcript, after the session it holds. Discovery does not
            // require that, but a fixture that matches the real layout is the one worth testing.
            Provider::Pi => root.join(label).join(format!(
                "2026-06-18T17-31-17-343Z_019edbc9-83df-72a0-a95b-64e6d810ad7{}.jsonl",
                if label == "first" { "5" } else { "6" }
            )),
            Provider::PrimeAgent => root.join(format!(
                "019fea39-38c2-710e-8100-3624dfc0ac0{}.jsonl",
                if label == "first" { "7" } else { "8" }
            )),
            Provider::AiStudio => root.join(format!("{label}.json")),
            Provider::GeminiCli => root
                .join(label)
                .join("chats")
                .join(format!("session-{label}.json")),
        };
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}").unwrap();
        path
    }

    #[test]
    fn every_provider_discovers_two_root_ordered_union_once() {
        for provider in PROVIDERS {
            let dir = tempfile::tempdir().unwrap();
            // Deliberately reverse lexical order: first encounter, not sorting, is canonical.
            let first_root = dir.path().join("z-first");
            let second_root = dir.path().join("a-second");
            std::fs::create_dir_all(&first_root).unwrap();
            std::fs::create_dir_all(&second_root).unwrap();
            let first =
                std::fs::canonicalize(provider_fixture_path(provider, &first_root, "first"))
                    .unwrap();
            let second =
                std::fs::canonicalize(provider_fixture_path(provider, &second_root, "second"))
                    .unwrap();
            let paths = vec![
                first_root.display().to_string(),
                first_root.join(".").display().to_string(),
                second_root.display().to_string(),
            ];
            let mut config = Config::default();
            disable_all(&mut config);
            let provider_config = match provider {
                Provider::Claude => &mut config.providers.claude,
                Provider::ClaudeDesktop => &mut config.providers.claude_desktop,
                Provider::Codex => &mut config.providers.codex,
                Provider::Cursor => &mut config.providers.cursor,
                Provider::Antigravity => &mut config.providers.antigravity,
                Provider::Pi => &mut config.providers.pi,
                Provider::PrimeAgent => &mut config.providers.prime_agent,
                Provider::AiStudio => &mut config.providers.aistudio,
                Provider::GeminiCli => &mut config.providers.gemini_cli,
            };
            provider_config.enabled = true;
            provider_config.paths = paths;

            let discovered = ProviderSet::new(&config).discover_enabled(&config);

            assert!(
                discovered.warnings.is_empty(),
                "{provider}: {:?}",
                discovered.warnings
            );
            assert_eq!(
                discovered
                    .sources
                    .iter()
                    .map(|source| (source.provider, source.path.clone()))
                    .collect::<Vec<_>>(),
                vec![(provider, first), (provider, second)],
                "{provider} must preserve configured-root encounter order and dedupe aliases"
            );
        }
    }
}
