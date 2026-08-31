# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Added

- **Named, hashed API keys.** A gateway can now issue a key per consumer, each
  nameable and revocable on its own. Keys are stored as SHA-256 hashes and a
  display prefix in `config/api-keys.json`; the plaintext is shown once, at
  creation, and never again. Create, list and revoke them with `hermes key`, or
  on the panel's new **Access & Keys** screen. The existing `--api-key` /
  `HERMES_API_KEY` static key still works alongside them.
- **Per-key rate limits.** Each key can carry a per-minute and a per-day
  ceiling, enforced live: a key over its limit gets a `429` with a `Retry-After`.
  Loopback and anonymous callers (the panel, a local script) are never metered.
- **Persisted bind configuration** in `config/api.json` — the hosts and port the
  gateway serves on, read beneath the command-line flags so a typed `--host` or
  `--port` always wins. Edit it with `hermes config`, or the panel's *Serve on*
  control, which lists the machine's reachable addresses tagged with the reserved
  range each falls in (a Tailscale/CGNAT address reads *shared range*).
- **The `lightweight` command**, a second entry point identical to `hermes` that
  prints a feather welcome mark on an interactive terminal. `NO_COLOR` and
  `LIGHTWEIGHT_NO_BANNER` are honoured; `--json` and pipes are never decorated.
- `hermes sysinfo` reports an address's reserved-range scope, in the human output
  and as an `addresses` array under `--json`.

### Changed

- **The default port is now 11434** (was 8737), so the CLI, the desktop shell and
  the dev proxy agree and a client assuming the common local-LLM port finds the
  gateway. Anyone who relied on the old default must now pass `--port 8737`.
- The desktop shell no longer mints a fresh API key on every launch — the bug
  that broke a key shared with a remote agent. Keys are the gateway's own, and the
  tray's "Copy API key" is now "Manage API keys…", which opens the panel.
- State-changing control endpoints under `/api/v1` now refuse a request from a
  foreign origin, and creating keys or widening the bind set is refused from a
  non-loopback peer: those take access to the machine running the gateway.

## [0.1.2] - 2026-08-30

### Added

- CPU utilization is now reported on macOS and Windows, so the Performance
  page's CPU Usage tile shows a live figure on every supported platform instead
  of only on Linux. It is read through `host_statistics` on macOS and
  `GetSystemTimes` on Windows, and normalised to the same tick units the panel
  already differences.

### Changed

- Completed the rename to **Lightweight**: the native desktop application — the
  window title, tray, menus, dialogs, and the installer and artifact names —
  now reads "Lightweight" instead of "Hermes", following the panel rename in
  0.1.1.
- Renamed the internal workspace crates from `hermes-*` to `lightweight-*`. The
  `hermes` command and its `HERMES_*` environment variables, the `hermes_*`
  metric names, the `hermes::` log targets, and the existing data directory are
  deliberately unchanged, so nothing that scripts, scrapers, or existing
  installs depend on has moved.

### Fixed

- Long file-path values no longer overflow their cards on the Settings and API
  Gateway pages; they wrap within the card instead.

## [0.1.1] - 2026-08-26

### Changed

- Renamed the desktop shell to **Lightweight**: the sidebar brand name and the
  window title bar now read "Lightweight" instead of "Hermes".

## [0.1.0] - 2026-08-25

### Added

- An OpenAI-compatible, CPU-only inference gateway backed by a supervised
  llama.cpp process.
- GGUF model discovery, verified downloads, imports, and live model switching.
- Conservative RAM admission control with context and KV-cache sizing.
- Benchmarking and machine-scoped calibration with trust checks that reject
  unsafe fits.
- A desktop UI and CLI packages for macOS, Windows, and Linux.

### Known limitations

- Calibration is intentionally deferred for pinned llama.cpp `b10590`:
  `hermes bench --fit` safely refuses every honest fit, so the shipped estimates
  remain conservative by 1.37×–2.85×.

[Unreleased]: https://github.com/dlroqa/Lightweight/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/dlroqa/Lightweight/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/dlroqa/Lightweight/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/dlroqa/Lightweight/releases/tag/v0.1.0
