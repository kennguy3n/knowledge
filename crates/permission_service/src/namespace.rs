//! Namespace configurations — the per-object-type relation
//! inheritance chain.
//!
//! Per `ARCHITECTURE.md` §6, namespace configs encode userset
//! rewrites of the form *"if `subject` has relation `parent` to
//! `object`, then it also has relation `child` to `object`"*. The
//! default substrate config wires:
//!
//! ```text
//! Owner ⇒ Admin ⇒ Editor ⇒ Member ⇒ Viewer
//! ```
//!
//! to every scope-style object type (`Tenant`, `Domain`, `Channel`).
//! Other relations (`Synthesizer`, `Proposer`) are orthogonal and not
//! part of the inheritance chain.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::{PermissionError, Result};
use crate::tuple::{ObjectType, Relation};

/// Namespace config for one object type. The
/// [`Self::implies`] map encodes inheritance: if a key is satisfied
/// (the principal has the key relation), every value is implied.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceConfig {
    /// Object type this config applies to.
    pub object_type: ObjectType,
    /// `parent -> implied children` map.
    pub implies: HashMap<Relation, Vec<Relation>>,
}

impl NamespaceConfig {
    /// Construct an empty namespace config for `object_type`.
    pub fn new(object_type: ObjectType) -> Self {
        Self {
            object_type,
            implies: HashMap::new(),
        }
    }

    /// Record that holding `parent` implies holding all of `children`.
    pub fn imply(mut self, parent: Relation, children: &[Relation]) -> Self {
        self.implies
            .entry(parent)
            .or_default()
            .extend(children.iter().copied());
        self
    }

    /// Resolve every relation transitively reachable from `from`
    /// through this namespace's `implies` map (including `from`
    /// itself). Returns a deterministic order suitable for tests.
    pub fn closure(&self, from: Relation) -> Vec<Relation> {
        let mut out = Vec::new();
        let mut stack = vec![from];
        while let Some(r) = stack.pop() {
            if !out.contains(&r) {
                out.push(r);
                if let Some(children) = self.implies.get(&r) {
                    for c in children {
                        stack.push(*c);
                    }
                }
            }
        }
        out
    }
}

/// Registry of [`NamespaceConfig`]s indexed by object type.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceRegistry {
    namespaces: HashMap<ObjectType, NamespaceConfig>,
}

impl NamespaceRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct the *default* substrate registry — wires the
    /// `Owner ⇒ Admin ⇒ Editor ⇒ Member ⇒ Viewer` chain to every
    /// scope-style object type.
    pub fn with_defaults() -> Self {
        let mut reg = Self::new();
        for object_type in [ObjectType::Tenant, ObjectType::Domain, ObjectType::Channel] {
            let cfg = NamespaceConfig::new(object_type)
                .imply(Relation::Owner, &[Relation::Admin])
                .imply(Relation::Admin, &[Relation::Editor])
                .imply(Relation::Editor, &[Relation::Member])
                .imply(Relation::Member, &[Relation::Viewer]);
            // Safe: we know object_type is fresh in the loop.
            reg.register(cfg).expect("default config registration");
        }
        reg
    }

    /// Register a namespace config. Returns
    /// [`PermissionError::NamespaceAlreadyRegistered`] if a config
    /// already exists for `cfg.object_type`.
    pub fn register(&mut self, cfg: NamespaceConfig) -> Result<()> {
        if self.namespaces.contains_key(&cfg.object_type) {
            return Err(PermissionError::NamespaceAlreadyRegistered(cfg.object_type));
        }
        self.namespaces.insert(cfg.object_type, cfg);
        Ok(())
    }

    /// Look up the config for `object_type`, if any.
    pub fn get(&self, object_type: ObjectType) -> Option<&NamespaceConfig> {
        self.namespaces.get(&object_type)
    }

    /// Convenience: return the closure of relations implied by
    /// `from` for `object_type`. If no namespace is registered, the
    /// closure is `[from]`.
    pub fn closure(&self, object_type: ObjectType, from: Relation) -> Vec<Relation> {
        match self.namespaces.get(&object_type) {
            Some(ns) => ns.closure(from),
            None => vec![from],
        }
    }

    /// True iff `held` implies `wanted` under `object_type`'s
    /// namespace.
    pub fn implies(&self, object_type: ObjectType, held: Relation, wanted: Relation) -> bool {
        self.closure(object_type, held).contains(&wanted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closure_is_transitive() {
        let cfg = NamespaceConfig::new(ObjectType::Tenant)
            .imply(Relation::Owner, &[Relation::Admin])
            .imply(Relation::Admin, &[Relation::Editor]);
        let mut closure = cfg.closure(Relation::Owner);
        closure.sort_by_key(|r| r.as_str());
        assert!(closure.contains(&Relation::Owner));
        assert!(closure.contains(&Relation::Admin));
        assert!(closure.contains(&Relation::Editor));
    }

    #[test]
    fn defaults_chain_owner_to_viewer() {
        let reg = NamespaceRegistry::with_defaults();
        assert!(reg.implies(ObjectType::Tenant, Relation::Owner, Relation::Viewer));
        assert!(reg.implies(ObjectType::Tenant, Relation::Admin, Relation::Member));
        assert!(!reg.implies(ObjectType::Tenant, Relation::Viewer, Relation::Owner));
    }

    #[test]
    fn duplicate_registration_errors() {
        let mut reg = NamespaceRegistry::new();
        let cfg = NamespaceConfig::new(ObjectType::Tenant);
        reg.register(cfg.clone()).unwrap();
        let err = reg.register(cfg).unwrap_err();
        assert_eq!(
            err,
            PermissionError::NamespaceAlreadyRegistered(ObjectType::Tenant)
        );
    }
}
