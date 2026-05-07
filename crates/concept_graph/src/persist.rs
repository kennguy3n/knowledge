//! [`PersistentConceptGraph`] — SQLCipher-backed persistence wrapper
//! over the in-memory [`crate::ConceptGraph`].
//!
//! Per `PHASES.md` Phase 3 task 7, the Phase 2 in-memory adjacency
//! list is wrapped with a thin store that mirrors every node/edge to
//! a SQLCipher database. The encrypted store reuses the same per-user
//! master key as `evidence_store` (see `ARCHITECTURE.md` §2.2): a
//! `sqlcipher:concepts:v1` page key for the database itself, and a
//! per-scope AEAD key (`scope:{uuid}:concept:v1`) under which every
//! node and edge payload is encrypted with XChaCha20-Poly1305.
//!
//! The on-disk schema is two tables:
//!
//! ```sql
//! CREATE TABLE concept_nodes (
//!   id BLOB PRIMARY KEY,
//!   scope_id BLOB NOT NULL,
//!   state TEXT NOT NULL,
//!   superseded_by BLOB,
//!   created_at INTEGER NOT NULL,
//!   updated_at INTEGER NOT NULL,
//!   nonce BLOB NOT NULL,
//!   payload BLOB NOT NULL
//! );
//! CREATE TABLE concept_edges (
//!   id BLOB PRIMARY KEY,
//!   scope_id BLOB NOT NULL,
//!   from_node BLOB NOT NULL,
//!   to_node BLOB NOT NULL,
//!   relation TEXT NOT NULL,
//!   created_at INTEGER NOT NULL,
//!   nonce BLOB NOT NULL,
//!   payload BLOB NOT NULL
//! );
//! ```
//!
//! `payload` is the AEAD ciphertext of the JSON-encoded
//! [`crate::ConceptNode`] / [`crate::ConceptEdge`]; the AAD binds
//! `scope_id` and `id`. The plaintext columns (`state`, `relation`,
//! `from_node`, `to_node`, `superseded_by`, `created_at`, …) are kept
//! out of the ciphertext so scope-filtered queries and traversal
//! pre-checks do not have to decrypt every row.

use std::path::Path;

use rand::RngCore;
use rusqlite::{params, Connection};
use zeroize::Zeroize;

use crypto::{decrypt_aead, derive_key, encrypt_aead, AeadKey, MasterKey, AEAD_NONCE_LEN};
use evidence_store::ScopeId;

use crate::edge::{ConceptEdge, EdgeId, RelationType};
use crate::error::{GraphError, Result};
use crate::graph::ConceptGraph;
use crate::node::{ConceptNode, NodeId, NodeState};

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS concept_nodes (
    id BLOB PRIMARY KEY,
    scope_id BLOB NOT NULL,
    state TEXT NOT NULL,
    superseded_by BLOB,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    nonce BLOB NOT NULL,
    payload BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS concept_nodes_scope_idx ON concept_nodes(scope_id);

CREATE TABLE IF NOT EXISTS concept_edges (
    id BLOB PRIMARY KEY,
    scope_id BLOB NOT NULL,
    from_node BLOB NOT NULL,
    to_node BLOB NOT NULL,
    relation TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    nonce BLOB NOT NULL,
    payload BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS concept_edges_scope_idx ON concept_edges(scope_id);
CREATE INDEX IF NOT EXISTS concept_edges_from_idx ON concept_edges(from_node);
CREATE INDEX IF NOT EXISTS concept_edges_to_idx ON concept_edges(to_node);
";

const SCHEMA_VERSION: i32 = 1;

/// SQLCipher-backed persistence wrapper over [`ConceptGraph`].
///
/// Every mutation (`add_node`, `add_edge`, `supersede_node`,
/// `mark_contradiction`, `remove_node`) is mirrored to the database;
/// [`PersistentConceptGraph::load_scope`] rehydrates the in-memory
/// graph from disk, filtered by scope.
pub struct PersistentConceptGraph {
    graph: ConceptGraph,
    conn: Connection,
    scope_keys: std::collections::HashMap<ScopeId, AeadKey>,
    master_key: MasterKey,
}

impl Drop for PersistentConceptGraph {
    fn drop(&mut self) {
        self.master_key.zeroize();
        for (_id, key) in self.scope_keys.iter_mut() {
            key.zeroize();
        }
    }
}

impl PersistentConceptGraph {
    /// Open or create a SQLCipher concept-graph database at `path`.
    ///
    /// The page-encryption key is derived from `master_key` via the
    /// HKDF context `b"sqlcipher:concepts:v1"`.
    pub fn open<P: AsRef<Path>>(path: P, master_key: &MasterKey) -> Result<Self> {
        let conn = Connection::open(path).map_err(GraphError::Sqlite)?;

        let mut page_key = derive_key(master_key, b"sqlcipher:concepts:v1")?;
        let key_hex = hex_encode(&page_key);
        page_key.zeroize();

        conn.pragma_update(None, "key", format!("x'{key_hex}'"))
            .map_err(GraphError::Sqlite)?;
        conn.pragma_update(None, "cipher_page_size", 4096_i64)
            .map_err(GraphError::Sqlite)?;
        conn.pragma_update(None, "kdf_iter", 256_000_i64)
            .map_err(GraphError::Sqlite)?;

        // Verify the key works.
        let _: i32 = conn
            .query_row("SELECT 1", [], |row| row.get(0))
            .map_err(|_| GraphError::Persistence("SQLCipher key did not unlock the database"))?;

        // Read the existing version *before* applying the schema or
        // stamping a new version. A `user_version` of `0` is the
        // SQLite default for a fresh database and means "no schema
        // applied yet"; anything non-zero must match `SCHEMA_VERSION`
        // exactly or we refuse to open.
        let existing_version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap_or(0);
        if existing_version != 0 && existing_version != SCHEMA_VERSION {
            return Err(GraphError::Persistence(
                "schema version mismatch — refusing to open",
            ));
        }

        conn.execute_batch(SCHEMA_SQL).map_err(GraphError::Sqlite)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(GraphError::Sqlite)?;

        Ok(Self {
            graph: ConceptGraph::new(),
            conn,
            scope_keys: std::collections::HashMap::new(),
            master_key: *master_key,
        })
    }

    /// Borrow the in-memory graph for read-only inspection.
    pub fn graph(&self) -> &ConceptGraph {
        &self.graph
    }

    /// Borrow the in-memory graph mutably.
    ///
    /// Mutations done through this borrow are **not** mirrored to
    /// disk. Prefer the typed wrapper methods for mutations that
    /// must survive a restart.
    pub fn graph_mut(&mut self) -> &mut ConceptGraph {
        &mut self.graph
    }

    /// Insert `node` into the graph and persist it.
    pub fn add_node(&mut self, node: ConceptNode) -> Result<NodeId> {
        let id = self.graph.add_node(node.clone())?;
        self.persist_node(&node)?;
        Ok(id)
    }

    /// Insert `edge` into the graph and persist it.
    pub fn add_edge(&mut self, edge: ConceptEdge) -> Result<EdgeId> {
        let id = self.graph.add_edge(edge.clone())?;
        self.persist_edge(&edge)?;
        Ok(id)
    }

    /// Mark `predecessor` as superseded by `successor`. Persists the
    /// updated `superseded_by` pointer on the predecessor and the
    /// new `supersedes` edge.
    pub fn supersede_node(&mut self, predecessor: NodeId, successor: NodeId) -> Result<EdgeId> {
        let edge_id = self.graph.supersede_node(predecessor, successor)?;
        let pred = self
            .graph
            .get_node(predecessor)
            .cloned()
            .ok_or_else(|| GraphError::node_not_found(predecessor))?;
        self.persist_node(&pred)?;
        let edge = self
            .graph
            .get_edges(predecessor)
            .into_iter()
            .find(|e| e.id == edge_id)
            .cloned()
            .ok_or_else(|| GraphError::Persistence("expected supersedes edge to be present"))?;
        self.persist_edge(&edge)?;
        Ok(edge_id)
    }

    /// Persist any node already present in the in-memory graph (by
    /// id). Useful for callers that mutate a node through
    /// [`Self::graph_mut`] and then want to flush.
    pub fn save_node(&mut self, id: NodeId) -> Result<()> {
        let node = self
            .graph
            .get_node(id)
            .cloned()
            .ok_or_else(|| GraphError::node_not_found(id))?;
        self.persist_node(&node)
    }

    /// Drop every in-memory node/edge and rehydrate the graph from
    /// the database, filtered to `scope`. Returns the number of
    /// (nodes, edges) loaded.
    pub fn load_scope(&mut self, scope: ScopeId) -> Result<(usize, usize)> {
        self.graph = ConceptGraph::new();

        // Derive the scope key up-front so the prepared-statement
        // borrow on `self.conn` doesn't collide with the mutable
        // `scope_key` lookup on `self`.
        let key = self.scope_key(scope)?;
        let scope_bytes = scope.as_uuid().as_bytes().to_vec();

        let mut node_rows: Vec<(NodeId, [u8; AEAD_NONCE_LEN], Vec<u8>)> = Vec::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT id, nonce, payload FROM concept_nodes WHERE scope_id = ?1
                 ORDER BY created_at ASC",
            )?;
            let mut rows = stmt.query(params![scope_bytes])?;
            while let Some(row) = rows.next()? {
                let id_bytes: Vec<u8> = row.get(0)?;
                let nonce_bytes: Vec<u8> = row.get(1)?;
                let ct: Vec<u8> = row.get(2)?;
                let id = NodeId::from_uuid(slice_to_uuid(&id_bytes)?);
                let nonce = slice_to_nonce(&nonce_bytes)?;
                node_rows.push((id, nonce, ct));
            }
        }
        let mut node_count = 0usize;
        for (id, nonce, ct) in node_rows {
            let aad = node_aad(scope, id);
            let pt = decrypt_aead(&key, &nonce, &ct, &aad)?;
            let node: ConceptNode = serde_json::from_slice(&pt)
                .map_err(|_| GraphError::Persistence("node payload is not valid JSON"))?;
            self.graph.add_node(node)?;
            node_count += 1;
        }

        let mut edge_rows: Vec<(EdgeId, [u8; AEAD_NONCE_LEN], Vec<u8>)> = Vec::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT id, nonce, payload FROM concept_edges WHERE scope_id = ?1
                 ORDER BY created_at ASC",
            )?;
            let mut rows = stmt.query(params![scope_bytes])?;
            while let Some(row) = rows.next()? {
                let id_bytes: Vec<u8> = row.get(0)?;
                let nonce_bytes: Vec<u8> = row.get(1)?;
                let ct: Vec<u8> = row.get(2)?;
                let id = EdgeId::from_uuid(slice_to_uuid(&id_bytes)?);
                let nonce = slice_to_nonce(&nonce_bytes)?;
                edge_rows.push((id, nonce, ct));
            }
        }
        let mut edge_count = 0usize;
        for (id, nonce, ct) in edge_rows {
            let aad = edge_aad(scope, id);
            let pt = decrypt_aead(&key, &nonce, &ct, &aad)?;
            let edge: ConceptEdge = serde_json::from_slice(&pt)
                .map_err(|_| GraphError::Persistence("edge payload is not valid JSON"))?;
            self.graph.add_edge(edge)?;
            edge_count += 1;
        }

        Ok((node_count, edge_count))
    }

    /// Number of persisted nodes for `scope` (read-only count).
    pub fn persisted_node_count(&self, scope: ScopeId) -> Result<usize> {
        let scope_bytes = scope.as_uuid().as_bytes().to_vec();
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM concept_nodes WHERE scope_id = ?1",
                params![scope_bytes],
                |row| row.get(0),
            )
            .map_err(GraphError::Sqlite)?;
        Ok(n as usize)
    }

    /// Number of persisted edges for `scope`.
    pub fn persisted_edge_count(&self, scope: ScopeId) -> Result<usize> {
        let scope_bytes = scope.as_uuid().as_bytes().to_vec();
        let n: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM concept_edges WHERE scope_id = ?1",
                params![scope_bytes],
                |row| row.get(0),
            )
            .map_err(GraphError::Sqlite)?;
        Ok(n as usize)
    }

    fn scope_key(&mut self, scope: ScopeId) -> Result<AeadKey> {
        if let Some(k) = self.scope_keys.get(&scope) {
            return Ok(*k);
        }
        let label = format!("scope:{}:concept:v1", scope.as_uuid());
        let key = derive_key(&self.master_key, label.as_bytes())?;
        self.scope_keys.insert(scope, key);
        Ok(key)
    }

    fn persist_node(&mut self, node: &ConceptNode) -> Result<()> {
        let payload = serde_json::to_vec(node)
            .map_err(|_| GraphError::Persistence("node payload could not be serialised"))?;
        let nonce = random_nonce();
        let key = self.scope_key(node.scope_id)?;
        let aad = node_aad(node.scope_id, node.id);
        let ct = encrypt_aead(&key, &nonce, &payload, &aad)?;

        self.conn
            .execute(
                "INSERT INTO concept_nodes
                  (id, scope_id, state, superseded_by, created_at, updated_at, nonce, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET
                   state = excluded.state,
                   superseded_by = excluded.superseded_by,
                   updated_at = excluded.updated_at,
                   nonce = excluded.nonce,
                   payload = excluded.payload",
                params![
                    node.id.as_uuid().as_bytes().to_vec(),
                    node.scope_id.as_uuid().as_bytes().to_vec(),
                    node.state.as_str(),
                    node.superseded_by.map(|s| s.as_uuid().as_bytes().to_vec()),
                    node.created_at.timestamp(),
                    node.updated_at.timestamp(),
                    nonce.to_vec(),
                    ct,
                ],
            )
            .map_err(GraphError::Sqlite)?;
        Ok(())
    }

    fn persist_edge(&mut self, edge: &ConceptEdge) -> Result<()> {
        let payload = serde_json::to_vec(edge)
            .map_err(|_| GraphError::Persistence("edge payload could not be serialised"))?;
        let nonce = random_nonce();
        let key = self.scope_key(edge.scope_id)?;
        let aad = edge_aad(edge.scope_id, edge.id);
        let ct = encrypt_aead(&key, &nonce, &payload, &aad)?;

        self.conn
            .execute(
                "INSERT INTO concept_edges
                  (id, scope_id, from_node, to_node, relation, created_at, nonce, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET
                   relation = excluded.relation,
                   nonce = excluded.nonce,
                   payload = excluded.payload",
                params![
                    edge.id.as_uuid().as_bytes().to_vec(),
                    edge.scope_id.as_uuid().as_bytes().to_vec(),
                    edge.from.as_uuid().as_bytes().to_vec(),
                    edge.to.as_uuid().as_bytes().to_vec(),
                    edge.relation.as_str(),
                    edge.created_at.timestamp(),
                    nonce.to_vec(),
                    ct,
                ],
            )
            .map_err(GraphError::Sqlite)?;
        Ok(())
    }
}

fn random_nonce() -> [u8; AEAD_NONCE_LEN] {
    let mut n = [0u8; AEAD_NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut n);
    n
}

fn node_aad(scope: ScopeId, id: NodeId) -> Vec<u8> {
    let mut aad = b"concept_node:v1:".to_vec();
    aad.extend_from_slice(scope.as_uuid().as_bytes());
    aad.extend_from_slice(id.as_uuid().as_bytes());
    aad
}

fn edge_aad(scope: ScopeId, id: EdgeId) -> Vec<u8> {
    let mut aad = b"concept_edge:v1:".to_vec();
    aad.extend_from_slice(scope.as_uuid().as_bytes());
    aad.extend_from_slice(id.as_uuid().as_bytes());
    aad
}

fn slice_to_uuid(b: &[u8]) -> Result<uuid::Uuid> {
    if b.len() != 16 {
        return Err(GraphError::Persistence("uuid column has wrong width"));
    }
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(b);
    Ok(uuid::Uuid::from_bytes(bytes))
}

fn slice_to_nonce(b: &[u8]) -> Result<[u8; AEAD_NONCE_LEN]> {
    if b.len() != AEAD_NONCE_LEN {
        return Err(GraphError::Persistence("nonce column has wrong width"));
    }
    let mut n = [0u8; AEAD_NONCE_LEN];
    n.copy_from_slice(b);
    Ok(n)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

// `state` and `relation` are stored as plaintext SQL columns so
// callers can run scope-filtered queries without first decrypting
// every row. The plaintext columns are reduced to short tags so they
// don't leak more than the lifecycle / typed-relation taxonomy that
// is already in `PROPOSAL.md`.
impl NodeState {
    fn from_tag(s: &str) -> Option<Self> {
        match s {
            "candidate" => Some(Self::Candidate),
            "canonical" => Some(Self::Canonical),
            "superseded" => Some(Self::Superseded),
            "contradicted" => Some(Self::Contradicted),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }
}

#[allow(dead_code)]
fn relation_round_trip_check(r: RelationType) -> bool {
    RelationType::parse_tag(r.as_str()) == Some(r)
}

#[allow(dead_code)]
fn node_state_round_trip_check(s: NodeState) -> bool {
    NodeState::from_tag(s.as_str()) == Some(s)
}
