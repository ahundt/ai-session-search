// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

//! App-owned durable state for search permission policy generations.
//!
//! The search index is deliberately not the owner of authorization state: index rebuilds must
//! not erase policy generations or later temporary grants. This module owns its SQLite connection
//! and serializes generation transitions across threads and processes.

use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct PolicyGeneration(u64);

impl PolicyGeneration {
    pub fn new(value: u64) -> Option<Self> {
        (value > 0 && i64::try_from(value).is_ok()).then_some(Self(value))
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequestId(String);

impl PermissionRequestId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionGrantId(String);

impl PermissionGrantId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOverlayId(String);

impl PermissionOverlayId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct OverlayActivation {
    pub profile: String,
    pub policy_generation: PolicyGeneration,
    pub expires_at_epoch_seconds: i64,
    pub session_binding: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PendingRequestInput {
    pub policy_generation: PolicyGeneration,
    pub caller_context_binding: String,
    pub operation: crate::search_scope::SearchOperation,
    pub selector_digest: String,
}

#[derive(Debug, Clone, Copy)]
pub struct PermissionRequestBounds {
    pub max_uses: NonZeroU64,
    pub max_expires_in_seconds: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
pub struct GrantApproval {
    pub uses: Option<NonZeroU64>,
    pub expires_at_epoch_seconds: Option<i64>,
}

impl GrantApproval {
    pub const fn one_use() -> Self {
        Self {
            uses: Some(NonZeroU64::MIN),
            expires_at_epoch_seconds: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AdmissionInput<'a> {
    pub grant_id: &'a str,
    pub operation_id: &'a str,
    pub policy_generation: PolicyGeneration,
    pub caller_context_binding: &'a str,
    pub operation: crate::search_scope::SearchOperation,
    pub now_epoch_seconds: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimTerminal {
    Succeeded,
    Failed,
    Cancelled,
    TransportFailed,
}

impl ClaimTerminal {
    fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TransportFailed => "transport-failed",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "transport-failed" => Some(Self::TransportFailed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionOutcome {
    Admitted,
    DuplicateInFlight,
    DuplicateTerminal(ClaimTerminal),
}

struct PendingApprovalRow {
    generation: i64,
    caller: String,
    operation: String,
    selector_digest: String,
    selector_json: Option<String>,
    request_expiry: i64,
    resolved_grant_id: Option<String>,
    max_uses: i64,
    max_expires_in_seconds: Option<i64>,
}

struct GrantAdmissionRow {
    policy_generation: i64,
    caller_context_binding: String,
    operation: String,
    expires_at: Option<i64>,
    remaining_uses: Option<i64>,
    revoked_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct PermissionStateStatus {
    pub pending_requests: u64,
    pub active_grants: u64,
    pub active_overlays: u64,
    pub in_flight_claims: u64,
}

pub struct PermissionStore {
    connection: Mutex<Connection>,
}

impl PermissionStore {
    pub fn open(path: &Path) -> Result<Self> {
        let parent = path.parent().ok_or_else(|| {
            anyhow!("permission state database has no parent directory: {path:?}")
        })?;
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create permission state directory {parent:?}"))?;
        let connection = Connection::open(path)
            .with_context(|| format!("cannot open permission state database {path:?}"))?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .context("cannot configure permission state busy timeout")?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .context("cannot enable WAL for permission state database")?;
        connection
            .execute_batch(
                "create table if not exists policy_sources (
                    source_id text primary key not null,
                    digest blob not null check(length(digest) = 32),
                    generation integer not null check(generation >= 1),
                    valid integer not null check(valid in (0, 1)),
                    observed_at text not null default (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
                 );
                 create table if not exists pending_requests (
                    id text primary key not null check(length(id) = 32),
                    policy_generation integer not null,
                    caller_context_binding text not null,
                    operation text not null,
                    selector_digest text not null,
                    selector_json text,
                    max_uses integer not null default 1 check(max_uses >= 1),
                    max_expires_in_seconds integer check(max_expires_in_seconds is null or max_expires_in_seconds >= 1),
                    created_at integer not null,
                    expires_at integer not null,
                    resolved_grant_id text
                 );
                 create table if not exists grants (
                    id text primary key not null check(length(id) = 32),
                    request_id text not null unique,
                    policy_generation integer not null,
                    caller_context_binding text not null,
                    operation text not null,
                    selector_digest text not null,
                    selector_json text,
                    issued_at integer not null,
                    expires_at integer,
                    remaining_uses integer check(remaining_uses is null or remaining_uses >= 0),
                    revoked_at integer
                 );
                 create index if not exists idx_grants_admission
                    on grants(policy_generation, caller_context_binding, operation, revoked_at, expires_at);
                 create table if not exists grant_claims (
                    grant_id text not null,
                    operation_id text not null,
                    policy_generation integer not null,
                    caller_context_binding text not null,
                    state text not null,
                    admitted_at integer not null,
                    finished_at integer,
                    primary key(grant_id, operation_id)
                 );
                 create table if not exists restrictive_overlays (
                    id text primary key not null check(length(id) = 32),
                    profile text not null,
                    policy_generation integer not null,
                    session_binding text,
                    activated_at integer not null,
                    expires_at integer not null,
                    deactivated_at integer
                 );
                 create index if not exists idx_restrictive_overlays_active
                    on restrictive_overlays(policy_generation, session_binding, expires_at, deactivated_at);",
            )
            .context("cannot initialize permission state schema")?;
        ensure_column(&connection, "pending_requests", "selector_json", "text")?;
        ensure_column(
            &connection,
            "pending_requests",
            "max_uses",
            "integer not null default 1 check(max_uses >= 1)",
        )?;
        ensure_column(
            &connection,
            "pending_requests",
            "max_expires_in_seconds",
            "integer check(max_expires_in_seconds is null or max_expires_in_seconds >= 1)",
        )?;
        ensure_column(&connection, "grants", "selector_json", "text")?;
        set_private_file_mode(path)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn status(
        &self,
        generation: PolicyGeneration,
        now_epoch_seconds: i64,
    ) -> Result<PermissionStateStatus> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("permission state database mutex is poisoned"))?;
        let generation = generation_i64(generation)?;
        let count = |sql: &str| -> Result<u64> {
            let value: i64 =
                connection.query_row(sql, params![generation, now_epoch_seconds], |row| {
                    row.get(0)
                })?;
            u64::try_from(value).context("permission state count is negative")
        };
        Ok(PermissionStateStatus {
            pending_requests: count(
                "select count(*) from pending_requests
                  where policy_generation = ?1 and resolved_grant_id is null and expires_at > ?2",
            )?,
            active_grants: count(
                "select count(*) from grants
                  where policy_generation = ?1 and revoked_at is null
                    and (expires_at is null or expires_at > ?2)
                    and (remaining_uses is null or remaining_uses > 0)",
            )?,
            active_overlays: count(
                "select count(*) from restrictive_overlays
                  where policy_generation = ?1 and deactivated_at is null and expires_at > ?2",
            )?,
            in_flight_claims: count(
                "select count(*) from grant_claims
                  where policy_generation = ?1 and state = 'in-flight' and ?2 is not null",
            )?,
        })
    }

    pub fn prune_terminal_state(
        &self,
        now_epoch_seconds: i64,
        retention_seconds: i64,
    ) -> Result<usize> {
        if retention_seconds < 1 {
            bail!("permission retention_seconds must be at least 1");
        }
        let cutoff = now_epoch_seconds.saturating_sub(retention_seconds);
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("permission state database mutex is poisoned"))?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("cannot begin permission pruning transaction")?;
        let mut removed = transaction.execute(
            "delete from grant_claims where finished_at is not null and finished_at < ?1",
            [cutoff],
        )?;
        removed += transaction.execute(
            "delete from grants
              where issued_at < ?1 and (
                revoked_at is not null or
                (expires_at is not null and expires_at < ?2) or
                remaining_uses = 0
              ) and not exists (
                select 1 from grant_claims where grant_claims.grant_id = grants.id
              )",
            params![cutoff, now_epoch_seconds],
        )?;
        removed += transaction.execute(
            "delete from pending_requests
              where created_at < ?1 and (
                (resolved_grant_id is null and expires_at < ?1) or
                (resolved_grant_id is not null and not exists (
                    select 1 from grants where grants.id = pending_requests.resolved_grant_id
                ))
              )",
            [cutoff],
        )?;
        removed += transaction.execute(
            "delete from restrictive_overlays
              where activated_at < ?1 and (
                deactivated_at is not null or expires_at < ?2
              )",
            params![cutoff, now_epoch_seconds],
        )?;
        transaction.commit()?;
        Ok(removed)
    }

    pub fn activate_overlay(
        &self,
        activation: OverlayActivation,
        now_epoch_seconds: i64,
    ) -> Result<PermissionOverlayId> {
        if activation.profile.is_empty() {
            bail!("overlay profile must not be empty");
        }
        if activation
            .session_binding
            .as_ref()
            .is_some_and(|binding| binding.is_empty())
        {
            bail!("overlay session binding must not be empty");
        }
        if activation.expires_at_epoch_seconds <= now_epoch_seconds {
            bail!("overlay expiry must be later than activation time");
        }
        if activation
            .expires_at_epoch_seconds
            .saturating_sub(now_epoch_seconds)
            > 30 * 24 * 60 * 60
        {
            bail!("overlay expiry may be at most 30 days after activation");
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("permission state database mutex is poisoned"))?;
        let id = random_id(&connection)?;
        connection.execute(
            "insert into restrictive_overlays(
                id, profile, policy_generation, session_binding, activated_at, expires_at
             ) values (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                activation.profile,
                generation_i64(activation.policy_generation)?,
                activation.session_binding,
                now_epoch_seconds,
                activation.expires_at_epoch_seconds,
            ],
        )?;
        Ok(PermissionOverlayId(id))
    }

    pub fn active_overlay_profiles(
        &self,
        policy_generation: PolicyGeneration,
        session_binding: Option<&str>,
        now_epoch_seconds: i64,
    ) -> Result<Vec<String>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("permission state database mutex is poisoned"))?;
        let mut statement = connection.prepare(
            "select distinct profile from restrictive_overlays
              where policy_generation = ?1 and deactivated_at is null and expires_at > ?2
                and (session_binding is null or session_binding = ?3)
              order by profile",
        )?;
        let profiles = statement
            .query_map(
                params![
                    generation_i64(policy_generation)?,
                    now_epoch_seconds,
                    session_binding,
                ],
                |row| row.get(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(profiles)
    }

    pub fn deactivate_overlay(&self, overlay_id: &str, now_epoch_seconds: i64) -> Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("permission state database mutex is poisoned"))?;
        let changed = connection.execute(
            "update restrictive_overlays set deactivated_at = ?2
              where id = ?1 and deactivated_at is null",
            params![overlay_id, now_epoch_seconds],
        )?;
        if changed != 1 {
            bail!("active restrictive overlay {overlay_id:?} was not found");
        }
        Ok(())
    }

    pub fn create_pending_request(
        &self,
        input: PendingRequestInput,
        now_epoch_seconds: i64,
    ) -> Result<PermissionRequestId> {
        self.create_pending_request_inner(
            input,
            None,
            PermissionRequestBounds {
                max_uses: NonZeroU64::new(1_000_000).expect("constant is nonzero"),
                max_expires_in_seconds: Some(30 * 24 * 60 * 60),
            },
            now_epoch_seconds,
        )
    }

    pub fn create_typed_pending_request(
        &self,
        input: PendingRequestInput,
        selector: &crate::search_scope::SessionPolicySelector,
        bounds: PermissionRequestBounds,
        now_epoch_seconds: i64,
    ) -> Result<PermissionRequestId> {
        if input.operation != selector.operation || input.selector_digest != selector.digest() {
            bail!("typed permission selector does not match its operation and digest");
        }
        let selector_json = serde_json::to_string(selector)
            .context("cannot serialize typed permission selector")?;
        self.create_pending_request_inner(input, Some(selector_json), bounds, now_epoch_seconds)
    }

    fn create_pending_request_inner(
        &self,
        input: PendingRequestInput,
        selector_json: Option<String>,
        bounds: PermissionRequestBounds,
        now_epoch_seconds: i64,
    ) -> Result<PermissionRequestId> {
        if input.caller_context_binding.is_empty() || input.selector_digest.is_empty() {
            bail!(
                "permission request caller_context_binding and selector_digest must not be empty"
            );
        }
        let expires_at = now_epoch_seconds
            .checked_add(300)
            .ok_or_else(|| anyhow!("pending permission request expiry overflows"))?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("permission state database mutex is poisoned"))?;
        let id = random_id(&connection)?;
        connection
            .execute(
                "insert into pending_requests(
                    id, policy_generation, caller_context_binding, operation, selector_digest,
                    selector_json, max_uses, max_expires_in_seconds, created_at, expires_at
                 ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    id,
                    generation_i64(input.policy_generation)?,
                    input.caller_context_binding,
                    input.operation.as_str(),
                    input.selector_digest,
                    selector_json,
                    i64::try_from(bounds.max_uses.get())
                        .map_err(|_| anyhow!("max_uses exceeds SQLite integer range"))?,
                    bounds.max_expires_in_seconds,
                    now_epoch_seconds,
                    expires_at,
                ],
            )
            .context("cannot create pending permission request")?;
        Ok(PermissionRequestId(id))
    }

    pub fn approve_request(
        &self,
        request_id: &str,
        approval: GrantApproval,
        now_epoch_seconds: i64,
    ) -> Result<PermissionGrantId> {
        let uses = approval.uses.map(NonZeroU64::get);
        if uses.is_some_and(|uses| uses > 1_000_000) {
            bail!("uses must be an integer from 1 through 1000000");
        }
        if let Some(expires_at) = approval.expires_at_epoch_seconds {
            if expires_at <= now_epoch_seconds {
                bail!("permission grant expiry must be later than approval time");
            }
            if expires_at.saturating_sub(now_epoch_seconds) > 30 * 24 * 60 * 60 {
                bail!("permission grant expiry may be at most 30 days after approval");
            }
        }
        if uses.is_none() && approval.expires_at_epoch_seconds.is_none() {
            bail!("permission approval must include uses or expiry; omit fields for one use");
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("permission state database mutex is poisoned"))?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("cannot begin permission approval transaction")?;
        let request: Option<PendingApprovalRow> = transaction
            .query_row(
                "select policy_generation, caller_context_binding, operation, selector_digest,
                        selector_json, expires_at, resolved_grant_id, max_uses,
                        max_expires_in_seconds
                   from pending_requests where id = ?1",
                [request_id],
                |row| {
                    Ok(PendingApprovalRow {
                        generation: row.get(0)?,
                        caller: row.get(1)?,
                        operation: row.get(2)?,
                        selector_digest: row.get(3)?,
                        selector_json: row.get(4)?,
                        request_expiry: row.get(5)?,
                        resolved_grant_id: row.get(6)?,
                        max_uses: row.get(7)?,
                        max_expires_in_seconds: row.get(8)?,
                    })
                },
            )
            .optional()
            .context("cannot read pending permission request")?;
        let Some(request) = request else {
            bail!("pending permission request {request_id:?} does not exist");
        };
        if let Some(grant_id) = request.resolved_grant_id.as_ref() {
            let grant_id = grant_id.clone();
            transaction.commit()?;
            return Ok(PermissionGrantId(grant_id));
        }
        if request.request_expiry <= now_epoch_seconds {
            bail!("pending permission request {request_id:?} expired");
        }
        if uses.is_some_and(|uses| i64::try_from(uses).map_or(true, |uses| uses > request.max_uses))
        {
            bail!(
                "uses exceeds the accepted maximum of {} for request {request_id:?}",
                request.max_uses
            );
        }
        if let Some(expires_at) = approval.expires_at_epoch_seconds {
            let requested = expires_at.saturating_sub(now_epoch_seconds);
            match request.max_expires_in_seconds {
                Some(maximum) if requested <= maximum => {}
                Some(maximum) => bail!(
                    "expires_in exceeds the accepted maximum of {maximum}s for request {request_id:?}"
                ),
                None => bail!("expires_in is not allowed for request {request_id:?}"),
            }
        }
        let grant_id = random_id(&transaction)?;
        let remaining_uses = uses
            .map(|uses| {
                i64::try_from(uses).map_err(|_| anyhow!("uses exceeds SQLite integer range"))
            })
            .transpose()?;
        transaction
            .execute(
                "insert into grants(
                    id, request_id, policy_generation, caller_context_binding, operation,
                    selector_digest, selector_json, issued_at, expires_at, remaining_uses
                 ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    grant_id,
                    request_id,
                    request.generation,
                    request.caller,
                    request.operation,
                    request.selector_digest,
                    request.selector_json,
                    now_epoch_seconds,
                    approval.expires_at_epoch_seconds,
                    remaining_uses,
                ],
            )
            .context("cannot create permission grant")?;
        transaction
            .execute(
                "update pending_requests set resolved_grant_id = ?2 where id = ?1",
                params![request_id, grant_id],
            )
            .context("cannot resolve pending permission request")?;
        transaction
            .commit()
            .context("cannot commit permission approval")?;
        Ok(PermissionGrantId(grant_id))
    }

    pub fn admit(&self, input: &AdmissionInput<'_>) -> Result<AdmissionOutcome> {
        if input.operation_id.is_empty() {
            bail!("operation_id must not be empty");
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("permission state database mutex is poisoned"))?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("cannot begin permission admission transaction")?;
        let duplicate: Option<(String, i64, String)> = transaction
            .query_row(
                "select state, policy_generation, caller_context_binding
                   from grant_claims where grant_id = ?1 and operation_id = ?2",
                params![input.grant_id, input.operation_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .context("cannot inspect duplicate permission claim")?;
        if let Some((state, generation, caller)) = duplicate {
            if generation != generation_i64(input.policy_generation)?
                || caller != input.caller_context_binding
            {
                bail!("duplicate permission operation has a different policy generation or caller context binding");
            }
            transaction.commit()?;
            return if state == "in-flight" {
                Ok(AdmissionOutcome::DuplicateInFlight)
            } else {
                ClaimTerminal::from_str(&state)
                    .map(AdmissionOutcome::DuplicateTerminal)
                    .ok_or_else(|| anyhow!("permission claim has unknown terminal state {state:?}"))
            };
        }
        let grant: Option<GrantAdmissionRow> = transaction
            .query_row(
                "select policy_generation, caller_context_binding, operation, expires_at,
                        remaining_uses, revoked_at
                   from grants where id = ?1",
                [input.grant_id],
                |row| {
                    Ok(GrantAdmissionRow {
                        policy_generation: row.get(0)?,
                        caller_context_binding: row.get(1)?,
                        operation: row.get(2)?,
                        expires_at: row.get(3)?,
                        remaining_uses: row.get(4)?,
                        revoked_at: row.get(5)?,
                    })
                },
            )
            .optional()
            .context("cannot read permission grant")?;
        let Some(grant) = grant else {
            bail!("permission grant {:?} does not exist", input.grant_id);
        };
        if grant.policy_generation != generation_i64(input.policy_generation)? {
            bail!("permission grant policy generation does not match the active generation");
        }
        if grant.caller_context_binding != input.caller_context_binding {
            bail!("permission grant caller context does not match the active caller context");
        }
        if grant.operation != input.operation.as_str() {
            bail!("permission grant operation does not match the requested operation");
        }
        if grant.revoked_at.is_some() {
            bail!("permission grant was revoked");
        }
        if grant
            .expires_at
            .is_some_and(|expiry| expiry <= input.now_epoch_seconds)
        {
            bail!("permission grant expired");
        }
        if grant.remaining_uses.is_some_and(|remaining| remaining <= 0) {
            bail!("no active permission grant has remaining uses");
        }
        if let Some(remaining) = grant.remaining_uses {
            let changed = transaction.execute(
                "update grants set remaining_uses = remaining_uses - 1
                   where id = ?1 and revoked_at is null and remaining_uses = ?2
                     and (expires_at is null or expires_at > ?3)",
                params![input.grant_id, remaining, input.now_epoch_seconds],
            )?;
            if changed != 1 {
                bail!("no active permission grant could be admitted");
            }
        }
        transaction
            .execute(
                "insert into grant_claims(
                    grant_id, operation_id, policy_generation, caller_context_binding,
                    state, admitted_at
                 ) values (?1, ?2, ?3, ?4, 'in-flight', ?5)",
                params![
                    input.grant_id,
                    input.operation_id,
                    generation_i64(input.policy_generation)?,
                    input.caller_context_binding,
                    input.now_epoch_seconds,
                ],
            )
            .context("cannot record admitted permission claim")?;
        transaction
            .commit()
            .context("cannot commit permission admission")?;
        Ok(AdmissionOutcome::Admitted)
    }

    pub fn finish_claim(
        &self,
        grant_id: &str,
        operation_id: &str,
        terminal: ClaimTerminal,
    ) -> Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("permission state database mutex is poisoned"))?;
        let changed = connection.execute(
            "update grant_claims
                set state = ?3, finished_at = strftime('%s', 'now')
              where grant_id = ?1 and operation_id = ?2 and state = 'in-flight'",
            params![grant_id, operation_id, terminal.as_str()],
        )?;
        if changed != 1 {
            bail!("in-flight permission claim was not found");
        }
        Ok(())
    }

    pub fn grant_selector(
        &self,
        grant_id: &str,
    ) -> Result<crate::search_scope::SessionPolicySelector> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("permission state database mutex is poisoned"))?;
        let selector_json: Option<Option<String>> = connection
            .query_row(
                "select selector_json from grants where id = ?1",
                [grant_id],
                |row| row.get(0),
            )
            .optional()
            .context("cannot read permission grant selector")?;
        let selector_json = selector_json
            .ok_or_else(|| anyhow!("permission grant {grant_id:?} does not exist"))?
            .ok_or_else(|| anyhow!("permission grant {grant_id:?} has no typed selector"))?;
        serde_json::from_str(&selector_json).context("stored permission grant selector is invalid")
    }

    pub fn revoke_grant(&self, grant_id: &str, now_epoch_seconds: i64) -> Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("permission state database mutex is poisoned"))?;
        let changed = connection.execute(
            "update grants set revoked_at = ?2 where id = ?1 and revoked_at is null",
            params![grant_id, now_epoch_seconds],
        )?;
        if changed != 1 {
            bail!("active permission grant {grant_id:?} was not found");
        }
        Ok(())
    }

    /// Observe one validated config source and return its monotonic generation.
    ///
    /// Time is `O(C)` for hashing `C` config bytes plus one indexed SQLite lookup/update. Memory
    /// is `O(1)` beyond the caller-owned bytes and SHA-256 state. Re-observing identical bytes is
    /// read-only; every byte transition increments once under `BEGIN IMMEDIATE`, including a
    /// transition back to a previously seen digest.
    pub fn observe_valid_policy_source(
        &self,
        config_path: &Path,
        config_bytes: &[u8],
    ) -> Result<PolicyGeneration> {
        let source_id = canonical_source_id(config_path)?;
        let digest: [u8; 32] = Sha256::digest(config_bytes).into();
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow!("permission state database mutex is poisoned"))?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("cannot begin permission policy generation transaction")?;
        let current: Option<(Vec<u8>, i64, bool)> = transaction
            .query_row(
                "select digest, generation, valid from policy_sources where source_id = ?1",
                [&source_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .context("cannot read permission policy generation")?;
        let generation = match current {
            Some((current_digest, generation, true)) if current_digest == digest => {
                u64::try_from(generation)
                    .map_err(|_| anyhow!("stored permission policy generation is invalid"))?
            }
            Some((_current_digest, generation, _valid)) => {
                let next = generation
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("permission policy generation overflow; archive the state database and reinitialize under operator control"))?;
                transaction
                    .execute(
                        "update policy_sources
                         set digest = ?2, generation = ?3, valid = 1,
                             observed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                         where source_id = ?1",
                        params![source_id, digest.as_slice(), next],
                    )
                    .context("cannot advance permission policy generation")?;
                u64::try_from(next)
                    .map_err(|_| anyhow!("stored permission policy generation is invalid"))?
            }
            None => {
                transaction
                    .execute(
                        "insert into policy_sources(source_id, digest, generation, valid)
                         values (?1, ?2, 1, 1)",
                        params![source_id, digest.as_slice()],
                    )
                    .context("cannot record initial permission policy generation")?;
                1
            }
        };
        transaction
            .commit()
            .context("cannot commit permission policy generation")?;
        Ok(PolicyGeneration(generation))
    }
}

fn ensure_column(
    connection: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
    let mut statement = connection.prepare(&format!("pragma table_info({table})"))?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    for existing in columns {
        if existing? == column {
            return Ok(());
        }
    }
    connection
        .execute_batch(&format!(
            "alter table {table} add column {column} {definition}"
        ))
        .with_context(|| format!("cannot add {table}.{column}"))?;
    Ok(())
}

fn generation_i64(generation: PolicyGeneration) -> Result<i64> {
    i64::try_from(generation.get())
        .map_err(|_| anyhow!("permission policy generation exceeds SQLite integer range"))
}

fn random_id(connection: &Connection) -> Result<String> {
    let id: String = connection
        .query_row("select lower(hex(randomblob(16)))", [], |row| row.get(0))
        .context("cannot generate permission correlation ID")?;
    if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("permission correlation ID generator returned an invalid value");
    }
    Ok(id)
}

fn canonical_source_id(path: &Path) -> Result<String> {
    if !path.is_absolute() {
        bail!("permission config source path must be absolute, got {path:?}");
    }
    let path: PathBuf = match fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => path.to_owned(),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot resolve permission config source {path:?}"));
        }
    };
    path.into_os_string()
        .into_string()
        .map_err(|_| anyhow!("permission config source path must be valid UTF-8"))
}

#[cfg(unix)]
fn set_private_file_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .with_context(|| format!("cannot inspect permission state database {path:?}"))?
        .permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("cannot protect permission state database {path:?}"))
}

#[cfg(not(unix))]
fn set_private_file_mode(_path: &Path) -> Result<()> {
    Ok(())
}
