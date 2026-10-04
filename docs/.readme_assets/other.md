## HTTP

| Route | |
|---|---|
| `GET /health` | no auth |
| `GET /targets` | |
| `GET /targets/{id}/reviews?since=&gone=` | `since`: date or RFC 3339, on first sighting |
| `GET /captures/{sha256}.avif` | only hashes the archive recorded |
| `GET /stats?target=&from=&to=` | per target and day; `Accept: text/csv` for CSV |

## When Maps changes

Selectors live in `src/sources/maps/selectors.rs` and nowhere else. Refresh the fixtures with
`review_archive scan <id> --dump-html tmp/dump` (a failing step also dumps the whole page and a
screenshot there), fix the selectors, and `cargo insta review` the parser snapshots.

The live test is `cargo test --test live -- --ignored`; it needs a browser and the network.
