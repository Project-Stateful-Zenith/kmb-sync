# Changelog

All notable changes to `kmb-sync`, `kmb-store` and `kmb-envis`. The crates
share one version, which started at 1.1.3, the version of
[zenith-kmb](https://github.com/Project-Stateful-Zenith/zenith-kmb) they were
first released with. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning 2.0.0](https://semver.org/spec/v2.0.0.html).

## [1.3.5] - 2026-11-22

### `kmb-sync`

- Changed: durations in the configuration are parsed with jiff instead of
  humantime, which is unmaintained (RUSTSEC-2026-0542). The documented
  units `ms`, `s`, `m`, `h` and `d`, combinations such as `"1h 30m"` or
  `"1h30m"`, and `w` parse to the same values as before. Months and years
  (`"1M"`, `"1y"`), which were never documented and which humantime read as
  30.44 and 365.25 days, are now refused by `kmb-sync check` and at start;
  write them in days instead.
- Correction: other forms humantime took are refused too: a bare `"0"`
  (write `"0s"`), units out of order or repeated (`"30m 1h"`, `"1s 1s"`),
  fractional days or weeks (`"1.5d"`), digit groups (`"1 000s"`) and values
  above about 20,000 years. ISO 8601 forms such as `"PT15M"` are newly
  accepted.
- `kmb-sync check` prints a job interval of a day or more in hours, such
  as `every 24h` instead of `every 1day`.

## [1.3.4] - 2026-11-22

### Security

- The 1.3.3 entry said the dependencies were past RUSTSEC-2026-0346 to
  0349, 0382, 0385 and 0386. That was true of zenith-kmb's lockfile, not
  of this one. 1.3.3 did fix 0382 (rustls-webpki) and 0386 (tokio), but
  still locked rustls 0.23.45 (0346 to 0349), ctutils 0.4.3 (0385),
  webpki-roots 1.0.9 (0381), libsqlite3-sys 0.38.2 (0459) and rustix 1.1.5
  (0486). This release fixes all of them.
- rustls 0.23.46 (RUSTSEC-2026-0346 to 0349), used for the ENVIS
  connection and for KMB over TLS.
- ctutils 0.4.8 (RUSTSEC-2026-0385), webpki-roots 1.0.12
  (RUSTSEC-2026-0381) and rustix 1.1.6 (RUSTSEC-2026-0486).
- rusqlite 0.43 with libsqlite3-sys 0.40, whose bundled SQLite is past
  CVE-2026-119375 (RUSTSEC-2026-0459). The local store and its migrations
  are unchanged.
- CI runs `cargo audit` on pushes to `main`, on pull requests and weekly.
  It fails on vulnerabilities; unmaintained crates only warn.

### Build

- The minimum Rust version is now 1.100 (was 1.88). rusqlite 0.41 and
  later need at least 1.89; the project follows the current stable.

## [1.3.3] - 2026-11-22

### Dependencies

- Correction: the advisory list below is wrong for this release; see 1.3.4
  for what 1.3.3 left open. The CP1252 note is incomplete: see the
  correction after it.
- `kmb` and `kmb-tokio` 1.2.1 (from 1.1.3), with their security fixes:
  replies must answer the request, archive downloads and replies are
  bounded, each reply frame has a time limit, and rustls-webpki 0.103.17
  and tokio 1.54.1 are past RUSTSEC-2026-0346 to 0349, 0382, 0385 and 0386.
  A late reply after a timeout can no longer be taken for the next
  request's answer.
- Names read from the meter (notebook, identify, archive definitions) are
  now decoded as CP1252. A name with a non-ASCII character such as `ä` used
  to read as `�` and now reads correctly. With ENVIS, the station and
  measurement names come from the notebook, so such a meter is written to
  a new ENVIS object under the corrected name; ASCII names are unchanged.
- Correction: LB feeder measurement names (`<module>.<feeder>`) are decoded
  the same way and change too. Only CP1252 bytes read correctly now; a name
  the meter stores as UTF-8 read correctly before and now reads as
  `AbgÃ¤nge`, and other code pages such as CP1250 read wrongly in both.
  Every changed name gets a new ENVIS object or measurement, whose archive
  is downloaded again from `start_from`. kmb-sync finds ENVIS objects by
  name, so renaming the old ones to the new name before upgrading should
  keep one history; this has not been tested against ENVIS.

### `kmb-sync`

- Fixed: after a request to the meter timed out, every later archive of the
  same run failed with "a reply timed out and may still arrive; reconnect".
  The run now reconnects before the next archive, checks that the same
  meter answers, and goes on.

## [1.3.2] - 2026-11-09

### `kmb-envis`

- Fixed: when all feeders of a UMD-98LB were new to ENVIS (for example
  after an MMI module was added and the feeders were renumbered), the
  definition fallback could pick a definition without any feeder props,
  or one with other field lengths. The LB archive then failed with
  `row of N bytes, the definition has M`, or would have stored master
  values only. A feeder definition now needs at least one LocalBus prop,
  and its master props must have the meter's field lengths.

### `kmb-sync`

- Fixed: a feeder row that does not match its definition's length is now
  reported as "definition does not fit the LB record" and rolled back,
  instead of a `RowLength` error from the database layer.

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

[1.3.5]: https://github.com/Project-Stateful-Zenith/kmb-sync/releases/tag/v1.3.5
[1.3.4]: https://github.com/Project-Stateful-Zenith/kmb-sync/releases/tag/v1.3.4
[1.3.3]: https://github.com/Project-Stateful-Zenith/kmb-sync/releases/tag/v1.3.3
[1.3.2]: https://github.com/Project-Stateful-Zenith/kmb-sync/releases/tag/v1.3.2
[1.3.1]: https://github.com/Project-Stateful-Zenith/kmb-sync/releases/tag/v1.3.1
[1.3.0]: https://github.com/Project-Stateful-Zenith/kmb-sync/releases/tag/v1.3.0
[1.2.1]: https://github.com/Project-Stateful-Zenith/kmb-sync/releases/tag/v1.2.1
[1.2.0]: https://github.com/Project-Stateful-Zenith/kmb-sync/releases/tag/v1.2.0
[1.1.4]: https://github.com/Project-Stateful-Zenith/kmb-sync/releases/tag/v1.1.4
[1.1.3]: https://github.com/Project-Stateful-Zenith/kmb-sync/releases/tag/v1.1.3
