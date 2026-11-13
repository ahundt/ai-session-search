-- SPDX-FileCopyrightText: 2026 Andrew Hundt
-- SPDX-License-Identifier: Apache-2.0
-- Exact schema from v1.0.0 (531ca8cd), populated by its CLI with the adjacent synthetic JSONL.
begin;
CREATE TABLE sessions (
                id text primary key,
                provider text not null,
                provider_session_id text not null,
                title text,
                summary text,
                cwd text,
                repo_root text,
                created_at text,
                updated_at text,
                last_message_at text,
                preview_text text not null,
                source_path text not null,
                message_count integer,
                parse_version text not null,
                raw_metadata_json text,
                parse_warning text,
                discovery_source text not null,
                parent_session_id text,
                agent_label text
            );
CREATE TABLE transcripts (
                session_id text primary key references sessions(id) on delete cascade,
                transcript_text text not null
            );
CREATE TABLE files_seen (
                provider text not null,
                source_path text not null,
                mtime_ns integer not null,
                size_bytes integer not null,
                last_indexed_at text not null,
                content_hash text,
                parse_version text not null default '',
                -- Incremental tail-parse checkpoint (§7): byte offset (at a newline boundary)
                -- up to which the file is parsed, and a fingerprint of the file's leading bytes
                -- used to detect rewrite/rotation. NULL = no checkpoint → always a full parse.
                tail_byte_offset integer,
                prefix_fingerprint text,
                primary key(provider, source_path)
            );
CREATE TABLE index_metadata (
                key text primary key,
                value integer not null
            );
CREATE INDEX idx_sessions_provider on sessions(provider);
CREATE INDEX idx_sessions_updated_at on sessions(updated_at desc);
CREATE INDEX idx_sessions_provider_id on sessions(provider_session_id);
CREATE TABLE messages (
                id integer primary key,
                session_id text not null references sessions(id) on delete cascade,
                provider text not null,
                seq integer not null,
                role text not null,
                ts text,
                tool_name text,
                kind text not null default 'unknown',
                tool_call_id text,
                is_compaction integer not null default 0,
                content text not null,
                authorship text not null default 'unknown',
                correlation_authority text,
                correlation_scope text,
                correlation_id text,
                record_relation text not null default 'unknown'
                    check (record_relation in ('original', 'mirror', 'unknown')),
                check (
                    (correlation_authority is null and correlation_scope is null and correlation_id is null)
                    or
                    (correlation_authority is not null and correlation_scope is not null and correlation_id is not null)
                )
            );
CREATE INDEX idx_messages_ts on messages(ts);
CREATE INDEX idx_messages_session_seq on messages(session_id, seq);
CREATE INDEX idx_messages_provider_session_seq
                on messages(provider, session_id, seq);
CREATE INDEX idx_messages_role_ts on messages(role, ts);
CREATE INDEX idx_messages_tool_name
                on messages(tool_name) where tool_name is not null;
CREATE TABLE file_edits (
                id integer primary key,
                session_id text not null references sessions(id) on delete cascade,
                provider text not null,
                seq integer not null,
                ts text,
                tool text not null,
                file_path text not null,
                file_name text not null,
                new_content text,
                edits_json text
            );
CREATE INDEX idx_file_edits_session on file_edits(session_id);
CREATE INDEX idx_file_edits_provider on file_edits(provider);
CREATE INDEX idx_file_edits_path on file_edits(file_path);
CREATE INDEX idx_file_edits_name on file_edits(file_name);
CREATE INDEX idx_messages_tool_calls
                 on messages(session_id, seq) where kind = 'tool_call';
CREATE INDEX idx_messages_session_authorship_relation_seq
                 on messages(session_id, authorship, record_relation, seq);
CREATE INDEX idx_messages_authorship_relation_ts
                 on messages(authorship, record_relation, ts desc, session_id, seq);
CREATE INDEX idx_messages_correlation
                 on messages(correlation_authority, correlation_scope, correlation_id)
                 where correlation_id is not null;
CREATE TABLE message_content_parts (
                 message_id integer not null references messages(id) on delete cascade,
                 ordinal integer not null check (ordinal >= 0),
                 start_char integer not null check (start_char >= 0),
                 end_char integer not null check (end_char > start_char),
                 authorship text not null,
                 origin text not null,
                 primary key(message_id, ordinal)
             ) without rowid;
CREATE VIRTUAL TABLE sessions_fts using fts5(
                title, summary, preview_text, transcript_text
            );
CREATE VIRTUAL TABLE messages_fts
         using fts5(content, content='messages', content_rowid='id');
CREATE INDEX idx_messages_excluded_kinds on messages(kind, role, provider, session_id, ts, tool_name) where kind in ('compaction', 'harness_notice');
CREATE VIRTUAL TABLE messages_trigram using fts5(
             content,
             content='messages',
             content_rowid='id',
             tokenize='trigram',
             detail=none,
             columnsize=0
         );
CREATE VIRTUAL TABLE messages_trigram_vocab
             using fts5vocab(messages_trigram, instance);
CREATE VIRTUAL TABLE messages_trigram_terms
             using fts5vocab(messages_trigram, row);
CREATE VIRTUAL TABLE messages_vocab
                 using fts5vocab('messages_fts', 'row');
insert into "sessions" values ('claude:upgrade-claude','claude','upgrade-claude','synthetic upgrade question','synthetic upgrade question','/fixture/project',NULL,'2026-06-01T10:00:00+00:00','2026-06-01T10:01:00+00:00','2026-06-01T10:01:00+00:00','synthetic upgrade question','@SOURCES@/claude/session.jsonl',4,'claude-v4','{"line_count":3,"session_path":"@SOURCES@/claude/session.jsonl","valid_record_count":3}',NULL,'jsonl',NULL,NULL);
insert into "sessions" values ('codex:upgrade-codex','codex','upgrade-codex','synthetic codex question','synthetic codex question','/fixture/project',NULL,'2026-06-01T10:00:00+00:00','2026-06-01T10:02:00+00:00','2026-06-01T10:02:00+00:00','synthetic codex question','@SOURCES@/codex/session.jsonl',2,'codex-v5','{"line_count":3,"rollout_path":null,"session_path":"@SOURCES@/codex/session.jsonl","valid_record_count":3}',NULL,'jsonl+sqlite',NULL,NULL);
insert into "transcripts" values ('claude:upgrade-claude','[2026-06-01 10:00:00 UTC] user
synthetic upgrade question

[2026-06-01 10:01:00 UTC] assistant
synthetic saved file');
insert into "transcripts" values ('codex:upgrade-codex','[2026-06-01 10:01:00 UTC] user
synthetic codex question

[2026-06-01 10:02:00 UTC] assistant
synthetic codex reply');
insert into "files_seen" values ('claude','@SOURCES@/claude/session.jsonl',1794530082642417557,716,'2026-11-13T00:34:42.972647+00:00',NULL,'claude-v4',716,'716:febd7059ec756e1c');
insert into "files_seen" values ('codex','@SOURCES@/codex/session.jsonl',1794530082642695436,496,'2026-11-13T00:34:42.974719+00:00',NULL,'codex-v5',496,'496:ef5853807b3e967b');
insert into "index_metadata" values ('sessions_fts_retired',1);
insert into "index_metadata" values ('auto_reindex_completed_ms',1794530082976);
insert into "index_metadata" values ('auto_reindex_completed_ns',1794530082976042000);
insert into "index_metadata" values ('auto_reindex_parser_contract_0',-2928293429736582971);
insert into "index_metadata" values ('auto_reindex_parser_contract_1',-1507923954240068520);
insert into "index_metadata" values ('auto_reindex_parser_contract_2',-5986360394228736081);
insert into "index_metadata" values ('auto_reindex_parser_contract_3',-3637957560423140987);
insert into "messages" values (1,'claude:upgrade-claude','claude',0,'user','2026-06-01T10:00:00+00:00',NULL,'conversation',NULL,0,'synthetic upgrade question','human','anthropic','claude:upgrade-claude','input','original');
insert into "messages" values (2,'claude:upgrade-claude','claude',1,'tool','2026-06-01T10:01:00+00:00','Write','tool_call','write-1',0,'{"args":{"content":"synthetic saved file","file_path":"/fixture/project/saved.txt"},"kind":"tool_call","tool_name":"Write"}','agent',NULL,NULL,NULL,'original');
insert into "messages" values (3,'claude:upgrade-claude','claude',2,'assistant','2026-06-01T10:01:00+00:00',NULL,'conversation',NULL,0,'synthetic saved file','agent','anthropic','claude:upgrade-claude','write','original');
insert into "messages" values (4,'claude:upgrade-claude','claude',3,'tool','2026-06-01T10:02:00+00:00','Write','tool_result','write-1',0,'written','generated','anthropic','claude:upgrade-claude','result','original');
insert into "messages" values (5,'codex:upgrade-codex','codex',0,'user','2026-06-01T10:01:00+00:00',NULL,'conversation',NULL,0,'synthetic codex question','human',NULL,NULL,NULL,'original');
insert into "messages" values (6,'codex:upgrade-codex','codex',1,'assistant','2026-06-01T10:02:00+00:00',NULL,'conversation',NULL,0,'synthetic codex reply','agent',NULL,NULL,NULL,'original');
insert into "file_edits" values (1,'claude:upgrade-claude','claude',0,'2026-06-01T10:01:00+00:00','Write','/fixture/project/saved.txt','saved.txt','synthetic saved file',NULL);
CREATE TRIGGER messages_correlation_all_or_none_insert
             before insert on messages
             when ((new.correlation_authority is not null)
                   + (new.correlation_scope is not null)
                   + (new.correlation_id is not null)) not in (0, 3)
             begin
                 select raise(abort, 'message correlation authority, scope, and id must be all null or all non-null');
             end;
CREATE TRIGGER messages_correlation_all_or_none_update
             before update of correlation_authority, correlation_scope, correlation_id on messages
             when ((new.correlation_authority is not null)
                   + (new.correlation_scope is not null)
                   + (new.correlation_id is not null)) not in (0, 3)
             begin
                 select raise(abort, 'message correlation authority, scope, and id must be all null or all non-null');
             end;
CREATE TRIGGER messages_ai after insert on messages begin
             insert into messages_fts(rowid, content) values (new.id, new.content);
             insert into messages_trigram(rowid, content) values (new.id, new.content);
         end;
CREATE TRIGGER messages_ad after delete on messages begin
             insert into messages_fts(messages_fts, rowid, content)
                 values ('delete', old.id, old.content);
             insert into messages_trigram(messages_trigram, rowid, content)
                 values ('delete', old.id, old.content);
         end;
CREATE TRIGGER messages_au after update of content on messages begin
             insert into messages_fts(messages_fts, rowid, content)
                 values ('delete', old.id, old.content);
             insert into messages_trigram(messages_trigram, rowid, content)
                 values ('delete', old.id, old.content);
             insert into messages_fts(rowid, content) values (new.id, new.content);
             insert into messages_trigram(rowid, content) values (new.id, new.content);
         end;
insert into messages_fts(messages_fts) values('rebuild');
insert into messages_trigram(messages_trigram) values('rebuild');
pragma user_version=5;
commit;
