# review_archive
![Minimum Supported Rust Version](https://img.shields.io/badge/nightly-1.100+-ab6000.svg)
![Lines Of Code](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/valeratrades/b48e6f02c61942200e7d1e3eeabf9bcb/raw/review_archive-loc.json)
<br>
[<img alt="ci errors" src="https://img.shields.io/github/actions/workflow/status/Service-Arb/review_archive/errors.yml?branch=main&style=for-the-badge&style=flat-square&label=errors&labelColor=420d09" height="20">](https://github.com/Service-Arb/review_archive/actions?query=branch%3Amain) <!--NB: Won't find it if repo is private-->
[<img alt="ci warnings" src="https://img.shields.io/github/actions/workflow/status/Service-Arb/review_archive/warnings.yml?branch=main&style=for-the-badge&style=flat-square&label=warnings&labelColor=d16002" height="20">](https://github.com/Service-Arb/review_archive/actions?query=branch%3Amain) <!--NB: Won't find it if repo is private-->

An archive of public place reviews. `review_archive` watches a list of places, and every review it
has not seen before is stored twice: as structured data in SQLite, and as a PNG screenshot of the
review card as it first appeared. Edits are kept as history, never overwritten; a review that stops
being listed is marked gone, and unmarked if it comes back. Statistics and exports are read from
what was stored.

Two sources. `maps` reads any public place through a plain headless Chromium at a polite rate.
`gbp` reads the places we manage through the official Business Profile API, which is complete and
authoritative, and still takes its screenshots from the public Maps page.

It only reads. Nothing is posted, replied to, reported or appealed, and there is no fingerprint
masking, proxy or account rotation, or CAPTCHA solving: if Google blocks the scanner, the run fails
and says so. See [docs/SPEC.md](docs/SPEC.md) for what it does and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for where.
<!-- markdownlint-disable -->
<details>
<summary>
<h2>Installation</h2>
</summary>

```sh
cargo install --path .
```

</details>
<!-- markdownlint-restore -->

## Usage
```sh
# Everything lives under one data dir: `review_archive.db`, `blobs/` with the PNGs, and the
# browser profile that remembers the consent answer. Set it, the bind address and the defaults in
# a TOML file passed as `--config`; secrets only ever come from the environment.
review_archive --config config.toml target add 'https://www.google.com/maps/place/?q=place_id:ChIJ...' --label cafe --lang fr

# A Maps URL without a place id is resolved through the Places API (needs GOOGLE_MAPS_KEY).
review_archive target add 'https://www.google.com/maps/place/Le+Procope/@48.853,2.338,17z' --interval 12h

# A profile we manage: the review list comes from the Business Profile API
# (GBP_CLIENT_ID, GBP_CLIENT_SECRET, GBP_REFRESH_TOKEN), the screenshots from Maps.
review_archive target add ChIJ... --gbp 1234567890/9876543210

review_archive target list
review_archive scan 1          # one pass now, prints a summary
review_archive scan --all

# Scheduler + HTTP on 127.0.0.1:59110. Every route but /health wants
# `Authorization: Bearer $REVIEW_ARCHIVE_TOKEN`.
review_archive serve

# PNGs + manifest.json, to a directory or a .zip
review_archive export --target 1 --since 2026-01-01 --out cafe.zip
```

```toml
# config.toml — every key is optional
data_dir = "./data"
bind = "127.0.0.1:59110"

[browser]
executable = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
no_sandbox = false

[defaults]
lang = "en"
interval = "6h"
max_reviews_per_scan = 200
max_reviews_initial = 2000
```

## HTTP

| Route | |
|---|---|
| `GET /health` | no auth |
| `GET /targets` | |
| `GET /targets/{id}/reviews?since=&gone=` | `since`: date or RFC 3339, on first sighting |
| `GET /captures/{sha256}.png` | only hashes the archive recorded |
| `GET /stats?target=&from=&to=` | per target and day; `Accept: text/csv` for CSV |

## When Maps changes

Selectors live in `src/sources/maps/selectors.rs` and nowhere else. Refresh the fixtures with
`review_archive scan <id> --dump-html tmp/dump` (a failing step also dumps the whole page and a
screenshot there), fix the selectors, and `cargo insta review` the parser snapshots.

The live test is `cargo test --test live -- --ignored`; it needs a browser and the network.


<br>

<sup>
	This repository follows <a href="https://github.com/valeratrades/.github/tree/master/best_practices">my best practices</a> and <a href="https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md">Tiger Style</a> (except "proper capitalization for acronyms": (VsrState, not VSRState) and formatting). For project's architecture, see <a href="./docs/ARCHITECTURE.md">ARCHITECTURE.md</a>.
</sup>

#### License

<sup>
	Licensed under <a href="LICENSE">Blue Oak 1.0.0</a>
</sup>

<br>

<sub>
	Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be licensed as above, without any additional terms or conditions.
</sub>

