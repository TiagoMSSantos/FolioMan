# Architecture

A map of where things live and how they fit. The README says what each command does; this says which
file does it. Receipt numbers like `(#1184)` in code comments are the project's change ids, one sequence
across `src/` and `tests/`.

## Layout

One crate, `folioman`: a library (`src/lib.rs`) that the binary (`src/main.rs`, a thin dispatcher) and
the tests share.

- `src/core.rs` - pure logic: types, number formatting, market, trend and inflation maths. No network.
- `src/fetch.rs` - every network call. One shared HTTP client, a concurrency bound plus a pacer, soft
  failure (a dead feed gives `None`, not a panic). Every URL comes from the `urls` section of the config.
- `src/config.rs` - the settings struct, loading and merging (see below), the list of retired knobs.
- `src/picks.rs` - the ranking. `growth_score` is the lane `check` and `screen` print through `render`.
  `buy_score` (the on-sale lane) is a foil the backtest grades against, not a lane the page shows.
- `src/web.rs` - the page engine: re-ranks the published pool under an uploaded `settings.yaml`. Pure,
  no fetch and no filesystem, so it also compiles to wasm.
- `src/commands/` - one file per subcommand: `accounts`, `alert`, `backtest`, `check`, `config`,
  `explain`, `perf`, `report`, `screen`, `sim`, `size`, `track`, `trade`. `mod.rs` holds the shared
  helpers. `backtest.rs` is the research harness; `screen.rs` is the daily run.
- `src/heuristics/` - signals that sit beside the main growth ranking, each with its own config
  section that defaults to off: `intraday.rs`, `social.rs`.
- `src/broker/` - Binance, Trading 212 and Trade Republic clients. Trading is manual and confirm-gated.
- `src/buy-heuristics/` - small JSON tables the ranking reads (aliases, superinvestor list).
- `tools/` - Python helpers that build the index-membership, dead-member and failure archives under
  `tests/`.
- `web/` - the static page (`index.html`, `app.js`, `worker.js`, `style.css`) plus the `.jq` contract
  checks CI runs on the published payload.
- `web/engine/` - a separate wasm32 crate with its own `Cargo.lock`. It compiles `src/lib.rs` for the
  browser (`broker`, `commands` and `fetch` are left out) and runs the same `picks::render` the terminal
  does. `smoke.mjs` is its parity test.
- `tests/` - `cli.rs` (binary end to end), `backtest_fixture.rs` (frozen-data goldens), `network.rs`
  (live-feed drift checks and the backtest gate), `ci-settings.yaml` (the shipped tuning), CSV ponds and
  archives, `fixture/` (the frozen data).

## Config merge order

1. The base is `tests/ci-settings.yaml`, found by walking up from the executable, then from the working
   directory. It is the shipped tuning and the shared record of every knob decision.
2. The overlay is the private `config/settings.yaml` (not committed). Lookup order: the
   `FOLIOMAN_CONFIG` environment variable, then next to or above the executable, then up from the
   working directory.
3. `merge_yaml` lays the overlay over the base key by key (maps merge, scalars and lists replace, an
   empty `key:` replaces nothing) after dropping any key listed in `RETIRED`, so an old private file does
   not fail on a deleted knob.
4. When the overlay path is `ci-settings.yaml` itself (CI) nothing is merged twice.
5. `load` exits 1 if the file cannot be read or no buy heuristic resolves. At start-up a tripwire prints
   any knob that has drifted off its validated value.

`folioman config show --diff` lists the overlay values that differ from the base. The wasm engine
installs the same merge with the uploaded file as the overlay. Data files (caches, journal) are
anchored with `config::data_path` at the grandparent of the settings file: the repo root for
`config/settings.yaml`, and `tests/fixture/` for the fixture config, which is how the fixture gets its
own data root.

## Goldens

`tests/fixture/*.golden` pin whole `backtest` reports. They run the real binary over a frozen cache
(`tests/fixture/.long_history_cache.json`, plus `.sp500_history.json` and `.sec_cache/`) with
`FOLIOMAN_OFFLINE=1`, so nothing can be fetched. The universe is the `tickers:` list in
`tests/fixture/config/settings.yaml`, and a test checks that list equals the cache's keys. The config is
deliberately thin: it overlays only `tickers:` on `ci-settings.yaml`, so moving a shipped knob moves the
goldens.

Re-bless after an intended change:

- Locally: `FOLIOMAN_BLESS=1 cargo test --release --test backtest_fixture -- --nocapture`.
- On CI: run the `Probe` workflow with `bless` ticked. It runs the same test with
  `FOLIOMAN_BLESS=1 FOLIOMAN_CONFIG=tests/ci-settings.yaml` and uploads the goldens as an artifact. Commit
  them in the same push as the change that moved them.
- Regenerate the frozen cache from a warm real one: `cargo test --release --test backtest_fixture --
  --ignored regen_backtest_fixture`.

## Forward record

`screen` appends one line a day to `.screen_snapshots.jsonl` at the repo root; the file is committed on
`main`. The `record` job in `.github/workflows/pages.yml` commits that line (and `.intraday_since`) back,
and appends only a line newer than the last one. `track` grades the forward sections from this file. It
is the out-of-sample record: the backtest tunes on the past, this file only grows with time.

## CI shape

`.github/workflows/ci.yml` runs the checks (unit and integration tests, clippy, audit, `cargo deny`, MSRV
build, coverage, mutation grading, the backtest gate). `automerge.yml` lists the jobs it waits on and
`.github/scripts/automerge-need.py` checks that list against the workflow files. `pages.yml` builds and
publishes the page. `probe.yml` runs a backtest by hand for a knob or factor question; see
`docs/SWEEP.md`. Every job is pinned to `ubuntu-24.04`.
