# Electron bundle & runtime optimization

This guide is the **resource-optimization checklist** for desktop
hosts (macOS + Windows) that embed the Knowledge N-API addon
(`crates/napi`) inside Electron. It is the in-place alternative to a
Tauri migration: the substrate stays on Electron, but the host app
applies the optimizations below to shrink install size and bound
runtime memory.

> Scope: these are **host-app** responsibilities. The substrate ships
> the `.node` addon unchanged; nothing here changes the Rust core. For
> the renderer threat model that every host MUST also implement, see
> [`../security/electron-hardening.md`](../security/electron-hardening.md)
> — none of the optimizations below may relax a hardening control
> (e.g. you may not enable `nodeIntegration` to "save memory").

## Why bother (instead of migrating to Tauri)

Electron bundles its own Chromium + Node, so the wins are in *what the
host ships and how the renderer is configured*, not in the engine
itself. The eight items below recover the bulk of the install-size and
idle-memory gap a Tauri port would have closed, without giving up the
mature Electron packaging / auto-update tooling the desktop hosts
already depend on.

## Checklist

Each item is independently shippable. Numbers are order-of-magnitude
guides on the reference build, not guarantees — measure your own
bundle.

### 1. Strip unused Electron locales (~7 MB)

Electron ships ~80 locale `.pak` files (~100 KB each). Keep only the
locales the app actually localizes. With `electron-builder`:

```jsonc
// electron-builder config
{
  "electronLanguages": ["en-US"] // add only locales you ship
}
```

### 2. `asar` packing, unpack only the `.node` addon

Pack all JS/HTML/CSS into `app.asar` to cut file count and improve
load time, but the native addon must stay unpacked so `require()` can
`dlopen` it from a real path:

```jsonc
{
  "asar": true,
  "asarUnpack": ["**/*.node"]
}
```

### 3. Tree-shake the React renderer in production mode

- Build the renderer with `NODE_ENV=production` so React drops dev
  warnings/checks and uses the production reconciler.
- Enable dead-code elimination (webpack `mode: 'production'` / Vite
  `build` defaults) and minification.
- Verify the production React build is actually bundled (the dev build
  is ~3× larger and slower).

### 4. V8 snapshot for renderer startup

Snapshot the renderer's initial JS heap (Electron's `--snapshot-blob`,
or `electron-link` to build the blob) so cold start skips re-parsing
and re-executing the bootstrap JS. Biggest win on large SPA bundles.

### 5. Cap renderer heap with `--max-old-space-size`

A chat UI does not need V8's default ~1.5 GB old-space ceiling. Cap it
in the renderer's `webPreferences`:

```js
new BrowserWindow({
  webPreferences: {
    // 256 MB is generous for the chat renderer
    additionalArguments: ['--js-flags=--max-old-space-size=256'],
  },
});
```

This bounds the renderer's worst-case resident set and surfaces leaks
as crashes in testing rather than silent multi-GB growth in the field.

### 6. Keep `backgroundThrottling` at its default (`true`)

Do **not** set `backgroundThrottling: false`. Leaving it at the default
lets Chromium throttle timers / `requestAnimationFrame` when the window
is backgrounded — directly cutting CPU wake-ups while the user is in
another app, which complements the substrate's battery gating
(see [`../technical/platforms.md`](../technical/platforms.md) "Battery").

### 7. Drop the spellcheck dictionary when unused (~40 MB on some platforms)

If the app does not need in-renderer spellcheck, disable it so Chromium
never loads the dictionary:

```js
webPreferences: { spellcheck: false }
```

(Historically hosts also flipped `nativeWindowOpen: true`; on modern
Electron that is the default and `new-window` is removed — the relevant
control today is the mandatory `setWindowOpenHandler` deny-by-default
from the hardening guide. Keep that handler; do not add extra windows
just to re-enable legacy behaviour.)

### 8. Single `BrowserWindow`

Each `BrowserWindow` is a full renderer process (its own Chromium heap,
GPU resources, IPC channels). Reuse **one** window and route
settings / preferences / modals via in-app (React) routing instead of
spawning additional windows. This is the single largest idle-memory
lever for a multi-pane desktop app.

## Verifying the wins

- **Install size** — compare the packaged artifact before/after items
  1–3 (`du -sh` the unpacked app, or inspect the installer size).
- **Cold start** — measure renderer first-paint before/after item 4.
- **Idle RSS** — background the window for 60 s and sample resident set
  (items 5–8). The combination of the heap cap, background throttling,
  and a single window is what keeps idle memory bounded across the 5000
  SME-tenant fleet.

## See also

- [`embed-in-electron.md`](embed-in-electron.md) — how to build and
  wire the addon in the first place.
- [`../security/electron-hardening.md`](../security/electron-hardening.md)
  — §8 "Resource optimization" mirrors this checklist from the security
  reviewer's perspective.
- [`../technical/platforms.md`](../technical/platforms.md) — macOS /
  Windows platform notes and the device-level tuning the substrate does
  on its own.
