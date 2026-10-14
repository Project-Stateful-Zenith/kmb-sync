# kmb-sync

An always-on collector service for KMB/KMP meters (UMD-98 and other
universal-config meters). It polls meters on a schedule, stores their
archive records with the layout that decodes them, resumes after restarts
and outages, and offers a local JSON API, a status page and a command-line
client. It can also write the records into an ENVIS 2.5 MSSQL database.

| Crate | Contents |
| --- | --- |
| `kmb-store` | SQLite store: objects, identities, measurements, archive definitions, records, cursors, runs |
| `kmb-envis` | writes downloads into an ENVIS 2.5 MSSQL database |
| `kmb-sync` | the service (`serve`), one-shot runs (`once`), export and the API client commands |

The protocol comes from the `kmb` and `kmb-tokio` crates of
[zenith-kmb](https://github.com/Project-Stateful-Zenith/zenith-kmb), which
Cargo fetches from GitHub.

## Build

```sh
cargo build --release              # add --features tls for KMB over TLS
cargo test --workspace
```

## Use

```sh
kmb-sync check -c kmb-sync.toml   # validate the configuration
kmb-sync serve -c kmb-sync.toml   # run the service
kmb-sync status                   # ask the running service
kmb-sync --help
```

`deploy/` has an example configuration and a systemd unit.

A device with `tls = true` needs `tls_root` (the root CA public key as an
uncompressed P-256 point in hex, `04` followed by X and Y), `tls_pins`
(SHA-256 fingerprints of accepted meter certificates), or both. No root
key is built in.

## Disclaimer

This project is an independent implementation, made for interoperability
with meters and databases its users own or operate. The code was written
from a separately prepared specification of the protocol and of the
database layout. No vendor source code is contained in or was copied into
this repository, and no vendor keys, certificates or product tables are
included.

This project is not affiliated with, endorsed by or supported by the
manufacturers of the meters or of ENVIS. All product names and trademarks
belong to their respective owners and are used only to identify the
devices and software this project works with.

The software is provided "as is", without warranty of any kind. Use it at
your own risk, especially against production databases.

## License

Apache-2.0, see [LICENSE](LICENSE).
Copyright 2026 Project-Stateful-Zenith Contributors.
