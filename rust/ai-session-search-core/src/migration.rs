//! Transactional migration of a live SQLite session index.
//!
//! The source is never modified or removed. A migration uses SQLite's online
//! backup API so committed WAL content is included, validates a same-filesystem
//! staging database, syncs it, and atomically publishes it only after every gate.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::durable_fs::{entry_exists, rename_noreplace, sync_parent};
use crate::indexer::{index_update_lock_path, open_index_update_lock};

#[derive(Debug, Clone)]
pub struct DatabaseMigrationOptions {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub receipt: PathBuf,
    pub pages_per_step: i32,
    pub pause_between_steps: Duration,
}

impl DatabaseMigrationOptions {
    pub fn new(source: PathBuf, destination: PathBuf, receipt: PathBuf) -> Self {
        Self {
            source,
            destination,
            receipt,
            pages_per_step: 256,
            pause_between_steps: Duration::from_millis(10),
        }
    }

    fn validate(&self) -> Result<()> {
        if !self.source.is_file() {
            bail!("source database does not exist: {}", self.source.display());
        }
        if entry_exists(&self.destination)? {
            bail!(
                "destination already exists; refusing to overwrite: {}",
                self.destination.display()
            );
        }
        if entry_exists(&self.receipt)? {
            bail!(
                "migration receipt already exists; refusing ambiguous cutover: {}",
                self.receipt.display()
            );
        }
        let prepared = staging_path(&self.receipt, "prepared");
        if entry_exists(&prepared)? {
            bail!(
                "prepared migration receipt requires recovery: {}",
                prepared.display()
            );
        }
        if self.pages_per_step <= 0 {
            bail!("pages_per_step must be greater than zero");
        }
        if normalized_absolute(&self.source)? == normalized_absolute(&self.destination)? {
            bail!("source and destination must be different paths");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseMigrationPhase {
    Prepared,
    Published,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseMigrationReceipt {
    pub phase: DatabaseMigrationPhase,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub snapshot_sha256: String,
    pub table_rows: BTreeMap<String, i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigImportReport {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub mapped: Vec<String>,
    pub ignored: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ConfigImport {
    pub config: Config,
    pub report: ConfigImportReport,
}

#[derive(Debug, Clone)]
pub struct ConfigPublishOptions {
    pub destination: PathBuf,
    pub replace_existing: bool,
    pub rollback_copy: Option<PathBuf>,
}

struct StagingFile {
    path: PathBuf,
    published: bool,
}

impl StagingFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            published: false,
        }
    }

    fn publish_new(mut self, destination: &Path) -> Result<()> {
        rename_noreplace(&self.path, destination).with_context(|| {
            format!(
                "failed to atomically claim new destination {} from {}",
                destination.display(),
                self.path.display()
            )
        })?;
        self.published = true;
        sync_parent(destination)?;
        Ok(())
    }

    fn publish_replace(mut self, destination: &Path) -> Result<()> {
        fs::rename(&self.path, destination).with_context(|| {
            format!(
                "failed to atomically publish {} as {}",
                self.path.display(),
                destination.display()
            )
        })?;
        self.published = true;
        sync_parent(destination)?;
        Ok(())
    }
}

impl Drop for StagingFile {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub fn migrate_database(options: &DatabaseMigrationOptions) -> Result<DatabaseMigrationReceipt> {
    options.validate()?;
    create_parent(&options.destination)?;
    create_parent(&options.receipt)?;

    let mut source_lock = open_index_update_lock(&index_update_lock_path(&options.source))?;
    let _source_guard = source_lock
        .try_write()
        .with_context(|| format!("source database is in use: {}", options.source.display()))?;
    let mut destination_lock =
        open_index_update_lock(&index_update_lock_path(&options.destination))?;
    let _destination_guard = destination_lock.try_write().with_context(|| {
        format!(
            "destination database is in use: {}",
            options.destination.display()
        )
    })?;

    let staging = StagingFile::new(staging_path(&options.destination, "database"));
    if entry_exists(&staging.path)? {
        bail!(
            "stale database staging file requires inspection: {}",
            staging.path.display()
        );
    }

    let source = Connection::open_with_flags(&options.source, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| {
            format!(
                "failed to open source database {}",
                options.source.display()
            )
        })?;
    let source_rows = core_table_rows(&source)?;
    {
        let mut destination = Connection::open(&staging.path).with_context(|| {
            format!(
                "failed to create migration staging database {}",
                staging.path.display()
            )
        })?;
        let backup = Backup::new(&source, &mut destination)?;
        backup.run_to_completion(options.pages_per_step, options.pause_between_steps, None)?;
    }

    let staged = Connection::open_with_flags(&staging.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    validate_integrity(&staged)?;
    let destination_rows = core_table_rows(&staged)?;
    if source_rows != destination_rows {
        bail!(
            "database row counts changed during migration: source={source_rows:?}, destination={destination_rows:?}"
        );
    }
    drop(staged);

    File::open(&staging.path)?.sync_all()?;
    let snapshot_sha256 = sha256_file(&staging.path)?;
    let prepared_receipt = DatabaseMigrationReceipt {
        phase: DatabaseMigrationPhase::Prepared,
        source: normalized_absolute(&options.source)?,
        destination: normalized_absolute(&options.destination)?,
        snapshot_sha256,
        table_rows: destination_rows,
    };

    let prepared_path = staging_path(&options.receipt, "prepared");
    if entry_exists(&prepared_path)? {
        bail!(
            "prepared migration receipt requires recovery: {}",
            prepared_path.display()
        );
    }
    write_receipt(&prepared_path, &prepared_receipt)?;
    sync_parent(&prepared_path)?;
    staging.publish_new(&options.destination)?;

    let receipt = DatabaseMigrationReceipt {
        phase: DatabaseMigrationPhase::Published,
        ..prepared_receipt
    };
    let receipt_staging = StagingFile::new(staging_path(&options.receipt, "receipt"));
    write_receipt(&receipt_staging.path, &receipt)?;
    receipt_staging.publish_new(&options.receipt)?;
    fs::remove_file(&prepared_path)?;
    sync_parent(&prepared_path)?;
    Ok(receipt)
}

pub fn verify_migration(receipt: &DatabaseMigrationReceipt) -> Result<()> {
    if receipt.phase != DatabaseMigrationPhase::Published {
        bail!("migration is prepared but not published");
    }
    verify_migration_snapshot(receipt, &receipt.destination)
}

/// Resume or finalize a database migration interrupted after its prepared receipt was synced.
///
/// This operation is idempotent once the published receipt exists. It never discards ambiguous
/// evidence: the prepared receipt, staging database, destination, and any staged final receipt
/// must agree before a state transition is performed.
pub fn recover_database_migration(receipt_path: &Path) -> Result<DatabaseMigrationReceipt> {
    let prepared_path = staging_path(receipt_path, "prepared");
    if entry_exists(receipt_path)? {
        let receipt = load_receipt(receipt_path)?;
        let mut source_lock = open_index_update_lock(&index_update_lock_path(&receipt.source))?;
        let _source_guard = source_lock
            .try_write()
            .with_context(|| format!("source database is in use: {}", receipt.source.display()))?;
        let mut destination_lock =
            open_index_update_lock(&index_update_lock_path(&receipt.destination))?;
        let _destination_guard = destination_lock.try_write().with_context(|| {
            format!(
                "destination database is in use: {}",
                receipt.destination.display()
            )
        })?;
        verify_migration(&receipt)?;
        if entry_exists(&prepared_path)? {
            let prepared = load_receipt(&prepared_path)?;
            ensure_prepared_matches_published(&prepared, &receipt)?;
            fs::remove_file(&prepared_path)?;
            sync_parent(&prepared_path)?;
        }
        return Ok(receipt);
    }

    if !entry_exists(&prepared_path)? {
        bail!(
            "no published or prepared migration receipt exists for recovery: {}",
            receipt_path.display()
        );
    }
    let prepared = load_receipt(&prepared_path)?;
    if prepared.phase != DatabaseMigrationPhase::Prepared {
        bail!("prepared migration evidence has an invalid phase");
    }

    let mut source_lock = open_index_update_lock(&index_update_lock_path(&prepared.source))?;
    let _source_guard = source_lock
        .try_write()
        .with_context(|| format!("source database is in use: {}", prepared.source.display()))?;
    let mut destination_lock =
        open_index_update_lock(&index_update_lock_path(&prepared.destination))?;
    let _destination_guard = destination_lock.try_write().with_context(|| {
        format!(
            "destination database is in use: {}",
            prepared.destination.display()
        )
    })?;

    let published = DatabaseMigrationReceipt {
        phase: DatabaseMigrationPhase::Published,
        source: prepared.source.clone(),
        destination: prepared.destination.clone(),
        snapshot_sha256: prepared.snapshot_sha256.clone(),
        table_rows: prepared.table_rows.clone(),
    };
    if entry_exists(&published.destination)? {
        let database_staging = staging_path(&published.destination, "database");
        if entry_exists(&database_staging)? {
            bail!(
                "both migration destination and staging database exist; refusing ambiguous recovery: {}, {}",
                published.destination.display(),
                database_staging.display()
            );
        }
        verify_migration_snapshot(&published, &published.destination)?;
    } else {
        let database_staging = staging_path(&published.destination, "database");
        if !entry_exists(&database_staging)? {
            bail!(
                "prepared migration has neither a destination nor staging database: {}",
                database_staging.display()
            );
        }
        verify_migration_snapshot(&published, &database_staging)?;
        StagingFile::new(database_staging).publish_new(&published.destination)?;
        verify_migration_snapshot(&published, &published.destination)?;
    }

    let receipt_staging_path = staging_path(receipt_path, "receipt");
    if entry_exists(&receipt_staging_path)? {
        let staged_receipt = load_receipt(&receipt_staging_path)?;
        if staged_receipt != published {
            bail!(
                "staged final receipt conflicts with prepared migration evidence: {}",
                receipt_staging_path.display()
            );
        }
    } else {
        write_receipt(&receipt_staging_path, &published)?;
    }
    StagingFile::new(receipt_staging_path).publish_new(receipt_path)?;
    fs::remove_file(&prepared_path)?;
    sync_parent(&prepared_path)?;
    Ok(published)
}

fn ensure_prepared_matches_published(
    prepared: &DatabaseMigrationReceipt,
    published: &DatabaseMigrationReceipt,
) -> Result<()> {
    let expected = DatabaseMigrationReceipt {
        phase: DatabaseMigrationPhase::Published,
        source: prepared.source.clone(),
        destination: prepared.destination.clone(),
        snapshot_sha256: prepared.snapshot_sha256.clone(),
        table_rows: prepared.table_rows.clone(),
    };
    if prepared.phase != DatabaseMigrationPhase::Prepared || expected != *published {
        bail!("prepared migration evidence conflicts with published receipt");
    }
    Ok(())
}

fn verify_migration_snapshot(receipt: &DatabaseMigrationReceipt, snapshot: &Path) -> Result<()> {
    if sha256_file(snapshot)? != receipt.snapshot_sha256 {
        bail!("destination database changed since migration");
    }
    let source = Connection::open_with_flags(&receipt.source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    validate_integrity(&source)?;
    if core_table_rows(&source)? != receipt.table_rows {
        bail!("source database changed since migration");
    }
    let destination = Connection::open_with_flags(snapshot, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    validate_integrity(&destination)?;
    if core_table_rows(&destination)? != receipt.table_rows {
        bail!("destination row counts no longer match migration receipt");
    }
    Ok(())
}

pub fn load_receipt(path: &Path) -> Result<DatabaseMigrationReceipt> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read migration receipt {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse migration receipt {}", path.display()))
}

pub fn import_legacy_config(
    source: &Path,
    destination: PathBuf,
    database_path: PathBuf,
    cache_dir: PathBuf,
) -> Result<ConfigImport> {
    let raw = fs::read_to_string(source)
        .with_context(|| format!("failed to read legacy config {}", source.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse legacy config {}", source.display()))?;
    let object = value
        .as_object()
        .context("legacy config root must be a JSON object")?;
    let mut config = Config::default();
    config.index.db_path = Some(database_path.to_string_lossy().into_owned());
    config.index.cache_dir = Some(cache_dir.to_string_lossy().into_owned());
    let mut report = ConfigImportReport {
        source: normalized_absolute(source)?,
        destination: normalized_absolute(&destination)?,
        mapped: vec!["index.db_path".to_string(), "index.cache_dir".to_string()],
        ignored: Vec::new(),
        warnings: Vec::new(),
    };

    if let Some(claude_dir) = object.get("claude_dir") {
        if let Some(path) = claude_dir.as_str().filter(|path| !path.trim().is_empty()) {
            config.providers.claude.paths = vec![Path::new(path)
                .join("projects")
                .to_string_lossy()
                .into_owned()];
            report
                .mapped
                .push("claude_dir -> providers.claude.paths".to_string());
        } else {
            report
                .warnings
                .push("claude_dir was not a non-empty string".to_string());
        }
    }

    if let Some(source_dirs) = object.get("source_dirs") {
        if let Some(source_dirs) = source_dirs.as_object() {
            for (provider, value) in source_dirs {
                let Some(paths) = json_paths(value) else {
                    report.warnings.push(format!(
                        "source_dirs.{provider} must be a string or string array"
                    ));
                    continue;
                };
                if set_provider_paths(&mut config, provider, paths) {
                    report.mapped.push(format!(
                        "source_dirs.{provider} -> providers.{}.paths",
                        canonical_provider_name(provider)
                    ));
                } else {
                    report.ignored.push(format!("source_dirs.{provider}"));
                }
            }
        } else {
            report
                .warnings
                .push("source_dirs was not a JSON object".to_string());
        }
    }

    for key in object.keys() {
        if !matches!(key.as_str(), "claude_dir" | "source_dirs") {
            report.ignored.push(key.clone());
        }
    }
    report.mapped.sort();
    report.mapped.dedup();
    report.ignored.sort();
    report.ignored.dedup();
    Ok(ConfigImport { config, report })
}

pub fn publish_imported_config(
    import: &ConfigImport,
    options: &ConfigPublishOptions,
) -> Result<()> {
    if options.destination != import.report.destination {
        let absolute = normalized_absolute(&options.destination)?;
        if absolute != import.report.destination {
            bail!("publish destination differs from reviewed import destination");
        }
    }
    create_parent(&options.destination)?;
    let replacing = entry_exists(&options.destination)?;
    if replacing {
        if fs::symlink_metadata(&options.destination)?
            .file_type()
            .is_symlink()
        {
            bail!(
                "refusing to replace symbolic-link config destination: {}",
                options.destination.display()
            );
        }
        if !options.replace_existing {
            bail!(
                "config destination already exists; use explicit replacement with a rollback copy: {}",
                options.destination.display()
            );
        }
        let rollback = options
            .rollback_copy
            .as_deref()
            .context("replacement requires rollback_copy")?;
        if entry_exists(rollback)? {
            bail!("rollback config already exists: {}", rollback.display());
        }
        create_parent(rollback)?;
        fs::copy(&options.destination, rollback).with_context(|| {
            format!(
                "failed to preserve config {} as {}",
                options.destination.display(),
                rollback.display()
            )
        })?;
        File::open(rollback)?.sync_all()?;
        sync_parent(rollback)?;
    } else if options.rollback_copy.is_some() {
        bail!("rollback_copy is only valid when replacing an existing config");
    }

    let staging = StagingFile::new(staging_path(&options.destination, "config"));
    if entry_exists(&staging.path)? {
        bail!(
            "stale config staging file requires inspection: {}",
            staging.path.display()
        );
    }
    let toml = toml::to_string_pretty(&import.config)?;
    let _: Config = toml::from_str(&toml).context("generated config failed round-trip parsing")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging.path)?;
    file.write_all(toml.as_bytes())?;
    file.sync_all()?;
    drop(file);
    if replacing {
        staging.publish_replace(&options.destination)
    } else {
        staging.publish_new(&options.destination)
    }
}

fn json_paths(value: &serde_json::Value) -> Option<Vec<String>> {
    match value {
        serde_json::Value::String(path) if !path.trim().is_empty() => Some(vec![path.clone()]),
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| value.as_str().map(str::to_string))
            .collect(),
        _ => None,
    }
}

fn canonical_provider_name(name: &str) -> &str {
    match name {
        "aistudio" | "ai_studio" | "ai-studio" => "ai-studio",
        "gemini" | "gemini_cli" | "gemini-cli" => "gemini-cli",
        "claude_desktop" | "claude-desktop" => "claude-desktop",
        other => other,
    }
}

fn set_provider_paths(config: &mut Config, name: &str, paths: Vec<String>) -> bool {
    let provider = match canonical_provider_name(name) {
        "claude" => &mut config.providers.claude,
        "claude-desktop" => &mut config.providers.claude_desktop,
        "codex" => &mut config.providers.codex,
        "cursor" => &mut config.providers.cursor,
        "antigravity" => &mut config.providers.antigravity,
        "pi" => &mut config.providers.pi,
        "ai-studio" => &mut config.providers.aistudio,
        "gemini-cli" => &mut config.providers.gemini_cli,
        _ => return false,
    };
    provider.paths = paths;
    true
}

fn create_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    Ok(())
}

fn normalized_absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn staging_path(path: &Path, kind: &str) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("migration"))
        .to_os_string();
    name.push(format!(".{kind}.staging"));
    path.with_file_name(name)
}

fn validate_integrity(connection: &Connection) -> Result<()> {
    let result: String = connection.query_row("pragma integrity_check", [], |row| row.get(0))?;
    if result != "ok" {
        bail!("SQLite integrity_check failed: {result}");
    }
    Ok(())
}

fn core_table_rows(connection: &Connection) -> Result<BTreeMap<String, i64>> {
    let mut counts = BTreeMap::new();
    for table in ["sessions", "messages", "file_edits"] {
        let exists: bool = connection.query_row(
            "select exists(select 1 from sqlite_master where type = 'table' and name = ?1)",
            [table],
            |row| row.get(0),
        )?;
        if exists {
            let count =
                connection.query_row(&format!("select count(*) from {table}"), [], |row| {
                    row.get(0)
                })?;
            counts.insert(table.to_string(), count);
        }
    }
    Ok(counts)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)
        .with_context(|| format!("failed to open {} for checksum", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn write_receipt(path: &Path, receipt: &DatabaseMigrationReceipt) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, receipt)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(dir: &Path) -> DatabaseMigrationOptions {
        DatabaseMigrationOptions::new(
            dir.join("legacy/index.db"),
            dir.join("new/index.db"),
            dir.join("new/migration.json"),
        )
    }

    fn source_with_wal(path: &Path) -> Connection {
        create_parent(path).unwrap();
        let connection = Connection::open(path).unwrap();
        connection
            .pragma_update(None, "journal_mode", "wal")
            .unwrap();
        connection
            .execute_batch(
                "create table sessions(id text primary key);\n\
                 create table messages(id integer primary key, content text);\n\
                 create table file_edits(id integer primary key);\n\
                 insert into sessions values ('s1');\n\
                 insert into messages(content) values ('committed WAL content');",
            )
            .unwrap();
        connection
    }

    fn prepare_interrupted_migration(
        options: &DatabaseMigrationOptions,
        destination_published: bool,
    ) -> (DatabaseMigrationReceipt, PathBuf) {
        create_parent(&options.destination).unwrap();
        let snapshot = if destination_published {
            options.destination.clone()
        } else {
            staging_path(&options.destination, "database")
        };
        {
            let source =
                Connection::open_with_flags(&options.source, OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .unwrap();
            let mut destination = Connection::open(&snapshot).unwrap();
            Backup::new(&source, &mut destination)
                .unwrap()
                .run_to_completion(options.pages_per_step, options.pause_between_steps, None)
                .unwrap();
        }
        let prepared = DatabaseMigrationReceipt {
            phase: DatabaseMigrationPhase::Prepared,
            source: normalized_absolute(&options.source).unwrap(),
            destination: normalized_absolute(&options.destination).unwrap(),
            snapshot_sha256: sha256_file(&snapshot).unwrap(),
            table_rows: core_table_rows(&Connection::open(&snapshot).unwrap()).unwrap(),
        };
        let prepared_path = staging_path(&options.receipt, "prepared");
        create_parent(&prepared_path).unwrap();
        write_receipt(&prepared_path, &prepared).unwrap();
        (prepared, snapshot)
    }

    #[test]
    fn online_backup_includes_wal_and_publishes_verified_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let options = options(dir.path());
        let source = source_with_wal(&options.source);
        assert!(options.source.with_extension("db-wal").exists());

        let receipt = migrate_database(&options).unwrap();

        assert!(options.source.exists(), "source remains rollback-ready");
        assert_eq!(receipt.phase, DatabaseMigrationPhase::Published);
        assert!(!staging_path(&options.receipt, "prepared").exists());
        assert_eq!(receipt.table_rows["sessions"], 1);
        assert_eq!(receipt.table_rows["messages"], 1);
        verify_migration(&receipt).unwrap();
        let migrated = Connection::open(&options.destination).unwrap();
        let content: String = migrated
            .query_row("select content from messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(content, "committed WAL content");
        drop(source);
    }

    #[test]
    fn existing_destination_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let options = options(dir.path());
        let _source = source_with_wal(&options.source);
        create_parent(&options.destination).unwrap();
        fs::write(&options.destination, b"keep me").unwrap();

        let error = migrate_database(&options).unwrap_err();

        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(fs::read(&options.destination).unwrap(), b"keep me");
        assert!(!options.receipt.exists());
    }

    #[test]
    fn destination_claim_race_never_replaces_the_winner() {
        let dir = tempfile::tempdir().unwrap();
        let staging_path = dir.path().join("index.db.database.staging");
        let destination = dir.path().join("index.db");
        fs::write(&staging_path, b"staged database").unwrap();
        let staging = StagingFile::new(staging_path.clone());

        fs::write(&destination, b"concurrent winner").unwrap();
        let error = staging.publish_new(&destination).unwrap_err();

        assert!(error.to_string().contains("atomically claim"));
        assert_eq!(fs::read(destination).unwrap(), b"concurrent winner");
        assert!(!staging_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn broken_destination_symlink_is_an_existing_entry() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let options = options(dir.path());
        let _source = source_with_wal(&options.source);
        create_parent(&options.destination).unwrap();
        symlink(dir.path().join("missing.db"), &options.destination).unwrap();

        let error = migrate_database(&options).unwrap_err();

        assert!(error.to_string().contains("refusing to overwrite"));
        assert!(options
            .destination
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!options.receipt.exists());
    }

    #[test]
    fn corrupt_source_never_publishes_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        let options = options(dir.path());
        create_parent(&options.source).unwrap();
        fs::write(&options.source, b"not sqlite").unwrap();

        assert!(migrate_database(&options).is_err());
        assert!(!options.destination.exists());
        assert!(!options.receipt.exists());
        assert!(!staging_path(&options.destination, "database").exists());
    }

    #[test]
    fn held_source_lock_fails_without_waiting_or_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let options = options(dir.path());
        let _source = source_with_wal(&options.source);
        let mut lock = open_index_update_lock(&index_update_lock_path(&options.source)).unwrap();
        let _guard = lock.try_write().unwrap();

        let error = migrate_database(&options).unwrap_err();

        assert!(error.to_string().contains("source database is in use"));
        assert!(!options.destination.exists());
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_link_lock_fails_without_publishing_or_touching_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let options = options(dir.path());
        let _source = source_with_wal(&options.source);
        let target = dir.path().join("unrelated-lock-target");
        fs::write(&target, b"preserve me").unwrap();
        symlink(&target, index_update_lock_path(&options.source)).unwrap();

        let error = migrate_database(&options).unwrap_err();

        assert!(error.to_string().contains("is not a regular file"));
        assert_eq!(fs::read(target).unwrap(), b"preserve me");
        assert!(!options.destination.exists());
        assert!(!options.receipt.exists());
    }

    #[test]
    fn prepared_receipt_blocks_ambiguous_retry() {
        let dir = tempfile::tempdir().unwrap();
        let options = options(dir.path());
        let _source = source_with_wal(&options.source);
        let prepared = staging_path(&options.receipt, "prepared");
        create_parent(&prepared).unwrap();
        fs::write(&prepared, b"recovery evidence").unwrap();

        let error = migrate_database(&options).unwrap_err();

        assert!(error.to_string().contains("requires recovery"));
        assert_eq!(fs::read(prepared).unwrap(), b"recovery evidence");
        assert!(!options.destination.exists());
    }

    #[test]
    fn recovery_resumes_verified_staging_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let options = options(dir.path());
        let _source = source_with_wal(&options.source);
        let (_prepared, staging) = prepare_interrupted_migration(&options, false);
        let prepared_path = staging_path(&options.receipt, "prepared");

        let recovered = recover_database_migration(&options.receipt).unwrap();

        assert_eq!(recovered.phase, DatabaseMigrationPhase::Published);
        assert!(options.destination.exists());
        assert!(options.receipt.exists());
        assert!(!staging.exists());
        assert!(!prepared_path.exists());

        let mut destination_lock =
            open_index_update_lock(&index_update_lock_path(&options.destination)).unwrap();
        let destination_guard = destination_lock.try_write().unwrap();
        let error = recover_database_migration(&options.receipt).unwrap_err();
        assert!(error.to_string().contains("destination database is in use"));
        drop(destination_guard);
        assert_eq!(
            recover_database_migration(&options.receipt).unwrap(),
            recovered
        );
    }

    #[test]
    fn recovery_finalizes_published_destination_and_rejects_conflicting_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let options = options(dir.path());
        let _source = source_with_wal(&options.source);
        let (prepared, _snapshot) = prepare_interrupted_migration(&options, true);
        let prepared_path = staging_path(&options.receipt, "prepared");

        let recovered = recover_database_migration(&options.receipt).unwrap();
        verify_migration(&recovered).unwrap();

        fs::remove_file(&options.receipt).unwrap();
        let mut conflicting = prepared;
        conflicting.snapshot_sha256 = "conflict".to_string();
        write_receipt(&prepared_path, &conflicting).unwrap();
        let error = recover_database_migration(&options.receipt).unwrap_err();
        assert!(error
            .to_string()
            .contains("destination database changed since migration"));
        assert!(prepared_path.exists(), "conflicting evidence is preserved");
        assert!(
            options.destination.exists(),
            "published database is preserved"
        );
    }

    #[test]
    fn recovery_preserves_corrupt_and_ambiguous_database_evidence() {
        let corrupt_dir = tempfile::tempdir().unwrap();
        let corrupt_options = options(corrupt_dir.path());
        let _corrupt_source = source_with_wal(&corrupt_options.source);
        let (_prepared, corrupt_staging) = prepare_interrupted_migration(&corrupt_options, false);
        fs::write(&corrupt_staging, b"corrupt staged snapshot").unwrap();

        let error = recover_database_migration(&corrupt_options.receipt).unwrap_err();
        assert!(error.to_string().contains("changed since migration"));
        assert!(corrupt_staging.exists());
        assert!(staging_path(&corrupt_options.receipt, "prepared").exists());

        let ambiguous_dir = tempfile::tempdir().unwrap();
        let ambiguous_options = options(ambiguous_dir.path());
        let _ambiguous_source = source_with_wal(&ambiguous_options.source);
        let (_prepared, ambiguous_staging) =
            prepare_interrupted_migration(&ambiguous_options, false);
        fs::copy(&ambiguous_staging, &ambiguous_options.destination).unwrap();

        let error = recover_database_migration(&ambiguous_options.receipt).unwrap_err();
        assert!(error.to_string().contains("both migration destination"));
        assert!(ambiguous_staging.exists());
        assert!(ambiguous_options.destination.exists());
        assert!(staging_path(&ambiguous_options.receipt, "prepared").exists());
    }

    #[test]
    fn recovery_preserves_conflicting_staged_final_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let options = options(dir.path());
        let _source = source_with_wal(&options.source);
        let (prepared, _snapshot) = prepare_interrupted_migration(&options, true);
        let mut conflicting = DatabaseMigrationReceipt {
            phase: DatabaseMigrationPhase::Published,
            ..prepared
        };
        conflicting.table_rows.insert("sessions".to_string(), 99);
        let receipt_staging = staging_path(&options.receipt, "receipt");
        write_receipt(&receipt_staging, &conflicting).unwrap();

        let error = recover_database_migration(&options.receipt).unwrap_err();

        assert!(error.to_string().contains("staged final receipt conflicts"));
        assert!(receipt_staging.exists());
        assert!(staging_path(&options.receipt, "prepared").exists());
        assert!(options.destination.exists());
        assert!(!options.receipt.exists());
    }

    #[test]
    fn legacy_config_import_maps_providers_and_reports_unsupported_fields() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("config.json");
        let destination = dir.path().join("new/config.toml");
        fs::write(
            &source,
            r#"{
                "claude_dir": "~/custom-claude",
                "source_dirs": {
                    "aistudio": ["~/studio-a", "~/studio-b"],
                    "gemini_cli": "~/gemini/tmp",
                    "future_provider": "/future"
                },
                "org_dir": "~/organized",
                "defaults": {"format": "json"}
            }"#,
        )
        .unwrap();

        let import = import_legacy_config(
            &source,
            destination,
            dir.path().join("new/index.db"),
            dir.path().join("new/cache"),
        )
        .unwrap();

        assert_eq!(
            import.config.providers.claude.paths,
            vec!["~/custom-claude/projects"]
        );
        assert_eq!(
            import.config.providers.aistudio.paths,
            vec!["~/studio-a", "~/studio-b"]
        );
        assert_eq!(
            import.config.providers.gemini_cli.paths,
            vec!["~/gemini/tmp"]
        );
        assert!(import
            .report
            .ignored
            .contains(&"source_dirs.future_provider".to_string()));
        assert!(import.report.ignored.contains(&"org_dir".to_string()));
        assert!(import.report.ignored.contains(&"defaults".to_string()));
    }

    #[test]
    fn config_publish_is_atomic_and_replacement_requires_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("legacy.json");
        let destination = dir.path().join("new/config.toml");
        fs::write(&source, "{}").unwrap();
        let import = import_legacy_config(
            &source,
            destination.clone(),
            dir.path().join("new/index.db"),
            dir.path().join("new/cache"),
        )
        .unwrap();
        let initial = ConfigPublishOptions {
            destination: destination.clone(),
            replace_existing: false,
            rollback_copy: None,
        };
        publish_imported_config(&import, &initial).unwrap();
        let original = fs::read(&destination).unwrap();

        let error = publish_imported_config(&import, &initial).unwrap_err();
        assert!(error.to_string().contains("already exists"));
        let rollback = dir.path().join("rollback/config.toml");
        publish_imported_config(
            &import,
            &ConfigPublishOptions {
                destination: destination.clone(),
                replace_existing: true,
                rollback_copy: Some(rollback.clone()),
            },
        )
        .unwrap();

        assert_eq!(fs::read(rollback).unwrap(), original);
        let parsed: Config = toml::from_str(&fs::read_to_string(destination).unwrap()).unwrap();
        assert_eq!(parsed.db_path(), dir.path().join("new/index.db"));
    }

    #[cfg(unix)]
    #[test]
    fn config_publish_never_replaces_a_symbolic_link() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("legacy.json");
        let destination = dir.path().join("new/config.toml");
        let rollback = dir.path().join("rollback/config.toml");
        fs::write(&source, "{}").unwrap();
        let import = import_legacy_config(
            &source,
            destination.clone(),
            dir.path().join("new/index.db"),
            dir.path().join("new/cache"),
        )
        .unwrap();
        create_parent(&destination).unwrap();
        symlink(dir.path().join("missing.toml"), &destination).unwrap();

        let error = publish_imported_config(
            &import,
            &ConfigPublishOptions {
                destination: destination.clone(),
                replace_existing: true,
                rollback_copy: Some(rollback.clone()),
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("symbolic-link"));
        assert!(destination
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(!rollback.exists());
    }
}
