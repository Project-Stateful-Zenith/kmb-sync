# Configuring kmb-sync

kmb-sync reads one TOML file. It has four parts:

| Section | Count | What it is |
| --- | --- | --- |
| [`[service]`](#service) | one | the service itself: its database, API, status page |
| [`[[device]]`](#device) | one per meter | how to reach a meter |
| [`[[job]]`](#job) | any number | what to fetch from a meter, and how often |
| [`[envis]`](#envis) | optional | also write archive records into an ENVIS 2.5 database |

## Examples

Every example uses made-up names and documentation addresses (`192.0.2.x`, `198.51.100.x`, `203.0.113.x`).
Copy the closest one and adapt it.

| File | Shows |
| --- | --- |
| [`examples/minimal.toml`](examples/minimal.toml) | one meter: archives every 15 minutes, live values every minute, a clock check every hour |
| [`examples/multi-station.toml`](examples/multi-station.toml) | several meters, bus addresses, object paths, `archives = "all"`, `start_from`, a nightly job at a local time, different byte budgets |
| [`examples/secure-meter.toml`](examples/secure-meter.toml) | a meter over TLS (root key and pins), a meter with an encrypted link (PSK), user logins, longer timeouts, automatic clock adjustment |
| [`examples/envis.toml`](examples/envis.toml) | writing archive records into an ENVIS database |
| [`examples/remote-api.toml`](examples/remote-api.toml) | the control API and the status page's JSON reachable from other hosts, with tokens |

## Install and check

```sh
sudo install -m 0755 target/release/kmb-sync /usr/local/bin/
sudo install -d -m 0750 /etc/kmb-sync
sudo cp deploy/examples/minimal.toml /etc/kmb-sync/config.toml
kmb-sync check -c /etc/kmb-sync/config.toml
```

`kmb-sync check` validates the file without contacting any meter and prints
what it understood: every device and job, and per meter how much of the
time its jobs may hold the link and how many archive bytes per day the
limits allow. Fix what it reports before starting the service.

[`kmb-sync.service`](kmb-sync.service) is a hardened systemd unit that runs
`kmb-sync serve --config /etc/kmb-sync/config.toml` as user `kmb-sync`, with
its state in `/var/lib/kmb-sync`:

```sh
sudo useradd --system --home /var/lib/kmb-sync --shell /usr/sbin/nologin kmb-sync
sudo cp deploy/kmb-sync.service /etc/systemd/system/
sudo systemctl enable --now kmb-sync
```

## Value formats

| Kind | Format | Examples |
| --- | --- | --- |
| Duration | number and unit (`ms`, `s`, `m`, `h`, `d`), combinable | `"20s"`, `"15m"`, `"1h 30m"`, `"1d"` |
| Byte size | number and unit: `B`, `kB`, `KiB`, `MB`, `MiB`, `GB`, `GiB`; or a plain number of bytes | `"512KiB"`, `"2MiB"`, `1048576` |
| Time | RFC 3339 with a zone | `"2026-01-01T00:00:00Z"`, `"2026-01-01T01:00:00+01:00"` |
| Time of day | `HH:MM` or `HH:MM:SS`, local time | `"02:30"` |
| Address | `IP:port` | `"127.0.0.1:8650"` |
| Name | letters, digits, `-`, `_` and `.` | `"riverside-sub"` |

Secrets never go into the configuration. Options ending in `_file` name a
file that holds the secret; give such files mode `0600` or `0640` and make
them readable by the service user only.

## `[service]`

| Option | Default | Meaning |
| --- | --- | --- |
| `database` | required | path of the SQLite database for records, cursors and the run log; created on first start |
| `listen` | off | address of the control API (pause, resume, run jobs, query records). A non-loopback address requires `api_token_file` |
| `api_token_file` | none | file with the bearer token the control API requires |
| `status_listen` | `"0.0.0.0:8651"` | address of the read-only status page; `"off"` disables it. Must not share a port with `listen` |
| `status_api` | `false` | also serve the status page's data as JSON at `/api/stations`; requires `status_api_token_file` |
| `status_api_token_file` | none | file with the bearer token for `/api/stations` |
| `max_concurrency` | `4` | jobs that may run at the same time, across all meters. Jobs of one meter never run at the same time |
| `heartbeat` | `"5m"` | how often the service records that it is alive; after a restart it reports how long it was down |
| `describe_every` | `"6h"` | how often a meter's self-description (its archives and fields) is read again, besides when its identity changes |

The status page itself never needs a token: it is read only and shows no
secrets. Put it behind a firewall or reverse proxy if the station list
should not be visible.

## `[[device]]`

| Option | Default | Meaning |
| --- | --- | --- |
| `name` | required | unique name, used by jobs and shown on the status page |
| `host` | required | IP address or host name of the meter or its gateway |
| `port` | `2101`, or `7101` with `tls` | TCP port |
| `address` | `1` | bus address of the meter behind the host |
| `path` | `[name]` | where the meter's measurements sit in the object tree, e.g. `["Network", "Riverside", "Riverside Substation"]` |
| `credentials_file` | none | file with the meter login: user on the first line, password on the second |
| `psk_file` | none | file with the meter's pre-shared key for encrypted links, Base64 of 32 bytes |
| `tls` | `false` | connect over TLS; needs a build with `--features tls` |
| `tls_root` | none | public key of the root CA that signed the meter's certificate: an uncompressed P-256 point in hex (`04`, then X and Y) |
| `tls_pins` | `[]` | SHA-256 fingerprints of meter certificates to accept, hex with or without `:` |
| `connect_timeout` | `"5s"` | time allowed to open the connection |
| `byte_timeout` | `"10s"` | time allowed between bytes of a reply |
| `packed` | `6800` | packet size written into the ENVIS device URL (only with `[envis]`) |
| `envis_url` | built from host, port, address and `packed` | the device URL stored in ENVIS, when it must match an existing one exactly |

With `tls = true`, set `tls_root`, `tls_pins` or both. A certificate is
accepted when it is pinned or when its chain leads to `tls_root`. No root
key is built in.

## `[[job]]`

Settings every job has:

| Option | Default | Meaning |
| --- | --- | --- |
| `device` | required | name of the device the job talks to |
| `kind` | required | `"archive"`, `"values"` or `"clock"` (below) |
| `name` | `<device>-<kind>-<n>` | unique name, used by `kmb-sync run`, `pause`, `resume` and in the run log |
| `every` | `"1m"` | how often the job runs |
| `anchor` | none | align runs to this time plus multiples of `every`, e.g. on the quarter hour |
| `start_time` | none | align runs to this local time of day (instead of `anchor`, with `every` of at most `24h`); the zone is `[envis].time_zone` or the host's |
| `timeout` | `"10m"` | maximum time of one run. It counts only while the run holds the meter, so jobs waiting for the same meter do not use up their time |
| `enabled` | `true` | `false` keeps the job configured but never runs it |
| `measurement` | `"default"` | name of the measurement the records are stored under, when one meter feeds several |

Without `anchor` or `start_time`, the first run starts when the service
starts and later runs follow every `every`.

### `kind = "archive"`

Downloads archive records. Each run continues behind the newest record
already stored, so only new records cross the link.

| Option | Default | Meaning |
| --- | --- | --- |
| `archives` | required | `"all"` (every archive the meter offers), or a list of names or numbers |
| `start_from` | `"all"` | for the first download: `"all"` takes everything the meter still holds; a time takes only records from then on |
| `max_size` | no limit | byte budget per `every` period, retries and manual runs included. A run that reaches it ends as "limited", and the rest follows next period |
| `max_mib` | no limit | the same budget in MiB; set `max_size` or `max_mib`, not both |

Archive names (case does not matter): `Main` (0), `SProfile` (1),
`MProfile` (2), `Log` (3), `MainPQ` (4), `PQevents` (5), `Elmeter` (6),
`GO` (11), `Modbus Master` (13), `Histogram` (14), `Unbalance` (15),
`Event Log` (16), `Trend` (17), `Avg` (18), `H2M` (24), `LB` (25), `LB2` (26).
Which of them a meter has depends on the meter: `kmb-sync archives -c CONFIG DEVICE`
lists what it offers.

Bytes are counted on the socket, TLS included, and each read is cut to fit
the budget. Size the budget to the link: at 8 kB/s, a 15-minute period
carries at most about 7 MB.

### `kind = "values"`

Reads one set of current values and stores it.

| Option | Default | Meaning |
| --- | --- | --- |
| `set` | required | `"ActData"` (live values), `"AvgData"`, `"MaxData"`, `"MinData"`, `"Edata"` (energy) or `"Status"` |

Values jobs run at most every 5 seconds.

### `kind = "clock"`

Compares the meter clock with the host clock and records the offset.

| Option | Default | Meaning |
| --- | --- | --- |
| `adjust_above` | none | when the offset is larger, set the meter clock to the host clock. Without it the job only reads |

> [!CAUTION]
> `adjust_above` is the only option that writes to a meter. Use it only
> when the host clock is reliable (NTP), as the meter's records take their
> time from its clock.

## `[envis]`

With this section, archive records are also written into an ENVIS 2.5
database, so the ENVIS desktop application shows them. Without it, records
stay in kmb-sync's own database only.

| Option | Default | Meaning |
| --- | --- | --- |
| `host` | required | SQL Server host |
| `port` | `1433` | SQL Server port |
| `database` | `"pqplusdb"` | database name |
| `user` | required | SQL login |
| `password_file` | required | file with that login's password |
| `trust_cert` | `false` | accept the server's certificate without checking it, for self-signed servers |
| `time_zone` | the host's | time zone the archive periods are cut in (IANA name, e.g. `"UTC"`) |
| `allow_new_definitions` | `false` | store a record layout ENVIS has not seen before. Off, such archives are refused until ENVIS has stored that meter once |

The login needs to read and write the archive tables only; it should not
be able to change the schema. GO, Modbus Master, Avg, H2M and LB2 archives
are not written to ENVIS; they are still stored locally.

## Secret files

| File | Content |
| --- | --- |
| `credentials_file` | two lines: user, password |
| `psk_file` | one line: Base64 of the 32-byte key |
| `api_token_file`, `status_api_token_file` | one line: the token clients send as `Authorization: Bearer <token>` |
| `password_file` | one line: the SQL password |

Trailing line breaks are ignored.
