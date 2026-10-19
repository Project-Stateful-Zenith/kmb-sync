# Changelog

All notable changes to `kmb-sync`, `kmb-store` and `kmb-envis`. The crates
share one version, which started at 1.1.3, the version of
[zenith-kmb](https://github.com/Project-Stateful-Zenith/zenith-kmb) they were
first released with. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).

## [1.3.1] - 2026-10-19

### `kmb-sync`

- Fixed: after the meter clock stepped back, the first record written
  after the cursor's slot was skipped for good once a newer record came.
  The run now goes on from the cursor's slot, stores those records and
  notes the step back, instead of failing until the meter's time passes
  the stored one. Local SQLite only; the ENVIS path keeps the old check.

### `kmb-envis`

- Fixed: a recorded field without an ENVIS type (signed arrays, unknown
  type codes) was left out of the definition while its bytes stayed in
  the row, so every value after it was read from the wrong place. Such an
  archive is now refused with the field's name.
- Fixed: a stored definition was reused for a layout with another unit
  or scale factor (`mul`), so values could show 1000 times off. A
  different scale or unit now needs its own definition, which with
  `allow_new_definitions = false` stops that archive with "no definition".
- Fixed: the version check read an arbitrary row of
  `dbo.ENVDBUpdates`; it now reads the newest.

## [1.3.0] - 2026-10-19

### `kmb-sync`

- Added: the status page shows each meter's clock offset from the server,
  as measured by its clock job, and warns when it is over 60 s. Meters
  without a clock job say so. After `adjust_above` sets a meter's clock,
  the offset shows 0 instead of the value from before.

## [1.2.1] - 2026-10-19

### `kmb-envis`

- Fixed: opening the TCP connection to the database had no time limit.
  When the host dropped packets, each connect waited about 2 minutes for
  the operating system to give up, and since 1.2.0 that could happen once
  per archive, holding the meter link meanwhile. It now gives up after
  15 s; the SQL login after it already had a 15 s limit.

## [1.2.0] - 2026-10-19

### `kmb-sync`

- `check` reads every secret file and prints warnings; the service logs
  the same warnings at start. Warned: the status page open to the network
  only because `status_listen` is not set, and `start_time` with an `every`
  that does not divide 24h.
- A restart no longer resets the byte budget of a job without `anchor` or
  `start_time`: run bytes are recorded while a run goes on, and the budget
  counts the runs of the last period.
- When the meter's `CfgSetTime` changes, its configuration and archive
  descriptions are read again in the same run, instead of up to
  `describe_every` later.
- A dropped ENVIS connection is opened again for the next archive of the
  run, instead of failing the rest of it.
- Fixed: `start_time` jobs with an `every` under about 15 s skipped about
  40 minutes when the clocks went back.
- Faster export and `/api/records`: each record layout is built once.
- Fixed: an archive without a stored ENVIS definition no longer drops the
  database connection, which made every later archive of the run fail.
  Only SQL errors drop it now.
- Fixed: slots the meter leaves out of a reply are read again instead of
  being skipped for good. After two tries without progress they are logged
  as missing, as before.

### `kmb-store`

- `rotate_runs` also deletes download states and actual values from before
  the previous period.
- New `run_traffic`, `job_bytes_since` and `definition::decode_with`;
  removed the unused `Store::object`.

## [1.1.4] - 2026-10-16

### `kmb-sync`

- Status page: "kmb-sync" in the footer links to the project on GitHub.

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

[1.3.1]: https://github.com/Project-Stateful-Zenith/kmb-sync/releases/tag/v1.3.1
[1.3.0]: https://github.com/Project-Stateful-Zenith/kmb-sync/releases/tag/v1.3.0
[1.2.1]: https://github.com/Project-Stateful-Zenith/kmb-sync/releases/tag/v1.2.1
[1.2.0]: https://github.com/Project-Stateful-Zenith/kmb-sync/releases/tag/v1.2.0
[1.1.4]: https://github.com/Project-Stateful-Zenith/kmb-sync/releases/tag/v1.1.4
[1.1.3]: https://github.com/Project-Stateful-Zenith/kmb-sync/releases/tag/v1.1.3
