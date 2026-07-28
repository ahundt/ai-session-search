//! Typed application services shared by CLI, MCP, and language bindings.
//!
//! Services own operation boundaries, while [`Db`] remains the storage layer.
//! Adapters must not duplicate SQL, filtering, pagination, or lifecycle policy.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::num::NonZeroUsize;

use anyhow::{anyhow, bail, Context, Result};

use crate::config::{Config, IndexRefresh, ScoringConfig};
use crate::db::{Db, MessageBatchControl, SchemaState, MIN_READABLE_SCHEMA_VERSION};
use crate::indexer::{self, AutoReindexOutcome, IndexCoordinator};
use crate::message_search::{
    apply_message_presentation, attach_match_evidence, ContextWindow, DetailLevel, ExecutionOrder,
    FieldViewBudget, LineWindow, MatchViewBudget, MatchWindow, MessageResponsePlan,
    MessageRetrievalPlan, MessageSearchInclude, MessageSearchIncludedData,
    MessageSearchOrderedDigest, MessageSearchOrigins, MessageSearchPlan, MessageSearchRequest,
    MessageSearchResponse, MessageSearchRuntimeDiagnostics, PageInfo, ReceiptLevel, ResolvedExtent,
    ResolvedMessagePredicates, ResolvedMessagePresentation, ResolvedMessageSearchRequest,
    SearchSurface, ValueOrigin, DEFAULT_MATCH_EVIDENCE_MAX_CHARS,
};
use crate::models::{
    DiagnosticStatus, FileCrossRef, FileEditSummary, FileQuery, FileVersion, IndexStatus,
    MessageFilters, MessageHit, SearchExplain, SearchFilters, SearchHit, SessionMeta,
    SessionRecord,
};
use crate::search_scope::{EffectiveAccessScope, TrustedAccessInputs};

/// RAII application root shared by native frontends and language bindings.
///
/// Opening an instance applies the configured SQLite contention and performance
/// policy exactly once. Dropping it closes the owned database connection.
pub struct SessionSearch {
    config: Config,
    access: EffectiveAccessScope,
    db: Db,
}

#[cfg(test)]
mod analysis_service_tests {
    use super::*;
    use crate::models::{Message, MessageKind, Provider, Role};
    use crate::util::minimal_record;

    /// Index one user message per string and return the opened app, so a selection test can state
    /// its corpus in one line instead of rebuilding `ParsedSession` per case.
    fn app_with_user_messages(
        dir: &std::path::Path,
        config: Config,
        texts: &[&str],
    ) -> SessionSearch {
        let mut config = config;
        config.index.db_path = Some(dir.join("index.db").to_string_lossy().into_owned());
        config.index.cache_dir = Some(dir.join("cache").to_string_lossy().into_owned());
        let app = SessionSearch::open(config).unwrap();
        let mut parsed = minimal_record(
            Provider::Claude,
            std::path::Path::new("/fixture/selection.jsonl"),
            String::new(),
        );
        parsed.session.id = "claude:selection".into();
        parsed.session.provider_session_id = "selection".into();
        parsed.messages = texts
            .iter()
            .enumerate()
            .map(|(seq, text)| Message {
                seq: seq as i64,
                role: Role::User,
                ts: None,
                tool_name: None,
                kind: MessageKind::Conversation,
                tool_call_id: None,
                is_compaction: false,
                content: (*text).to_string(),
            })
            .collect();
        app.database().upsert_session(&parsed, 0, 0).unwrap();
        app
    }

    /// Write a minimal standard-shaped skill package with one classification category.
    fn write_skill(search_root: &std::path::Path, name: &str, category: &str, pattern: &str) {
        let root = search_root.join(name);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: test skill\nmetadata:\n  version: 2.1.0\n---\n\nbody\n"
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("capability.toml"),
            format!(
                "schema_version = 1\nkind = \"message-classification\"\n\n\
                 [[categories]]\nname = \"{category}\"\npatterns = [\'\'\'{pattern}\'\'\']\n"
            ),
        )
        .unwrap();
    }

    fn classify_messages(
        analysis: &AnalysisService<'_>,
        skill: crate::skill_catalog::SkillSelector,
        filters: MessageFilters,
    ) -> crate::corrections::MessageClassificationReport {
        let report = analysis
            .run_skill(&crate::skill_run::SkillRunQuery {
                skill,
                input: crate::skill_run::SkillCapabilityInput::MessageClassification(
                    crate::skill_run::MessageClassificationQuery {
                        filters,
                        additional_skills: Vec::new(),
                    },
                ),
            })
            .unwrap();
        let crate::skill_run::SkillCapabilityOutput::MessageClassification(output) = report.output;
        output.report
    }

    #[test]
    fn typed_skill_run_resolves_one_descriptor_and_returns_tagged_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let skills_root = dir.path().join("skills");
        let skill_root = skills_root.join("my-review");
        std::fs::create_dir_all(&skill_root).unwrap();
        std::fs::write(
            skill_root.join("SKILL.md"),
            "---\nname: my-review\ndescription: test classification\nmetadata:\n  version: \
             2.1.0\n---\n\ninstructions\n",
        )
        .unwrap();
        std::fs::write(
            skill_root.join("capability.toml"),
            "schema_version = 1\nkind = \"message-classification\"\n\n\
             [[categories]]\nname = \"clobber\"\npatterns = ['''\\byou overwrote\\b''']\n",
        )
        .unwrap();

        let mut config = Config::default();
        config.skills.search_paths = vec![skills_root.to_string_lossy().into_owned()];
        let app = app_with_user_messages(dir.path(), config, &["you overwrote the notes"]);
        let report = app
            .analysis()
            .run_skill(&crate::skill_run::SkillRunQuery {
                skill: crate::skill_catalog::SkillSelector::Name(
                    crate::skill_catalog::SkillNameSelector {
                        name: crate::skill_catalog::SkillName::try_from("my-review".to_string())
                            .unwrap(),
                    },
                ),
                input: crate::skill_run::SkillCapabilityInput::MessageClassification(
                    crate::skill_run::MessageClassificationQuery {
                        filters: MessageFilters::default(),
                        additional_skills: Vec::new(),
                    },
                ),
            })
            .unwrap();

        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["requested_selector"]["name"], "my-review");
        assert_eq!(value["resolved_skill"]["name"], "my-review");
        assert_eq!(value["output"]["capability"], "message-classification");
        assert_eq!(
            value["output"]["result"]["report"]["matches"][0]["category"],
            "clobber"
        );
        assert_eq!(
            value["output"]["result"]["receipt"]["sha256"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        assert_eq!(
            value["output"]["result"]["receipt"],
            value["output"]["result"]["report"]["policies"][0],
            "the primary envelope receipt is derived from the first evaluated policy"
        );

        let duplicate = app
            .analysis()
            .run_skill(&crate::skill_run::SkillRunQuery {
                skill: crate::skill_catalog::SkillSelector::Name(
                    crate::skill_catalog::SkillNameSelector {
                        name: crate::skill_catalog::SkillName::try_from("my-review".to_string())
                            .unwrap(),
                    },
                ),
                input: crate::skill_run::SkillCapabilityInput::MessageClassification(
                    crate::skill_run::MessageClassificationQuery {
                        filters: MessageFilters::default(),
                        additional_skills: vec![crate::skill_catalog::SkillSelector::Path(
                            crate::skill_catalog::SkillPathSelector { path: skill_root },
                        )],
                    },
                ),
            })
            .expect_err("the same canonical skill selected by name and path must fail");
        assert!(
            duplicate.to_string().contains("selected more than once"),
            "{duplicate:#}"
        );
    }

    /// `--skill NAME` must REPLACE the built-in rules, not merge with them, and the report must
    /// say which policy produced each match.
    ///
    /// The two fixture messages are chosen so each policy matches exactly one and misses the
    /// other: `you overwrote` hits no built-in category, and `you forgot` hits no category the
    /// external skill defines. A merge, a silent fallback, or an ignored selection each produce a
    /// different count here, so this fails loudly rather than looking plausible.
    #[test]
    fn a_named_skill_replaces_the_built_in_rules_and_names_itself_in_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let skills_root = dir.path().join("skills");
        write_skill(
            &skills_root,
            "my-corrections",
            "clobber",
            r"\byou overwrote\b",
        );

        let mut config = Config::default();
        config.skills.search_paths = vec![skills_root.to_string_lossy().into_owned()];
        let app = app_with_user_messages(
            dir.path(),
            config,
            &["you overwrote my notes", "you forgot the tests"],
        );
        let analysis = app.analysis();

        let selected = classify_messages(
            &analysis,
            crate::skill_catalog::SkillSelector::name("my-corrections").unwrap(),
            MessageFilters::default(),
        );
        assert_eq!(
            selected
                .matches
                .iter()
                .map(|hit| (
                    hit.policy_name.as_str(),
                    hit.category.as_str(),
                    hit.matched_text.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![("my-corrections", "clobber", "you overwrote")],
            "only the selected skill's categories may apply"
        );
        assert_eq!(
            selected
                .policies
                .iter()
                .map(|receipt| (receipt.name.as_str(), receipt.version.as_str()))
                .collect::<Vec<_>>(),
            vec![("my-corrections", "2.1.0")],
            "the receipt reports the policy's own version, not the aise version"
        );
        assert_eq!(
            selected.policies[0].sha256.len(),
            64,
            "a receipt without a digest cannot reproduce a run"
        );

        let defaulted = classify_messages(
            &analysis,
            crate::skill_catalog::SkillSelector::name("corrections").unwrap(),
            MessageFilters::default(),
        );
        assert_eq!(
            defaulted
                .matches
                .iter()
                .map(|hit| (hit.policy_name.as_str(), hit.category.as_str()))
                .collect::<Vec<_>>(),
            vec![(crate::corrections::EMBEDDED_POLICY_NAME, "skip_step")],
            "omitting --skill must evaluate the embedded policy, and ONLY it"
        );
    }

    /// An unknown `--skill` name must fail, naming the value and where to look up valid ones.
    ///
    /// The dangerous alternative is not a bad message: it is returning `Ok` with default-policy
    /// results, which looks like a successful run against rules the caller never selected.
    #[test]
    fn an_unknown_skill_name_fails_instead_of_silently_using_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_with_user_messages(dir.path(), Config::default(), &["you forgot the tests"]);
        let error = app
            .analysis()
            .run_skill(&crate::skill_run::SkillRunQuery {
                skill: crate::skill_catalog::SkillSelector::name("not-installed").unwrap(),
                input: crate::skill_run::SkillCapabilityInput::MessageClassification(
                    crate::skill_run::MessageClassificationQuery::default(),
                ),
            })
            .expect_err("an unknown skill must not resolve to the defaults");
        let message = format!("{error:#}");
        assert!(
            message.contains("not-installed") && message.contains("aise skills list"),
            "the error must name the value and how to find valid ones: {message}"
        );
    }

    #[test]
    fn analysis_service_reuses_indexed_correction_planning_and_role_queries() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().into_owned());
        config.index.cache_dir = Some(dir.path().join("cache").to_string_lossy().into_owned());
        let app = SessionSearch::open(config).unwrap();
        let mut parsed = minimal_record(
            Provider::Claude,
            std::path::Path::new("/fixture/session.jsonl"),
            String::new(),
        );
        parsed.session.id = "claude:analysis".into();
        parsed.session.provider_session_id = "analysis".into();
        parsed.messages = vec![
            Message {
                seq: 0,
                role: Role::User,
                ts: None,
                tool_name: None,
                kind: MessageKind::Conversation,
                tool_call_id: None,
                is_compaction: false,
                content: "actually, that is wrong".into(),
            },
            Message {
                seq: 1,
                role: Role::Slash,
                ts: None,
                tool_name: None,
                kind: MessageKind::Conversation,
                tool_call_id: None,
                is_compaction: false,
                content: "/plan verify migration".into(),
            },
        ];
        app.database().upsert_session(&parsed, 0, 0).unwrap();
        let mut other = minimal_record(
            Provider::Codex,
            std::path::Path::new("/fixture/codex.jsonl"),
            String::new(),
        );
        other.session.id = "codex:analysis".into();
        other.session.provider_session_id = "analysis".into();
        other.messages = vec![Message {
            seq: 0,
            role: Role::User,
            ts: None,
            tool_name: None,
            kind: MessageKind::Conversation,
            tool_call_id: None,
            is_compaction: false,
            content: "unrelated provider message".into(),
        }];
        app.database().upsert_session(&other, 0, 0).unwrap();

        let analysis = app.analysis();
        let filters = MessageFilters::default();
        let corrections = classify_messages(
            &analysis,
            crate::skill_catalog::SkillSelector::name("corrections").unwrap(),
            filters.clone(),
        );
        assert_eq!(corrections.matches.len(), 1);
        // A default run evaluates exactly the embedded policy, and says so in its receipt.
        assert_eq!(
            corrections
                .policies
                .iter()
                .map(|receipt| receipt.name.as_str())
                .collect::<Vec<_>>(),
            vec![crate::corrections::EMBEDDED_POLICY_NAME]
        );
        assert_eq!(
            corrections.matches[0].policy_name,
            crate::corrections::EMBEDDED_POLICY_NAME
        );
        let planning = analysis.planning(&filters, &["^/plan$".into()]).unwrap();
        assert_eq!(planning.len(), 1);
        assert_eq!(planning[0].command, "/plan");
        let roles = analysis.role_statistics(&filters).unwrap();
        assert_eq!(roles.len(), 2);
        assert_eq!(roles.iter().map(|row| row.count).sum::<i64>(), 3);
        let provider_roles = analysis
            .role_statistics(&MessageFilters {
                provider: Some(Provider::Claude),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(provider_roles.iter().map(|row| row.count).sum::<i64>(), 2);
    }

    #[test]
    fn analysis_run_matches_paged_documents_reference_on_indexed_sessions() {
        use crate::analysis_pipeline::{
            AnalysisPolicySpec, ClassificationRuleSpec, ClassificationTarget, PhraseTextMode,
            PhraseVocabularyPolicySpec,
        };

        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().into_owned());
        config.index.cache_dir = Some(dir.path().join("cache").to_string_lossy().into_owned());
        let app = SessionSearch::open(config).unwrap();
        // Multi-message sessions with junction-spanning phrases and a code fence that
        // opens in one message and closes in the next.
        for (id, provider, texts) in [
            (
                "claude:multi",
                Provider::Claude,
                vec!["use tdd across chunks", "tdd again across chunks"],
            ),
            (
                "codex:fence",
                Provider::Codex,
                vec!["prose one\n```", "code hidden\n```\nprose two"],
            ),
            ("gemini-cli:empty", Provider::GeminiCli, vec![]),
        ] {
            let mut parsed = minimal_record(
                Provider::Claude,
                std::path::Path::new("/fixture/session.jsonl"),
                String::new(),
            );
            parsed.session.provider = provider;
            parsed.session.id = id.into();
            parsed.session.provider_session_id = id.into();
            parsed.messages = texts
                .iter()
                .enumerate()
                .map(|(seq, text)| Message {
                    seq: seq as i64,
                    role: Role::User,
                    ts: None,
                    tool_name: None,
                    kind: MessageKind::Conversation,
                    tool_call_id: None,
                    is_compaction: false,
                    content: (*text).into(),
                })
                .collect();
            app.database().upsert_session(&parsed, 0, 0).unwrap();
        }
        let policy = AnalysisPolicySpec {
            classification_rules: vec![ClassificationRuleSpec {
                dimension: "technique".into(),
                label: "tdd".into(),
                target: ClassificationTarget::UserText,
                pattern: "(?i)\\btdd\\b".into(),
                weight: 1,
            }],
            relationship_rules: vec![],
            phrase_vocabulary: Some(PhraseVocabularyPolicySpec {
                widths: vec![2],
                max_unique_phrases: 1000,
                min_document_tokens: 0,
                excluded_tokens: vec![],
                exclude_numeric_tokens: true,
                text_mode: PhraseTextMode::ProseOnly,
            }),
            max_classification_chars: None,
        }
        .compile()
        .unwrap();
        let filters = SearchFilters::default();

        let streamed = app.analysis().run(&filters, &policy).unwrap();

        // Reference: the public paged-documents API still materializes joined text; the
        // streaming run must be indistinguishable from analyzing those documents.
        let limited = SearchFilters {
            limit: 2,
            ..filters
        };
        let mut documents = Vec::new();
        let mut cursor = None;
        loop {
            let page = app.analysis().documents(&limited, cursor.as_ref()).unwrap();
            documents.extend(page.documents);
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        let reference = policy.analyze(documents).unwrap();

        assert_eq!(
            serde_json::to_value(&streamed).unwrap(),
            serde_json::to_value(&reference).unwrap()
        );
        assert!(!streamed.vocabulary.is_empty());
        assert!(streamed.sessions["claude:multi"].has_user_text);
        assert!(!streamed.sessions["gemini-cli:empty"].has_user_text);
    }

    #[test]
    fn analysis_run_streams_one_snapshot_and_resolves_across_pages() {
        use crate::analysis_pipeline::{
            AnalysisPolicy, ClassificationRuleSpec, ClassificationTarget, RelationshipKind,
            RelationshipResolution, RelationshipRuleSpec,
        };

        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().into_owned());
        config.index.cache_dir = Some(dir.path().join("cache").to_string_lossy().into_owned());
        let app = SessionSearch::open(config).unwrap();
        for (id, provider, title, text) in [
            ("claude:root", Provider::Claude, "Root", "use TDD"),
            ("codex:root", Provider::Codex, "Root", ""),
            (
                "gemini-cli:child",
                Provider::GeminiCli,
                "Branch of Root",
                "use TDD",
            ),
        ] {
            let mut parsed = minimal_record(
                provider,
                std::path::Path::new("/fixture/session.jsonl"),
                String::new(),
            );
            parsed.session.id = id.into();
            parsed.session.provider_session_id = id.into();
            parsed.session.title = Some(title.into());
            if !text.is_empty() {
                parsed.messages.push(Message {
                    seq: 0,
                    role: Role::User,
                    ts: None,
                    tool_name: None,
                    kind: MessageKind::Conversation,
                    tool_call_id: None,
                    is_compaction: false,
                    content: text.into(),
                });
            }
            app.database().upsert_session(&parsed, 0, 0).unwrap();
        }
        let policy = AnalysisPolicy::compile(
            vec![ClassificationRuleSpec {
                dimension: "technique".into(),
                label: "tdd".into(),
                target: ClassificationTarget::UserText,
                pattern: "(?i)\\btdd\\b".into(),
                weight: 1,
            }],
            vec![RelationshipRuleSpec {
                id: "branch_of".into(),
                kind: RelationshipKind::Branch,
                pattern: "^Branch of (?P<parent>.+)$".into(),
            }],
        )
        .unwrap();
        let filters = SearchFilters::default();
        let result = app
            .analysis()
            .run_with_session_batch_size(&filters, std::num::NonZeroUsize::new(1).unwrap(), &policy)
            .unwrap();
        let automatic_result = app.analysis().run(&filters, &policy).unwrap();

        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            serde_json::to_value(&automatic_result).unwrap()
        );

        assert_eq!(result.sessions.len(), 3);
        assert_eq!(result.sessions["gemini-cli:child"].score, 1);
        assert_eq!(
            result.sessions["gemini-cli:child"].relationship_hints[0].resolution,
            RelationshipResolution::Ambiguous {
                session_ids: vec!["claude:root".into(), "codex:root".into()]
            }
        );

        let limited = SearchFilters {
            limit: 2,
            ..filters
        };
        assert_eq!(
            app.analysis()
                .run_with_session_batch_size(
                    &limited,
                    std::num::NonZeroUsize::new(1).unwrap(),
                    &policy,
                )
                .unwrap()
                .sessions
                .len(),
            2
        );
    }
}

impl SessionSearch {
    /// Load platform configuration and open the configured index.
    pub fn load() -> Result<Self> {
        Self::open(Config::load()?)
    }

    /// Open an index using an explicit configuration.
    ///
    /// `existing-only` opens a SQLite-enforced read-only handle and never creates directories,
    /// installs schema objects, refreshes sources, or changes journal policy. SQLite may create or
    /// update empty `-wal`/`-shm` coordination sidecars when opening a WAL database whose sidecars
    /// are absent; it does not modify the durable database contents. Other refresh modes
    /// may create a new current-schema database and later perform the configured incremental
    /// refresh. Newer, hybrid, and incomplete current schemas fail before an ordinary query.
    pub fn open(config: Config) -> Result<Self> {
        let inputs = TrustedAccessInputs::capture(&config.search.scope, Vec::new())?;
        Self::open_with_access_inputs(config, inputs)
    }

    /// Open an index with trusted runtime roots supplied by a harness integration.
    pub fn open_with_access_inputs(
        config: Config,
        access_inputs: TrustedAccessInputs,
    ) -> Result<Self> {
        let access = EffectiveAccessScope::resolve(&config.search.scope, access_inputs)?;
        let schema_state = IndexCoordinator::new(&config).inspect_schema()?;
        match schema_state {
            SchemaState::Missing if config.index.refresh == IndexRefresh::ExistingOnly => {
                anyhow::bail!(
                    "existing-only index {} does not exist; run `aise reindex --full` without --index-refresh existing-only",
                    config.db_path().display()
                )
            }
            SchemaState::Older { current, .. }
                if config.index.refresh == IndexRefresh::ExistingOnly
                    && current < MIN_READABLE_SCHEMA_VERSION =>
            {
                anyhow::bail!(
                    "existing-only index {} uses unreadable schema generation {current}; run `aise reindex --full` without --index-refresh existing-only",
                    config.db_path().display()
                )
            }
            SchemaState::Missing | SchemaState::Current | SchemaState::Older { .. } => {}
            SchemaState::Newer { current, supported } => anyhow::bail!(
                "index {} uses schema generation {current}, newer than this aise build supports ({supported}); upgrade aise before opening it",
                config.db_path().display()
            ),
            SchemaState::RepairableLayout { reason }
                if config.index.refresh == IndexRefresh::ExistingOnly =>
            {
                anyhow::bail!(
                    "existing-only index {} needs a one-time message-search index rebuild ({reason}); run a writable aise command (without --index-refresh existing-only) to self-heal it in place",
                    config.db_path().display()
                )
            }
            SchemaState::RepairableLayout { .. } => {
                // Elect one cross-process writer, re-inspect under the lock (a peer process may have
                // already healed it), and rebuild the derived message-search indexes online from the
                // intact base rows via the atomic exclusive migration. Then fall through to the
                // normal open below against the now-current schema.
                let heal_threads = NonZeroUsize::new(config.resolve_threads())
                    .expect("Config::resolve_threads always returns at least one");
                let heal_busy_timeout = config.index.busy_timeout_ms;
                crate::indexer::with_index_update_lock(&config, || {
                    if matches!(
                        IndexCoordinator::new(&config).inspect_schema()?,
                        SchemaState::RepairableLayout { .. }
                    ) {
                        let db = Db::open_with_threads(
                            &config.db_path(),
                            heal_busy_timeout,
                            heal_threads,
                        )?;
                        db.migrate_message_search_schema_exclusive()?;
                    }
                    Ok(())
                })?;
            }
            SchemaState::RecoveryRequired { reason } => anyhow::bail!(
                "index {} requires offline recovery: {reason}; stop AISE processes, then run `aise reindex --full`",
                config.db_path().display()
            ),
        }
        if config.index.refresh != IndexRefresh::ExistingOnly {
            fs::create_dir_all(config.cache_dir())?;
        }
        let worker_threads = NonZeroUsize::new(config.resolve_threads())
            .expect("Config::resolve_threads always returns at least one");
        let mut db = if config.index.refresh == IndexRefresh::ExistingOnly {
            Db::open_existing_read_only_with_threads(
                &config.db_path(),
                config.index.busy_timeout_ms,
                worker_threads,
            )?
        } else {
            Db::open_with_threads(
                &config.db_path(),
                config.index.busy_timeout_ms,
                worker_threads,
            )?
        };
        db.set_access_scope(access.clone());
        db.set_implicit_index_maintenance(config.index.refresh != IndexRefresh::ExistingOnly);
        Ok(Self { config, access, db })
    }

    /// Open with explicit maintenance authority even when implicit refresh is configured as
    /// `existing-only`. CLI `reindex` and `compact` are explicit write requests; ordinary query
    /// commands continue to receive a SQLite-enforced read-only handle. The stored configuration
    /// retains the user's selected refresh policy.
    pub(crate) fn open_for_maintenance(mut config: Config) -> Result<Self> {
        let refresh = config.index.refresh;
        config.index.refresh = IndexRefresh::BeforeQuery;
        let mut app = Self::open(config)?;
        app.config.index.refresh = refresh;
        Ok(app)
    }

    /// Effective configuration used by every service handle.
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// Immutable access authority used by every read service.
    pub const fn access_scope(&self) -> &EffectiveAccessScope {
        &self.access
    }

    /// Session catalog operations.
    pub const fn catalog(&self) -> CatalogService<'_> {
        CatalogService::new(&self.db)
    }

    /// Message search and context operations.
    pub const fn messages(&self) -> MessageService<'_> {
        MessageService::new(&self.config, &self.db, SearchSurface::Rust)
    }

    /// Construct message search for a language or protocol adapter's documented defaults.
    #[doc(hidden)]
    pub const fn messages_for_surface(&self, surface: SearchSurface) -> MessageService<'_> {
        MessageService::new(&self.config, &self.db, surface)
    }

    /// File history operations.
    pub const fn files(&self) -> FileService<'_> {
        FileService::new(&self.db)
    }

    /// Destination-independent session export operations.
    pub const fn exports(&self) -> ExportService<'_> {
        ExportService::new(&self.db)
    }

    /// Index lifecycle operations.
    pub const fn index(&self) -> IndexService<'_> {
        IndexService::new(&self.config, &self.db)
    }

    /// Index diagnostics and destructive maintenance operations.
    pub const fn maintenance(&self) -> MaintenanceService<'_> {
        MaintenanceService::new(&self.config, &self.db)
    }

    /// Effective provider roots and discovery status.
    pub const fn sources(&self) -> SourceService<'_> {
        SourceService::new(&self.config)
    }

    /// Indexed correction, planning, and statistics operations.
    pub const fn analysis(&self) -> AnalysisService<'_> {
        AnalysisService::new(&self.config, &self.db)
    }

    /// Crate-internal storage access for operations not yet represented by a service.
    ///
    /// External consumers (Python bindings, integration tests, downstream crates) must use
    /// the typed services above; raw [`Db`] access is not part of the supported API.
    pub(crate) const fn database(&self) -> &Db {
        &self.db
    }

    /// Install a frontend-specific progress sink.
    pub fn set_progress_reporter(&mut self, reporter: impl Fn(&str) + Send + Sync + 'static) {
        self.db.set_progress_reporter(reporter);
    }
}

#[cfg(test)]
mod execution_runtime_tests {
    use super::*;
    use crate::config::{SearchScopeConfig, SearchScopeMode};
    use crate::message_search::{MessageQuery, MessageTarget};
    use crate::models::{
        FileEdit, FileQuery, Message, MessageFilters, MessageKind, MessageSearchMode, Provider,
        Role, SearchFilters,
    };
    use crate::util::minimal_record;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    fn all_session_filters(limit: usize) -> SearchFilters {
        SearchFilters {
            limit,
            ..SearchFilters::default()
        }
    }

    #[test]
    fn ordinary_open_refuses_newer_schema_before_initialization_can_mutate_it() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("index.db");
        drop(Db::open(&db_path).unwrap());
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "user_version", crate::db::SCHEMA_VERSION + 1)
            .unwrap();
        let schema_before: String = conn
            .query_row(
                "select group_concat(coalesce(sql, ''), char(10))
                   from (select sql from sqlite_schema order by type, name)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        let mut config = Config::default();
        config.index.db_path = Some(db_path.to_string_lossy().into_owned());
        let error = SessionSearch::open(config)
            .err()
            .expect("newer schema must be rejected")
            .to_string();
        assert!(error.contains("newer than this aise build"), "{error}");

        let conn = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let schema_after: String = conn
            .query_row(
                "select group_concat(coalesce(sql, ''), char(10))
                   from (select sql from sqlite_schema order by type, name)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(schema_after, schema_before);
    }

    #[test]
    fn restricted_open_without_authoritative_roots_fails_before_database_creation() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("must-not-exist.db");
        let mut config = Config::default();
        config.index.db_path = Some(db_path.to_string_lossy().into_owned());
        config.search.scope = SearchScopeConfig {
            mode: SearchScopeMode::AllowedRoots,
            roots: Vec::new(),
            include_invocation_directory: false,
        };

        let error = SessionSearch::open_with_access_inputs(config, TrustedAccessInputs::default())
            .err()
            .expect("restricted open must fail")
            .to_string();

        assert!(error.contains("resolved no authoritative roots"), "{error}");
        assert!(!db_path.exists());
    }

    #[test]
    fn allowed_roots_scope_is_consistent_across_read_services_and_exact_ids() {
        let root = tempfile::tempdir().unwrap();
        let allowed = root.path().join("allowed");
        let outside = root.path().join("outside");
        let sibling = root.path().join("allowed-sibling");
        fs::create_dir_all(&allowed).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&sibling).unwrap();

        let mut config = Config::default();
        config.index.db_path = Some(root.path().join("index.db").to_string_lossy().into_owned());
        config.index.cache_dir = Some(root.path().join("cache").to_string_lossy().into_owned());
        config.search.scope = SearchScopeConfig {
            mode: SearchScopeMode::AllowedRoots,
            roots: vec![allowed.to_string_lossy().into_owned()],
            include_invocation_directory: false,
        };
        let app =
            SessionSearch::open_with_access_inputs(config, TrustedAccessInputs::default()).unwrap();

        let insert = |id: &str, workspace: &std::path::Path, transcript: &std::path::Path| {
            let mut parsed = minimal_record(Provider::Claude, transcript, String::new());
            parsed.session.id = id.into();
            parsed.session.provider_session_id = id.replace(':', "-");
            parsed.session.cwd = Some(workspace.to_string_lossy().into_owned());
            parsed.session.repo_root = Some(workspace.join("repo").to_string_lossy().into_owned());
            if id != "claude:allowed" {
                parsed.session.title = Some("scope needle scope needle scope needle".into());
            }
            parsed.session.preview_text = "scope needle".into();
            parsed.transcript_text = "scope needle transcript".into();
            parsed.messages = vec![Message {
                seq: 0,
                role: Role::User,
                ts: None,
                tool_name: None,
                kind: MessageKind::Conversation,
                tool_call_id: None,
                is_compaction: false,
                content: "scope needle message".into(),
            }];
            parsed.file_edits = vec![FileEdit {
                seq: 0,
                ts: None,
                tool: "Write".into(),
                file_path: workspace.join("scope.txt").to_string_lossy().into_owned(),
                file_name: "scope.txt".into(),
                new_content: Some(format!("content from {id}")),
                edits: Vec::new(),
            }];
            app.database().upsert_session(&parsed, 0, 0).unwrap();
        };

        insert(
            "claude:allowed",
            &allowed.join("project"),
            &outside.join("allowed-transcript.jsonl"),
        );
        // A transcript below an allowed root must not grant authority to an unrelated workspace.
        insert(
            "claude:hidden",
            &outside.join("project"),
            &allowed.join("hidden-transcript.jsonl"),
        );
        // Prefix matching must respect path components rather than string prefixes.
        insert(
            "claude:sibling",
            &sibling.join("project"),
            &outside.join("sibling-transcript.jsonl"),
        );

        let sessions = app
            .catalog()
            .list_sessions(&all_session_filters(0))
            .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "claude:allowed");
        let session_hits = app
            .catalog()
            .search_sessions(
                "scope needle",
                &all_session_filters(0),
                None,
                &ScoringConfig::default(),
            )
            .unwrap();
        assert_eq!(session_hits.len(), 1);
        assert_eq!(session_hits[0].session.id, "claude:allowed");
        let narrow_scoring = ScoringConfig::default();
        let bounded_hits = app
            .catalog()
            .search_sessions(
                "scope needle",
                &all_session_filters(1),
                None,
                &narrow_scoring,
            )
            .unwrap();
        assert_eq!(bounded_hits.len(), 1);
        assert_eq!(bounded_hits[0].session.id, "claude:allowed");

        let message_response = app
            .messages()
            .search(
                MessageSearchRequest::builder(
                    MessageQuery::literal("scope needle").unwrap(),
                    MessageTarget::content(),
                )
                .build()
                .unwrap(),
            )
            .unwrap();
        assert_eq!(message_response.hits().len(), 1);
        assert_eq!(message_response.hits()[0].session_id, "claude:allowed");
        assert!(app
            .messages()
            .context("claude:hidden", 0, 1, 1)
            .unwrap()
            .is_empty());
        assert!(app
            .messages()
            .session_metadata(&["claude:hidden".into()])
            .unwrap()
            .is_empty());

        let files = app.files().search(&FileQuery::default()).unwrap();
        assert_eq!(files.len(), 1);
        assert!(std::path::Path::new(&files[0].file_path).starts_with(&allowed));
        let cross_reference = app.files().cross_reference(&FileQuery::default()).unwrap();
        assert_eq!(cross_reference.len(), 1);
        assert_eq!(cross_reference[0].session_id, "claude:allowed");
        let history = app
            .files()
            .history("scope.txt", &FileQuery::default())
            .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].session_id, "claude:allowed");
        let reconstructed = app
            .files()
            .reconstruct("scope.txt", &FileQuery::default(), None)
            .unwrap();
        assert_eq!(reconstructed.session_id, "claude:allowed");
        assert_eq!(reconstructed.content, "content from claude:allowed");
        let analysis = app
            .analysis()
            .documents(&all_session_filters(10), None)
            .unwrap();
        assert_eq!(analysis.documents.len(), 1);
        assert_eq!(analysis.documents[0].session.id, "claude:allowed");

        assert!(app
            .exports()
            .render_full("claude:allowed", crate::export::ExportFormat::Json)
            .is_ok());
        let hidden_error = app
            .catalog()
            .resolve_session("claude:hidden")
            .unwrap_err()
            .to_string();
        assert!(hidden_error.contains("no session matches"));
        assert!(!hidden_error.contains("allowed-sibling"));
        assert!(!hidden_error.contains("outside"));
        assert!(app
            .exports()
            .render_full("claude:hidden", crate::export::ExportFormat::Json)
            .unwrap_err()
            .to_string()
            .contains("no session matches"));
    }

    #[test]
    fn applications_own_independent_worker_counts() {
        let root = tempfile::tempdir().unwrap();
        let mut one_config = Config::default();
        one_config.index.db_path = Some(root.path().join("one.db").to_string_lossy().into_owned());
        one_config.performance.threads = 1;
        let mut two_config = Config::default();
        two_config.index.db_path = Some(root.path().join("two.db").to_string_lossy().into_owned());
        two_config.performance.threads = 2;

        let one = SessionSearch::open(one_config).unwrap();
        let two = SessionSearch::open(two_config).unwrap();

        assert_eq!(one.database().worker_threads(), 1);
        assert_eq!(two.database().worker_threads(), 2);
    }

    #[test]
    fn existing_only_search_never_builds_the_lazy_trigram_index() {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.db_path = Some(root.path().join("index.db").to_string_lossy().into_owned());
        config.index.cache_dir = Some(root.path().join("cache").to_string_lossy().into_owned());
        let mut parsed = minimal_record(
            Provider::Claude,
            std::path::Path::new("/fixture/session.jsonl"),
            String::new(),
        );
        parsed.session.id = "claude:existing-only".into();
        parsed.session.provider_session_id = "existing-only".into();
        parsed.messages = vec![Message {
            seq: 0,
            role: Role::User,
            ts: None,
            tool_name: None,
            kind: MessageKind::Conversation,
            tool_call_id: None,
            is_compaction: false,
            content: "request failed with ECONNRESET".into(),
        }];
        let writer = SessionSearch::open(config.clone()).unwrap();
        writer.database().upsert_session(&parsed, 0, 0).unwrap();
        drop(writer);
        let conn = rusqlite::Connection::open_with_flags(
            config.db_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let schema_before: String = conn
            .query_row(
                "select group_concat(coalesce(sql, ''), char(10))
                   from (select sql from sqlite_schema order by type, name)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        config.index.refresh = IndexRefresh::ExistingOnly;
        let mut app = SessionSearch::open(config).unwrap();
        let write_error = app
            .database()
            .upsert_session(&parsed, 1, 1)
            .expect_err("existing-only database authority must reject writes")
            .to_string();
        assert!(
            write_error.contains("readonly") || write_error.contains("read-only"),
            "{write_error}"
        );

        let progress_calls = Arc::new(AtomicU32::new(0));
        let observed_calls = Arc::clone(&progress_calls);
        app.set_progress_reporter(move |_message| {
            observed_calls.fetch_add(1, Ordering::Relaxed);
        });
        let hits = app
            .messages()
            .search_legacy(
                "ECONNRESET",
                &MessageFilters {
                    match_mode: MessageSearchMode::Regex,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(
            hits.len(),
            1,
            "existing-only still scans the unindexed delta"
        );
        assert_eq!(
            progress_calls.load(Ordering::Relaxed),
            0,
            "existing-only must not start implicit trigram maintenance"
        );
        assert!(
            app.database()
                .vocabulary(true, 0)
                .unwrap()
                .iter()
                .any(|(term, _, _)| term == "eco" || term == "con" || term == "res"),
            "v4 trigram vocabulary remains readable without legacy maintenance"
        );
        drop(app);
        let conn = rusqlite::Connection::open_with_flags(
            root.path().join("index.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let schema_after: String = conn
            .query_row(
                "select group_concat(coalesce(sql, ''), char(10))
                   from (select sql from sqlite_schema order by type, name)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(schema_after, schema_before);
    }

    #[test]
    fn existing_only_open_refuses_missing_index_without_creating_paths() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("missing/index.db");
        let cache_path = root.path().join("missing/cache");
        let mut config = Config::default();
        config.index.db_path = Some(db_path.to_string_lossy().into_owned());
        config.index.cache_dir = Some(cache_path.to_string_lossy().into_owned());
        config.index.refresh = IndexRefresh::ExistingOnly;

        let error = SessionSearch::open(config)
            .err()
            .expect("missing existing-only index must fail")
            .to_string();

        assert!(error.contains("does not exist"), "{error}");
        assert!(error.contains("aise reindex --full"), "{error}");
        assert!(!db_path.exists());
        assert!(!cache_path.exists());
        assert!(!root.path().join("missing").exists());
    }
}

/// Typed indexed analysis shared by native adapters.
///
/// This service returns data and performs no terminal or filesystem I/O. It deliberately keeps
/// legacy recovery-directory counters out of the canonical API: statistics describe the shared
/// index and honor the same structural message filters as CLI search.
#[derive(Clone, Copy)]
pub struct AnalysisService<'app> {
    config: &'app Config,
    db: &'app Db,
}

const DEFAULT_ANALYSIS_SESSION_BATCH_SIZE: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(50).expect("analysis session batch constant is nonzero");

impl<'app> AnalysisService<'app> {
    /// Create an analysis service with configuration-backed capability and planning policy.
    pub const fn new(config: &'app Config, db: &'app Db) -> Self {
        Self { config, db }
    }

    pub(crate) fn corrections_with_resolved_policies(
        &self,
        filters: &crate::models::MessageFilters,
        policies: &crate::corrections::ResolvedCorrectionPolicySet,
    ) -> Result<crate::corrections::MessageClassificationReport> {
        let matches = self.db.find_corrections(policies, filters)?;
        Ok(crate::corrections::MessageClassificationReport {
            policies: policies.receipts(),
            matches,
        })
    }

    /// Resolve and execute one read-only deterministic skill capability.
    ///
    /// For `K` selected packages and `N` catalog entries, descriptor lookup is `O(K * N)`;
    /// canonical duplicate detection is expected `O(K)`. Only selected capability documents are
    /// compiled, under one aggregate byte budget. Message classification then uses the existing
    /// indexed correction query exactly once.
    pub fn run_skill(
        &self,
        query: &crate::skill_run::SkillRunQuery,
    ) -> Result<crate::skill_run::SkillRunReport> {
        let crate::skill_run::SkillCapabilityInput::MessageClassification(arguments) = &query.input;
        let embedded_primary = matches!(
            &query.skill,
            crate::skill_catalog::SkillSelector::Name(selector)
                if selector.name.as_str() == crate::corrections::EMBEDDED_POLICY_NAME
        );
        let roots = || {
            self.config
                .skills
                .search_paths
                .iter()
                .map(|root| crate::util::expand_tilde(root))
                .collect::<Vec<_>>()
        };

        let (resolved_skill, policies) = if embedded_primary {
            if arguments.additional_skills.iter().any(|selector| {
                matches!(
                    selector,
                    crate::skill_catalog::SkillSelector::Name(selector)
                        if selector.name.as_str() == crate::corrections::EMBEDDED_POLICY_NAME
                )
            }) {
                bail!("skill \"corrections\" was selected more than once");
            }
            let mut compiled = vec![crate::corrections::embedded_policy()?];
            if !arguments.additional_skills.is_empty() {
                let catalog = crate::skill_catalog::load_skill_catalog(&roots());
                let descriptors = crate::skill_catalog::resolve_skill_selectors(
                    &arguments.additional_skills,
                    &catalog,
                )?;
                if descriptors.iter().any(|descriptor| {
                    descriptor.frontmatter.as_ref().is_some_and(|frontmatter| {
                        frontmatter.name == crate::corrections::EMBEDDED_POLICY_NAME
                    })
                }) {
                    bail!(
                        "the embedded corrections skill and an installed corrections package \
                         identify the same capability; remove the duplicate --skill selector"
                    );
                }
                let additional =
                    crate::message_classification::compile_skill_descriptors(descriptors)?;
                compiled.extend(additional.policies().iter().cloned());
            }
            (
                crate::skill_run::ResolvedSkillReceipt {
                    name: crate::skill_catalog::SkillName::try_from(
                        crate::corrections::EMBEDDED_POLICY_NAME.to_string(),
                    )
                    .map_err(anyhow::Error::msg)?,
                    package_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                    selected_location: crate::skill_run::SelectedSkillLocation::Embedded,
                    execution_source: crate::skill_run::CapabilityExecutionSource::Embedded,
                },
                crate::corrections::ResolvedCorrectionPolicySet::from_policies(compiled),
            )
        } else {
            let catalog = crate::skill_catalog::load_skill_catalog(&roots());
            let selectors = std::iter::once(query.skill.clone())
                .chain(arguments.additional_skills.iter().cloned())
                .collect::<Vec<_>>();
            let descriptors = crate::skill_catalog::resolve_skill_selectors(&selectors, &catalog)?;
            let primary = descriptors
                .first()
                .context("a skill run requires one primary resolved skill")?;
            let frontmatter = primary
                .frontmatter
                .as_ref()
                .context("resolved skill has no valid frontmatter")?;
            let resolved_skill = crate::skill_run::ResolvedSkillReceipt {
                name: crate::skill_catalog::SkillName::try_from(frontmatter.name.clone())
                    .map_err(anyhow::Error::msg)?,
                package_version: frontmatter.metadata.get("version").cloned(),
                selected_location: crate::skill_run::SelectedSkillLocation::Path {
                    canonical_skill_md: primary.root.join("SKILL.md"),
                },
                execution_source: match &primary.capability {
                    crate::skill_catalog::CapabilityFileState::Available { path } => {
                        crate::skill_run::CapabilityExecutionSource::Path {
                            canonical_capability_toml: path.clone(),
                        }
                    }
                    crate::skill_catalog::CapabilityFileState::Absent => {
                        bail!(
                            "skill {:?} has no adjacent message-classification capability.toml; \
                             load its SKILL.md in an agent harness instead",
                            frontmatter.name
                        )
                    }
                    crate::skill_catalog::CapabilityFileState::Invalid { problem, .. } => {
                        bail!(
                            "skill {:?} has an invalid capability: {problem}",
                            frontmatter.name
                        )
                    }
                },
            };
            (
                resolved_skill,
                crate::message_classification::compile_skill_descriptors(descriptors)?,
            )
        };
        let report = self.corrections_with_resolved_policies(&arguments.filters, &policies)?;
        let receipt = report
            .policies
            .first()
            .cloned()
            .context("a primary capability produced no policy receipt")?;
        Ok(crate::skill_run::SkillRunReport {
            requested_selector: query.skill.clone(),
            resolved_skill,
            output: crate::skill_run::SkillCapabilityOutput::MessageClassification(
                crate::skill_run::MessageClassificationResult { receipt, report },
            ),
        })
    }

    /// Aggregate slash-command usage with configured and request-specific token patterns.
    ///
    /// # Errors
    ///
    /// Returns an error when a configured/request pattern is invalid or the index query fails.
    ///
    /// # Complexity
    ///
    /// Time is proportional to filtered user-message rows times the combined command patterns.
    /// Memory is proportional to distinct matched commands, sessions, and projects.
    pub fn planning(
        &self,
        filters: &MessageFilters,
        command_patterns: &[String],
    ) -> Result<Vec<crate::models::PlanningCount>> {
        let command_filters =
            crate::analytics::compile_planning_filters(self.config, command_patterns)?;
        self.db.planning_usage(filters, &command_filters)
    }

    /// Count indexed messages by normalized role, ordered by role.
    ///
    /// # Errors
    ///
    /// Returns an error when the index query fails.
    pub fn role_statistics(
        &self,
        filters: &MessageFilters,
    ) -> Result<Vec<crate::analytics::RoleStat>> {
        let rows: Vec<_> = self
            .db
            .message_role_counts(filters)?
            .into_iter()
            .map(|(role, count)| crate::analytics::RoleStat { role, count })
            .collect();
        let limit = if filters.limit == 0 {
            usize::MAX
        } else {
            filters.limit
        };
        Ok(rows.into_iter().skip(filters.offset).take(limit).collect())
    }

    /// Return one bounded keyset page of provider-normalized session text for outward analysis.
    ///
    /// # Complexity
    ///
    /// Time and returned memory are proportional to the page's selected sessions plus their
    /// joined user-message text. Keyset traversal avoids work proportional to prior page offsets.
    pub fn documents(
        &self,
        filters: &SearchFilters,
        cursor: Option<&crate::models::AnalysisCursor>,
    ) -> Result<crate::models::AnalysisDocumentPage> {
        self.db.analysis_documents(filters, cursor)
    }

    /// Run a compiled provider-neutral policy over one snapshot of the indexed corpus.
    ///
    /// `filters.limit` bounds the total number of sessions (`0` means all). Internal keyset
    /// traversal is automatic and does not alter analysis or publication results. User-message
    /// text streams through per-message, so memory is bounded by the policy's explicit bounds
    /// plus one message — a single session's aggregate user text is never materialized (except
    /// when a `user_text`/`any` classification rule runs without `max_classification_chars`).
    ///
    /// # Complexity
    ///
    /// Time is proportional to streamed user-message bytes times applicable policy rules, plus
    /// phrase aggregation. Memory is bounded by policy limits plus one message, except an
    /// explicitly unbounded `user_text`/`any` classification rule retains its matched text.
    pub fn run(
        &self,
        filters: &SearchFilters,
        policy: &crate::analysis_pipeline::AnalysisPolicy,
    ) -> Result<crate::analysis_pipeline::AnalysisResult> {
        self.run_with_session_batch_size(filters, DEFAULT_ANALYSIS_SESSION_BATCH_SIZE, policy)
    }

    /// Run with an explicit internal session batch size for invariant tests and internal tuning.
    /// This value controls database traversal only and must never change returned results.
    pub(crate) fn run_with_session_batch_size(
        &self,
        filters: &SearchFilters,
        session_batch_size: std::num::NonZeroUsize,
        policy: &crate::analysis_pipeline::AnalysisPolicy,
    ) -> Result<crate::analysis_pipeline::AnalysisResult> {
        let mut accumulator = policy.accumulator();
        self.db.visit_analysis_sessions(
            filters,
            session_batch_size,
            |session, message_count, user_message_count, chunks| {
                accumulator.push_session_text_stream(
                    session,
                    message_count,
                    user_message_count,
                    chunks,
                )
            },
        )?;
        accumulator.finish()
    }
}

/// Read-only session rendering over the shared catalog database.
#[derive(Clone, Copy)]
pub struct ExportService<'db> {
    db: &'db Db,
}

impl<'db> ExportService<'db> {
    /// Create an export service over an existing database connection.
    pub const fn new(db: &'db Db) -> Self {
        Self { db }
    }

    /// Resolve an exact session ID or unambiguous prefix and render its complete transcript.
    ///
    /// The returned document is held in memory and performs no terminal or filesystem I/O.
    /// Resolution errors include candidate context for ambiguous prefixes.
    ///
    /// # Errors
    ///
    /// Returns an error when the identifier is missing or ambiguous, database access fails, or
    /// structured serialization fails.
    pub fn render_full(
        &self,
        id_or_prefix: &str,
        format: crate::export::ExportFormat,
    ) -> Result<crate::export::ExportDocument> {
        let session = self.db.resolve_session(id_or_prefix)?;
        crate::export::render_full(&session, format)
    }

    /// Select sessions and stream their rendered documents into one immutable bundle.
    ///
    /// This retains no full-corpus transcript collection: memory is bounded by the selected
    /// session metadata plus the largest individual rendered document.
    /// Time and output bytes are proportional to the selected transcripts; `filters.limit = 0`
    /// intentionally selects the complete filtered corpus.
    pub fn publish_bundle(
        &self,
        filters: &SearchFilters,
        plan: &crate::export::ExportPublicationPlan,
    ) -> Result<crate::export::ExportPublicationReceipt> {
        let sessions = self.db.list_recent(filters)?;
        let format = plan.format();
        let documents = sessions.into_iter().map(|session| {
            self.render_full(&session.id, format)
                .map(|document| (session.id, document))
        });
        plan.publish(documents)
    }
}

#[derive(Clone, Copy)]
/// Read-only discovery operations bound to a validated configuration.
pub struct SourceService<'config> {
    config: &'config Config,
}

impl<'config> SourceService<'config> {
    /// Create a source service over `config`.
    pub const fn new(config: &'config Config) -> Self {
        Self { config }
    }

    /// Discover enabled sources and return status for every supported provider.
    pub fn inventory(&self) -> Vec<crate::source::ProviderSourceStatus> {
        crate::source::inventory(self.config)
    }
}

/// Measurements from one completed compaction transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactOutcome {
    pub before_bytes: u64,
    pub after_bytes: u64,
}

impl CompactOutcome {
    pub const fn reclaimed_bytes(self) -> u64 {
        self.before_bytes.saturating_sub(self.after_bytes)
    }
}

/// Diagnostics and writer-exclusive index maintenance.
#[derive(Clone, Copy)]
pub struct MaintenanceService<'app> {
    config: &'app Config,
    db: &'app Db,
}

impl<'app> MaintenanceService<'app> {
    pub const fn new(config: &'app Config, db: &'app Db) -> Self {
        Self { config, db }
    }

    pub fn diagnostics(&self) -> Result<DiagnosticStatus> {
        crate::diagnostics::collect(self.config, self.db)
    }

    /// Merge FTS segments, rebuild the database, and checkpoint the WAL while
    /// holding the same process lock used by index writers.
    pub fn compact(&self) -> Result<CompactOutcome> {
        indexer::with_index_update_lock(self.config, || {
            let before_bytes = self.db.storage_allocation()?.total_bytes;
            self.db.optimize_fts()?;
            self.db.vacuum()?;
            self.db.checkpoint_truncate()?;
            Ok(CompactOutcome {
                before_bytes,
                after_bytes: self.db.storage_allocation()?.total_bytes,
            })
        })
    }
}

#[derive(Clone, Copy)]
/// Explicit refresh, reindex, and status operations sharing the application's elected writer.
pub struct IndexService<'app> {
    config: &'app Config,
    db: &'app Db,
}

impl<'app> IndexService<'app> {
    pub const fn new(config: &'app Config, db: &'app Db) -> Self {
        Self { config, db }
    }

    pub fn refresh(&self) -> Result<AutoReindexOutcome> {
        indexer::refresh_index_opportunistically(self.config, self.db, None)
    }

    pub fn reindex(&self, full: bool) -> Result<(usize, usize)> {
        let outcome = indexer::explicit_reindex_and_migrate(self.config, self.db, full, None)?;
        Ok((outcome.files_seen, outcome.sessions_updated))
    }

    /// Report parser/schema freshness and only repairs applicable to discoverable sources.
    pub fn status(&self) -> Result<IndexStatus> {
        crate::diagnostics::index_status(self.config, self.db)
    }
}

#[derive(Clone, Copy)]
/// Typed session listing, search, resolution, and compact inspection operations.
pub struct CatalogService<'db> {
    db: &'db Db,
}

impl<'db> CatalogService<'db> {
    pub const fn new(db: &'db Db) -> Self {
        Self { db }
    }

    /// List filtered sessions in database order.
    ///
    /// # Complexity
    ///
    /// Current list paging uses SQL `OFFSET`: with `N` eligible rows, offset `O`, and positive
    /// limit `K`, favorable indexed work is `O(log N + O + K)`, not keyset `O(log N + K)`.
    /// Returned memory is proportional to the selected rows and their text bytes. A zero limit
    /// intentionally returns the complete filtered corpus.
    pub fn list_sessions(&self, filters: &SearchFilters) -> Result<Vec<SessionRecord>> {
        self.db.list_recent(filters)
    }

    /// List a numeric-offset page in the same `(updated_at DESC, id ASC)` order as
    /// [`CatalogService::list_sessions`]. Intended for bounded protocol adapters.
    pub fn list_sessions_page(
        &self,
        filters: &SearchFilters,
        offset: usize,
    ) -> Result<Vec<SessionRecord>> {
        self.db.list_recent_page(filters, offset)
    }

    /// Search all structurally eligible session fields with configured relevance scoring.
    ///
    /// # Complexity
    ///
    /// Let `B` be the total eligible field and transcript bytes, `N` the eligible sessions, `K` a
    /// positive result limit, `D_K` the text bytes retained in those result records, and `D_max`
    /// the largest current candidate's fields/transcript. Work is `O(B + N log K)` and peak result
    /// processing memory is `O(K + D_K + D_max)` because each streamed transcript is also
    /// lowercased transiently. A zero limit intentionally retains every matching session.
    pub fn search_sessions(
        &self,
        query: &str,
        filters: &SearchFilters,
        current_repo: Option<&str>,
        scoring: &ScoringConfig,
    ) -> Result<Vec<SearchHit>> {
        self.db.search(query, filters, current_repo, scoring)
    }

    /// Resolve one canonical session ID or unique prefix; ambiguous errors list candidates.
    pub fn resolve_session(&self, id_or_prefix: &str) -> Result<SessionRecord> {
        self.db.resolve_session_record(id_or_prefix)
    }

    /// Build a bounded evidence summary for one resolved session without filesystem writes.
    pub fn inspect(
        &self,
        id_or_prefix: &str,
        options: crate::inspect::InspectionOptions,
    ) -> Result<crate::inspect::SessionInspection> {
        crate::inspect::inspect_session(self.db, id_or_prefix, options)
    }
}

#[derive(Clone, Copy)]
/// Canonical message search, explain, metadata, and context operations used by all adapters.
pub struct MessageService<'db> {
    config: &'db Config,
    db: &'db Db,
    surface: SearchSurface,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum MessageSearchBatchControl {
    Continue,
    Stop,
}

/// One owned, fully enriched result batch. The aligned context window at index `i` belongs to
/// result `i`; included session data is a mergeable delta for sessions present in this batch.
pub(crate) struct MessageSearchBatch {
    pub(crate) results: Vec<crate::message_search::MessageSearchHit>,
    pub(crate) context_windows: Vec<Vec<MessageHit>>,
    pub(crate) included: MessageSearchIncludedData,
}

/// Terminal state from a bounded traversal. `page` and `ordered_digest` exist only after natural
/// exhaustion; a stopped consumer has not received a complete requested result set.
pub(crate) struct MessageSearchBatchVisitOutcome {
    pub(crate) request: ResolvedMessageSearchRequest,
    pub(crate) emitted: usize,
    pub(crate) exhausted: bool,
    pub(crate) page: Option<PageInfo>,
    pub(crate) planner: Option<SearchExplain>,
    pub(crate) origins: Option<MessageSearchOrigins>,
    pub(crate) ordered_digest: Option<String>,
}

impl<'db> MessageService<'db> {
    pub const fn new(config: &'db Config, db: &'db Db, surface: SearchSurface) -> Self {
        Self {
            config,
            db,
            surface,
        }
    }

    /// Search individual messages using the selected exact, fuzzy, or regex mode.
    ///
    /// # Complexity
    ///
    /// Schema-v4 exact/regex uses SQLite trigram candidates when a safe literal exists, then
    /// authoritative verification. Fuzzy search scores the complete structurally filtered corpus
    /// and retains only the requested top-K page window. Regex without a safe literal may scan the
    /// filtered corpus. Literal/regex output is unbounded only when `filters.limit == 0`; fuzzy
    /// validation rejects an unbounded page.
    pub fn search_legacy(&self, query: &str, filters: &MessageFilters) -> Result<Vec<MessageHit>> {
        self.db.search_messages(query, filters)
    }

    pub fn plan(&self, request: MessageSearchRequest) -> Result<MessageSearchPlan> {
        let purpose = request
            .purpose()
            .map(|selection| {
                let definition = self
                    .config
                    .search
                    .purposes
                    .get(selection.name())
                    .ok_or_else(|| {
                        anyhow!("unknown message-search purpose {:?}", selection.name())
                    })?;
                if selection
                    .version()
                    .is_some_and(|version| version != definition.version)
                {
                    bail!(
                        "purpose {:?} version {} is unavailable; configured version is {}",
                        selection.name(),
                        selection.version().unwrap(),
                        definition.version
                    );
                }
                Ok((selection.name().to_string(), definition))
            })
            .transpose()?;
        let purpose_origin = || {
            purpose
                .as_ref()
                .map(|(name, definition)| ValueOrigin::Purpose {
                    name: name.clone(),
                    version: definition.version,
                })
        };
        let purpose_preferences = purpose
            .as_ref()
            .map(|(_, definition)| &definition.preferences);

        let (requested_limit, offset, explicit_all) = match request.extent() {
            crate::message_search::RequestedExtent::Page { limit, offset } => {
                (limit, offset, false)
            }
            crate::message_search::RequestedExtent::AllResults { offset } => (None, offset, true),
        };
        let (limit, limit_origin) = if explicit_all {
            (None, ValueOrigin::Explicit)
        } else if let Some(limit) = requested_limit {
            (Some(limit), ValueOrigin::Explicit)
        } else if let Some(limit) = purpose_preferences.and_then(|value| value.default_limit) {
            (Some(limit), purpose_origin().unwrap())
        } else if let Some(limit) = self.config.search.message_search.default_limit {
            (Some(limit), ValueOrigin::OperationConfig)
        } else {
            let surface_limit = match self.surface {
                SearchSurface::Mcp => NonZeroUsize::new(self.config.mcp.search_messages_limit),
                // Native programmatic/interactive surfaces preserve the complete selected corpus
                // when no operation/purpose/call limit was supplied. MCP alone supplies an
                // implicit finite page because its response is injected directly into model
                // context. Fuzzy validation below still rejects an unbounded resolved extent.
                SearchSurface::Rust | SearchSurface::Cli | SearchSurface::Python => None,
            };
            match surface_limit {
                Some(limit) => (
                    Some(limit),
                    ValueOrigin::SurfaceConfig {
                        surface: self.surface,
                    },
                ),
                None => (None, ValueOrigin::TypedDefault),
            }
        };
        if let (Some(current), Some(maximum)) =
            (limit, self.config.search.budgets.max_hits_per_page)
        {
            if current > maximum {
                bail!(
                    "resolved message-search limit {} exceeds search.budgets.max_hits_per_page {}; lower the request, purpose, operation default, or MCP default",
                    current,
                    maximum
                );
            }
        }
        let extent = if explicit_all {
            ResolvedExtent::AllResults { offset }
        } else if let Some(limit) = limit {
            ResolvedExtent::Page { limit, offset }
        } else if matches!(
            request.query(),
            crate::message_search::MessageQuery::Fuzzy(_)
        ) {
            bail!("fuzzy search requires a positive page size from the request or configuration");
        } else {
            ResolvedExtent::AllResults { offset }
        };

        let (context, context_before_origin, context_after_origin) = if let Some(context) =
            request.context()
        {
            (context, ValueOrigin::Explicit, ValueOrigin::Explicit)
        } else {
            let (before, before_origin) = if let Some(value) =
                purpose_preferences.and_then(|preferences| preferences.context_before)
            {
                (value, purpose_origin().unwrap())
            } else if let Some(value) = self.config.search.message_search.context.context_before {
                (value, ValueOrigin::OperationConfig)
            } else {
                (0, ValueOrigin::TypedDefault)
            };
            let (after, after_origin) = if let Some(value) =
                purpose_preferences.and_then(|preferences| preferences.context_after)
            {
                (value, purpose_origin().unwrap())
            } else if let Some(value) = self.config.search.message_search.context.context_after {
                (value, ValueOrigin::OperationConfig)
            } else {
                (0, ValueOrigin::TypedDefault)
            };
            (
                ContextWindow::new(before, after),
                before_origin,
                after_origin,
            )
        };
        if let Some(maximum) = self.config.search.budgets.max_context_neighbors_per_hit {
            let total = context
                .messages_before()
                .checked_add(context.messages_after())
                .ok_or_else(|| anyhow!("resolved message-search context total overflows"))?;
            if total > maximum.get() {
                bail!(
                    "resolved message-search context total {} exceeds search.budgets.max_context_neighbors_per_hit {}; lower context_before or context_after",
                    total,
                    maximum
                );
            }
        }

        let (includes, includes_origin) = if let Some(includes) = request.includes() {
            (includes.to_vec(), ValueOrigin::Explicit)
        } else if let Some(value) = request.presentation().include_refs() {
            (
                value
                    .then_some(MessageSearchInclude::ParsedReferences)
                    .into_iter()
                    .collect(),
                ValueOrigin::Explicit,
            )
        } else if let Some(value) =
            purpose_preferences.and_then(|preferences| preferences.include_refs)
        {
            (
                value
                    .then_some(MessageSearchInclude::ParsedReferences)
                    .into_iter()
                    .collect(),
                purpose_origin().unwrap(),
            )
        } else if self.surface == SearchSurface::Mcp {
            (
                vec![MessageSearchInclude::NormalizedSessionMetadata],
                ValueOrigin::SurfaceConfig {
                    surface: self.surface,
                },
            )
        } else {
            (Vec::new(), ValueOrigin::TypedDefault)
        };
        let include_refs = includes.contains(&MessageSearchInclude::ParsedReferences);
        let detail = request.presentation().detail();
        let detail_origin = if detail.is_some() {
            ValueOrigin::Explicit
        } else {
            ValueOrigin::TypedDefault
        };
        let (message_lines, message_lines_origin) =
            if let Some(value) = request.presentation().message_lines() {
                (value, ValueOrigin::Explicit)
            } else if detail == Some(DetailLevel::Full) {
                (
                    LineWindow::Full,
                    ValueOrigin::DetailPreset {
                        detail: DetailLevel::Full,
                    },
                )
            } else if let Some(value) =
                purpose_preferences.and_then(|preferences| preferences.lines_per_message)
            {
                (LineWindow::from_signed(value)?, purpose_origin().unwrap())
            } else {
                let value = match self.surface {
                    SearchSurface::Cli => self.config.cli.lines_per_message,
                    SearchSurface::Mcp => self.config.mcp.lines_per_message,
                    SearchSurface::Rust | SearchSurface::Python => 0,
                };
                (
                    LineWindow::from_signed(value)?,
                    ValueOrigin::SurfaceConfig {
                        surface: self.surface,
                    },
                )
            };
        let (match_evidence_max_chars, match_evidence_max_chars_origin) =
            if let Some(value) = request.presentation().match_evidence_max_chars() {
                (value, ValueOrigin::Explicit)
            } else if let Some(value) =
                purpose_preferences.and_then(|preferences| preferences.match_evidence_max_chars)
            {
                (value, purpose_origin().unwrap())
            } else if let Some(value) = self.config.search.message_search.match_evidence_max_chars {
                (value, ValueOrigin::OperationConfig)
            } else {
                (
                    NonZeroUsize::new(DEFAULT_MATCH_EVIDENCE_MAX_CHARS)
                        .expect("typed match evidence default is positive"),
                    ValueOrigin::TypedDefault,
                )
            };
        let compact_boundary_chars = match self.surface {
            SearchSurface::Mcp => self.config.mcp.preview_chars,
            SearchSurface::Cli => self.config.cli.evidence_preview_chars,
            SearchSurface::Rust | SearchSurface::Python => DEFAULT_MATCH_EVIDENCE_MAX_CHARS,
        };
        let (field_view, field_view_origin) =
            if let Some(value) = request.presentation().field_view() {
                (value, ValueOrigin::Explicit)
            } else {
                match detail {
                    Some(DetailLevel::Compact) => (
                        FieldViewBudget::MaxChars {
                            max_chars: NonZeroUsize::new(compact_boundary_chars)
                                .expect("validated presentation defaults are positive"),
                        },
                        ValueOrigin::DetailPreset {
                            detail: DetailLevel::Compact,
                        },
                    ),
                    Some(DetailLevel::Full) => (
                        FieldViewBudget::NoCharLimit,
                        ValueOrigin::DetailPreset {
                            detail: DetailLevel::Full,
                        },
                    ),
                    None if self.surface == SearchSurface::Mcp => (
                        FieldViewBudget::MaxChars {
                            max_chars: NonZeroUsize::new(self.config.mcp.preview_chars)
                                .expect("validated MCP preview_chars is positive"),
                        },
                        ValueOrigin::SurfaceConfig {
                            surface: self.surface,
                        },
                    ),
                    None => (FieldViewBudget::NoCharLimit, ValueOrigin::TypedDefault),
                }
            };
        let (match_view, match_view_origin) =
            if let Some(value) = request.presentation().match_view() {
                (value, ValueOrigin::Explicit)
            } else {
                match detail {
                    Some(DetailLevel::Full) => (
                        MatchViewBudget::MinimalSpan,
                        ValueOrigin::DetailPreset {
                            detail: DetailLevel::Full,
                        },
                    ),
                    Some(DetailLevel::Compact) => (
                        MatchViewBudget::MaxChars {
                            max_chars: match_evidence_max_chars,
                        },
                        ValueOrigin::DetailPreset {
                            detail: DetailLevel::Compact,
                        },
                    ),
                    None => (
                        MatchViewBudget::MaxChars {
                            max_chars: match_evidence_max_chars,
                        },
                        match_evidence_max_chars_origin.clone(),
                    ),
                }
            };
        let effective_match_evidence_max_chars = match match_view {
            MatchViewBudget::MinimalSpan => match_evidence_max_chars,
            MatchViewBudget::MaxChars { max_chars } => max_chars,
        };
        let (receipt, receipt_origin) = if let Some(value) = request.receipt_level() {
            (value, ValueOrigin::Explicit)
        } else if let Some(value) =
            purpose_preferences.and_then(|preferences| preferences.receipt_level)
        {
            (value, purpose_origin().unwrap())
        } else {
            (ReceiptLevel::None, ValueOrigin::TypedDefault)
        };

        let predicates = request.predicates();
        let session_id = predicates
            .session()
            .map(|value| {
                self.catalog()
                    .resolve_session(value)
                    .map(|session| session.id)
            })
            .transpose()?;
        let normalize = |value: &str| crate::util::normalize_path_prefix(value);
        let resolved_predicates = ResolvedMessagePredicates {
            role: predicates.role(),
            kinds: predicates.kinds().map(<[_]>::to_vec),
            provider: predicates.provider(),
            session_id,
            workspace_path_prefix: predicates.workspace_path_prefix().map(normalize),
            transcript_path_prefix: predicates.transcript_path_prefix().map(normalize),
            exclude_workspace_path_prefixes: predicates
                .exclude_workspace_path_prefixes()
                .map(normalize)
                .collect(),
            exclude_transcript_path_prefixes: predicates
                .exclude_transcript_path_prefixes()
                .map(normalize)
                .collect(),
            exclude_session_ids: predicates
                .exclude_session_ids()
                .map(str::to_string)
                .collect(),
            time: predicates.time(),
            sequence: predicates.sequence(),
            tool_name_contains: predicates.tool_name_contains().map(str::to_string),
            include_compaction: predicates.include_compaction(),
        };
        let ordering = if matches!(
            request.query(),
            crate::message_search::MessageQuery::Fuzzy(_)
        ) {
            ExecutionOrder::FuzzyRelevance
        } else {
            ExecutionOrder::SessionSequence
        };
        Ok(MessageSearchPlan {
            retrieval: MessageRetrievalPlan {
                query: request.query().clone(),
                target: request.target().clone(),
                predicates: resolved_predicates,
                match_window: request.match_window(),
                ordering,
                extent,
            },
            response: MessageResponsePlan {
                context,
                includes,
                presentation: ResolvedMessagePresentation {
                    include_refs,
                    message_lines,
                    match_evidence_max_chars: effective_match_evidence_max_chars,
                    detail,
                    field_view,
                    match_view,
                },
            },
            receipt,
            origins: MessageSearchOrigins {
                result_extent: limit_origin,
                context_messages_before: context_before_origin,
                context_messages_after: context_after_origin,
                includes: includes_origin,
                detail: detail_origin,
                lines_per_message: message_lines_origin,
                field_view: field_view_origin,
                match_view: match_view_origin,
                receipt_level: receipt_origin,
                result_order: ValueOrigin::Derived,
            },
        })
    }

    /// Execute the canonical message-search request with surface-specific omitted-limit semantics.
    ///
    /// With no explicit, purpose, or operation limit, Rust/CLI/Python preserve every literal,
    /// regex, or no-text match; MCP uses its configured finite page. Fuzzy search always requires
    /// a finite resolved page. Presentation limits are resolved separately and never affect hit
    /// membership.
    pub fn search(&self, request: MessageSearchRequest) -> Result<MessageSearchResponse> {
        let plan = self.plan(request)?;
        let resolved_request = ResolvedMessageSearchRequest::from_plan(&plan)?;
        let include_explain = plan.receipt != ReceiptLevel::None;
        let (mut hits, planner) = self
            .db
            .search_message_plan(&plan.retrieval, include_explain)?;
        let (next_offset, extent) = match plan.retrieval.extent {
            ResolvedExtent::Page { limit, offset } => {
                let has_more = hits.len() > limit.get();
                hits.truncate(limit.get());
                let next_offset = if has_more {
                    Some(
                        offset
                            .checked_add(limit.get())
                            .ok_or_else(|| anyhow!("message page next offset overflows"))?,
                    )
                } else {
                    None
                };
                (next_offset, plan.retrieval.extent)
            }
            ResolvedExtent::AllResults { .. } => (None, plan.retrieval.extent),
        };
        if plan.retrieval.match_window == Some(MatchWindow::Latest) {
            hits.reverse();
        }
        let (hits, context_windows, included) = self.enrich_hits(&plan, hits)?;
        let origins = (plan.receipt == ReceiptLevel::Full).then(|| plan.origins.clone());
        let match_mode = plan.retrieval.query.mode();
        let match_details = match_mode.map(|mode| (plan.retrieval.target.clone(), mode));
        let response_query = plan.retrieval.query.text().map(str::to_owned);
        let returned = hits.len();
        Ok(MessageSearchResponse::new(
            resolved_request,
            match_details,
            hits,
            context_windows,
            PageInfo::new(extent, returned, next_offset, plan.retrieval.ordering),
            plan.response,
            planner,
            origins,
            included,
        )
        .with_query(response_query))
    }

    /// Visit an exhaustive non-fuzzy request in fully enriched, bounded batches.
    ///
    /// This callback seam is the shared implementation for streaming adapters. The materialized
    /// [`MessageService::search`] API remains the simple default for ordinary callers. This method
    /// retains only one raw/enriched batch plus its active selected-field, context, include, and
    /// presentation bytes; stopping or returning an error drops the SQLite snapshot without
    /// draining unread results.
    pub(crate) fn visit_search_batches(
        &self,
        request: MessageSearchRequest,
        batch_size: NonZeroUsize,
        mut visitor: impl FnMut(MessageSearchBatch) -> Result<MessageSearchBatchControl>,
    ) -> Result<MessageSearchBatchVisitOutcome> {
        let mut plan = self.plan(request)?;
        let offset = match plan.retrieval.extent {
            ResolvedExtent::AllResults { offset } => offset,
            ResolvedExtent::Page { .. } => {
                bail!(
                    "bounded message-search traversal requires all_results; use search() for a finite materialized page"
                )
            }
        };
        anyhow::ensure!(
            !matches!(
                plan.retrieval.query,
                crate::message_search::MessageQuery::Fuzzy(_)
            ),
            "bounded message-search traversal supports literal, regex, and queryless all-results requests; pass a positive limit to search() for fuzzy results"
        );
        if plan.retrieval.match_window == Some(MatchWindow::Latest) {
            anyhow::ensure!(
                offset == 0,
                "bounded message-search traversal cannot yet stream match_window=latest with a positive offset without changing global chronological order; use search() for this request"
            );
            // With an exhaustive zero-offset request, earliest and latest select the same rows.
            // Traverse oldest-first so batches preserve the public chronological order globally.
            plan.retrieval.match_window = None;
        }

        let resolved_request = ResolvedMessageSearchRequest::from_plan(&plan)?;
        let include_explain = plan.receipt != ReceiptLevel::None;
        let match_mode = plan.retrieval.query.mode();
        let mut digest = MessageSearchOrderedDigest::new(plan.retrieval.target.clone(), match_mode);
        let mut emitted = 0_usize;
        let visit = self.db.visit_message_plan_batches(
            &plan.retrieval,
            include_explain,
            batch_size,
            |raw_hits| {
                let (results, context_windows, included) = self.enrich_hits(&plan, raw_hits)?;
                for result in &results {
                    digest.update(result);
                }
                emitted = emitted
                    .checked_add(results.len())
                    .ok_or_else(|| anyhow!("bounded message-search emitted count overflows"))?;
                let control = visitor(MessageSearchBatch {
                    results,
                    context_windows,
                    included,
                })?;
                Ok(match control {
                    MessageSearchBatchControl::Continue => MessageBatchControl::Continue,
                    MessageSearchBatchControl::Stop => MessageBatchControl::Stop,
                })
            },
        )?;
        debug_assert_eq!(visit.rows_visited, emitted);
        let exhausted = visit.exhausted;
        let page = exhausted.then(|| {
            PageInfo::new(
                plan.retrieval.extent,
                emitted,
                None,
                plan.retrieval.ordering,
            )
        });
        let origins =
            (exhausted && plan.receipt == ReceiptLevel::Full).then(|| plan.origins.clone());
        let ordered_digest =
            (exhausted && plan.receipt == ReceiptLevel::Full).then(|| digest.finish());
        Ok(MessageSearchBatchVisitOutcome {
            request: resolved_request,
            emitted,
            exhausted,
            page,
            planner: visit.explain,
            origins,
            ordered_digest,
        })
    }

    /// Attach match proof, presentation, requested includes, and context to one active batch.
    ///
    /// This is the single enrichment owner for materialized and bounded-batch search. Its retained
    /// application memory is proportional to this batch's source/context/view bytes, not prior
    /// batches or unread results.
    fn enrich_hits(
        &self,
        plan: &MessageSearchPlan,
        hits: Vec<MessageHit>,
    ) -> Result<(
        Vec<crate::message_search::MessageSearchHit>,
        Vec<Vec<MessageHit>>,
        MessageSearchIncludedData,
    )> {
        let mut hits = attach_match_evidence(
            &plan.retrieval.query,
            &plan.retrieval.target,
            plan.response.presentation.match_view,
            hits,
        )?;
        apply_message_presentation(
            &plan.retrieval.target,
            plan.response.presentation,
            &mut hits,
        )?;
        if plan
            .response
            .includes
            .contains(&MessageSearchInclude::ParsedReferences)
        {
            for hit in &mut hits {
                hit.set_parsed_references(crate::refs::extract_refs_from_text(
                    &hit.message().content,
                    hit.message().tool_name.as_deref(),
                ));
            }
        }
        let session_ids = hits
            .iter()
            .map(|hit| hit.message().session_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let normalized_session_metadata = plan
            .response
            .includes
            .contains(&MessageSearchInclude::NormalizedSessionMetadata)
            .then(|| {
                self.db
                    .session_metadata(&session_ids)
                    .map(|metadata| metadata.into_iter().collect::<BTreeMap<_, _>>())
            })
            .transpose()?;
        let raw_provider_metadata = plan
            .response
            .includes
            .contains(&MessageSearchInclude::RawProviderMetadata)
            .then(|| self.db.session_raw_provider_metadata(&session_ids))
            .transpose()?;
        let runtime_diagnostics = plan
            .response
            .includes
            .contains(&MessageSearchInclude::RuntimeDiagnostics)
            .then(|| {
                serde_json::to_vec(self.config).map(|bytes| {
                    MessageSearchRuntimeDiagnostics::new(
                        self.surface,
                        format!("sha256:{}", crate::hashing::sha256(&bytes)),
                    )
                })
            })
            .transpose()?;
        let included = MessageSearchIncludedData::new(
            normalized_session_metadata,
            raw_provider_metadata,
            runtime_diagnostics,
        );
        let context_windows = if plan.response.context.messages_before() == 0
            && plan.response.context.messages_after() == 0
        {
            Vec::new()
        } else {
            let before = i64::try_from(plan.response.context.messages_before())
                .map_err(|_| anyhow!("resolved context_before exceeds SQLite's signed range"))?;
            let after = i64::try_from(plan.response.context.messages_after())
                .map_err(|_| anyhow!("resolved context_after exceeds SQLite's signed range"))?;
            let anchors = hits
                .iter()
                .map(|hit| (hit.message.session_id.clone(), hit.message.seq))
                .collect::<Vec<_>>();
            self.db.message_context_windows(&anchors, before, after)?
        };
        Ok((hits, context_windows, included))
    }

    fn catalog(&self) -> CatalogService<'db> {
        CatalogService::new(self.db)
    }

    /// Run the same search as [`MessageService::search`] and optionally return its actual planner
    /// receipt, including corpus size and any exact/regex prefilter decision.
    pub fn search_with_explain(
        &self,
        query: &str,
        filters: &MessageFilters,
        include_explain: bool,
    ) -> Result<(Vec<MessageHit>, Option<SearchExplain>)> {
        self.db
            .search_messages_with_explain(query, filters, include_explain)
    }

    /// Load compact metadata for the requested canonical IDs; absent IDs are omitted.
    pub fn session_metadata(&self, session_ids: &[String]) -> Result<HashMap<String, SessionMeta>> {
        self.db.session_metadata(session_ids)
    }

    /// Return a bounded sequence window around one message.
    ///
    /// # Complexity
    ///
    /// The `(session_id, seq)` index makes database work and memory proportional to
    /// `before + after + 1`, subject to messages available in the session.
    pub fn context(
        &self,
        session_id: &str,
        seq: i64,
        before: i64,
        after: i64,
    ) -> Result<Vec<MessageHit>> {
        self.db.message_context(session_id, seq, before, after)
    }

    /// Read one session's messages, selecting the oldest or newest `filters.limit` by `order`,
    /// always returned in chronological (seq-ascending) order. Wraps
    /// [`Db::read_session_messages`]; `filters.session_id` must be set.
    pub fn read_session(
        &self,
        filters: &MessageFilters,
        order: crate::db::MessageOrder,
    ) -> Result<Vec<MessageHit>> {
        self.db.read_session_messages(filters, order)
    }
}

#[derive(Clone, Copy)]
/// Read-only file activity, causal history, reconstruction, and safe publication operations.
pub struct FileService<'db> {
    db: &'db Db,
}

impl<'db> FileService<'db> {
    pub const fn new(db: &'db Db) -> Self {
        Self { db }
    }

    /// Aggregate matching file-edit rows.
    ///
    /// # Complexity
    ///
    /// Work is proportional to filtered edit rows; returned memory is proportional to grouped
    /// files and is unbounded only when the query explicitly uses a zero limit.
    pub fn search(&self, query: &FileQuery) -> Result<Vec<FileEditSummary>> {
        self.db.file_search(query)
    }

    /// Return deterministic file/session edit-count pages using `query.limit` and `query.offset`.
    pub fn cross_reference(&self, query: &FileQuery) -> Result<Vec<FileCrossRef>> {
        self.db.file_cross_ref(query)
    }

    /// Return causally ordered edits for one selected file.
    ///
    /// # Complexity
    ///
    /// Reconstruction work is proportional to matching versions and their stored edit payloads;
    /// returned memory is bounded by `query.limit` unless it is explicitly zero. `query.offset`
    /// skips versions after stable session/version ordering.
    pub fn history(&self, file: &str, query: &FileQuery) -> Result<Vec<FileVersion>> {
        crate::files::history(self.db, file, query)
    }

    /// Replay edits through one requested version.
    ///
    /// # Complexity
    ///
    /// Time is proportional to replayed edit operations and content bytes; peak memory includes
    /// the reconstructed file plus the selected edit history.
    pub fn reconstruct(
        &self,
        file: &str,
        query: &FileQuery,
        version: Option<usize>,
    ) -> Result<crate::files::ReconstructedFile> {
        crate::files::reconstruct_query(self.db, file, query, version)
    }

    /// Lazily reconstruct every causally ordered version with a complete replay path.
    pub fn reconstruct_versions(
        &self,
        file: &str,
        query: &FileQuery,
    ) -> Result<crate::files::ReconstructedFileVersions> {
        crate::files::reconstruct_versions_query(self.db, file, query)
    }

    /// Atomically publish every reconstructable version to a new non-replacing directory.
    pub fn publish_versions(
        &self,
        file: &str,
        query: &FileQuery,
        destination: &std::path::Path,
    ) -> Result<crate::files::RecoveryPublicationReceipt> {
        let versions = self.reconstruct_versions(file, query)?;
        crate::files::publish_reconstructed_versions(versions, destination)
    }

    /// Collision-safely write a reconstructed file through the shared recovery policy.
    pub fn restore(
        &self,
        reconstructed: &crate::files::ReconstructedFile,
        output_dir: Option<&std::path::Path>,
    ) -> Result<std::path::PathBuf> {
        crate::files::restore_reconstructed(reconstructed, output_dir)
    }
}

#[cfg(test)]
mod message_search_service_tests {
    use std::num::{NonZeroU32, NonZeroUsize};

    use super::*;
    use crate::config::{MessagePurposePreferences, PurposeDefinition, SearchOperation};
    use crate::message_search::{
        MessageQuery, MessageSearchRequestBuilder, MessageTarget, PurposeSelection,
        RequestedExtent, ResolvedExtent,
    };
    use crate::models::{Message, MessageKind, Provider, Role};
    use crate::util::minimal_record;

    fn disposable_db() -> (tempfile::TempDir, Db) {
        let directory = tempfile::tempdir().unwrap();
        let db = Db::open(&directory.path().join("message-search.db")).unwrap();
        db.mark_schema_current().unwrap();
        (directory, db)
    }

    fn literal_request() -> MessageSearchRequestBuilder {
        MessageSearchRequest::builder(
            MessageQuery::literal("needle").unwrap(),
            MessageTarget::content(),
        )
    }

    fn limit_of(plan: &MessageSearchPlan) -> Option<usize> {
        match plan.extent() {
            ResolvedExtent::Page { limit, .. } => Some(limit.get()),
            ResolvedExtent::AllResults { .. } => None,
        }
    }

    fn insert_session(db: &Db, id: &str, workspace: &str, transcript: &str, contents: &[&str]) {
        let mut parsed = minimal_record(
            Provider::Claude,
            std::path::Path::new(transcript),
            String::new(),
        );
        parsed.session.id = id.into();
        parsed.session.provider_session_id = id.replace(':', "-");
        parsed.session.cwd = Some(workspace.into());
        parsed.session.repo_root = Some(format!("{workspace}/repo"));
        parsed.session.source_path = transcript.into();
        parsed.messages = contents
            .iter()
            .enumerate()
            .map(|(sequence, content)| Message {
                seq: sequence as i64,
                role: Role::User,
                ts: None,
                tool_name: None,
                kind: MessageKind::Conversation,
                tool_call_id: None,
                is_compaction: false,
                content: (*content).into(),
            })
            .collect();
        db.upsert_session(&parsed, 0, 0).unwrap();
    }

    #[test]
    fn omitted_limits_keep_rust_cli_and_python_unbounded_while_mcp_stays_bounded() {
        let (_directory, db) = disposable_db();
        let config = Config::default();
        let request = literal_request()
            .extent(RequestedExtent::page(None, 7).unwrap())
            .build()
            .unwrap();

        for surface in [
            SearchSurface::Rust,
            SearchSurface::Cli,
            SearchSurface::Python,
        ] {
            let plan = MessageService::new(&config, &db, surface)
                .plan(request.clone())
                .unwrap();
            assert_eq!(plan.extent(), ResolvedExtent::AllResults { offset: 7 });
            assert_eq!(plan.origins().result_extent(), &ValueOrigin::TypedDefault);
        }

        let mcp = MessageService::new(&config, &db, SearchSurface::Mcp)
            .plan(request.clone())
            .unwrap();
        assert_eq!(limit_of(&mcp), Some(config.mcp.search_messages_limit));
        assert_eq!(
            mcp.origins().result_extent(),
            &ValueOrigin::SurfaceConfig {
                surface: SearchSurface::Mcp,
            }
        );

        let fuzzy = MessageSearchRequest::builder(
            MessageQuery::fuzzy("needle").unwrap(),
            MessageTarget::content(),
        )
        .build()
        .unwrap();
        for surface in [
            SearchSurface::Rust,
            SearchSurface::Cli,
            SearchSurface::Python,
        ] {
            assert!(
                MessageService::new(&config, &db, surface)
                    .plan(fuzzy.clone())
                    .unwrap_err()
                    .to_string()
                    .contains("requires a positive page size"),
                "{surface:?} fuzzy search must not acquire an implicit non-MCP page"
            );
        }
        assert_eq!(
            limit_of(
                &MessageService::new(&config, &db, SearchSurface::Mcp)
                    .plan(fuzzy)
                    .unwrap()
            ),
            Some(config.mcp.search_messages_limit)
        );
    }

    #[test]
    fn explicit_all_results_truthfully_bypasses_only_the_finite_page_ceiling() {
        let (_directory, db) = disposable_db();
        let mut config = Config::default();
        config.search.budgets.max_hits_per_page = NonZeroUsize::new(50);
        let request = literal_request()
            .extent(RequestedExtent::all_results())
            .build()
            .unwrap();

        let plan = MessageService::new(&config, &db, SearchSurface::Rust)
            .plan(request)
            .expect("an explicit all_results request is not a finite page");
        assert_eq!(plan.extent(), ResolvedExtent::AllResults { offset: 0 });
        assert_eq!(
            plan.origins().result_extent(),
            &ValueOrigin::Explicit,
            "the receipt must expose the explicit bypass, not an unused default page size"
        );
    }

    #[test]
    fn planner_precedence_is_explicit_and_policy_conflicts_are_rejected() {
        let (_directory, db) = disposable_db();
        let mut config = Config::default();
        config.search.message_search.default_limit = NonZeroUsize::new(8);
        config.search.message_search.context.context_before = Some(2);
        config.search.purposes.insert(
            "focused-review".into(),
            PurposeDefinition {
                version: NonZeroU32::new(1).unwrap(),
                operation: SearchOperation::MessageSearch,
                preferences: MessagePurposePreferences {
                    default_limit: NonZeroUsize::new(6),
                    context_before: Some(3),
                    context_after: Some(4),
                    receipt_level: Some(ReceiptLevel::Summary),
                    include_refs: Some(true),
                    lines_per_message: Some(-5),
                    match_evidence_max_chars: NonZeroUsize::new(90),
                },
            },
        );
        let purpose = PurposeSelection::new("focused-review", NonZeroU32::new(1)).unwrap();
        let service = MessageService::new(&config, &db, SearchSurface::Mcp);

        let purpose_plan = service
            .plan(literal_request().purpose(purpose.clone()).build().unwrap())
            .unwrap();
        assert_eq!(limit_of(&purpose_plan), Some(6));
        assert!(matches!(
            purpose_plan.origins().result_extent(),
            ValueOrigin::Purpose { name, version }
                if name == "focused-review" && version.get() == 1
        ));
        assert_eq!(purpose_plan.context(), ContextWindow::new(3, 4));
        assert!(purpose_plan.presentation().include_refs());
        assert_eq!(
            purpose_plan.presentation().message_lines(),
            LineWindow::Tail(NonZeroUsize::new(5).unwrap())
        );
        assert_eq!(
            purpose_plan.presentation().match_evidence_max_chars().get(),
            90
        );
        assert!(matches!(
            purpose_plan.origins().match_view(),
            ValueOrigin::Purpose { name, version }
                if name == "focused-review" && version.get() == 1
        ));
        assert_eq!(purpose_plan.receipt_level(), ReceiptLevel::Summary);

        let explicit = service
            .plan(
                literal_request()
                    .purpose(purpose)
                    .extent(RequestedExtent::page(Some(3), 0).unwrap())
                    .context(ContextWindow::new(9, 10))
                    .include_refs(false)
                    .message_lines(LineWindow::Head(NonZeroUsize::new(2).unwrap()))
                    .match_evidence_max_chars(NonZeroUsize::new(44).unwrap())
                    .receipt_level(ReceiptLevel::Full)
                    .build()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(limit_of(&explicit), Some(3));
        assert_eq!(explicit.origins().result_extent(), &ValueOrigin::Explicit);
        assert_eq!(
            explicit.origins().context_messages_before(),
            &ValueOrigin::Explicit
        );
        assert_eq!(explicit.origins().receipt_level(), &ValueOrigin::Explicit);
        assert_eq!(explicit.presentation().match_evidence_max_chars().get(), 44);
        assert_eq!(explicit.origins().match_view(), &ValueOrigin::Explicit);

        config.search.budgets.max_hits_per_page = NonZeroUsize::new(4);
        let limit_error = MessageService::new(&config, &db, SearchSurface::Mcp)
            .plan(literal_request().build().unwrap())
            .unwrap_err()
            .to_string();
        assert!(limit_error.contains("max_hits_per_page"), "{limit_error}");
        assert!(limit_error.contains("8"), "{limit_error}");
        assert!(limit_error.contains("4"), "{limit_error}");

        config.search.budgets.max_hits_per_page = None;
        config.search.budgets.max_context_neighbors_per_hit = NonZeroUsize::new(4);
        let context_error = MessageService::new(&config, &db, SearchSurface::Mcp)
            .plan(
                literal_request()
                    .context(ContextWindow::new(9, 10))
                    .build()
                    .unwrap(),
            )
            .unwrap_err()
            .to_string();
        assert!(
            context_error.contains("max_context_neighbors_per_hit"),
            "{context_error}"
        );
        assert!(context_error.contains("19"), "{context_error}");
        assert!(context_error.contains("4"), "{context_error}");
    }

    #[test]
    fn planner_resolves_unique_session_prefix_and_rejects_ambiguous_prefix() {
        let (_directory, db) = disposable_db();
        insert_session(
            &db,
            "claude:abcdef",
            "/workspace/a",
            "/transcripts/a.jsonl",
            &["needle"],
        );
        insert_session(
            &db,
            "claude:abcxyz",
            "/workspace/b",
            "/transcripts/b.jsonl",
            &["needle"],
        );
        let config = Config::default();
        let service = MessageService::new(&config, &db, SearchSurface::Rust);

        let unique = service
            .plan(
                literal_request()
                    .session_id("claude:abcd")
                    .unwrap()
                    .build()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(
            unique.retrieval.predicates.session_id.as_deref(),
            Some("claude:abcdef")
        );

        assert!(service
            .plan(
                literal_request()
                    .session_id("claude:abc")
                    .unwrap()
                    .build()
                    .unwrap(),
            )
            .unwrap_err()
            .to_string()
            .contains("ambiguous"));
    }

    #[test]
    fn bounded_batches_match_the_simple_materialized_response_and_stop_without_completion() {
        let (_directory, db) = disposable_db();
        insert_session(
            &db,
            "claude:bounded-service",
            "/workspace/bounded",
            "/transcripts/bounded.jsonl",
            &[
                "needle zero https://example.com/zero",
                "context one",
                "needle two https://example.com/two",
                "needle three",
                "needle four",
            ],
        );
        let config = Config::default();
        let service = MessageService::new(&config, &db, SearchSurface::Rust);
        let request = literal_request()
            .session_id("claude:bounded-service")
            .unwrap()
            .extent(RequestedExtent::all_results())
            .context(ContextWindow::new(1, 1))
            .includes([
                MessageSearchInclude::NormalizedSessionMetadata,
                MessageSearchInclude::ParsedReferences,
            ])
            .receipt_level(ReceiptLevel::Full)
            .build()
            .unwrap();
        let materialized = service.search(request.clone()).unwrap();

        let mut batch_lengths = Vec::new();
        let mut sequences = Vec::new();
        let mut field_views = Vec::new();
        let mut context_sequences = Vec::new();
        let mut parsed_reference_counts = Vec::new();
        let mut included_sessions = BTreeSet::new();
        let outcome =
            service
                .visit_search_batches(request.clone(), NonZeroUsize::new(2).unwrap(), |batch| {
                    batch_lengths.push(batch.results.len());
                    sequences.extend(batch.results.iter().map(|result| result.seq));
                    field_views.extend(
                        batch
                            .results
                            .iter()
                            .map(|result| result.field_view().text().to_string()),
                    );
                    parsed_reference_counts.extend(
                        batch
                            .results
                            .iter()
                            .map(|result| result.parsed_references().map_or(0, <[_]>::len)),
                    );
                    context_sequences.extend(batch.context_windows.iter().map(|window| {
                        window.iter().map(|message| message.seq).collect::<Vec<_>>()
                    }));
                    if let Some(metadata) = batch.included.normalized_session_metadata() {
                        included_sessions.extend(metadata.keys().cloned());
                    }
                    Ok(MessageSearchBatchControl::Continue)
                })
                .unwrap();

        assert_eq!(batch_lengths, vec![2, 2]);
        assert_eq!(
            sequences,
            materialized
                .results()
                .iter()
                .map(|result| result.seq)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            field_views,
            materialized
                .results()
                .iter()
                .map(|result| result.field_view().text().to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            parsed_reference_counts,
            materialized
                .results()
                .iter()
                .map(|result| result.parsed_references().map_or(0, <[_]>::len))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            context_sequences,
            materialized
                .context_windows()
                .iter()
                .map(|window| window.iter().map(|message| message.seq).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            included_sessions,
            BTreeSet::from(["claude:bounded-service".to_string()])
        );
        assert!(outcome.exhausted);
        assert_eq!(outcome.emitted, materialized.results().len());
        assert_eq!(
            outcome.page.expect("natural exhaustion has page metadata"),
            materialized.page()
        );
        assert_eq!(
            outcome.ordered_digest.as_deref(),
            Some(materialized.ordered_digest().as_str())
        );
        assert!(outcome.planner.is_some());
        assert!(outcome.origins.is_some());
        assert_eq!(outcome.request.receipt_level(), ReceiptLevel::Full);

        let mut stopped_sequences = Vec::new();
        let stopped = service
            .visit_search_batches(request, NonZeroUsize::new(2).unwrap(), |batch| {
                stopped_sequences.extend(batch.results.iter().map(|result| result.seq));
                Ok(MessageSearchBatchControl::Stop)
            })
            .unwrap();
        assert_eq!(stopped_sequences, vec![0, 2]);
        assert_eq!(stopped.emitted, 2);
        assert!(!stopped.exhausted);
        assert!(stopped.page.is_none());
        assert!(stopped.planner.is_none());
        assert!(stopped.origins.is_none());
        assert!(stopped.ordered_digest.is_none());
    }

    #[test]
    fn latest_literal_and_regex_select_newest_matches_then_present_chronologically() {
        let (_directory, db) = disposable_db();
        insert_session(
            &db,
            "claude:latest-window",
            "/workspace/latest",
            "/transcripts/latest.jsonl",
            &["needle zero", "unrelated", "needle two", "needle three"],
        );
        let config = Config::default();
        let service = MessageService::new(&config, &db, SearchSurface::Rust);

        for query in [
            MessageQuery::literal("needle").unwrap(),
            MessageQuery::regex("needle (zero|two|three)").unwrap(),
        ] {
            let response = service
                .search(
                    MessageSearchRequest::builder(query, MessageTarget::content())
                        .session_id("claude:latest")
                        .unwrap()
                        .match_window(MatchWindow::Latest)
                        .extent(RequestedExtent::page(Some(2), 0).unwrap())
                        .build()
                        .unwrap(),
                )
                .unwrap();
            assert_eq!(
                response
                    .hits()
                    .iter()
                    .map(|hit| hit.seq)
                    .collect::<Vec<_>>(),
                vec![2, 3]
            );
            assert_eq!(response.page().next_offset(), Some(2));
        }
    }

    #[test]
    fn latest_window_applies_to_tool_name_and_tool_argument_targets() {
        let (_directory, db) = disposable_db();
        let mut parsed = minimal_record(
            Provider::Claude,
            std::path::Path::new("/transcripts/latest-tools.jsonl"),
            String::new(),
        );
        parsed.session.id = "claude:latest-tools".into();
        parsed.session.provider_session_id = "latest-tools".into();
        parsed.session.cwd = Some("/workspace/latest-tools".into());
        parsed.messages = (0..4)
            .map(|seq| Message {
                seq,
                role: Role::Tool,
                ts: None,
                tool_name: Some(format!("exec-{seq}")),
                kind: MessageKind::ToolCall,
                tool_call_id: Some(format!("call-{seq}")),
                is_compaction: false,
                content: format!(r#"{{"args":{{"cmd":"needle {seq}"}}}}"#),
            })
            .collect();
        db.upsert_session(&parsed, 0, 0).unwrap();
        let config = Config::default();
        let service = MessageService::new(&config, &db, SearchSurface::Rust);

        for (query, target) in [
            (
                MessageQuery::literal("exec").unwrap(),
                MessageTarget::tool_name(),
            ),
            (
                MessageQuery::regex(r"exec-[0-3]").unwrap(),
                MessageTarget::tool_name(),
            ),
            (
                MessageQuery::literal("needle").unwrap(),
                MessageTarget::tool_argument("/cmd").unwrap(),
            ),
            (
                MessageQuery::regex(r"needle [0-3]").unwrap(),
                MessageTarget::tool_argument("/cmd").unwrap(),
            ),
        ] {
            let response = service
                .search(
                    MessageSearchRequest::builder(query, target)
                        .session_id("claude:latest-tools")
                        .unwrap()
                        .match_window(MatchWindow::Latest)
                        .extent(RequestedExtent::page(Some(2), 0).unwrap())
                        .build()
                        .unwrap(),
                )
                .unwrap();
            assert_eq!(
                response
                    .hits()
                    .iter()
                    .map(|hit| hit.seq)
                    .collect::<Vec<_>>(),
                vec![2, 3]
            );
        }
    }

    #[test]
    fn typed_path_domains_are_separate_while_legacy_path_remains_broad() {
        let (_directory, db) = disposable_db();
        insert_session(
            &db,
            "claude:workspace-domain",
            "/domains/shared/workspace",
            "/elsewhere/workspace.jsonl",
            &["needle"],
        );
        insert_session(
            &db,
            "claude:transcript-domain",
            "/elsewhere/transcript",
            "/domains/shared/transcript/session.jsonl",
            &["needle"],
        );
        let config = Config::default();
        let service = MessageService::new(&config, &db, SearchSurface::Rust);

        let workspace = service
            .search(
                literal_request()
                    .workspace_path_prefix("/domains/shared")
                    .unwrap()
                    .build()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(workspace.hits().len(), 1);
        assert_eq!(workspace.hits()[0].session_id, "claude:workspace-domain");

        let transcript = service
            .search(
                literal_request()
                    .transcript_path_prefix("/domains/shared")
                    .unwrap()
                    .build()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(transcript.hits().len(), 1);
        assert_eq!(transcript.hits()[0].session_id, "claude:transcript-domain");

        let excluded_workspace = service
            .search(
                literal_request()
                    .exclude_workspace_path_prefix("/domains/shared")
                    .unwrap()
                    .build()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(excluded_workspace.hits().len(), 1);
        assert_eq!(
            excluded_workspace.hits()[0].session_id,
            "claude:transcript-domain"
        );

        let excluded_transcript = service
            .search(
                literal_request()
                    .exclude_transcript_path_prefix("/domains/shared")
                    .unwrap()
                    .build()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(excluded_transcript.hits().len(), 1);
        assert_eq!(
            excluded_transcript.hits()[0].session_id,
            "claude:workspace-domain"
        );

        let legacy = service
            .search_legacy(
                "needle",
                &MessageFilters {
                    path_prefix: Some("/domains/shared".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(legacy.len(), 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn services_share_one_database_and_return_typed_empty_results() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.mark_schema_current().unwrap();

        let config = Config::default();
        let messages = MessageService::new(&config, &db, SearchSurface::Rust)
            .search_legacy("missing", &MessageFilters::default())
            .unwrap();
        let files = FileService::new(&db).search(&FileQuery::default()).unwrap();
        let mut config = Config::default();
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().to_string());
        let app = SessionSearch::open(config).unwrap();
        app.database().mark_schema_current().unwrap();
        let status = app.index().status().unwrap();

        assert!(messages.is_empty());
        assert!(files.is_empty());
        assert!(status.parser_health.schema_current);
        assert_eq!(status.parser_health.indexed_sessions, 0);
    }

    #[test]
    fn application_owns_config_database_and_service_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().to_string());
        config.index.cache_dir = Some(dir.path().join("cache").to_string_lossy().to_string());
        config.providers.claude.enabled = false;
        config.providers.claude_desktop.enabled = false;
        config.providers.codex.enabled = false;
        config.providers.cursor.enabled = false;
        config.providers.antigravity.enabled = false;
        config.providers.pi.enabled = false;
        config.providers.aistudio.enabled = false;
        config.providers.gemini_cli.enabled = false;

        let app = SessionSearch::open(config).unwrap();
        let outcome = app.index().refresh().unwrap();

        assert!(matches!(outcome, AutoReindexOutcome::Updated { .. }));
        assert!(app.config().db_path().exists());
        assert!(app
            .catalog()
            .list_sessions(&SearchFilters {
                limit: 1,
                ..SearchFilters::default()
            })
            .unwrap()
            .is_empty());

        let diagnostics = app.maintenance().diagnostics().unwrap();
        assert_eq!(diagnostics.db_path, app.config().db_path());
        let compacted = app.maintenance().compact().unwrap();
        assert!(compacted.after_bytes > 0);
        assert_eq!(
            compacted.reclaimed_bytes(),
            compacted.before_bytes.saturating_sub(compacted.after_bytes)
        );
    }

    #[test]
    fn explicit_reindex_owns_lock_on_current_schema() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().to_string());
        config.index.cache_dir = Some(dir.path().join("cache").to_string_lossy().to_string());
        config.providers.claude.enabled = false;
        config.providers.claude_desktop.enabled = false;
        config.providers.codex.enabled = false;
        config.providers.cursor.enabled = false;
        config.providers.antigravity.enabled = false;
        config.providers.pi.enabled = false;
        config.providers.aistudio.enabled = false;
        config.providers.gemini_cli.enabled = false;

        let app = SessionSearch::open(config).unwrap();
        assert!(!app.database().needs_backfill().unwrap());

        assert_eq!(app.index().reindex(false).unwrap(), (0, 0));

        assert!(!app.database().needs_backfill().unwrap());
        assert!(app.index().status().unwrap().parser_health.schema_current);
        assert!(indexer::index_update_lock_path(&app.config().db_path()).exists());
    }

    #[test]
    fn public_full_reindex_promotes_v3_to_current_search_schema() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().to_string());
        config.index.cache_dir = Some(dir.path().join("cache").to_string_lossy().to_string());
        config.providers.claude.enabled = false;
        config.providers.claude_desktop.enabled = false;
        config.providers.codex.enabled = false;
        config.providers.cursor.enabled = false;
        config.providers.antigravity.enabled = false;
        config.providers.pi.enabled = false;
        config.providers.aistudio.enabled = false;
        config.providers.gemini_cli.enabled = false;

        drop(Db::open(&config.db_path()).unwrap());
        let conn = rusqlite::Connection::open(config.db_path()).unwrap();
        conn.execute_batch(
            "drop trigger messages_ai;
                 drop trigger messages_ad;
                 drop trigger messages_au;
                 drop table messages_trigram_terms;
                 drop table messages_trigram_vocab;
                 drop table messages_trigram;
                 pragma user_version=3;",
        )
        .unwrap();
        crate::fts::install_released_message_word_index(&conn).unwrap();
        crate::trigram_index::ensure_schema(&conn).unwrap();
        drop(conn);

        let app = SessionSearch::open(config).unwrap();

        assert_eq!(app.index().reindex(true).unwrap(), (0, 0));
        assert_eq!(
            app.database().schema_version().unwrap(),
            crate::db::SCHEMA_VERSION
        );
        let conn = rusqlite::Connection::open_with_flags(
            app.config().db_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let layout: (bool, bool, bool) = conn
            .query_row(
                "select exists(select 1 from sqlite_schema where name='messages_trigram'),
                        exists(select 1 from sqlite_schema where name='messages_trigram_vocab'),
                        not exists(select 1 from sqlite_schema where name='trigram_postings')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(layout, (true, true, true));
    }
}
