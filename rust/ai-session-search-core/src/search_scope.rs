use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;

use crate::config::{SearchScopeConfig, SearchScopeMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AccessRootOrigin {
    HarnessRoots,
    ExplicitConfig,
    InvocationDirectory,
}

impl AccessRootOrigin {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HarnessRoots => "harness-roots",
            Self::ExplicitConfig => "explicit-config",
            Self::InvocationDirectory => "invocation-directory",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct AccessRootSource {
    configured_path: PathBuf,
    canonicalized_at_startup: bool,
    origin: AccessRootOrigin,
}

impl AccessRootSource {
    pub fn configured_path(&self) -> &Path {
        &self.configured_path
    }

    pub const fn origin(&self) -> AccessRootOrigin {
        self.origin
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct AccessRoot {
    canonical_path: PathBuf,
    match_paths: Vec<PathBuf>,
    sources: Vec<AccessRootSource>,
}

impl AccessRoot {
    pub fn path(&self) -> &Path {
        &self.canonical_path
    }

    pub fn configured_path(&self) -> &Path {
        self.sources[0].configured_path()
    }

    pub fn origin(&self) -> AccessRootOrigin {
        self.sources[0].origin()
    }

    pub fn sources(&self) -> &[AccessRootSource] {
        &self.sources
    }

    fn database_prefixes(&self) -> impl Iterator<Item = &str> {
        self.match_paths
            .iter()
            .map(|path| path.to_str().expect("validated access roots are UTF-8"))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedAccessInputs {
    pub harness_roots: Vec<PathBuf>,
    pub invocation_directory: Option<PathBuf>,
}

impl TrustedAccessInputs {
    pub(crate) fn capture(config: &SearchScopeConfig, harness_roots: Vec<PathBuf>) -> Result<Self> {
        let invocation_directory = if config.include_invocation_directory {
            Some(std::env::current_dir().map_err(|error| {
                anyhow!("cannot resolve invocation directory for allowed-roots scope: {error}")
            })?)
        } else {
            None
        };
        Ok(Self {
            harness_roots,
            invocation_directory,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
pub enum EffectiveAccessScope {
    All,
    AllowedRoots { roots: Vec<AccessRoot> },
}

impl EffectiveAccessScope {
    pub fn resolve(config: &SearchScopeConfig, inputs: TrustedAccessInputs) -> Result<Self> {
        if config.mode == SearchScopeMode::All {
            return Ok(Self::All);
        }

        let mut candidates = Vec::new();
        candidates.extend(
            inputs
                .harness_roots
                .into_iter()
                .map(|path| (path, AccessRootOrigin::HarnessRoots)),
        );
        candidates.extend(
            config
                .roots
                .iter()
                .map(|path| (PathBuf::from(path), AccessRootOrigin::ExplicitConfig)),
        );
        if config.include_invocation_directory {
            if let Some(path) = inputs.invocation_directory {
                candidates.push((path, AccessRootOrigin::InvocationDirectory));
            }
        }

        let mut roots = Vec::new();
        for (path, origin) in candidates {
            let candidate = normalize_authority_root(&path, origin).with_context(|| {
                format!("invalid {} access root {:?}", origin_name(origin), path)
            })?;
            if let Some(existing) = roots
                .iter_mut()
                .find(|root: &&mut AccessRoot| root.canonical_path == candidate.canonical_path)
            {
                for path in candidate.match_paths {
                    if !existing.match_paths.contains(&path) {
                        existing.match_paths.push(path);
                    }
                }
                existing.sources.extend(candidate.sources);
            } else {
                roots.push(candidate);
            }
        }

        if roots.is_empty() {
            bail!(
                "search scope mode allowed-roots resolved no authoritative roots; configure search.scope.roots, enable include_invocation_directory, or supply trusted harness roots"
            );
        }
        Ok(Self::AllowedRoots { roots })
    }

    pub const fn is_unrestricted(&self) -> bool {
        matches!(self, Self::All)
    }

    pub fn roots(&self) -> &[AccessRoot] {
        match self {
            Self::All => &[],
            Self::AllowedRoots { roots } => roots,
        }
    }

    pub fn workspace_prefixes(&self) -> impl Iterator<Item = &str> {
        self.roots().iter().flat_map(AccessRoot::database_prefixes)
    }

    pub fn allows_workspace_path(&self, path: &Path) -> bool {
        match self {
            Self::All => true,
            Self::AllowedRoots { roots } => lexical_absolute(path).is_ok_and(|path| {
                roots
                    .iter()
                    .flat_map(|root| &root.match_paths)
                    .any(|root| path.starts_with(root))
            }),
        }
    }

    pub fn validate_stable(&self) -> Result<()> {
        for root in self.roots() {
            for source in &root.sources {
                if !source.canonicalized_at_startup {
                    continue;
                }
                let current = fs::canonicalize(&source.configured_path).with_context(|| {
                    format!(
                        "allowed root {:?} disappeared after scope resolution",
                        source.configured_path
                    )
                })?;
                if current != root.canonical_path {
                    bail!(
                        "allowed root {:?} changed target after scope resolution; refusing to widen access",
                        source.configured_path
                    );
                }
            }
        }
        Ok(())
    }
}

fn origin_name(origin: AccessRootOrigin) -> &'static str {
    origin.as_str()
}

fn normalize_authority_root(path: &Path, origin: AccessRootOrigin) -> Result<AccessRoot> {
    let lexical = lexical_absolute(path)?;
    let (canonical, canonicalized_at_startup) = match fs::canonicalize(&lexical) {
        Ok(path) => {
            if !path.is_dir() {
                bail!("access root exists but is not a directory");
            }
            (lexical_absolute(&path)?, true)
        }
        Err(error)
            if error.kind() == ErrorKind::NotFound
                && origin == AccessRootOrigin::ExplicitConfig =>
        {
            (lexical.clone(), false)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            bail!("trusted runtime access root does not exist")
        }
        Err(error) => {
            return Err(error).with_context(|| format!("cannot resolve access root {lexical:?}"));
        }
    };
    if canonical.parent().is_none() {
        bail!("filesystem roots are not valid allowed-roots entries");
    }
    if lexical.to_str().is_none() || canonical.to_str().is_none() {
        bail!("access roots must be valid UTF-8 for the SQLite path model");
    }
    let mut match_paths = vec![canonical.clone()];
    if lexical != canonical {
        match_paths.push(lexical.clone());
    }
    Ok(AccessRoot {
        canonical_path: canonical,
        match_paths,
        sources: vec![AccessRootSource {
            configured_path: lexical,
            canonicalized_at_startup,
            origin,
        }],
    })
}

pub(crate) fn validate_configured_root(path: &Path) -> Result<()> {
    normalize_authority_root(path, AccessRootOrigin::ExplicitConfig).map(|_| ())
}

pub(crate) fn ensure_raw_sql_allowed(config: &SearchScopeConfig, operation: &str) -> Result<()> {
    if config.mode == SearchScopeMode::AllowedRoots {
        bail!(
            "{operation} is unavailable while search.scope.mode is allowed-roots because arbitrary SQL cannot enforce workspace authority; use typed search, session, message, analysis, file, or export operations"
        );
    }
    Ok(())
}

fn lexical_absolute(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("access roots must be absolute paths");
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() || !normalized.has_root() {
                    return Err(anyhow!("access root escapes its filesystem root"));
                }
            }
        }
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn restricted(roots: &[&Path]) -> SearchScopeConfig {
        SearchScopeConfig {
            mode: SearchScopeMode::AllowedRoots,
            roots: roots
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            include_invocation_directory: false,
        }
    }

    #[test]
    fn unrestricted_scope_preserves_every_path_and_ignores_unused_inputs() {
        let scope = EffectiveAccessScope::resolve(
            &SearchScopeConfig::default(),
            TrustedAccessInputs {
                harness_roots: vec![PathBuf::from("relative-is-unused")],
                invocation_directory: None,
            },
        )
        .unwrap();
        assert!(scope.is_unrestricted());
        assert!(scope.allows_workspace_path(Path::new("relative-is-still-visible")));
    }

    #[test]
    fn restricted_scope_unions_deduplicates_and_records_origins() {
        let root = tempfile::tempdir().unwrap();
        let configured = root.path().join("configured");
        let harness = root.path().join("harness");
        let invocation = root.path().join("invocation");
        fs::create_dir_all(&configured).unwrap();
        fs::create_dir_all(&harness).unwrap();
        fs::create_dir_all(&invocation).unwrap();
        let mut config = restricted(&[&configured]);
        config.include_invocation_directory = true;

        let scope = EffectiveAccessScope::resolve(
            &config,
            TrustedAccessInputs {
                harness_roots: vec![harness.clone(), configured.clone()],
                invocation_directory: Some(invocation.clone()),
            },
        )
        .unwrap();

        assert_eq!(scope.roots().len(), 3);
        assert_eq!(scope.roots()[0].origin(), AccessRootOrigin::HarnessRoots);
        assert_eq!(scope.roots()[1].origin(), AccessRootOrigin::HarnessRoots);
        assert_eq!(scope.roots()[1].sources().len(), 2);
        assert_eq!(
            scope.roots()[1].sources()[1].origin(),
            AccessRootOrigin::ExplicitConfig
        );
        assert_eq!(
            scope.roots()[2].origin(),
            AccessRootOrigin::InvocationDirectory
        );
        assert!(scope.allows_workspace_path(&harness.join("child")));
        assert!(scope.allows_workspace_path(&configured));
        assert!(scope.allows_workspace_path(&invocation.join("child")));
        assert!(!scope.allows_workspace_path(root.path().join("outside").as_path()));
    }

    #[test]
    fn restricted_scope_fails_closed_without_authoritative_roots() {
        let error = EffectiveAccessScope::resolve(&restricted(&[]), TrustedAccessInputs::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("resolved no authoritative roots"), "{error}");
    }

    #[test]
    fn roots_require_absolute_non_root_directory_paths() {
        for value in [Path::new("relative"), Path::new("/")] {
            let error = EffectiveAccessScope::resolve(
                &restricted(&[value]),
                TrustedAccessInputs::default(),
            )
            .unwrap_err()
            .to_string();
            assert!(
                error.contains("invalid explicit-config access root"),
                "{error}"
            );
        }
    }

    #[test]
    fn arbitrary_sql_is_allowed_only_for_unrestricted_scope() {
        ensure_raw_sql_allowed(&SearchScopeConfig::default(), "test query").unwrap();
        let error = ensure_raw_sql_allowed(&restricted(&[Path::new("/configured")]), "test query")
            .unwrap_err()
            .to_string();
        assert!(error.contains("test query is unavailable"));
        assert!(error.contains("arbitrary SQL cannot enforce workspace authority"));
    }

    #[test]
    fn lexical_normalization_preserves_component_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let allowed = root.path().join("a/inside/../allowed");
        fs::create_dir_all(root.path().join("a/allowed")).unwrap();
        let scope =
            EffectiveAccessScope::resolve(&restricted(&[&allowed]), TrustedAccessInputs::default())
                .unwrap();
        assert!(scope.allows_workspace_path(&root.path().join("a/allowed/child")));
        assert!(!scope.allows_workspace_path(&root.path().join("a/allowed-sibling")));
    }

    #[cfg(unix)]
    #[test]
    fn existing_symlink_keeps_lexical_alias_and_fails_closed_after_retarget() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let alias = root.path().join("alias");
        fs::create_dir(&target).unwrap();
        symlink(&target, &alias).unwrap();

        let scope =
            EffectiveAccessScope::resolve(&restricted(&[&alias]), TrustedAccessInputs::default())
                .unwrap();
        assert_eq!(scope.roots()[0].path(), target.canonicalize().unwrap());
        assert!(scope.allows_workspace_path(&scope.roots()[0].path().join("child")));
        assert!(scope.allows_workspace_path(&alias.join("child")));
        scope.validate_stable().unwrap();

        let other = root.path().join("other");
        fs::create_dir(&other).unwrap();
        fs::remove_file(&alias).unwrap();
        symlink(&other, &alias).unwrap();
        let error = scope.validate_stable().unwrap_err().to_string();
        assert!(error.contains("changed target"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn every_deduplicated_symlink_alias_is_revalidated() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let other = root.path().join("other");
        let configured_alias = root.path().join("configured-alias");
        let harness_alias = root.path().join("harness-alias");
        fs::create_dir(&target).unwrap();
        fs::create_dir(&other).unwrap();
        symlink(&target, &configured_alias).unwrap();
        symlink(&target, &harness_alias).unwrap();

        let scope = EffectiveAccessScope::resolve(
            &restricted(&[&configured_alias]),
            TrustedAccessInputs {
                harness_roots: vec![harness_alias],
                invocation_directory: None,
            },
        )
        .unwrap();
        assert_eq!(scope.roots().len(), 1);
        assert_eq!(scope.roots()[0].sources().len(), 2);

        fs::remove_file(&configured_alias).unwrap();
        symlink(&other, &configured_alias).unwrap();
        let error = scope.validate_stable().unwrap_err().to_string();
        assert!(error.contains("changed target"), "{error}");
    }
}
