// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;
use rusqlite::Connection;

/// Released message word-index definition. Keep this byte-stable until an explicit offline schema
/// transition installs the combined word/trigram trigger bundle.
pub(crate) const MESSAGE_WORD_INDEX_SQL: &str = "create virtual table if not exists messages_fts
         using fts5(content, content='messages', content_rowid='id');
     create trigger if not exists messages_ai after insert on messages begin
         insert into messages_fts(rowid, content) values (new.id, new.content);
     end;
     create trigger if not exists messages_ad after delete on messages begin
         insert into messages_fts(messages_fts, rowid, content)
         values ('delete', old.id, old.content);
     end;
     create trigger if not exists messages_au after update on messages begin
         insert into messages_fts(messages_fts, rowid, content)
         values ('delete', old.id, old.content);
         insert into messages_fts(rowid, content) values (new.id, new.content);
     end;";

/// Install only the currently released word index. This function deliberately does not infer,
/// migrate, or repair any future trigram schema during an ordinary database open.
pub(crate) fn install_released_message_word_index(conn: &Connection) -> Result<()> {
    conn.execute_batch(MESSAGE_WORD_INDEX_SQL)?;
    Ok(())
}

/// Install and incrementally maintain the schema-v4 word and trigram indexes.
///
/// For one inserted/deleted/changed message of `J` Unicode scalar values, FTS tokenization and
/// postings maintenance are O(J) work with implementation-owned bounded streaming state; the index
/// stores O(T) postings for the corpus's emitted trigrams (`T <= O(total content scalars)`, before
/// compression). This is synchronous writer latency—triggers complete before the row mutation
/// commits—so query acceleration does not trade correctness for asynchronous index lag. Reusing
/// this existing index for another search field adds no new asymptotic storage or write cost.
pub(crate) fn install_target_message_search_indexes(conn: &Connection) -> Result<()> {
    install_released_message_word_index(conn)?;
    conn.execute_batch(
        // Idempotently clear any partial/hybrid remnant before (re)creating the trigram objects.
        // A `create virtual table` without `if not exists` fails with "table already exists" on the
        // real hybrid DB, where `messages_trigram_terms` (an fts5vocab shadow) can survive after
        // `messages_trigram` itself was dropped. Dropping all three first makes this a clean rebuild.
        // Safe for the fresh-install caller too, where none of them exist yet.
        "drop table if exists messages_trigram_terms;
         drop table if exists messages_trigram_vocab;
         drop table if exists messages_trigram;
         create virtual table messages_trigram using fts5(
             content,
             content='messages',
             content_rowid='id',
             tokenize='trigram',
             detail=none,
             columnsize=0
         );
         create virtual table messages_trigram_vocab
             using fts5vocab(messages_trigram, instance);
         create virtual table messages_trigram_terms
             using fts5vocab(messages_trigram, row);
         drop trigger if exists messages_ai;
         drop trigger if exists messages_ad;
         drop trigger if exists messages_au;
         create trigger messages_ai after insert on messages begin
             insert into messages_fts(rowid, content) values (new.id, new.content);
             insert into messages_trigram(rowid, content) values (new.id, new.content);
         end;
         create trigger messages_ad after delete on messages begin
             insert into messages_fts(messages_fts, rowid, content)
                 values ('delete', old.id, old.content);
             insert into messages_trigram(messages_trigram, rowid, content)
                 values ('delete', old.id, old.content);
         end;
         create trigger messages_au after update of content on messages begin
             insert into messages_fts(messages_fts, rowid, content)
                 values ('delete', old.id, old.content);
             insert into messages_trigram(messages_trigram, rowid, content)
                 values ('delete', old.id, old.content);
             insert into messages_fts(rowid, content) values (new.id, new.content);
             insert into messages_trigram(rowid, content) values (new.id, new.content);
         end;",
    )?;
    Ok(())
}

pub(crate) fn migrate_message_search_schema_offline(
    conn: &Connection,
    target_version: i64,
) -> Result<()> {
    let locking_mode: String =
        conn.query_row("pragma locking_mode=exclusive", [], |row| row.get(0))?;
    anyhow::ensure!(
        locking_mode.eq_ignore_ascii_case("exclusive"),
        "SQLite refused exclusive locking mode: {locking_mode}"
    );
    let (busy, log_frames, checkpointed_frames): (i64, i64, i64) =
        conn.query_row("pragma wal_checkpoint(truncate)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    anyhow::ensure!(
        busy == 0 && log_frames == checkpointed_frames,
        "offline migration checkpoint incomplete: busy={busy}, log={log_frames}, checkpointed={checkpointed_frames}"
    );
    let journal_mode: String =
        conn.query_row("pragma journal_mode=delete", [], |row| row.get(0))?;
    anyhow::ensure!(
        journal_mode.eq_ignore_ascii_case("delete"),
        "SQLite refused rollback-journal mode: {journal_mode}"
    );

    let migration = (|| -> Result<()> {
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Exclusive)?;
        install_target_message_search_indexes(&tx)?;
        tx.execute_batch(
            "insert into messages_fts(messages_fts) values ('rebuild');
             insert into messages_trigram(messages_trigram) values ('rebuild');
             insert into messages_fts(messages_fts, rank) values ('integrity-check', 1);
             insert into messages_trigram(messages_trigram, rank) values ('integrity-check', 1);
             drop table if exists trigram_postings;
             drop table if exists trigram_meta;",
        )?;
        let quick_check: String = tx.query_row("pragma quick_check", [], |row| row.get(0))?;
        anyhow::ensure!(
            quick_check == "ok",
            "SQLite quick_check failed: {quick_check}"
        );
        tx.pragma_update(None, "user_version", target_version)?;
        tx.commit()?;
        Ok(())
    })();

    let restore_wal = (|| -> Result<()> {
        // SQLite retains locks acquired in EXCLUSIVE mode until locking_mode is changed to
        // NORMAL *and the database is accessed again*. Change the connection policy while it is
        // still in rollback-journal mode; the following journal transition is that access. Doing
        // this after entering WAL can be a no-op for a connection that first entered WAL while
        // exclusive, leaving every subsequent reader blocked until this handle is dropped.
        let locking_mode: String =
            conn.query_row("pragma locking_mode=normal", [], |row| row.get(0))?;
        anyhow::ensure!(
            locking_mode.eq_ignore_ascii_case("normal"),
            "SQLite did not restore normal locking mode: {locking_mode}"
        );
        let journal_mode: String =
            conn.query_row("pragma journal_mode=wal", [], |row| row.get(0))?;
        anyhow::ensure!(
            journal_mode.eq_ignore_ascii_case("wal"),
            "SQLite did not restore WAL mode: {journal_mode}"
        );
        // Force one ordinary database read after NORMAL so the release is observable before this
        // function returns, as required by SQLite's locking_mode contract.
        let _: i64 = conn.query_row("pragma schema_version", [], |row| row.get(0))?;
        Ok(())
    })();
    match (migration, restore_wal) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(migration), Ok(())) => Err(migration),
        (Ok(()), Err(restore)) => Err(restore),
        (Err(migration), Err(restore)) => Err(migration.context(format!(
            "migration also failed to restore WAL mode: {restore:#}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::functions::FunctionFlags;
    use rusqlite::{params, StatementStatus};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn content_fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "create table messages (
                 id integer primary key,
                 session_id text not null,
                 seq integer not null,
                 content text not null,
                 metadata text
             );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn released_word_index_tracks_insert_content_update_and_delete() {
        let conn = content_fixture();
        install_released_message_word_index(&conn).unwrap();

        conn.execute(
            "insert into messages(id, session_id, seq, content) values (?1, ?2, ?3, ?4)",
            params![1, "session", 0, "alpha beta"],
        )
        .unwrap();
        let alpha: i64 = conn
            .query_row(
                "select count(*) from messages_fts where messages_fts match 'alpha'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(alpha, 1);

        conn.execute(
            "update messages set content = 'gamma delta' where id = 1",
            [],
        )
        .unwrap();
        let old_term: i64 = conn
            .query_row(
                "select count(*) from messages_fts where messages_fts match 'alpha'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let new_term: i64 = conn
            .query_row(
                "select count(*) from messages_fts where messages_fts match 'gamma'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((old_term, new_term), (0, 1));

        conn.execute("delete from messages where id = 1", [])
            .unwrap();
        let deleted: i64 = conn
            .query_row(
                "select count(*) from messages_fts where messages_fts match 'gamma'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    fn bundled_sqlite_supports_compact_external_content_trigram_postings() {
        let conn = content_fixture();
        conn.execute_batch(
            "create virtual table messages_trigram using fts5(
                 content,
                 content='messages',
                 content_rowid='id',
                 tokenize='trigram',
                 detail=none,
                 columnsize=0
             );
             create virtual table messages_trigram_vocab
                 using fts5vocab(messages_trigram, instance);
             insert into messages(id, session_id, seq, content) values
                 (1, 's', 0, 'abcdef'),
                 (2, 's', 1, 'abcxyz');
             insert into messages_trigram(messages_trigram) values ('rebuild');",
        )
        .unwrap();

        let postings: Vec<i64> = conn
            .prepare("select doc from messages_trigram_vocab where term = ?1 order by doc")
            .unwrap()
            .query_map(["abc"], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(postings, vec![1, 2]);

        let non_match: bool = conn
            .query_row(
                "select not exists(
                     select 1 from messages_trigram_vocab where term = ?1
                 )",
                ["zzz"],
                |row| row.get(0),
            )
            .unwrap();
        assert!(non_match);
    }

    #[test]
    fn target_trigger_bundle_is_atomic_and_ignores_metadata_only_updates() {
        let mut conn = content_fixture();
        install_target_message_search_indexes(&conn).unwrap();
        conn.execute(
            "insert into messages(id, session_id, seq, content, metadata)
             values (1, 's', 0, 'alpha bravo', 'before')",
            [],
        )
        .unwrap();

        let index_fingerprint = |conn: &Connection| -> String {
            conn.query_row(
                "select group_concat(fingerprint, '|') from (
                     select 'word:' || id || ':' || hex(block) as fingerprint
                       from messages_fts_data
                     union all
                     select 'trigram:' || id || ':' || hex(block)
                       from messages_trigram_data
                     order by fingerprint
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        let before_metadata = index_fingerprint(&conn);
        conn.execute("update messages set metadata = 'after' where id = 1", [])
            .unwrap();
        assert_eq!(index_fingerprint(&conn), before_metadata);

        conn.execute(
            "update messages set content = 'charlie delta' where id = 1",
            [],
        )
        .unwrap();
        let old_word: bool = conn
            .query_row(
                "select exists(select 1 from messages_fts where messages_fts match 'alpha')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let new_trigram: bool = conn
            .query_row(
                "select exists(select 1 from messages_trigram where messages_trigram match 'cha')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!old_word);
        assert!(new_trigram);

        let tx = conn.transaction().unwrap();
        tx.execute(
            "insert into messages(id, session_id, seq, content) values (2, 's', 1, 'rollback')",
            [],
        )
        .unwrap();
        tx.rollback().unwrap();
        let rolled_back: bool = conn
            .query_row(
                "select exists(select 1 from messages_trigram where messages_trigram match 'rol')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!rolled_back);

        conn.execute("delete from messages where id = 1", [])
            .unwrap();
        let deleted: bool = conn
            .query_row(
                "select exists(select 1 from messages_trigram where messages_trigram match 'cha')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!deleted);
    }

    #[test]
    fn wal_reader_keeps_snapshot_while_writer_commits_message_and_both_fts_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let writer = Connection::open(&path).unwrap();
        writer.pragma_update(None, "journal_mode", "wal").unwrap();
        writer
            .execute_batch(
                "create table messages (
                     id integer primary key,
                     session_id text not null,
                     seq integer not null,
                     content text not null,
                     metadata text
                 );",
            )
            .unwrap();
        install_target_message_search_indexes(&writer).unwrap();
        writer
            .execute(
                "insert into messages(id, session_id, seq, content)
                 values (1, 's', 0, 'initial snapshot')",
                [],
            )
            .unwrap();

        let reader = Connection::open(&path).unwrap();
        reader.execute_batch("begin").unwrap();
        assert_eq!(
            reader
                .query_row("select count(*) from messages", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );

        writer
            .execute(
                "insert into messages(id, session_id, seq, content)
                 values (2, 's', 1, 'concurrent trigram commit')",
                [],
            )
            .unwrap();
        assert_eq!(
            reader
                .query_row("select count(*) from messages", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1,
            "active WAL reader retains its original snapshot"
        );
        reader.execute_batch("rollback").unwrap();

        let visible: (bool, bool, i64) = reader
            .query_row(
                "select exists(select 1 from messages_fts where messages_fts match 'concurrent'),
                        exists(select 1 from messages_trigram where messages_trigram match 'tri'),
                        (select count(*) from messages)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(visible, (true, true, 2));
    }

    #[test]
    fn offline_fixture_migration_is_atomic_removes_custom_index_and_restores_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "journal_mode", "wal").unwrap();
        conn.execute_batch(
            "create table messages (
                 id integer primary key,
                 session_id text not null,
                 seq integer not null,
                 content text not null,
                 metadata text
             );
             create table trigram_postings(tg text primary key, ids blob not null, df integer);
             create table trigram_meta(key text primary key, value integer not null);",
        )
        .unwrap();
        install_released_message_word_index(&conn).unwrap();
        conn.execute(
            "insert into messages(id, session_id, seq, content) values (1, 's', 0, 'alpha bravo')",
            [],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();

        migrate_message_search_schema_offline(&conn, 4).unwrap();

        let state: (i64, String, bool, bool) = conn
            .query_row(
                "select (select user_version from pragma_user_version),
                        (select journal_mode from pragma_journal_mode),
                        exists(select 1 from sqlite_schema where name='messages_trigram'),
                        not exists(select 1 from sqlite_schema where name='trigram_postings')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(state, (4, "wal".into(), true, true));
        let indexed: bool = conn
            .query_row(
                "select exists(select 1 from messages_trigram where messages_trigram match 'alp')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(indexed);
    }

    #[test]
    fn offline_fixture_migration_refuses_an_active_reader_before_schema_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let writer = Connection::open(&path).unwrap();
        writer.pragma_update(None, "journal_mode", "wal").unwrap();
        writer
            .execute_batch(
                "create table messages (
                     id integer primary key,
                     session_id text not null,
                     seq integer not null,
                     content text not null,
                     metadata text
                 );
                 insert into messages(id, session_id, seq, content)
                 values (1, 's', 0, 'reader snapshot');",
            )
            .unwrap();
        install_released_message_word_index(&writer).unwrap();

        let reader = Connection::open(&path).unwrap();
        reader.execute_batch("begin").unwrap();
        reader
            .query_row("select content from messages where id = 1", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap();

        let error = migrate_message_search_schema_offline(&writer, 4)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("checkpoint incomplete")
                || error.contains("locked")
                || error.contains("busy"),
            "{error}"
        );
        let mutated: bool = writer
            .query_row(
                "select exists(select 1 from sqlite_schema where name='messages_trigram')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!mutated);
        reader.execute_batch("rollback").unwrap();
    }

    #[test]
    fn offline_migration_heals_partial_trigram_remnant_and_keeps_wal() {
        // A bogus/partial `messages_trigram_vocab` remnant (the shape an interrupted pre-v4 build
        // can leave behind) must NOT abort the migration. `install_target_message_search_indexes`
        // idempotently drops the stale trigram objects first, so the offline migration heals in
        // place: it rebuilds the FTS5 trigram index from the intact `messages` rows, stamps v4, and
        // restores WAL. (Genuine mid-migration failures still roll back — see the SQLITE_FULL test.)
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "journal_mode", "wal").unwrap();
        conn.execute_batch(
            "create table messages (
                 id integer primary key,
                 session_id text not null,
                 seq integer not null,
                 content text not null,
                 metadata text
             );
             create table messages_trigram_vocab(conflict integer);
             insert into messages(id, session_id, seq, content)
             values (1, 's', 0, 'heal partial schema needle');",
        )
        .unwrap();
        install_released_message_word_index(&conn).unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();

        migrate_message_search_schema_offline(&conn, 4).unwrap();

        let state: (i64, String, bool, bool, bool) = conn
            .query_row(
                "select (select user_version from pragma_user_version),
                        (select journal_mode from pragma_journal_mode),
                        exists(select 1 from sqlite_schema where name='messages_trigram'),
                        exists(select 1 from sqlite_schema
                                where type='table' and name='messages_trigram_vocab'),
                        exists(select 1 from sqlite_schema
                                where type='trigger' and name='messages_ai')",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(state, (4, "wal".into(), true, true, true));

        // The intact base row is preserved through the in-place heal.
        let messages: i64 = conn
            .query_row("select count(*) from messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(messages, 1);
    }

    #[test]
    fn offline_fixture_migration_rolls_back_on_sqlite_full_and_restores_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "journal_mode", "wal").unwrap();
        conn.execute_batch(
            "create table messages (
                 id integer primary key,
                 session_id text not null,
                 seq integer not null,
                 content text not null,
                 metadata text
             );
             with recursive n(value) as (
                 values(1)
                 union all
                 select value + 1 from n where value < 200
             )
             insert into messages(id, session_id, seq, content)
             select value, 's', value, hex(randomblob(2048)) from n;",
        )
        .unwrap();
        install_released_message_word_index(&conn).unwrap();
        conn.execute_batch("insert into messages_fts(messages_fts) values ('rebuild')")
            .unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();
        conn.execute_batch("pragma wal_checkpoint(truncate)")
            .unwrap();
        let page_count: i64 = conn
            .query_row("pragma page_count", [], |row| row.get(0))
            .unwrap();
        conn.pragma_update(None, "max_page_count", page_count)
            .unwrap();

        let error = migrate_message_search_schema_offline(&conn, 4)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("database or disk is full") || error.contains("SQLITE_FULL"),
            "{error}"
        );

        let state: (i64, String, bool, bool) = conn
            .query_row(
                "select (select user_version from pragma_user_version),
                        (select journal_mode from pragma_journal_mode),
                        exists(select 1 from sqlite_schema where name='messages_trigram'),
                        exists(select 1 from messages where id=200)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(state, (3, "wal".into(), false, true));
    }

    #[test]
    fn offline_migration_crash_child() {
        let Some(path) = std::env::var_os("AISE_OFFLINE_MIGRATION_CRASH_CHILD_DB") else {
            return;
        };
        let mut conn = Connection::open(path).unwrap();
        conn.pragma_update(None, "locking_mode", "exclusive")
            .unwrap();
        let checkpoint: (i64, i64, i64) = conn
            .query_row("pragma wal_checkpoint(truncate)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!(checkpoint.0, 0);
        conn.pragma_update(None, "journal_mode", "delete").unwrap();
        conn.pragma_update(None, "cache_size", 10).unwrap();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Exclusive)
            .unwrap();
        tx.execute_batch(
            "create table crash_partial(payload blob not null);
             with recursive n(value) as (
                 values(1)
                 union all
                 select value + 1 from n where value < 20000
             )
             insert into crash_partial(payload)
             select randomblob(1024) from n;
             pragma user_version=4;",
        )
        .unwrap();
        std::process::abort();
    }

    #[test]
    fn killed_offline_migration_recovers_old_schema_and_normalizes_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "journal_mode", "wal").unwrap();
            conn.execute_batch(
                "create table messages (
                     id integer primary key,
                     session_id text not null,
                     seq integer not null,
                     content text not null,
                     metadata text
                 );
                 insert into messages(id, session_id, seq, content)
                 values (1, 's', 0, 'committed before crash');",
            )
            .unwrap();
            install_released_message_word_index(&conn).unwrap();
            conn.pragma_update(None, "user_version", 3).unwrap();
        }

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fts::tests::offline_migration_crash_child",
                "--nocapture",
            ])
            .env("AISE_OFFLINE_MIGRATION_CRASH_CHILD_DB", &path)
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "crash child unexpectedly succeeded: {}",
            String::from_utf8_lossy(&output.stdout)
        );

        let journal_path = path.with_file_name("index.db-journal");
        assert!(
            std::fs::metadata(&journal_path).is_ok_and(|metadata| metadata.len() > 512),
            "killed migration did not leave a rollback journal at {}",
            journal_path.display()
        );

        let conn = Connection::open(&path).unwrap();
        let recovered: (i64, bool, String) = conn
            .query_row(
                "select (select user_version from pragma_user_version),
                        not exists(select 1 from sqlite_schema where name='crash_partial'),
                        (select content from messages where id=1)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(recovered, (3, true, "committed before crash".into()));
        let journal_mode: String = conn
            .query_row("pragma journal_mode=wal", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode, "wal");
        let quick_check: String = conn
            .query_row("pragma quick_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(quick_check, "ok");
    }

    #[test]
    #[ignore = "requires AISE_SQLITE_MIGRATION_BENCH_COPY pointing to a disposable copied database"]
    fn copied_current_corpus_migration_benchmark() {
        let path = std::env::var_os("AISE_SQLITE_MIGRATION_BENCH_COPY")
            .map(std::path::PathBuf::from)
            .expect("set AISE_SQLITE_MIGRATION_BENCH_COPY to a disposable copied database");
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        assert!(
            filename.contains("bench") && filename != "index.db",
            "refusing to mutate a path that is not clearly a benchmark copy: {}",
            path.display()
        );
        let before_bytes = std::fs::metadata(&path).unwrap().len();
        let started = std::time::Instant::now();
        let conn = Connection::open(&path).unwrap();

        migrate_message_search_schema_offline(&conn, 4).unwrap();

        let elapsed = started.elapsed();
        drop(conn);
        let after_bytes = std::fs::metadata(&path).unwrap().len();
        eprintln!(
            "migration_benchmark path={} before_bytes={} after_bytes={} elapsed_ms={}",
            path.display(),
            before_bytes,
            after_bytes,
            elapsed.as_millis()
        );
    }

    #[test]
    #[ignore = "requires a migrated AISE_SQLITE_MIGRATION_BENCH_COPY"]
    fn copied_current_corpus_fuzzy_query_benchmark() {
        let path = std::env::var_os("AISE_SQLITE_MIGRATION_BENCH_COPY")
            .map(std::path::PathBuf::from)
            .expect("set AISE_SQLITE_MIGRATION_BENCH_COPY to a migrated benchmark copy");
        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("bench") && name != "index.db"),
            "refusing a path that is not clearly a benchmark copy: {}",
            path.display()
        );
        let conn = Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_PRIVATE_CACHE
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        crate::sql_functions::register(&conn).unwrap();
        let version: i64 = conn
            .query_row("pragma user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 4, "benchmark copy must be migrated first");

        let sql = "with query_trigrams(term) as materialized (
                 select distinct value from json_each(?1)
             ),
             trigram_probe as materialized (
                 select v.doc as id, count(*) as shared_trigrams
                   from messages_trigram_vocab v
                   join query_trigrams q on q.term = v.term
                  group by v.doc
                  order by shared_trigrams desc, v.doc asc
                  limit ?2 + 1
             ),
             word_probe as materialized (
                 select rowid as id
                   from messages_fts
                  where messages_fts match ?6
                  order by bm25(messages_fts), rowid asc
                  limit ?2 + 1
             ),
             candidate_ids as materialized (
                 select id, max(shared_trigrams) as shared_trigrams
                   from (
                       select id, shared_trigrams
                         from (select id, shared_trigrams from trigram_probe limit ?2)
                       union all
                       select id, 0
                         from (select id from word_probe limit ?2)
                   )
                  group by id
             ),
             scored as materialized (
                 select m.id,
                        m.session_id,
                        m.seq,
                        fuzzy_score(?3, m.content) as score,
                        unicode_lower_contains(m.content, ?4) as exact_phrase,
                        m.content,
                        c.shared_trigrams
                   from candidate_ids c
                   join messages m on m.id = c.id
             )
             select id, session_id, seq, score, shared_trigrams,
                    content,
                    (exists(select 1 from trigram_probe limit 1 offset ?2)
                     or exists(select 1 from word_probe limit 1 offset ?2)) as saturated
               from scored
              where score is not null
              order by score desc, exact_phrase desc, session_id asc, seq asc
              limit ?5";
        struct FuzzyCase<'a> {
            query: &'a str,
            exhaustive_top_five: Option<[(&'a str, i64); 5]>,
            required_stems: &'a [&'a str],
            held_out: bool,
        }

        let cases = [
            FuzzyCase {
                query: "fable",
                exhaustive_top_five: Some([
                    ("claude-desktop:4b0688cc-34bf-4ac6-ad20-69f591eb8c08", 6),
                    ("claude-desktop:896be1f0-627f-4c1f-aa24-3647686e08c3", 0),
                    ("claude-desktop:896be1f0-627f-4c1f-aa24-3647686e08c3", 1),
                    ("claude-desktop:896be1f0-627f-4c1f-aa24-3647686e08c3", 49),
                    ("claude-desktop:a7be3fd5-61dd-453a-8823-6b9d372532f2", 6),
                ]),
                required_stems: &["fable"],
                held_out: false,
            },
            FuzzyCase {
                query: "magic values",
                exhaustive_top_five: Some([
                    ("claude:1c669182-8b13-487a-9c7d-e651edb56a2c", 49),
                    ("claude:34a5c52f-8d7f-4a7d-b140-6b21481e97fb", 824),
                    ("claude:34a5c52f-8d7f-4a7d-b140-6b21481e97fb", 829),
                    ("claude:34a5c52f-8d7f-4a7d-b140-6b21481e97fb", 830),
                    ("claude:34a5c52f-8d7f-4a7d-b140-6b21481e97fb", 942),
                ]),
                required_stems: &["magic", "values"],
                held_out: false,
            },
            FuzzyCase {
                query: "actully",
                exhaustive_top_five: Some([
                    ("claude:903dc049-c6a1-4e68-99eb-34aa121c5791", 606),
                    ("claude-desktop:5732def7-87e4-4382-954c-5ca9ca0a5536", 125),
                    ("claude:1746ada6-100e-4e3b-a697-644fb1b45fc8", 1345),
                    ("claude:1ed0414b-4278-4a6a-a891-98bdc72b6dd2", 16500),
                    ("claude:77cf49a5-19cb-4fa2-8d9c-1e59523d6693", 121),
                ]),
                required_stems: &["actully"],
                held_out: false,
            },
            FuzzyCase {
                query: "accelarate",
                exhaustive_top_five: None,
                required_stems: &["accelerat"],
                held_out: false,
            },
            FuzzyCase {
                query: "incrimental",
                exhaustive_top_five: None,
                required_stems: &["increment"],
                held_out: false,
            },
            FuzzyCase {
                query: "lifecylce",
                exhaustive_top_five: None,
                required_stems: &["lifecycle"],
                held_out: false,
            },
            FuzzyCase {
                query: "trigram accelration",
                exhaustive_top_five: None,
                required_stems: &["trigram", "accel"],
                held_out: false,
            },
            FuzzyCase {
                query: "timestamp normalisation",
                exhaustive_top_five: None,
                required_stems: &["timestamp", "normaliz"],
                held_out: false,
            },
            // Deterministically selected from real user wording before this benchmark was run.
            // These cases are not used to tune the candidate budget or SQL shape.
            FuzzyCase {
                query: "neccessary",
                exhaustive_top_five: None,
                required_stems: &["necess"],
                held_out: true,
            },
            FuzzyCase {
                query: "proabaly",
                exhaustive_top_five: None,
                required_stems: &["probab"],
                held_out: true,
            },
            FuzzyCase {
                query: "accellerate",
                exhaustive_top_five: None,
                required_stems: &["accelerat"],
                held_out: true,
            },
            FuzzyCase {
                query: "priorty",
                exhaustive_top_five: None,
                required_stems: &["priorit"],
                held_out: true,
            },
            FuzzyCase {
                query: "matinainer",
                exhaustive_top_five: None,
                required_stems: &["maintain"],
                held_out: true,
            },
            FuzzyCase {
                query: "incrmentally",
                exhaustive_top_five: None,
                required_stems: &["increment"],
                held_out: true,
            },
            FuzzyCase {
                query: "teh correct naming",
                exhaustive_top_five: None,
                required_stems: &["correct", "naming"],
                held_out: true,
            },
            FuzzyCase {
                query: "asymptotic performnce",
                exhaustive_top_five: None,
                required_stems: &["asymptotic", "perform"],
                held_out: true,
            },
        ];
        let mut held_out_reciprocal_rank = 0.0_f64;
        let mut held_out_recalled = 0_usize;
        let mut held_out_count = 0_usize;
        for case in cases {
            let FuzzyCase {
                query,
                exhaustive_top_five: expected,
                required_stems,
                held_out,
            } = case;
            // The needle rule the shipped scorer uses, applied to the candidate trigrams, the
            // bound needle below, and the expected-result check further down. Lowercasing
            // instead writes a word-final `Σ` as `ς`, which is a spelling the production path
            // never asks for, so a benchmark digest taken that way measures a rule aise does not
            // ship.
            let mut trigrams: Vec<String> = crate::util::fold_caseless(query)
                .chars()
                .collect::<Vec<_>>()
                .windows(3)
                .map(|window| window.iter().collect())
                .collect();
            trigrams.sort_unstable();
            trigrams.dedup();
            let trigrams_json = serde_json::to_string(&trigrams).unwrap();
            let word_match = query
                .split_whitespace()
                .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" AND ");
            let started = std::time::Instant::now();
            let mut stmt = conn.prepare(sql).unwrap();
            let rows: Vec<(i64, String, i64, i64, i64, String, bool)> = stmt
                .query_map(
                    params![
                        trigrams_json,
                        1_200_i64,
                        query,
                        crate::util::fold_caseless(query),
                        10_i64,
                        word_match
                    ],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                        ))
                    },
                )
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            let elapsed = started.elapsed();
            let identities: Vec<_> = rows.iter().map(|row| (row.1.as_str(), row.2)).collect();
            if query == "actully" {
                assert_eq!(
                    identities.first(),
                    expected.as_ref().and_then(|rows| rows.first()),
                    "the exact typo discussion must remain the first bounded result"
                );
            } else if let Some(expected) = expected {
                assert_eq!(
                    &identities[..expected.len()],
                    &expected,
                    "candidate/scorer parity for {query:?}"
                );
            } else {
                assert!(
                    !rows.is_empty(),
                    "relevance probe returned no rows for {query:?}"
                );
            }
            let relevant_rank = rows.iter().position(|row| {
                let content = crate::util::fold_caseless(&row.5);
                content.contains(&crate::util::fold_caseless(query))
                    || required_stems.iter().all(|stem| content.contains(stem))
            });
            assert!(
                relevant_rank.is_some(),
                "top-ten results for {query:?} contain neither the exact remembered phrase nor all corrected stems {required_stems:?}"
            );
            if held_out {
                held_out_count += 1;
                if let Some(rank) = relevant_rank {
                    held_out_recalled += 1;
                    held_out_reciprocal_rank += 1.0 / (rank + 1) as f64;
                }
            }
            let first_preview: String = rows[0].5.chars().take(160).collect();
            eprintln!(
                "fuzzy_query_benchmark query={query:?} elapsed_us={} returned={} saturated={} fullscan_steps={} sort_operations={} first={first_preview:?}",
                elapsed.as_micros(),
                rows.len(),
                rows[0].6,
                stmt.get_status(StatementStatus::FullscanStep),
                stmt.get_status(StatementStatus::Sort),
            );
        }
        let recall_at_10 = held_out_recalled as f64 / held_out_count as f64;
        let mean_reciprocal_rank = held_out_reciprocal_rank / held_out_count as f64;
        eprintln!(
            "held_out_fuzzy_quality cases={held_out_count} recall_at_10={recall_at_10:.3} mrr={mean_reciprocal_rank:.3}"
        );
        eprintln!(
            "AISE_BENCHMARK_JSON={}",
            serde_json::json!({
                "kind": "fuzzy_relevance",
                "held_out_cases": held_out_count,
                "recall_at_10": recall_at_10,
                "mrr": mean_reciprocal_rank,
            })
        );
        assert_eq!(recall_at_10, 1.0);
        assert!(mean_reciprocal_rank >= 0.5, "MRR={mean_reciprocal_rank}");
    }

    #[test]
    fn overlap_query_bounds_body_scoring_before_ordering_and_saturation_probe() {
        let conn = content_fixture();
        conn.execute_batch(
            "create virtual table messages_trigram using fts5(
                 content,
                 content='messages',
                 content_rowid='id',
                 tokenize='trigram',
                 detail=none,
                 columnsize=0
             );
             create virtual table messages_trigram_vocab
                 using fts5vocab(messages_trigram, instance);
             insert into messages(id, session_id, seq, content) values
                 (1, 's', 0, 'abcdef'),
                 (2, 's', 1, 'abcxyz'),
                 (3, 's', 2, 'bcdxyz'),
                 (4, 's', 3, 'unrelated');
             with recursive n(value) as (
                 values(1)
                 union all
                 select value + 1 from n where value < 10000
             )
             insert into messages(id, session_id, seq, content)
             select 100 + value, 'noise', value, printf('unrelated-%d', value) from n;
             insert into messages_trigram(messages_trigram) values ('rebuild');",
        )
        .unwrap();

        let score_calls = Arc::new(AtomicUsize::new(0));
        let observed_calls = Arc::clone(&score_calls);
        conn.create_scalar_function(
            "test_fuzzy_score",
            2,
            FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_INNOCUOUS,
            move |ctx| {
                observed_calls.fetch_add(1, Ordering::Relaxed);
                let content = ctx.get::<String>(1)?;
                Ok(content.len() as i64)
            },
        )
        .unwrap();

        let sql = "with query_trigrams(term) as materialized (
                 select distinct value from json_each(?1)
             ),
             candidate_probe as materialized (
                 select v.doc as id, count(*) as shared_trigrams
                   from messages_trigram_vocab v
                   join query_trigrams q on q.term = v.term
                  group by v.doc
                  order by shared_trigrams desc, v.doc asc
                  limit ?2 + 1
             ),
             candidate_ids as materialized (
                 select id, shared_trigrams
                   from candidate_probe
                  order by shared_trigrams desc, id asc
                  limit ?2
             ),
             scored as materialized (
                 select m.id,
                        test_fuzzy_score(?3, m.content) as score,
                        c.shared_trigrams
                   from candidate_ids c
                   join messages m on m.id = c.id
             )
             select id,
                    score,
                    shared_trigrams,
                    exists(select 1 from candidate_probe limit 1 offset ?2) as saturated
               from scored
              where score is not null
              order by score desc, id asc";
        let mut stmt = conn.prepare(sql).unwrap();
        let rows: Vec<(i64, i64, i64, bool)> = stmt
            .query_map(params![r#"["abc","bcd"]"#, 1_i64, "abxdef"], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();

        assert_eq!(rows, vec![(1, 6, 2, true)]);
        assert_eq!(score_calls.load(Ordering::Relaxed), 1);
        assert!(
            stmt.get_status(StatementStatus::FullscanStep) <= 4,
            "bounded candidate relations may advance sequentially, but the 10,004-row messages table must not"
        );
    }
}
