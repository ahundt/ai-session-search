//! Deterministic, crash-safe publication of bounded analysis metadata.
//!
//! Rendering is pure and publication writes an immutable directory through a
//! same-parent staging directory. Existing destinations are never replaced.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::analysis_pipeline::{AnalysisResult, SessionGraph};
use crate::durable_fs::{entry_exists, rename_noreplace, sync_directory, sync_parent};

const BUNDLE_SCHEMA_VERSION: u32 = 1;
const ANALYSIS_JSON: &str = "analysis.v1.json";
const GRAPH_JSON: &str = "session-graph.v1.json";
const INDEX_MARKDOWN: &str = "index.md";
const TAXONOMY_MARKDOWN: &str = "taxonomy.md";
const GRAPH_MARKDOWN: &str = "knowledge-graph.md";
const MANIFEST_JSON: &str = "manifest.v1.json";
static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// One requested representation in an analysis publication bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisPublicationFormat {
    Json,
    Markdown,
}

/// One rendered artifact, available for inspection before filesystem publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisArtifact {
    name: &'static str,
    content: String,
    sha256: String,
}

impl AnalysisArtifact {
    /// Stable bundle-relative filename.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Complete UTF-8 artifact content.
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Lowercase SHA-256 digest of [`Self::content`].
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Serialized byte length of [`Self::content`].
    pub fn bytes(&self) -> usize {
        self.content.len()
    }
}

/// Durable evidence for one atomically published immutable artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedAnalysisArtifact {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
}

/// Receipt returned only after the complete publication directory is durable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalysisPublicationReceipt {
    pub destination: PathBuf,
    pub artifacts: Vec<PublishedAnalysisArtifact>,
}

#[derive(Serialize)]
struct VersionedAnalysis<'a> {
    schema_version: u32,
    analysis: &'a AnalysisResult,
}

#[derive(Serialize)]
struct VersionedGraph<'a> {
    schema_version: u32,
    node_count: usize,
    edge_count: usize,
    group_count: usize,
    graph: &'a SessionGraph,
}

#[derive(Debug, Serialize, Deserialize)]
struct BundleManifest {
    schema_version: u32,
    artifacts: Vec<PublishedAnalysisArtifact>,
}

/// Validated immutable publication request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisPublicationPlan {
    destination: PathBuf,
    formats: BTreeSet<AnalysisPublicationFormat>,
}

impl AnalysisPublicationPlan {
    /// Construct a publication plan.
    ///
    /// The destination must be absolute, must identify a child entry rather than a
    /// filesystem root, and must not already exist when [`Self::publish`] runs.
    pub fn new(
        destination: impl Into<PathBuf>,
        formats: impl IntoIterator<Item = AnalysisPublicationFormat>,
    ) -> Result<Self> {
        let destination = destination.into();
        if !destination.is_absolute() {
            bail!("analysis publication destination must be absolute");
        }
        if destination.file_name().is_none() {
            bail!("analysis publication destination cannot be a filesystem root");
        }
        let formats = formats.into_iter().collect::<BTreeSet<_>>();
        if formats.is_empty() {
            bail!("analysis publication requires at least one format");
        }
        Ok(Self {
            destination,
            formats,
        })
    }

    /// Final immutable bundle destination.
    pub fn destination(&self) -> &Path {
        &self.destination
    }

    /// Requested representations in deterministic order.
    pub fn formats(&self) -> impl ExactSizeIterator<Item = AnalysisPublicationFormat> + '_ {
        self.formats.iter().copied()
    }

    /// Render every requested artifact without touching the filesystem.
    ///
    /// Allocation is proportional to the serialized analysis metadata. Source
    /// messages are absent from [`AnalysisResult`] and therefore cannot leak here.
    pub fn render(&self, result: &AnalysisResult) -> Result<Vec<AnalysisArtifact>> {
        let graph = result.session_graph();
        let mut rendered = BTreeMap::<&'static str, String>::new();
        if self.formats.contains(&AnalysisPublicationFormat::Json) {
            rendered.insert(
                ANALYSIS_JSON,
                pretty_json(&VersionedAnalysis {
                    schema_version: BUNDLE_SCHEMA_VERSION,
                    analysis: result,
                })?,
            );
            rendered.insert(
                GRAPH_JSON,
                pretty_json(&VersionedGraph {
                    schema_version: BUNDLE_SCHEMA_VERSION,
                    node_count: graph.nodes.len(),
                    edge_count: graph.edges.len(),
                    group_count: graph.groups.len(),
                    graph: &graph,
                })?,
            );
        }
        if self.formats.contains(&AnalysisPublicationFormat::Markdown) {
            rendered.insert(INDEX_MARKDOWN, render_index(result, &graph));
            rendered.insert(TAXONOMY_MARKDOWN, render_taxonomy(result));
            rendered.insert(GRAPH_MARKDOWN, render_graph(&graph));
        }
        let manifest = BundleManifest {
            schema_version: BUNDLE_SCHEMA_VERSION,
            artifacts: rendered
                .iter()
                .map(|(name, content)| PublishedAnalysisArtifact {
                    name: (*name).to_owned(),
                    bytes: content.len() as u64,
                    sha256: sha256(content.as_bytes()),
                })
                .collect(),
        };
        rendered.insert(MANIFEST_JSON, pretty_json(&manifest)?);
        Ok(rendered
            .into_iter()
            .map(|(name, content)| AnalysisArtifact {
                name,
                sha256: sha256(content.as_bytes()),
                content,
            })
            .collect())
    }

    /// Publish a complete immutable bundle through one atomic directory rename.
    ///
    /// Every file is created with `create_new`, flushed, and synced before the
    /// destination becomes visible. A guard removes staging output on every error or
    /// unwind path. Existing destinations are rejected without mutation.
    pub fn publish(&self, result: &AnalysisResult) -> Result<AnalysisPublicationReceipt> {
        let artifacts = self.render(result)?;
        reject_existing(&self.destination)?;
        let parent = self
            .destination
            .parent()
            .context("analysis publication destination has no parent")?;
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create analysis publication parent {}",
                parent.display()
            )
        })?;
        let transaction = DirectoryTransaction::begin(parent)?;
        for artifact in &artifacts {
            transaction.write(artifact.name(), artifact.content().as_bytes())?;
        }
        sync_directory(transaction.path())?;
        reject_existing(&self.destination)?;
        transaction.publish(&self.destination)?;

        Ok(AnalysisPublicationReceipt {
            destination: self.destination.clone(),
            artifacts: artifacts
                .into_iter()
                .map(|artifact| PublishedAnalysisArtifact {
                    name: artifact.name().to_owned(),
                    bytes: artifact.bytes() as u64,
                    sha256: artifact.sha256().to_owned(),
                })
                .collect(),
        })
    }
}

fn pretty_json(value: &impl Serialize) -> Result<String> {
    let mut output = serde_json::to_string_pretty(value)?;
    output.push('\n');
    Ok(output)
}

fn render_index(result: &AnalysisResult, graph: &SessionGraph) -> String {
    let mut output = format!(
        "# AI Session Analysis\n\n\
         - Sessions: {}\n\
         - Recurring phrases: {}\n\
         - Resolved relationship edges: {}\n\
         - Shared project groups: {}\n\n\
         ## Artifacts\n\n\
         - [Session taxonomy]({TAXONOMY_MARKDOWN})\n\
         - [Knowledge graph]({GRAPH_MARKDOWN})\n",
        result.sessions.len(),
        result.vocabulary.len(),
        graph.edges.len(),
        graph.groups.len(),
    );
    output.push_str(
        "\n## Sessions ranked by policy score\n\n\
         Scores are the sum of the validated classification-rule weights that matched each session.\n\n\
         | Rank | Score | Session | Provider | Classifications |\n\
         | :--- | ---: | :--- | :--- | :--- |\n",
    );

    let mut ranked = result.sessions.iter().collect::<Vec<_>>();
    ranked.sort_by(|(left_id, left), (right_id, right)| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left_id.cmp(right_id))
    });
    for (rank, (session_id, analyzed)) in ranked.into_iter().enumerate() {
        let title = analyzed
            .session
            .title
            .as_deref()
            .filter(|title| !title.is_empty())
            .unwrap_or(session_id.as_str());
        let mut classifications = analyzed
            .classifications
            .iter()
            .map(|classification| format!("{}:{}", classification.dimension, classification.label))
            .collect::<Vec<_>>();
        classifications.sort();
        classifications.dedup();
        let classifications = if classifications.is_empty() {
            "-".to_owned()
        } else {
            classifications.join(", ")
        };
        output.push_str(&format!(
            "| {} | {} | {} ({}) | {} | {} |\n",
            rank + 1,
            analyzed.score,
            escape_markdown(title),
            escape_markdown(session_id),
            escape_markdown(analyzed.session.provider.as_str()),
            escape_markdown(&classifications),
        ));
    }
    output
}

fn render_taxonomy(result: &AnalysisResult) -> String {
    let mut rows = BTreeMap::<(String, String), BTreeSet<String>>::new();
    for (session_id, session) in &result.sessions {
        for classification in &session.classifications {
            rows.entry((
                classification.dimension.clone(),
                classification.label.clone(),
            ))
            .or_default()
            .insert(session_id.clone());
        }
    }

    let mut output = String::from(
        "# Session Taxonomy\n\n| Dimension | Label | Canonical sessions |\n| :--- | :--- | :--- |\n",
    );
    for ((dimension, label), session_ids) in rows {
        let sessions = session_ids
            .into_iter()
            .map(|value| escape_markdown(&value))
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&format!(
            "| {} | {} | {} |\n",
            escape_markdown(&dimension),
            escape_markdown(&label),
            sessions,
        ));
    }
    output
}

fn render_graph(graph: &SessionGraph) -> String {
    let mut output = String::from(
        "# Knowledge Graph\n\n## Explicit resolved relationships\n\n\
         | Source | Relationship | Target | Rule |\n| :--- | :--- | :--- | :--- |\n",
    );
    for edge in &graph.edges {
        output.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            escape_markdown(&edge.source_session_id),
            escape_markdown(&format!("{:?}", edge.kind).to_lowercase()),
            escape_markdown(&edge.target_session_id),
            escape_markdown(&edge.rule_id),
        ));
    }
    output.push_str(
        "\n## Shared project groups\n\n| Dimension | Key | Canonical sessions |\n| :--- | :--- | :--- |\n",
    );
    for group in &graph.groups {
        output.push_str(&format!(
            "| {} | {} | {} |\n",
            escape_markdown(&group.dimension),
            escape_markdown(&group.key),
            group
                .session_ids
                .iter()
                .map(|value| escape_markdown(value))
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }
    output
}

fn escape_markdown(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace('`', "\\`")
        .replace(['\r', '\n'], " ")
}

fn sha256(content: &[u8]) -> String {
    format!("{:x}", Sha256::digest(content))
}

fn reject_existing(path: &Path) -> Result<()> {
    match entry_exists(path) {
        Ok(true) => bail!(
            "analysis publication destination already exists: {}",
            path.display()
        ),
        Ok(false) => Ok(()),
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to inspect analysis publication destination {}",
                path.display()
            )
        }),
    }
}

struct DirectoryTransaction {
    path: PathBuf,
    committed: bool,
}

impl DirectoryTransaction {
    fn begin(parent: &Path) -> Result<Self> {
        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is earlier than the Unix epoch")?
            .as_nanos();
        let path = parent.join(format!(
            ".ai-session-search-analysis-stage-{}-{nonce}-{sequence}",
            std::process::id(),
        ));
        fs::create_dir(&path)
            .with_context(|| format!("failed to create staging directory {}", path.display()))?;
        Ok(Self {
            path,
            committed: false,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, name: &str, content: &[u8]) -> Result<()> {
        let path = self.path.join(name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("failed to create staged artifact {}", path.display()))?;
        file.write_all(content)
            .with_context(|| format!("failed to write staged artifact {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync staged artifact {}", path.display()))
    }

    fn publish(mut self, destination: &Path) -> Result<()> {
        rename_noreplace(&self.path, destination).with_context(|| {
            format!(
                "failed to atomically publish {} as {}",
                self.path.display(),
                destination.display()
            )
        })?;
        self.path = destination.to_path_buf();
        sync_parent(destination)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for DirectoryTransaction {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use tempfile::tempdir;

    use super::*;
    use crate::analysis_pipeline::{
        AnalyzedSession, ClassificationMatch, ClassificationTarget, RelationshipHint,
        RelationshipKind, RelationshipResolution,
    };
    use crate::models::Provider;
    use crate::util::minimal_record;

    fn fixture() -> AnalysisResult {
        let mut parent = minimal_record(
            Provider::Claude,
            Path::new("/fixture/parent.jsonl"),
            String::new(),
        )
        .session;
        parent.id = "claude:parent".to_owned();
        parent.provider_session_id = "parent".to_owned();
        parent.title = Some("Parent | session".to_owned());
        parent.cwd = Some("/workspace/shared".to_owned());

        let mut child = minimal_record(
            Provider::Codex,
            Path::new("/fixture/child.jsonl"),
            String::new(),
        )
        .session;
        child.id = "codex:child".to_owned();
        child.provider_session_id = "child".to_owned();
        child.title = Some("Child\nsession".to_owned());
        child.cwd = Some("/workspace/shared".to_owned());

        let classification = ClassificationMatch {
            dimension: "workflow".to_owned(),
            label: "review|repair".to_owned(),
            target: ClassificationTarget::Any,
            weight: 2,
        };
        AnalysisResult {
            sessions: BTreeMap::from([
                (
                    parent.id.clone(),
                    AnalyzedSession {
                        session: parent,
                        has_user_text: true,
                        classifications: vec![classification.clone()],
                        score: 2,
                        relationship_hints: Vec::new(),
                        message_count: 2,
                        user_message_count: 1,
                    },
                ),
                (
                    child.id.clone(),
                    AnalyzedSession {
                        session: child,
                        has_user_text: true,
                        classifications: vec![classification],
                        score: 2,
                        relationship_hints: vec![RelationshipHint {
                            rule_id: "explicit-parent".to_owned(),
                            kind: RelationshipKind::Branch,
                            parent_title: "Parent | session".to_owned(),
                            resolution: RelationshipResolution::Resolved {
                                session_id: "claude:parent".to_owned(),
                            },
                        }],
                        message_count: 3,
                        user_message_count: 1,
                    },
                ),
            ]),
            vocabulary: Vec::new(),
        }
    }

    #[test]
    fn render_is_deterministic_bounded_and_escapes_markdown_cells() {
        let dir = tempdir().unwrap();
        let plan = AnalysisPublicationPlan::new(
            dir.path().join("bundle"),
            [
                AnalysisPublicationFormat::Json,
                AnalysisPublicationFormat::Markdown,
            ],
        )
        .unwrap();
        let first = plan.render(&fixture()).unwrap();
        let second = plan.render(&fixture()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 6);
        let taxonomy = first
            .iter()
            .find(|artifact| artifact.name() == TAXONOMY_MARKDOWN)
            .unwrap();
        let index = first
            .iter()
            .find(|artifact| artifact.name() == INDEX_MARKDOWN)
            .unwrap();
        assert!(index
            .content()
            .contains("## Sessions ranked by policy score"));
        assert!(index
            .content()
            .contains("Parent \\| session (claude:parent)"));
        assert!(index.content().contains("Child session (codex:child)"));
        assert!(
            index.content().find("claude:parent").unwrap()
                < index.content().find("codex:child").unwrap(),
            "equal scores must use canonical session ID as the stable tie-breaker"
        );
        assert!(!index.content().contains("misc_research"));
        assert!(taxonomy.content().contains("review\\|repair"));
        assert!(!taxonomy.content().contains("Parent | session"));
        assert!(first.iter().all(|artifact| artifact.sha256().len() == 64));
        let manifest_artifact = first
            .iter()
            .find(|artifact| artifact.name() == MANIFEST_JSON)
            .unwrap();
        let manifest: BundleManifest = serde_json::from_str(manifest_artifact.content()).unwrap();
        assert_eq!(manifest.schema_version, BUNDLE_SCHEMA_VERSION);
        assert_eq!(manifest.artifacts.len(), 5);
        for payload in first
            .iter()
            .filter(|artifact| artifact.name() != MANIFEST_JSON)
        {
            let listed = manifest
                .artifacts
                .iter()
                .find(|artifact| artifact.name == payload.name())
                .unwrap();
            assert_eq!(listed.bytes, payload.bytes() as u64);
            assert_eq!(listed.sha256, payload.sha256());
        }
    }

    #[test]
    fn publish_is_atomic_immutable_and_receipted() {
        let dir = tempdir().unwrap();
        let destination = dir.path().join("bundle");
        let plan = AnalysisPublicationPlan::new(
            &destination,
            [
                AnalysisPublicationFormat::Json,
                AnalysisPublicationFormat::Markdown,
            ],
        )
        .unwrap();
        let receipt = plan.publish(&fixture()).unwrap();
        assert_eq!(receipt.destination, destination);
        assert_eq!(receipt.artifacts.len(), 6);
        for artifact in receipt.artifacts {
            let bytes = fs::read(destination.join(&artifact.name)).unwrap();
            assert_eq!(bytes.len() as u64, artifact.bytes);
            assert_eq!(sha256(&bytes), artifact.sha256);
        }
        assert!(!dir.path().read_dir().unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("stage")));
    }

    #[test]
    fn existing_destination_and_broken_symlink_are_never_replaced() {
        let dir = tempdir().unwrap();
        let destination = dir.path().join("bundle");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("sentinel"), "preserve").unwrap();
        let plan =
            AnalysisPublicationPlan::new(&destination, [AnalysisPublicationFormat::Json]).unwrap();
        assert!(plan.publish(&fixture()).is_err());
        assert_eq!(
            fs::read_to_string(destination.join("sentinel")).unwrap(),
            "preserve"
        );

        let link = dir.path().join("broken-link");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("missing"), &link).unwrap();
            let link_plan =
                AnalysisPublicationPlan::new(&link, [AnalysisPublicationFormat::Markdown]).unwrap();
            assert!(link_plan.publish(&fixture()).is_err());
            assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        }
    }

    #[test]
    fn destination_directory_claim_race_never_replaces_the_winner() {
        let dir = tempdir().unwrap();
        let destination = dir.path().join("bundle");
        let transaction = DirectoryTransaction::begin(dir.path()).unwrap();
        transaction
            .write("payload", b"complete staged bundle")
            .unwrap();
        sync_directory(transaction.path()).unwrap();

        fs::create_dir(&destination).unwrap();
        let error = transaction.publish(&destination).unwrap_err();

        assert!(error.to_string().contains("atomically publish"));
        assert!(destination.is_dir());
        assert_eq!(destination.read_dir().unwrap().count(), 0);
        assert!(!dir.path().read_dir().unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("stage")));
    }

    #[test]
    fn concurrent_publishers_leave_one_complete_bundle() {
        let dir = tempdir().unwrap();
        let destination = dir.path().join("bundle");
        let plan = Arc::new(
            AnalysisPublicationPlan::new(
                &destination,
                [
                    AnalysisPublicationFormat::Json,
                    AnalysisPublicationFormat::Markdown,
                ],
            )
            .unwrap(),
        );
        let result = Arc::new(fixture());
        let barrier = Arc::new(Barrier::new(2));
        let threads = (0..2)
            .map(|_| {
                let plan = Arc::clone(&plan);
                let result = Arc::clone(&result);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    plan.publish(&result)
                })
            })
            .collect::<Vec<_>>();
        let outcomes = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        assert_eq!(
            outcomes.iter().filter(|outcome| outcome.is_err()).count(),
            1
        );
        assert!(destination.join(ANALYSIS_JSON).is_file());
        assert!(destination.join(GRAPH_MARKDOWN).is_file());
    }

    #[test]
    fn invalid_plan_fails_before_filesystem_mutation() {
        assert!(AnalysisPublicationPlan::new(
            PathBuf::from("relative"),
            [AnalysisPublicationFormat::Json]
        )
        .is_err());
        #[cfg(unix)]
        assert!(AnalysisPublicationPlan::new(
            PathBuf::from("/"),
            [AnalysisPublicationFormat::Json]
        )
        .is_err());
        let no_formats = Vec::<AnalysisPublicationFormat>::new();
        assert!(AnalysisPublicationPlan::new(
            std::env::temp_dir().join("aise-analysis-output"),
            no_formats
        )
        .is_err());
    }
}
