# review_archive — spec

Archive of public place reviews: a PNG screenshot of every review as it first
appears, plus structured data for statistics. Google first; the source layer is a
trait so other platforms slot in later.

Replaces a person manually checking places and saving screenshots.

## Scope

In:

- Watch a list of **targets** (places) on an interval (hours, not seconds).
- On each scan: detect reviews not seen before, store their data, take a PNG
  screenshot of the review card, keep edit history, note reviews that are no
  longer listed.
- Statistics and export over what was stored.

Out — do not build, even as an option:

- Posting, editing or replying to anything; any write call to Google.
- Browser fingerprint masking, "stealth" plugins, proxy or account rotation,
  CAPTCHA solving. The scanner is a plain headless Chromium at a polite rate; if
  Google blocks it, the run fails and is reported.
- Automatic submission of anything to Google (appeals, reports).

## Sources

`trait ReviewSource { async fn scan(&self, target: &Target, known: &Known) -> Result<Scan> }`

### `maps` — any public place (headless browser)

- Chromium over CDP (`chromiumoxide`, tokio). One browser process, targets
  scanned sequentially.
- URL: `https://www.google.com/maps/place/?q=place_id:<PLACE_ID>&hl=<lang>`.
- EU consent interstitial (`consent.google.com`): click the reject-all button;
  persist the resulting cookies in the profile dir so it is not shown again.
- Open the Reviews tab, sort by *Newest*, scroll the feed. Stop when a full
  screen of cards is already known, or at `max_reviews_per_scan` (default 200).
  First scan of a target walks the whole list (bounded by
  `max_reviews_initial`, default 2000).
- Per card (`[data-review-id]`): click *More* to expand, extract review id,
  author name, author profile URL, star rating (from `aria-label`), relative
  date text, text, owner reply, photo count; element screenshot → PNG.
- Selectors live in one module with HTML fixtures under `tests/fixtures/` and
  parser tests on them — the page changes, and the fix must be one file.

### `gbp` — profiles we manage (official API)

- Google Business Profile API, `GET https://mybusiness.googleapis.com/v4/accounts/{account}/locations/{location}/reviews`
  (`pageSize=50`, paginate with `nextPageToken`). OAuth2 refresh-token flow;
  `GBP_CLIENT_ID`, `GBP_CLIENT_SECRET`, `GBP_REFRESH_TOKEN` from env.
- The API gives the authoritative, complete list (id, reviewer, starRating,
  comment, createTime, updateTime, reply), so `gone` detection is exact here.
- Screenshots still come from the public Maps page: for each new API review, run
  the `maps` capture on the target's `place_id` and match the card by
  (author name, rating, normalised text prefix). Unmatched → no capture row,
  `capture_pending` stays set, retried on the next scan (Maps lags the API).
- A `gbp` target therefore needs both `gbp_account`/`gbp_location` and
  `place_id`.

### Place id resolution

`target add` accepts a place id or a Google Maps URL. From a URL, parse the id if
present; otherwise resolve via Places API (New) `places:searchText`
(`GOOGLE_MAPS_KEY`, field mask `places.id,places.displayName,places.formattedAddress`),
same as `gmaps_optimal_placement_sources/src/poi.rs`.

## Storage

SQLite (sqlx, runtime `sqlx::query`, embedded migrations) + content-addressed
blob dir. Everything under one data dir (`/data` in the container).

- `targets(id, label, kind[maps|gbp], place_id, gbp_account, gbp_location, lang, interval_secs, enabled, created_at)`
- `reviews(id, target_id, source_review_id, author, author_url, rating, text, reply, published_raw, published_est, first_seen, last_seen, gone_at, content_hash, capture_pending)`
  — unique `(target_id, source_review_id)`.
- `review_versions(review_id, seen_at, content_hash, rating, text, reply)` — a
  row whenever `content_hash` changes; history is never overwritten.
- `captures(id, review_id, captured_at, sha256, width, height, page_url, scanner_version)`
- `runs(id, target_id, started_at, finished_at, status[ok|partial|failed], error, n_seen, n_new, n_changed, n_gone)`
- Blobs: `<data>/blobs/<sha256[0..2]>/<sha256>.png`. PNG gets `tEXt` chunks:
  capture time (UTC, RFC 3339), page URL, target label, source review id.

`gone`: set when a review is absent from a scan that covered its position
(`gbp`: always complete; `maps`: only if the scan walked past its
`published_est`). Cleared if it reappears. Reviews and captures are never
deleted.

## Scheduling

- Per-target `interval_secs`, default 6 h, minimum 1 h, ±10 % jitter.
- One scan at a time; 5–15 s random pause between targets.
- Failure → `runs.status=failed` with the error, exponential backoff for that
  target (cap: 24 h), other targets continue.

## Interfaces

CLI (`clap`):

- `review_archive target add <place-id|maps-url> [--label] [--lang fr] [--interval 6h] [--gbp <account>/<location>]`
- `review_archive target list | disable <id> | enable <id>`
- `review_archive scan <target-id|--all>` — one pass now, prints a summary.
- `review_archive serve` — scheduler + HTTP.
- `review_archive export --target <id> [--since <date>] --out <dir|file.zip>` — PNGs + `manifest.json`.

HTTP (axum), bearer token from `REVIEW_ARCHIVE_TOKEN`, binds `127.0.0.1` unless
configured:

- `GET /health` (no auth)
- `GET /targets`
- `GET /targets/{id}/reviews?since=&gone=`
- `GET /captures/{sha256}.png`
- `GET /stats?target=&from=&to=` — per target and per day: new, changed, gone,
  mean rating, rating histogram. `Accept: text/csv` → CSV.

Config: TOML file (`--config`) for data dir, bind address, defaults; secrets only
from env.

## Repo conventions

Follow `gmaps_optimal_placement` / `aquafix`: edition 2024, nightly via
`v_flakes` (`github:valeratrades/v_flakes?ref=v1.6`), `eyre` + `color-eyre`, the
same `rustfmt.toml`, `v_flakes.github` generated workflows, container via
`v_flakes.container.implement` (mount `/data`, `healthPath = "/health"`,
`chromium` in the image) and `github.containerRelease = { registry = "ghcr.io/service-arb"; }`.
A `v*` tag is a release — do not tag.

## Tests

- Parser tests on saved Maps HTML fixtures (insta snapshots).
- Repository tests on a temp SQLite: new / changed / gone / reappeared.
- Scheduler: interval, jitter bounds, backoff.
- `gbp` client against a stub HTTP server (pagination, token refresh).
- Live scan of one real place — `#[ignore]`, run by hand.
