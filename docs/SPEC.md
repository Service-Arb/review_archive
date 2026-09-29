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

- Chromium through `browser_manipulation` (Playwright protocol, patchright driver). One
  browser process, targets scanned sequentially.
- URL: `https://www.google.com/maps/place/?q=place_id:<PLACE_ID>&hl=<lang>`.
- EU consent interstitial (`consent.google.com`): click the reject-all button;
  persist the resulting cookies in the profile dir so it is not shown again.
- Open the Reviews tab, sort by *Newest* (when Google answers with its sign-in dialog
  instead, read what it shows in its own order and mark the run `partial`), scroll the
  feed. Stop when a full screen of
  cards is already known (cards still without a screenshot do not count, and the walk
  goes on until it has read or passed every one of those), or at `max_reviews_per_scan`
  (default 200).
  First scan of a target walks the whole list (bounded by
  `max_reviews_initial`, default 2000), whatever ad-hoc captures stored before it.
- A walk cut short before reaching archived cards — by its limit, or by the page failing
  under it — records its last card (`targets.cut_after`). The next scan reads past that
  card before a screen of known cards may end it, with `max_reviews_initial` as its
  limit, so what lay below is not skipped for good. A failure after cards were read
  keeps them (the run is `partial`); a block by Google still fails the run.
- Per card (`[data-review-id]`): click *More* to expand, extract review id,
  author name, author profile URL, star rating (from `aria-label`), relative
  date text, text, owner reply, photo count; element screenshot → PNG.
- Selectors live in one module with HTML fixtures under `tests/fixtures/` and
  parser tests on them — the page changes, and the fix must be one file.

### `gbp` — profiles we manage (official API)

- Google Business Profile API, `GET https://mybusiness.googleapis.com/v4/accounts/{account}/locations/{location}/reviews`
  (`pageSize=50`, paginate with `nextPageToken`; a token seen twice, or more than 1000
  pages, fails the run). `account` and `location` are numbers. OAuth2 refresh-token flow;
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

- `targets(id, label, kind[maps|gbp], place_id, gbp_account, gbp_location, lang, interval_secs, enabled, created_at, cut_after)`
  — `lang` is a language tag (`fr`, `pt-BR`); anything else is refused.
- `reviews(id, target_id, source_review_id, author, author_url, rating, text, reply, published_raw, published_est, first_seen, last_seen, gone_at, content_hash, capture_pending)`
  — unique `(target_id, source_review_id)`.
  — `published_raw` is the date text `published_est` was estimated from; a later
  reading ("Edited a day ago") does not replace it.
- `review_versions(review_id, seen_at, content_hash, rating, text, reply)` — a
  row whenever `content_hash` changes; history is never overwritten.
- `captures(id, review_id, captured_at, sha256, width, height, page_url, scanner_version)`
- `runs(id, target_id, started_at, finished_at, status[ok|partial|failed], error, n_seen, n_new, n_changed, n_gone, ad_hoc)`
  — `ad_hoc` marks the runs of ad-hoc captures. A run a stopped process left open is
  failed ("interrupted") on the next start of `serve` — a hand-run `scan` still going at
  that moment included; its run is marked ended again, with its real outcome, when it
  finishes.
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
- Ad-hoc captures run on the same browser but are not the target's schedule: their
  runs neither delay the next scan nor count as failures.
- A scan whose browser profile another process holds (a `scan` beside a running
  `serve`) fails with that reason and records no run.

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
  mean rating, rating histogram. `Accept: text/csv` → CSV. `gone` counts what the runs
  of that day marked gone, whether or not it came back later.

Errors are JSON (`{"error": …}`) with 400 for input the archive cannot use (a body or a
query it cannot read included), 404 for what does not exist, 429 when the job queue is
full, and a bare `internal error` for a 5xx. API requests take 180 s at most (an export as long as it
needs) and 64 are served at once; `/health` and `/openapi.json` are outside both limits.

Config: TOML file (`--config`) for data dir, bind address, defaults (including
`max_queued_jobs`), `[webhooks] allowed_hosts`; secrets only from env.

### HTTP API for other services

Everything the CLI can do is reachable over HTTP, so other services drive the
archive without shelling into its container. JSON in and out; DTOs live in the
library (below) and are shared with the client crate. OpenAPI document at
`GET /openapi.json` (utoipa, as in concierge).

Targets:

- `POST /targets` `{place | maps_url, label?, lang?, interval?, gbp?}` → `201`
  with the target. Same resolution as `target add`.
- `PATCH /targets/{id}` `{label?, lang?, interval?, enabled?}`
- `DELETE /targets/{id}` — disables; archived data is never deleted.
- `GET /targets/{id}` — target with last run, counts, next scheduled scan.

Jobs (one browser, one queue; on-demand jobs go ahead of scheduled scans):

- `POST /targets/{id}/scan` → `202 {job_id}` — scan now.
- `POST /captures` `{place | maps_url, lang?, max_reviews?, review_ids?}` →
  `202 {job_id}` — ad-hoc capture of a place **without** registering a target;
  results are stored under an implicit, disabled target so nothing is lost.
  `?wait=<secs>` (cap 120) blocks and returns the result directly if done.
  `max_reviews` and the number of `review_ids` are at most `max_reviews_initial`.
- The queue holds `max_queued_jobs` (default 20) at most: past that, `429`. Asking again
  for a job already queued (same target, kind and limits) returns its id. After three
  jobs in a row, an overdue scheduled scan goes first.
- `GET /jobs/{id}` → `queued | running | done | failed`, and on `done` the
  reviews with their capture URLs — for a capture of `review_ids`, those of them it
  found.
- `GET /targets/{id}/runs?limit=`

Reviews and captures:

- `GET /reviews/{id}` — review with versions and captures.
- `GET /targets/{id}/export.zip?since=` — same archive as `export`.

Events:

- `POST /webhooks` `{url, events: [review.new, review.changed, review.gone, review.reappeared, run.failed], secret}`,
    `GET /webhooks`, `DELETE /webhooks/{id}`. Deliveries are signed
  (`X-Signature: sha256=<hmac of body>`), retried with backoff, recorded in an
  outbox table so a restart does not drop them.
- A webhook URL is `http(s)`; with `[webhooks] allowed_hosts` empty it may not name a
  loopback, private, link-local, carrier-grade-NAT or otherwise local address, and a
  host name is refused at delivery when it resolves only to such addresses. A non-empty
  `allowed_hosts` is the whole list of hosts webhooks may go to (a service in the same
  cluster, say). Redirects are not followed; a delivery gets 3 s to connect and 10 s in
  all, and a receiver that cannot be reached is not held against the other hooks.

## Library

The crate is usable without the server, inside other systems, at the low level:

- `review_archive_core` — no I/O: domain types, DTOs, reconciliation, the Maps
  card parser and selectors, relative dates. Anyone with their own browser or
  their own HTML can parse and reconcile.
- `review_archive` — the engine: `ReviewSource` trait (public, so other
  platforms plug in), `maps` and `gbp` sources, a `Browser` handle that callers
  can own or share, `Store` (SQLite + blobs), and a facade
  `Archive::open(config)` with `capture_place(..)`, `scan_target(..)`,
    `add_target(..)`, `stats(..)`, `export(..)`. Features: `maps` (the browser, and the
  `maps` and `gbp` sources), `store`.
  A caller can also run `capture_place` with no store at all and get the reviews
  and PNG bytes back in memory.
- `review_archive_server` — binary: CLI, scheduler, HTTP, webhooks. Thin over
  the facade; no logic that the library does not also expose.
- `review_archive_client` — typed async HTTP client over the same DTOs, for
  services that call a running archive instead of embedding it.

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
