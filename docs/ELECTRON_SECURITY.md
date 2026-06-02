# Electron security guidance for `knowledge`'s N-API addon

This document is the **renderer-process threat model** that any
Electron host embedding the `knowledge` N-API addon
(`crates/napi`) must implement. The substrate ships the addon
unchanged across hosts; the boundary between the addon (main
process, full filesystem access) and the renderer process (HTML /
JS / DOM) is the **only** thing standing between a compromised
web view and the user's evidence store.

> If you implement an Electron renderer that has direct access to
> the `knowledge` N-API addon, you have effectively shipped a
> remote-code-execution sink for any malicious URL the renderer
> can be coerced into loading. The addon runs in the main process
> with full filesystem access by design — the renderer must never
> have direct access to it.

## 1. Required `BrowserWindow` settings

Every `BrowserWindow` (and every `BrowserView` /
`<webview>`-equivalent) must be created with the following
`webPreferences`. None of these are defaults in any current
Electron release; missing any one of them is a vulnerability.

```js
const win = new BrowserWindow({
  webPreferences: {
    // Renderer code runs in its own JS context, separated from
    // the preload bundle's context. Without this, any XSS in
    // the renderer trivially walks the preload's exports.
    contextIsolation: true,

    // Disables `require()`, `process`, and the rest of Node's
    // globals in the renderer. The renderer is JS-only.
    nodeIntegration: false,
    nodeIntegrationInSubFrames: false,
    nodeIntegrationInWorker: false,

    // Activates Chromium's renderer sandbox (seccomp / win32
    // job object). XSS in the renderer cannot spawn child
    // processes, cannot read arbitrary files, and cannot
    // import Node modules even if the bundle leaks one.
    sandbox: true,

    // The preload script is the ONLY way the renderer can call
    // into the addon. See §3 for the IPC allowlist contract.
    preload: path.join(__dirname, "preload.js"),

    // Disables the `<webview>` tag entirely. If you need it,
    // think again — it is a re-entrant `BrowserWindow` with
    // its own attack surface.
    webviewTag: false,

    // Disables remote module loading (already deprecated /
    // removed in modern Electron, but spelled out so
    // downstream forks cannot quietly re-enable it).
    enableRemoteModule: false,

    // Disables experimental web platform features that
    // Chromium has not yet hardened.
    experimentalFeatures: false,
  },
});
```

## 2. Content Security Policy (CSP)

Every HTML document loaded into a renderer must declare a CSP
header. The CSP below is the minimum acceptable baseline; tighten
further per app.

```http
Content-Security-Policy:
  default-src 'self';
  script-src 'self';
  style-src 'self';
  img-src 'self' data:;
  font-src 'self';
  connect-src 'self';
  object-src 'none';
  frame-ancestors 'none';
  base-uri 'none';
  form-action 'none';
```

Explicitly **forbidden**:

* `unsafe-eval` — `eval`, `new Function(…)`, and `setTimeout`
  string-form all become RCE sinks for an attacker who controls
  any DOM content.
* `unsafe-inline` for `script-src` and `style-src` — inline
  `<script>` and `style="…"` attributes are how XSS payloads
  most commonly land.
* Wildcard `connect-src` — outbound `fetch` / `WebSocket` must
  be allowlisted per host. Any leak here trivially exfiltrates
  the renderer-side evidence cache.

If you need a `connect-src` allowlist for OAuth callbacks or
substrate-internal HTTPS endpoints, name each origin
explicitly. **Never** use `https:` as a permissive scheme.

Set the CSP via an HTTP header from the main process'
`session.webRequest.onHeadersReceived` handler — `<meta>`-tag CSP
is ignored for some directives and cannot be relied on.

## 3. IPC channel allowlist

The renderer must **never** be able to call into the N-API addon
directly. The preload script is the chokepoint that exposes a
narrow, audited surface via Electron's `contextBridge`.

The allowlist below is the contract any preload script must
honour. Each exposed function maps to **exactly one** function
exported by `crates/napi/src/bindings.rs`; preload code that
introduces new IPC channels must update this table.

```js
// preload.js
const { contextBridge, ipcRenderer } = require("electron");

const ALLOWED_CALLS = new Set([
  "knowledge:open_store",
  "knowledge:close_store",
  "knowledge:ingest",
  "knowledge:query",
  "knowledge:forget",
  "knowledge:health_check",
  "knowledge:metrics_snapshot",
  // …extend ONLY when adding a corresponding N-API export
]);

contextBridge.exposeInMainWorld("knowledge", {
  invoke: (channel, payload) => {
    if (!ALLOWED_CALLS.has(channel)) {
      throw new Error(`renderer attempted disallowed IPC: ${channel}`);
    }
    return ipcRenderer.invoke(channel, payload);
  },
});
```

The main process registers `ipcMain.handle` for each entry in the
allowlist and delegates to the addon. **No other** IPC channel is
registered. `ipcRenderer.send` (fire-and-forget) is intentionally
unexposed — every renderer-initiated call returns through `invoke`
so the main process can deny / log / rate-limit as needed.

When a new N-API export lands in `crates/napi/src/bindings.rs`,
the corresponding `knowledge:*` channel name must be added to the
preload allowlist **and** to the main-process `ipcMain.handle`
registration in the same commit. CI in this repo flags drifts
between the addon exports and the preload bundle.

## 4. Preload script isolation pattern

The preload script runs in a **separate JS context** from the
renderer (because `contextIsolation: true` — §1). The
`contextBridge` is the only legal way to share values across the
boundary. In particular:

* Do **not** attach addon references to `window.*` directly —
  `window.knowledge = require("knowledge-napi")` is an RCE
  primitive for any XSS.
* Do **not** expose callable functions returned from the addon
  to the renderer — only POJO snapshots (`JSON.parse(
  JSON.stringify(value))`) cross the bridge.
* Do **not** expose `ipcRenderer` itself — only the narrow
  `invoke` wrapper that goes through the allowlist.

The preload bundle should be the **minimal** amount of code that
satisfies the allowlist contract. Any helper logic (request
shaping, response normalisation) belongs in the renderer where it
runs under the strict CSP.

## 5. Main-process posture

The N-API addon runs in the **main process** with full filesystem
access. That means:

* The main process MUST initialise the addon exactly once at
  startup, before any `BrowserWindow` is created.
* The main process MUST refuse `BrowserWindow` creation if its
  webPreferences violate §1. Reject the request loudly rather
  than silently downgrading the security posture.
* The main process MUST set
  `app.enableSandbox()` before any window is created so the
  default sandbox baseline is enforced even if a
  `webPreferences` entry omits it.
* The main process MUST install a
  `app.on("web-contents-created")` hook that calls
  `contents.setWindowOpenHandler` to deny every `target=_blank`
  / `window.open(…)` request that is not on the renderer's
  allowlist — Chromium's default behaviour creates a fresh
  unsandboxed `BrowserWindow`, which is a sandbox-escape
  primitive.
* The main process MUST install a
  `contents.on("will-navigate")` hook that aborts navigation to
  any URL outside the renderer's allowlist (typically
  `file://` for the bundled SPA and the OAuth callback origins).

## 6. Auto-update and packaging

Out of scope for this document, but related: the Electron host
must validate code-signing on every update before applying it;
the N-API addon is loaded by `require()` and a modified addon
file is an immediate full-system compromise. Defer to the
host's update framework (Squirrel, electron-updater) for the
signature-validation contract.

## 7. Checklist

When reviewing an Electron host that embeds this addon, every box
below must be checked. Anything left unchecked is a vulnerability.

- [ ] `contextIsolation: true` on every `BrowserWindow`.
- [ ] `nodeIntegration: false` on every `BrowserWindow`.
- [ ] `sandbox: true` on every `BrowserWindow`.
- [ ] CSP set via `onHeadersReceived`, not `<meta>`.
- [ ] CSP excludes `unsafe-eval`, `unsafe-inline`, wildcard
  `connect-src`.
- [ ] Preload script exposes only an explicit allowlist of IPC
  channels.
- [ ] Each IPC channel maps to exactly one N-API export.
- [ ] `app.enableSandbox()` called before any window creation.
- [ ] `setWindowOpenHandler` denies unknown origins.
- [ ] `will-navigate` denies unknown URLs.
- [ ] Auto-update path validates code-signing on the addon.
