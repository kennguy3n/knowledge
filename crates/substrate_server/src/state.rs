//! Shared application state handed to every axum handler.

use std::sync::{Arc, Mutex};

use ffi::RuntimeHandle;
use permission_service::{NamespaceRegistry, TupleStore};

use crate::config::ServerConfig;

/// In-memory permission state: the relation-tuple set plus the
/// namespace registry that defines the relation-implication closure
/// (Owner ⇒ Admin ⇒ … ⇒ Viewer). Guarded by a `std::sync::Mutex`
/// because every operation on it is a fast, non-blocking in-memory
/// mutation/lookup — no `.await` is ever held across the lock.
pub struct PermissionState {
    /// The relation-tuple set.
    pub store: TupleStore,
    /// Relation-implication namespace config.
    pub namespaces: NamespaceRegistry,
}

impl PermissionState {
    /// Construct a fresh permission state with the default namespace
    /// registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            store: TupleStore::new(),
            namespaces: NamespaceRegistry::default(),
        }
    }
}

impl Default for PermissionState {
    fn default() -> Self {
        Self::new()
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
}

impl AppState {
    /// Construct a new [`AppState`] around an already-opened runtime
    /// handle.
    #[must_use]
    pub fn new(handle: RuntimeHandle, config: Arc<ServerConfig>) -> Self {
        Self {
            handle,
            config,
            permissions: Arc::new(Mutex::new(PermissionState::new())),
        }
    }
}
