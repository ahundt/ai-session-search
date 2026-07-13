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
use fd_lock::RwLock;
use rusqlite::backup::Backup;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::indexer::index_update_lock_path;

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
        if self.destination.exists() {
            bail!(
                "destination already exists; refusing to overwrite: {}",
                self.destination.display()
            );
        }
        if self.receipt.exists() {
            bail!(
                "migration receipt already exists; refusing ambiguous cutover: {}",
                self.receipt.display()
            );
        }
        let prepared = staging_path(&self.receipt, "prepared");
        if prepared.exists() {
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

    fn publish(mut self, destination: &Path) -> Result<()> {
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

    let mut source_lock = open_lock(&index_update_lock_path(&options.source))?;
    let _source_guard = source_lock
        .try_write()
        .with_context(|| format!("source database is in use: {}", options.source.display()))?;
    let mut destination_lock = open_lock(&index_update_lock_path(&options.destination))?;
    let _destination_guard = destination_lock.try_write().with_context(|| {
        format!(
            "destination database is in use: {}",
            options.destination.display()
        )
    })?;

    let staging = StagingFile::new(staging_path(&options.destination, "database"));
    if staging.path.exists() {
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
    if prepared_path.exists() {
        bail!(
            "prepared migration receipt requires recovery: {}",
            prepared_path.display()
        );
    }
    write_receipt(&prepared_path, &prepared_receipt)?;
    sync_parent(&prepared_path)?;
    staging.publish(&options.destination)?;

    let receipt = DatabaseMigrationReceipt {
        phase: DatabaseMigrationPhase::Published,
        ..prepared_receipt
    };
    let receipt_staging = StagingFile::new(staging_path(&options.receipt, "receipt"));
    write_receipt(&receipt_staging.path, &receipt)?;
    receipt_staging.publish(&options.receipt)?;
    fs::remove_file(&prepared_path)?;
    sync_parent(&prepared_path)?;
    Ok(receipt)
}

pub fn verify_migration(receipt: &DatabaseMigrationReceipt) -> Result<()> {
    if receipt.phase != DatabaseMigrationPhase::Published {
        bail!("migration is prepared but not published");
    }
    if sha256_file(&receipt.destination)? != receipt.snapshot_sha256 {
        bail!("destination database changed since migration");
    }
    let source = Connection::open_with_flags(&receipt.source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    validate_integrity(&source)?;
    if core_table_rows(&source)? != receipt.table_rows {
        bail!("source database changed since migration");
    }
    let destination =
        Connection::open_with_flags(&receipt.destination, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
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

fn open_lock(path: &Path) -> Result<RwLock<File>> {
    create_parent(path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("failed to open migration lock {}", path.display()))?;
    Ok(RwLock::new(file))
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

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<()> {
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
        let mut lock = open_lock(&index_update_lock_path(&options.source)).unwrap();
        let _guard = lock.try_write().unwrap();

        let error = migrate_database(&options).unwrap_err();

        assert!(error.to_string().contains("source database is in use"));
        assert!(!options.destination.exists());
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
}
