# Changelog

All notable changes to `kmb-sync`, `kmb-store` and `kmb-envis`. The crates
share one version, kept in step with
[zenith-kmb](https://github.com/Project-Stateful-Zenith/zenith-kmb). The
format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and the project uses [Semantic Versioning](https://semver.org/).

## [1.1.3] - 2026-10-16

First tagged release, built on `kmb` and `kmb-tokio` 1.1.3.

### `kmb-sync`

- Always-on service (`serve`) that polls KMB/KMP meters on a schedule:
  archive downloads, live values and clock jobs, with per-device link time
  and daily byte budgets.
- Resumes after restarts and outages from stored cursors.
- One-shot runs (`once`), configuration check (`check`), export and API
  client commands.
- Local JSON API, optionally reachable from other hosts with tokens.
- Read-only status page: stations and jobs, the last 48 runs, the latest
  poll time, the next sync, link traffic and per-archive state, in light
  and dark. The footer shows the running version.
- Run log rotated in two periods.
- Feature `tls`: KMB over TLS.

### `kmb-store`

- SQLite store for objects, identities, measurements, archive definitions,
  records, cursors and runs.

### `kmb-envis`

- Writes downloaded records into an ENVIS 2.5 MSSQL database.

[1.1.3]: https://github.com/Project-Stateful-Zenith/kmb-sync/releases/tag/v1.1.3
