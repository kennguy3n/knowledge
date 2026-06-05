# Master-key rotation

This document describes how to rotate the **master key** of a deployed
Knowledge substrate: the procedure, what it does (and deliberately does
not) re-encrypt, the integrity guarantees, and how to roll back.

For where the master key lives and how it is protected on each host
platform, see [key-management.md](key-management.md). For the overall
threat model, see [threat-model.md](threat-model.md).

## When to rotate

- Suspected exposure of the master key (operator error, leaked secret,
  compromised secret manager).
- Routine, scheduled rotation as a hygiene control.
- Decommissioning a key custodian / re-provisioning a secret store.

Rotation is an **offline** maintenance operation. The substrate server
must be stopped for its duration so nothing writes to the encrypted
stores while the rotated copy is taken.

## What the master key protects (and what rotation touches)

The substrate seals two SQLCipher databases under the same 32-byte
master key:

| Store | Path (default) | How the master key is used |
|---|---|---|
| Evidence store | `/data/substrate.db` (`KNOWLEDGE_STORE_PATH`) | SQLCipher page key is master-derived. Evidence **bodies** are encrypted under per-scope DEKs; each DEK is *wrapped* under a master-derived wrapping key. |
| Permission store | `/data/permissions.db` (`KNOWLEDGE_PERMISSIONS_PATH`) | SQLCipher page key is master-derived **and** every relation-tuple payload is encrypted directly under a master-derived AEAD key. |

Consequently:

- **Evidence bodies are never re-encrypted.** They are sealed under
  per-scope DEKs that are independent of the master key. Rotation
  re-keys the SQLCipher pages and re-wraps the scope DEKs under the new
  master-derived wrapping key. Legacy HKDF-derived scope keys are
  materialised as explicit wrapped DEKs as part of this step, which
  decouples every body from the master key going forward.
- **Permission tuples are re-encrypted.** Because their payloads are
  encrypted directly under a master-derived key, rotation decrypts and
  re-encrypts every tuple under the new key.
- **Forgotten scopes stay forgotten.** A scope whose DEK has been
  destroyed (cryptographic forgetting) is skipped: its ciphertext is
  copied verbatim and remains undecryptable by design. It is *not*
  resurrected by rotation.

## Integrity guarantees

The rotation tool does not trust the copy blindly. Before the rotated
file is allowed to replace the live one, it verifies:

1. **Row/tuple counts match** between source and rotated copy.
2. **Every live scope DEK round-trips** under the new master-derived
   wrapping key.
3. **Every live evidence body decrypts to identical plaintext** from
   both the source and the rotated copy (byte-for-byte), so a subtle
   re-wrap bug cannot silently corrupt data.

If any check fails, the tool aborts and the live databases are left
untouched under the **old** key. (Implementation:
`EvidenceStore::rotate_master_key` in
`crates/evidence_store/src/store.rs` and
`PersistentTupleStore::rotate_master_key` in
`crates/permission_service/src/persist.rs`; orchestration in
`crates/substrate_server/src/key_rotation.rs`.)

## Procedure (Docker Compose)

Use the wrapper script, which stops the substrate, runs the rotation
tool against the same data volume, and (optionally) updates your env
file and restarts the stack:

```sh
# 1. Generate a new 32-byte key as 64 lowercase hex characters.
openssl rand -hex 32        # -> NEW_KEY

# 2. Rotate. The OLD key is whatever the deployment currently uses.
KNOWLEDGE_MASTER_KEY=<current 64-hex key> \
KNOWLEDGE_NEW_MASTER_KEY=<NEW_KEY> \
scripts/rotate-master-key.sh --env-file .env
```

With `--env-file`, on success the script rewrites
`KNOWLEDGE_MASTER_KEY` in that file to the new key and runs
`docker compose up -d`. Without it, the script leaves the stack stopped
and prints the manual finish-up steps (update the secret wherever the
deployment reads it, then start the stack).

The tool keeps timestamped backups of the **pre-rotation** databases
alongside the originals:

```
/data/substrate.db.bak.<unix>
/data/permissions.db.bak.<unix>
```

These open under the **old** key and are your rollback material.

### Running the tool directly

The script is a thin wrapper around the `knowledge-rotate-key` binary
shipped in the substrate image. To run it by hand against a stopped
substrate's volume:

```sh
docker compose stop knowledge-gateway knowledge-substrate
docker compose run --rm --no-deps \
  -e KNOWLEDGE_MASTER_KEY=<old 64-hex> \
  -e KNOWLEDGE_NEW_MASTER_KEY=<new 64-hex> \
  --entrypoint knowledge-rotate-key \
  knowledge-substrate
```

It reads `KNOWLEDGE_STORE_PATH` / `KNOWLEDGE_PERMISSIONS_PATH` from the
service environment, so it operates on the same files the server uses.

## Procedure (Kubernetes / Helm)

The same binary applies. Scale the substrate to zero, run the tool as a
one-off `Job` (or `kubectl debug`/ephemeral container) that mounts the
substrate PVC and carries the old + new keys, then update the master-key
Secret and scale back up:

```sh
kubectl scale deploy/knowledge-substrate --replicas=0
# Run a Job from the substrate image with command
#   ["knowledge-rotate-key"]
# mounting the substrate PVC at /data and setting
#   KNOWLEDGE_MASTER_KEY (old) and KNOWLEDGE_NEW_MASTER_KEY (new).
kubectl create -f rotate-job.yaml && kubectl wait --for=condition=complete job/rotate
# On success, update the Secret holding KNOWLEDGE_MASTER_KEY to the new
# key, then:
kubectl scale deploy/knowledge-substrate --replicas=1
```

Keep the PVC's `*.bak.<unix>` files until the substrate is confirmed
healthy under the new key.

## Verifying success

After restart:

- The substrate `/internal/metrics` healthcheck passes (the server only
  opens the stores if the master key unlocks them).
- Ingest + query a scope and confirm reads succeed.
- Confirm permission checks still resolve as before.

If the server fails to open the store on boot with the new key, it
means the env was not updated to the new key (or was updated to the
wrong value) — see rollback.

## Rollback

Because the originals are preserved, rollback is a file swap under the
**old** key:

1. Stop the substrate (and gateway).
2. Restore both databases from their backups, e.g. inside the data
   volume:
   ```sh
   mv /data/substrate.db        /data/substrate.db.rotated
   mv /data/substrate.db.bak.<unix>   /data/substrate.db
   mv /data/permissions.db      /data/permissions.db.rotated
   mv /data/permissions.db.bak.<unix> /data/permissions.db
   ```
3. Set `KNOWLEDGE_MASTER_KEY` back to the **old** key.
4. Start the stack.

## Risks and cautions

- **Stop the substrate first.** Rotating a live database risks copying a
  partially written state. The wrapper enforces this by stopping the
  services; the direct/Job paths must do the same.
- **The backups open under the old key.** Until you have confirmed the
  new key works, retain them — then destroy them securely. Leaving them
  on the volume indefinitely re-introduces the old key as an attack
  surface.
- **Update the key everywhere it is read.** The on-disk stores are
  re-keyed atomically, but the running deployment must be told the new
  key (env file / Secret / secret manager). A mismatch fails closed: the
  server refuses to open the store rather than serving wrong data.
- **Disk space.** Rotation writes a full second copy of each store
  before swapping; ensure the volume has headroom for the largest store
  plus its backup.
```
