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
