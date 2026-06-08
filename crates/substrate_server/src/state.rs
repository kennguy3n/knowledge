//! Shared application state handed to every axum handler.

use std::sync::{Arc, Mutex};

use ffi::RuntimeHandle;
use permission_service::{NamespaceRegistry, PersistentTupleStore};

use crate::config::{decode_master_key, ServerConfig};
use crate::replication::ReplicationShared;

/// Errors raised while assembling [`AppState`].
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// The configured master key was not valid 64-char hex. This is
    /// normally caught by [`ServerConfig::from_env`]; it is re-checked
    /// here because [`AppState::new`] can be handed a hand-built config.
    #[error("master key is not valid 64-char hex")]
    BadMasterKey,
    /// Opening the SQLCipher-backed permission store failed.
    #[error("opening permission store: {0}")]
    Permission(#[from] permission_service::PermissionError),
}

/// Persistent permission state: the SQLCipher-backed relation-tuple
/// store plus the namespace registry that defines the relation-
/// implication closure (Owner ⇒ Admin ⇒ … ⇒ Viewer). Guarded by a
/// `std::sync::Mutex` because every operation on it is a fast,
/// non-blocking mutation/lookup — no `.await` is ever held across the
/// lock.
///
/// Grants are mirrored to disk on every mutation and rehydrated on
/// open, so authorisation tuples survive a substrate_server restart.
pub struct PermissionState {
    /// The SQLCipher-backed relation-tuple store. Its in-memory
    /// [`permission_service::TupleStore`] (reached via
    /// [`PersistentTupleStore::store`]) remains the query surface for
    /// permission checks.
    pub store: PersistentTupleStore,
    /// Relation-implication namespace config.
    pub namespaces: NamespaceRegistry,
}

impl PermissionState {
    /// Open the persistent permission store at `path`, decrypting and
    /// rehydrating it with `master_key`, and pair it with the default
    /// namespace registry.
    ///
    /// # Errors
    ///
    /// Returns [`permission_service::PermissionError`] if the SQLCipher
    /// database cannot be opened/decrypted or rehydration fails.
    pub fn open(
        path: &str,
        master_key: &[u8; 32],
    ) -> Result<Self, permission_service::PermissionError> {
        Ok(Self {
            store: PersistentTupleStore::open(path, master_key)?,
            namespaces: NamespaceRegistry::default(),
        })
    }
}

/// Cloneable application state. All fields are cheaply clonable:
/// [`RuntimeHandle`] is a `Copy` newtype over `u64`, and the rest are
/// behind `Arc`.
#[derive(Clone)]
pub struct AppState {
    /// Handle to the opened evidence-store runtime.
    pub handle: RuntimeHandle,
    /// Immutable server configuration.
    pub config: Arc<ServerConfig>,
    /// Shared, mutable permission state.
    pub permissions: Arc<Mutex<PermissionState>>,
    /// Shared active-passive replication state (role, lag, watermarks).
    /// Defaults to [`ReplicationShared::disabled`] for a standalone
    /// substrate; [`AppState::with_replication`] swaps in the live state
    /// when HA is configured.
    pub replication: Arc<ReplicationShared>,
}

impl AppState {
    /// Construct a new [`AppState`] around an already-opened runtime
    /// handle, opening the persistent permission store described by
    /// `config`.
    ///
    /// # Errors
    ///
    /// Returns [`StateError`] if the master key is malformed or the
    /// permission store cannot be opened.
    pub fn new(handle: RuntimeHandle, config: Arc<ServerConfig>) -> Result<Self, StateError> {
        let master_key =
            decode_master_key(&config.master_key_hex).ok_or(StateError::BadMasterKey)?;
        let permissions = PermissionState::open(&config.permissions_path, &master_key)?;
        Ok(Self {
            handle,
            config,
            permissions: Arc::new(Mutex::new(permissions)),
            replication: Arc::new(ReplicationShared::disabled()),
        })
    }

    /// Attach live replication state, replacing the disabled default.
    /// Used by [`crate::run`] once the replication engine is wired up.
    #[must_use]
    pub fn with_replication(mut self, replication: Arc<ReplicationShared>) -> Self {
        self.replication = replication;
        self
    }
}
