//! The one shape every provider produces for a subagent transcript.
//!
//! Each agent CLI stores subagent runs differently — claude under
//! `<parent-session-id>/subagents/`, cursor under `<parent-session-id>/subagents/`, pi under
//! `<parent-session-dir>/<agent>/run-N/`, codex inside a `thread_spawn` payload — so *detection*
//! stays in each provider. What they share is the answer: which session spawned this run, and
//! what distinguishes this run from its siblings. That pair lives here so the identity rule is
//! written once.
//!
//! # Why a subagent's own id is not enough
//!
//! Providers name a subagent with an id that is unique only within its parent. On this
//! machine's 4,051 claude subagent transcripts, `agent-a0e105ee7f1fe2c65` appears under two
//! different parent sessions. Storing either under the bare id makes them one row, and the
//! `on conflict(id) do update` upsert in `db.rs` silently keeps whichever was written last.
//! That is the same failure that cost 65 of 414 codex sessions (see `codex.rs`) and that
//! overwrote four claude parent rows with subagent content.
//!
//! [`SpawnOrigin::session_id`] therefore qualifies the run with its parent. The suffix comes
//! from the path, so it is unique by construction: two files with the same parent and the same
//! suffix would have to be the same file. Verified against live data — 0 collisions across all
//! 4,051 transcripts, where the bare id collides once.

use std::path::{Component, Path};

use crate::models::Provider;

/// The directory name every provider that nests subagent transcripts uses for them.
pub(crate) const SUBAGENTS_DIR: &str = "subagents";

/// Where a subagent run sits relative to the session that spawned it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpawnOrigin {
    /// The spawning session's PROVIDER-native id, as the path or payload spells it. Combined
    /// with the provider by [`Self::parent_link`] to get what
    /// [`crate::models::SessionRecord::parent_session_id`] stores.
    pub parent_session_id: String,
    /// What distinguishes this run from its siblings under the same parent: the path below the
    /// parent's directory with the `subagents` marker removed and the extension dropped. For
    /// `<parent>/subagents/agent-a068fa11.jsonl` that is `agent-a068fa11`; for a workflow's
    /// agent, `workflows/wf_2c6289da-a12/agent-ae4f8452`.
    pub run_suffix: String,
}

impl SpawnOrigin {
    /// This run's `provider_session_id`: the parent's id joined to [`Self::run_suffix`].
    ///
    /// Parent-qualified because the provider's own id for a subagent is unique only within its
    /// parent; see the module docs for the live collision this prevents.
    pub(crate) fn session_id(&self) -> String {
        format!("{}/{}", self.parent_session_id, self.run_suffix)
    }

    /// The parent as [`crate::models::SessionRecord::parent_session_id`] stores it: the whole
    /// session id of the parent's row, provider prefix included.
    ///
    /// A link points at the primary key, so a reader comparing two records sees the parent's
    /// `id` and the run's `parent_session_id` written identically and needs no rule about
    /// stripping a prefix. See [`parent_link`], which every provider uses.
    pub(crate) fn parent_link(&self, provider: Provider) -> String {
        parent_link(provider, &self.parent_session_id)
    }
}

/// The value [`crate::models::SessionRecord::parent_session_id`] holds, from a provider and the
/// parent's provider-native id: the parent row's `id`.
///
/// Providers that read a parent id out of a payload rather than a path (codex's
/// `thread_spawn.parent_thread_id`) call this directly.
pub(crate) fn parent_link(provider: Provider, parent_provider_session_id: &str) -> String {
    format!("{provider}:{parent_provider_session_id}")
}

/// Read spawn origin off a path of the form
/// `<...>/<parent-session-id>/subagents/<nested...>/<file>.<ext>`.
///
/// Returns `None` when the path has no `subagents` component or nothing precedes it, which is
/// how a caller distinguishes a top-level session from a spawned run without opening the file.
/// Directory-derived rather than content-derived so discovery stays a walk: reading the parent
/// from inside every candidate would mean opening 4,051 files per index on this machine. The
/// two agree — the record `sessionId` matched the directory name in 4,051 of 4,051 live claude
/// transcripts, and was never absent — so nothing is lost by preferring the cheaper source.
pub(crate) fn subagents_dir_origin(path: &Path) -> Option<SpawnOrigin> {
    let mut parent_session_id: Option<String> = None;
    let mut nested: Vec<&str> = Vec::new();
    let mut previous: Option<&str> = None;

    for component in path.components() {
        let Component::Normal(name) = component else {
            previous = None;
            continue;
        };
        let name = name.to_str()?;
        if parent_session_id.is_some() {
            nested.push(name);
        } else if name == SUBAGENTS_DIR {
            parent_session_id = Some(previous?.to_string());
        }
        previous = Some(name);
    }

    let parent_session_id = parent_session_id?;
    let file = nested.pop()?;
    let stem = Path::new(file).file_stem()?.to_str()?;
    nested.push(stem);
    Some(SpawnOrigin {
        parent_session_id,
        run_suffix: nested.join("/"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The defect this module exists to prevent, stated as a test: two subagents that share
    /// their provider-assigned id under different parents must produce two distinct session
    /// ids. Asserting distinctness alone is not enough — swapping the two would preserve it —
    /// so each id is checked against the parent it belongs to.
    #[test]
    fn subagents_sharing_an_agent_id_under_different_parents_stay_separate() {
        // Both paths observed on live data, same `agent-` stem, different parents.
        let first = PathBuf::from(
            "/p/-Users-x/a2f3f693-e77f-4212-9e71-2b2331565fd4/subagents/agent-a0e105ee7f1fe2c65.jsonl",
        );
        let second = PathBuf::from(
            "/p/-Users-x/f2c5b7c7-c4bc-4ab3-bb21-7c381637b8ce/subagents/agent-a0e105ee7f1fe2c65.jsonl",
        );

        let first = subagents_dir_origin(&first).expect("a subagents/ path has an origin");
        let second = subagents_dir_origin(&second).expect("a subagents/ path has an origin");

        assert_eq!(
            first.session_id(),
            "a2f3f693-e77f-4212-9e71-2b2331565fd4/agent-a0e105ee7f1fe2c65",
            "a run is identified by its parent plus what distinguishes it under that parent"
        );
        assert_eq!(
            second.session_id(),
            "f2c5b7c7-c4bc-4ab3-bb21-7c381637b8ce/agent-a0e105ee7f1fe2c65",
            "the second run keeps its own parent; sharing an agent id must not merge the rows"
        );
        assert_eq!(
            first.parent_session_id,
            "a2f3f693-e77f-4212-9e71-2b2331565fd4"
        );
        assert_eq!(
            second.parent_session_id,
            "f2c5b7c7-c4bc-4ab3-bb21-7c381637b8ce"
        );
    }

    /// Workflow agents nest one level deeper. The intervening directories join the suffix
    /// rather than being dropped, both to keep the id unique and because the workflow id is
    /// the grouping a reader wants back.
    #[test]
    fn a_workflow_agent_keeps_the_workflow_in_its_run_suffix() {
        let path = PathBuf::from(
            "/p/-Users-x/77f26fc7-6ca3-4a98-a8b5-32f1963941ab/subagents/workflows/wf_4b4d88ab-f99/agent-ae4f8452cb555e0bd.jsonl",
        );
        let origin = subagents_dir_origin(&path).expect("nested runs have an origin too");
        assert_eq!(
            origin.parent_session_id,
            "77f26fc7-6ca3-4a98-a8b5-32f1963941ab"
        );
        assert_eq!(
            origin.run_suffix,
            "workflows/wf_4b4d88ab-f99/agent-ae4f8452cb555e0bd"
        );
    }

    /// A top-level session has no origin, which is what lets discovery tell the two apart
    /// without opening the file.
    #[test]
    fn a_path_without_a_subagents_component_has_no_origin() {
        let path = PathBuf::from("/p/-Users-x/7e745098-c299-4cf5-bdbe-5cdb1fb5a62d.jsonl");
        assert_eq!(subagents_dir_origin(&path), None);
    }

    /// `subagents` with nothing before it names no parent, so it yields no origin rather than
    /// an origin with an empty parent — an empty parent id would match every other empty one
    /// on the `parent_session_id` equality filter.
    #[test]
    fn a_subagents_directory_with_no_parent_before_it_has_no_origin() {
        assert_eq!(
            subagents_dir_origin(Path::new("subagents/agent-a068fa11.jsonl")),
            None
        );
        assert_eq!(
            subagents_dir_origin(Path::new("/subagents/agent-a068fa11.jsonl")),
            None
        );
    }

    /// The marker directory with no file under it is a directory, not a run.
    #[test]
    fn a_subagents_directory_with_no_file_under_it_has_no_origin() {
        assert_eq!(
            subagents_dir_origin(Path::new("/p/-Users-x/7e745098/subagents")),
            None
        );
    }
}
