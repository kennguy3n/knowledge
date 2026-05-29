// Smoke tests for the Node.js native addon produced by `napi build`
// in `crates/napi/`. Loads the platform-specific `.node` artefact
// via the generated `index.js` loader, exercises every exported
// function at least once, and asserts the JSON-envelope error
// contract documented in `crates/napi/src/bindings.rs`.
//
// Runs under `node --test test/`. Exits non-zero on any assertion
// failure so CI can fail fast.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, resolve as resolvePath } from 'node:path';
import { createRequire } from 'node:module';

const here = dirname(fileURLToPath(import.meta.url));
const requireCJS = createRequire(import.meta.url);
const core = requireCJS(resolvePath(here, '..', 'index.js'));

// Read the crate's Cargo.toml so we can pin the expected
// `coreVersion()` against the source of truth instead of a hardcoded
// string here — that way a `cargo release` bump can't silently drift
// the JS surface from the Rust crate.
//
// Section-scoped lookup: we deliberately walk the TOML line-by-line
// and only accept a `version = "..."` line that lives directly under
// the `[workspace.package]` table (or the crate's own top-level
// section, before any other `[...]` table starts). A naive
// `/^\s*version\s*=\s*"([^"]+)"/m` would match the first `version`
// in the file — fine today because `[workspace.package]` precedes
// `[workspace.dependencies]` and every workspace dep is declared
// inline (`uuid = { version = "1", ... }`), but the moment someone
// adds a multi-line table form before `[workspace.package]` —
// e.g. `[workspace.dependencies.foo]` / `version = "1"` on the next
// line — the JS smoke test would silently start pinning the wrong
// version. Anchoring on the table header makes that failure
// impossible.
// Strip a TOML line comment without eating `#` characters that
// live inside a basic string ("..."), a literal string ('...'),
// or a multi-line string ("""..."""). A naive `raw.replace(/#.*$/, '')`
// would mis-truncate lines like `description = "1.0 # latest"` or
// `description = "rust # crates"` — fine today because we only
// scan for `version = "x.y.z"` lines and version strings never
// contain `#`, but the helper would silently misbehave if reused
// for any other field. This stripper walks the line character by
// character, tracking whether we're inside a string literal, and
// only treats `#` as a comment start when it appears outside any
// quoted region. TOML's quote-escape rules are simpler than JSON's
// (`\"` inside `"..."`, no escapes inside `'...'`), so a
// state machine over the line is sufficient — no need for a full
// parser dependency.
function stripTomlComment(raw) {
  let quote = null; // null | '"' | "'" | '"""' | "'''"
  let i = 0;
  while (i < raw.length) {
    const ch = raw[i];
    if (quote === null) {
      // Multi-line triple-quoted strings open with `"""` / `'''`.
      if (raw.startsWith('"""', i)) {
        quote = '"""';
        i += 3;
        continue;
      }
      if (raw.startsWith("'''", i)) {
        quote = "'''";
        i += 3;
        continue;
      }
      if (ch === '"' || ch === "'") {
        quote = ch;
        i += 1;
        continue;
      }
      if (ch === '#') {
        return raw.slice(0, i);
      }
    } else if (quote === '"""' || quote === "'''") {
      if (raw.startsWith(quote, i)) {
        i += 3;
        quote = null;
        continue;
      }
      i += 1;
      continue;
    } else if (quote === '"') {
      // Basic strings honour backslash escapes — skip the next
      // character if we see one so `\"` doesn't close the string.
      if (ch === '\\' && i + 1 < raw.length) {
        i += 2;
        continue;
      }
      if (ch === '"') {
        quote = null;
      }
      i += 1;
      continue;
    } else if (quote === "'") {
      // Literal strings have no escapes; the next `'` closes.
      if (ch === "'") {
        quote = null;
      }
      i += 1;
      continue;
    }
    i += 1;
  }
  return raw;
}

function findVersionInSection(toml, sectionHeader) {
  const lines = toml.split(/\r?\n/);
  let inSection = false;
  for (const raw of lines) {
    const line = stripTomlComment(raw).trimEnd();
    const tableMatch = /^\[([^\]]+)\]\s*$/.exec(line);
    if (tableMatch) {
      inSection = tableMatch[1] === sectionHeader;
      continue;
    }
    if (!inSection) continue;
    const m = /^\s*version\s*=\s*"([^"]+)"/.exec(line);
    if (m) return m[1];
  }
  return null;
}

function readCargoVersion() {
  const toml = readFileSync(resolvePath(here, '..', 'Cargo.toml'), 'utf-8');
  // Workspace inheritance: `version.workspace = true` means the
  // value lives in the root `Cargo.toml` under `[workspace.package]`.
  if (/version\.workspace\s*=\s*true/.test(toml)) {
    const root = readFileSync(
      resolvePath(here, '..', '..', '..', 'Cargo.toml'),
      'utf-8',
    );
    const v = findVersionInSection(root, 'workspace.package');
    if (!v) {
      throw new Error(
        'cannot find version under [workspace.package] in root Cargo.toml',
      );
    }
    return v;
  }
  const v = findVersionInSection(toml, 'package');
  if (!v) throw new Error('cannot find version under [package] in crates/napi/Cargo.toml');
  return v;
}

function parseEnvelope(err) {
  // Errors thrown from `#[napi]`-annotated functions carry a JSON
  // envelope as their `message`. See `to_js_error` in
  // `crates/napi/src/bindings.rs`.
  return JSON.parse(err.message);
}

test('stripTomlComment ignores `#` inside basic/literal/multiline strings', () => {
  // The naive `replace(/#.*$/, '')` would truncate every one of
  // these to a wrong value. The state-machine stripper preserves
  // every `#` that lives inside any TOML string variant.
  assert.strictEqual(
    stripTomlComment('description = "1.0 # latest"   # real comment'),
    'description = "1.0 # latest"   ',
  );
  assert.strictEqual(
    stripTomlComment(`description = '1.0 # latest'   # real comment`),
    `description = '1.0 # latest'   `,
  );
  assert.strictEqual(
    stripTomlComment('escape = "say \\"hi # there\\""  # cmt'),
    'escape = "say \\"hi # there\\""  ',
  );
  // No `#` at all → pass-through unchanged.
  assert.strictEqual(
    stripTomlComment('plain = "no hashes"'),
    'plain = "no hashes"',
  );
  // Whole-line comment.
  assert.strictEqual(stripTomlComment('# just a comment'), '');
  // No comment at all on a string-free line.
  assert.strictEqual(stripTomlComment('foo = 42'), 'foo = 42');
});

test('findVersionInSection survives a `#`-containing string before the version line', () => {
  // Adversarial fixture: a string-valued field that contains
  // `version = "999.999.999"` inside a literal string before the
  // real `version = "0.1.0"` line. A regex matcher without proper
  // section anchoring (or with a naive comment stripper) would
  // either match the embedded text or strip the closing quote.
  const fixture = [
    '[workspace.package]',
    'description = "see version = \\"999.999.999\\" # not real"',
    'version = "0.1.0"',
    '',
  ].join('\n');
  assert.strictEqual(findVersionInSection(fixture, 'workspace.package'), '0.1.0');
});

test('every documented function is exported with camelCase name', () => {
  // The unconditional surface — every entry point that ships in
  // every build configuration of the addon. Kept sorted to mirror
  // the alphabetised order that `napi build` emits in `index.js`
  // so the diff on assertion failure is easy to read.
  const required = [
    'authenticateConnector',
    'clearOauthClientSecretResolver',
    'clearSyncSchedule',
    'closeStore',
    'configureSyncAutoSynthesize',
    'configureSyncSchedule',
    'configureSynthesisEngine',
    'coreVersion',
    'createConnector',
    'decrypt',
    'encrypt',
    'escapeFtsQuery',
    'forget',
    'forgetScope',
    'generateKeypair',
    'getChannelMemory',
    'getEvidence',
    'getUserMemory',
    'healthCheck',
    'ingestMessage',
    'init',
    'listConnectors',
    'listMemories',
    'listRecentSyntheses',
    'listWebhookServers',
    'openStore',
    'pin',
    'query',
    'refreshConnectorToken',
    'registerWebhookDispatch',
    'removeConnector',
    'runDecaySweep',
    'setOauthClientSecretResolver',
    'startSyncScheduler',
    'startWebhookServer',
    'stopSyncScheduler',
    'stopWebhookServer',
    'syncConnector',
    'syncSchedulerStatus',
    'synthesisStatus',
    'triggerServerSynthesis',
    'triggerSynthesis',
    'unpin',
    'unregisterWebhookDispatch',
  ];
  // Feature-gated entry points that MAY or MAY NOT be present
  // depending on which Cargo features the addon was built with.
  // Each entry is the name of one `#[napi]`-exported function whose
  // `#[cfg(feature = "...")]` gate omits it from default builds.
  // Asserting on the unconditional `required` set and filtering
  // these out of `got` keeps the smoke test green for both
  // `napi build` (default features — no extras) and
  // `napi build --features tracing-subscriber` (`initTracing`
  // present). When a future feature gates a new entry point, add
  // its camel-case export name here.
  const optional = new Set([
    'initTracing', // gated by the `tracing-subscriber` Cargo feature
  ]);
  const got = Object.keys(core)
    .filter((k) => typeof core[k] === 'function')
    .filter((k) => !optional.has(k))
    .sort();
  assert.deepStrictEqual(got, required);
});

test('coreVersion() matches the workspace Cargo.toml version', () => {
  assert.strictEqual(core.coreVersion(), readCargoVersion());
});

test('healthCheck() with no handle returns a bridge-only envelope', () => {
  // Phase 6 surface: `healthCheck()` returns a typed envelope, not
  // the legacy `"ok"` string. Without a handle the substrate skips
  // the per-runtime subsystem probes and returns just the bridge
  // entry — useful for the desktop status panel's bootstrap check
  // before `openStore`.
  const env = core.healthCheck();
  assert.strictEqual(typeof env, 'object', 'envelope is an object');
  assert.strictEqual(env.core_version, readCargoVersion());
  assert.strictEqual(typeof env.uptime_secs, 'number');
  assert.ok(env.uptime_secs >= 0, 'uptime_secs is non-negative');
  assert.strictEqual(typeof env.tracing_initialized, 'boolean');
  assert.ok(Array.isArray(env.subsystems), 'subsystems is an array');
  assert.strictEqual(env.subsystems.length, 1);
  assert.strictEqual(env.subsystems[0].name, 'bridge');
  assert.strictEqual(env.subsystems[0].status, 'ok');
  // The metrics block is wire-flat: every counter / gauge is a
  // sibling field on `env.metrics`, not nested under `counters`.
  assert.strictEqual(typeof env.metrics, 'object');
  assert.strictEqual(typeof env.metrics.ingest_total, 'number');
  assert.strictEqual(typeof env.metrics.query_total, 'number');
  assert.strictEqual(typeof env.metrics.open_handles, 'number');
  assert.strictEqual(typeof env.metrics.tombstone_count, 'number');
  assert.ok(env.metrics.boot_unix_secs > 0, 'boot_unix_secs is set');
  // The `errors_by_kind` block tracks per-FfiError-kind counters,
  // also wire-flat. Each field is a non-negative integer.
  assert.strictEqual(typeof env.metrics.errors_by_kind, 'object');
  for (const kind of [
    'unimplemented',
    'invalid_id',
    'not_found',
    'evidence',
    'memory',
    'synthesis',
    'crypto',
    'unavailable',
    'inference_failure',
  ]) {
    assert.strictEqual(
      typeof env.metrics.errors_by_kind[kind],
      'number',
      `errors_by_kind.${kind} is a number`,
    );
  }
});

test('healthCheck(0n) is equivalent to healthCheck() (NONE-sentinel passthrough)', () => {
  // The substrate treats the `0n` sentinel as "no handle" so JS
  // hosts that track an Option-like handle as `0n` for "not yet
  // opened" can pass it through without a null-check on the JS
  // side.
  const a = core.healthCheck();
  const b = core.healthCheck(0n);
  assert.deepStrictEqual(a.subsystems, b.subsystems);
});

test('healthCheck(unknownHandle) surfaces Unavailable error', () => {
  // Any handle that didn't come from openStore is invalid; the
  // substrate refuses to fabricate a probe envelope for an
  // unknown runtime and surfaces `Unavailable` so callers can
  // distinguish "closed runtime" from "bridge alive but
  // disconnected".
  assert.throws(
    () => core.healthCheck(0xffffffffffffffffn),
    (err) => {
      const env = parseEnvelope(err);
      assert.strictEqual(env.kind, 'Unavailable');
      return true;
    },
  );
});

test('escapeFtsQuery() round-trips through the FTS5 escape rules', () => {
  // The substrate quotes the entire input and doubles any embedded
  // double-quotes. This mirrors the Rust test
  // `escape_fts_query_wraps_in_quotes` in `crates/napi/src/lib.rs`.
  assert.strictEqual(core.escapeFtsQuery('hello'), '"hello"');
  assert.strictEqual(core.escapeFtsQuery('a"b'), '"a""b"');
  assert.strictEqual(core.escapeFtsQuery(''), '""');
});

test('init() accepts a valid InitConfig JSON', () => {
  const cfg = JSON.stringify({ data_dir: '/tmp/x', log_level: 'info' });
  assert.strictEqual(core.init(cfg), undefined);
});

test('init() rejects a malformed config with InvalidConfig kind', () => {
  assert.throws(
    () => core.init('not json'),
    (err) => {
      const env = parseEnvelope(err);
      assert.strictEqual(env.kind, 'InvalidConfig');
      assert.match(env.message, /invalid init config/i);
      return true;
    },
  );
});

test('ingestMessage() with malformed scope_id surfaces InvalidId', () => {
  assert.throws(
    () =>
      core.ingestMessage(0n, {
        scope_id: 'not-a-uuid',
        body: 'hello',
        source: 'Slack',
        importance: 'Important',
      }),
    (err) => {
      const env = parseEnvelope(err);
      assert.strictEqual(env.kind, 'InvalidId');
      return true;
    },
  );
});

test('ingestMessage() with malformed JSON shape surfaces InvalidArgument', () => {
  assert.throws(
    () => core.ingestMessage(0n, { only_one_field: 'oops' }),
    (err) => {
      const env = parseEnvelope(err);
      assert.strictEqual(env.kind, 'InvalidArgument');
      return true;
    },
  );
});

test('query() with malformed scope_id surfaces InvalidId', () => {
  assert.throws(
    () => core.query(0n, { scope_id: 'not-a-uuid', query_text: 'q', limit: 10 }),
    (err) => {
      const env = parseEnvelope(err);
      assert.strictEqual(env.kind, 'InvalidId');
      return true;
    },
  );
});

test('listMemories() accepts a fully-specified filter and surfaces InvalidId', () => {
  // The JS-side filter must match `MemoryFilter` exactly: `state`
  // is `Option<MemoryState>` (use `null` to mean "any") and
  // `pinned_only` is a required bool. The smoke test exercises the
  // common case from the desktop UI of "show me everything in this
  // scope, regardless of pin state".
  assert.throws(
    () => core.listMemories(0n, 'not-a-uuid', { state: null, pinned_only: false }),
    (err) => {
      const env = parseEnvelope(err);
      assert.strictEqual(env.kind, 'InvalidId');
      return true;
    },
  );
});

test('listMemories() rejects a malformed filter shape with InvalidArgument', () => {
  // Missing required `pinned_only` → JSON deserialization error.
  assert.throws(
    () => core.listMemories(0n, 'not-a-uuid', { state: 'Reinforced' }),
    (err) => {
      const env = parseEnvelope(err);
      assert.strictEqual(env.kind, 'InvalidArgument');
      return true;
    },
  );
});

test('listMemories() rejects camelCase `pinnedOnly` typo with a named InvalidArgument', () => {
  // Pins the `#[serde(deny_unknown_fields)]` guard on
  // `MemoryFilter`. A JS developer reaching for what looks like
  // the natural camelCase key (`pinnedOnly`) must get a clear
  // InvalidArgument naming the offending key — *not* an
  // InvalidId surfaced from a silently-defaulted `pinned_only:
  // false` filter that hit the scope-id parser. This test is the
  // last line of defence: it would have caught the doc-comment
  // typo at `bindings.rs:239` that Devin Review (BUG_0001) flagged.
  assert.throws(
    () => core.listMemories(0n, 'not-a-uuid', { state: null, pinnedOnly: false }),
    (err) => {
      const env = parseEnvelope(err);
      assert.strictEqual(env.kind, 'InvalidArgument');
      assert.match(env.message, /pinnedOnly/);
      return true;
    },
  );
});

test('triggerSynthesis() accepts known trigger enum strings', () => {
  for (const trig of [
    'ManualUserAction',
    'BackgroundIdle',
    'EvidenceThreshold',
    'ConnectorSyncCompleted',
  ]) {
    assert.throws(
      () => core.triggerSynthesis(0n, 'not-a-uuid', trig),
      (err) => {
        const env = parseEnvelope(err);
        // Either an InvalidId from the malformed scope OR the FFI
        // surfaces it as a kind that maps through Unavailable —
        // either is acceptable, we just want to confirm the trigger
        // string is not the failure point.
        assert.notStrictEqual(env.kind, 'InvalidArgument');
        return true;
      },
    );
  }
});

test('triggerSynthesis() rejects unknown trigger strings as InvalidArgument', () => {
  assert.throws(
    () => core.triggerSynthesis(0n, 'not-a-uuid', 'Bogus'),
    (err) => {
      const env = parseEnvelope(err);
      assert.strictEqual(env.kind, 'InvalidArgument');
      return true;
    },
  );
});

test('generateKeypair() returns an ml-dsa-65 envelope', () => {
  const kp = core.generateKeypair();
  assert.strictEqual(kp.algorithm, 'ml-dsa-65');
  // Public & private key are returned as arrays of byte integers.
  assert.ok(Array.isArray(kp.public_key));
  assert.ok(Array.isArray(kp.private_key));
  assert.ok(kp.public_key.length > 0);
  assert.ok(kp.private_key.length > 0);
});

test('closeStore() with the NONE sentinel handle is a no-op', () => {
  assert.strictEqual(core.closeStore(0n), undefined);
});

test('closeStore() rejects negative BigInt handles', () => {
  assert.throws(
    () => core.closeStore(-1n),
    (err) => {
      const env = parseEnvelope(err);
      assert.strictEqual(env.kind, 'InvalidArgument');
      return true;
    },
  );
});

test('closeStore() rejects BigInt handles that overflow u64', () => {
  // 2^65 — does not fit in a 64-bit unsigned integer.
  const tooBig = (1n << 65n) + 1n;
  assert.throws(
    () => core.closeStore(tooBig),
    (err) => {
      const env = parseEnvelope(err);
      assert.strictEqual(env.kind, 'InvalidArgument');
      return true;
    },
  );
});

test('pin/unpin/forget all surface InvalidId for malformed ids', () => {
  for (const fn of [core.pin, core.unpin, core.forget]) {
    assert.throws(
      () => fn(0n, 'not-a-uuid'),
      (err) => {
        const env = parseEnvelope(err);
        assert.strictEqual(env.kind, 'InvalidId');
        return true;
      },
    );
  }
});

test('forgetScope, getUserMemory, runDecaySweep, getChannelMemory all surface InvalidId', () => {
  for (const fn of [
    core.forgetScope,
    core.getUserMemory,
    core.runDecaySweep,
    core.getChannelMemory,
  ]) {
    assert.throws(
      () => fn(0n, 'not-a-uuid'),
      (err) => {
        const env = parseEnvelope(err);
        assert.strictEqual(env.kind, 'InvalidId');
        return true;
      },
    );
  }
});

test('getEvidence surfaces InvalidId for a non-UUID id', () => {
  assert.throws(
    () => core.getEvidence(0n, 'not-a-uuid'),
    (err) => {
      const env = parseEnvelope(err);
      assert.strictEqual(env.kind, 'InvalidId');
      return true;
    },
  );
});

test('encrypt/decrypt surface InvalidId for malformed scope', () => {
  for (const fn of [core.encrypt, core.decrypt]) {
    assert.throws(
      () => fn(0n, 'not-a-uuid', 'AAEC'),
      (err) => {
        const env = parseEnvelope(err);
        assert.strictEqual(env.kind, 'InvalidId');
        return true;
      },
    );
  }
});

test('the JSON envelope always contains kind / message / detail keys', () => {
  // Spec the envelope contract end-to-end so future refactors that
  // change `to_js_error` get caught by CI.
  try {
    core.init('not json');
    assert.fail('init should have thrown');
  } catch (err) {
    const env = parseEnvelope(err);
    assert.ok(Object.hasOwn(env, 'kind'), 'envelope missing `kind`');
    assert.ok(Object.hasOwn(env, 'message'), 'envelope missing `message`');
    assert.ok(Object.hasOwn(env, 'detail'), 'envelope missing `detail`');
    assert.strictEqual(typeof env.kind, 'string');
    assert.strictEqual(typeof env.message, 'string');
  }
});
