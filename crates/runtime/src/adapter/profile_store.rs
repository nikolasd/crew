//! Persistence for registered [`WorkerProfile`]s (the `adapter_profiles`
//! table). Deliberately separate from [`crate::domain::DomainRepository`]:
//! registering a profile is configuration, not an orchestration fact, so
//! it is never appended to the append-only `events` journal and never
//! broadcast to `events/subscribe` listeners -- unlike every
//! `DomainRepository` mutation, which must be (see
//! `docs/architecture.md` §18 item 3).

use rusqlite::Connection;

use super::profile::{EffectivePolicy, ProfileError, ProfileId, WorkerProfile};

/// Errors reading or writing the `adapter_profiles` table.
#[derive(Debug, thiserror::Error)]
pub enum ProfileStoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("profile {0} not found")]
    NotFound(ProfileId),
    #[error(transparent)]
    Serialize(#[from] serde_json::Error),
    #[error(transparent)]
    Invalid(#[from] ProfileError),
}

/// A repository over the `adapter_profiles` table. Holds no state of its
/// own; every method borrows a connection directly (unlike
/// `DomainRepository`, this never touches the `events` journal).
pub struct ProfileStore;

impl ProfileStore {
    /// Validates `profile` against `policy`, then -- only if validation
    /// passes -- persists it under its own `id`, alongside its content
    /// fingerprint. There is no lower-level insert path that skips
    /// validation: this is the *only* way a [`WorkerProfile`] reaches the
    /// `adapter_profiles` table, so an unsafe profile (empty model,
    /// adapter/startup-options mismatch, a disallowed environment name, or
    /// a secret-shaped `permissionEnvelope`) is structurally unpersistable.
    ///
    /// # Errors
    /// Returns [`ProfileStoreError::Invalid`] if `profile.validate(policy)`
    /// fails, or [`ProfileStoreError::Sqlite`]/[`ProfileStoreError::Serialize`]
    /// on a lower-level failure.
    pub fn register(
        conn: &Connection,
        profile: &WorkerProfile,
        policy: &EffectivePolicy,
        fingerprint: &str,
    ) -> Result<(), ProfileStoreError> {
        profile.validate(policy)?;
        conn.execute(
            "INSERT INTO adapter_profiles
                (id, adapter, model, permission_envelope, startup_options_json, environment_allowlist_json, source, fingerprint, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                profile.id.to_string(),
                profile.adapter,
                profile.model,
                serde_json::to_string(&profile.permission_envelope)?,
                serde_json::to_string(&profile.startup_options)?,
                serde_json::to_string(&profile.environment_allowlist)?,
                profile.source,
                fingerprint,
                crew_protocol::Timestamp::now().as_str(),
            ],
        )?;
        Ok(())
    }

    /// Resolves a registered profile by id, returning the reconstructed
    /// [`WorkerProfile`] and its stored fingerprint.
    ///
    /// # Errors
    /// Returns [`ProfileStoreError::NotFound`] if `id` was never
    /// registered.
    pub fn get(
        conn: &Connection,
        id: ProfileId,
    ) -> Result<(WorkerProfile, String), ProfileStoreError> {
        let row = conn
            .query_row(
                "SELECT adapter, model, permission_envelope, startup_options_json, environment_allowlist_json, source, fingerprint
                 FROM adapter_profiles WHERE id = ?1",
                [id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .map_err(|err| match err {
                rusqlite::Error::QueryReturnedNoRows => ProfileStoreError::NotFound(id),
                other => ProfileStoreError::Sqlite(other),
            })?;
        let (
            adapter,
            model,
            permission_envelope,
            startup_options_json,
            environment_allowlist_json,
            source,
            fingerprint,
        ) = row;
        let profile = WorkerProfile {
            id,
            adapter,
            model,
            permission_envelope: serde_json::from_str(&permission_envelope)?,
            startup_options: serde_json::from_str(&startup_options_json)?,
            environment_allowlist: serde_json::from_str(&environment_allowlist_json)?,
            source,
        };
        Ok((profile, fingerprint))
    }
}
