# Architecture

What the service does and why is in [SPEC.md](SPEC.md). This is where things live.

```text
src/domain/          what an archive is: targets, observations, the ReviewSource port,
  reconcile.rs       a scan against what is stored → new / changed / unchanged / gone / reappeared
  schedule.rs        when a target is next due: interval, jitter, backoff
  relative_date.rs   "il y a 3 semaines" → an estimated timestamp
src/sources/maps/    the public Maps page in Chromium over CDP
  selectors.rs       every assumption about Google's markup, and the in-page scripts
  parse.rs           cards out of HTML (pure; tested on tests/fixtures/)
  browser.rs         the session: consent, the review tab, sorting, the walk, screenshots
src/sources/gbp.rs   the Business Profile API; screenshots matched from Maps cards
src/store/           SQLite (runtime sqlx queries, embedded migrations) + content-addressed PNGs
src/archive.rs       one scan of one target: run row, source, blobs, reconcile, write
src/runner.rs        composition: the source for a target's kind, one browser per pass
src/scheduler.rs     the `serve` loop
src/http.rs          the read-only API
src/export.rs        manifest.json + PNGs, as a directory or a .zip
```

## Invariants

- **Nothing is ever written to Google.** Sources read. There is no stealth, no fingerprint
  masking, no proxy or account rotation, no CAPTCHA solving. When Google does not serve the
  full page — the "unusual traffic" page, or its "limited view" without reviews — the run
  fails and the error says which.
- **History is append-only.** Reviews and captures are never deleted; `review_versions` gets a
  row per distinct content, the first sighting included. `gone_at` is set and cleared, never a
  deletion.
- **`gone` is only concluded where the scan looked.** `gbp` lists everything, so absent means
  gone — unless the API returned fewer reviews than its own `totalReviewCount`. A `maps` walk is
  newest first and stops early; it only judges reviews whose *earliest* possible date (the
  estimate less one unit of its phrase: "a month ago" spans a month) is no earlier than the
  latest possible date of the last card it read. That needs the list to have visibly re-sorted
  to newest and its dates to run newest first; otherwise the run judges nothing. A walk judges
  everything only when it read as many cards as the list's histogram counts — an idle feed
  alone may be a stalled lazy load. A "complete" scan that lists nothing, against an archive
  with live reviews, is taken as a broken response: nothing is marked gone, the run is
  `partial`.
- **One process per browser profile.** A lock file in the profile says so; holding it means
  any Chromium `Singleton*` files there are stale (a crash, a pod with a new hostname) and are
  removed. A second `scan` while `serve` holds the profile fails with that reason.
- **Markup knowledge lives in `selectors.rs`.** A Maps change is fixed there, against fixtures
  refreshed with `scan --dump-html`, and checked by `cargo insta review`.
- **The domain has no I/O.** Time comes in as an argument or through `archive::Clock`; jitter is
  derived from the target and its last run, so asking twice gives one answer.
- **A capture is the review as it first appeared.** Cards are screenshotted when new (or while
  `capture_pending`), after "More" is expanded. The PNG carries its provenance in `tEXt` chunks
  and is stored under its SHA-256.
