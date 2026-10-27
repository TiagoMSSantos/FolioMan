# Sweep protocol

How a knob or factor question is run and recorded here. The aim is that a result can be re-read months
later and that a null result is as findable as a win. Receipts like `(#1184)` are the project's change
ids; every shipped or refused knob keeps its receipt in a comment beside it in `tests/ci-settings.yaml`.

## 1. Pre-register

Before any run, write down in the knob's comment block in `tests/ci-settings.yaml`:

- the question, in one sentence, and the knob or arm that asks it;
- the bar: which rows must move, by how much, on which legs (for example "mean and median each +0.2 or
  better on both lanes"). Fix it before you see a number;
- what happens on each outcome: ship, refuse, or re-ask only on a new ruler.

A knob that is off by default and only logs is a "shadow". It ships nothing and is graded later on the
forward record (`track`), never on the same backtest that suggested it.

## 2. Run the control and the arm on the same commit

Use the `Probe` workflow (`.github/workflows/probe.yml`, manual dispatch). Inputs:

- `years`: horizons, one run each (default `12 8`; the 20 year leg takes `20`).
- `args`: what follows the horizon (default `universe fund pit`; the survivorship legs are `pit dead` and
  `stress`).
- `knobs`: `key=value` overrides for this run only, numbers only.
- `extra`: extra `growth_fund_extra` terms for this run only.

Dispatch the control (no `knobs`) and the arm (with `knobs`) on the same commit, together. There is no
concurrency group, so they run in parallel. A control from an earlier day is stale: the data pool drifts,
and a knob measured against yesterday's control is measured against yesterday. Never compare an arm with
the fixture run either; the fixture is frozen data and answers a different question.

Never dispatch two Tiingo fill runs at once, and never use a fill run as an arm.

## 3. Check the run before grading it

- Read the log with `gh run view <id> --log` within two days. The Pages cleanup job deletes finished runs
  two days after they end, and a batch of more than 20 runs loses its oldest logs.
- Read `windows scored` on every horizon. If the arm and the control score different window counts, the
  knob voided windows and the comparison is not like for like.
- A run with a missing feed (a Wikidata blip, a cold cache) moves the pool. Check that the cache steps
  hit before reading any difference as the knob's.
- `FOLIOMAN_OFFLINE=1` cuts the window count sharply. It belongs to the frozen fixture, not to a probe.

## 4. Grade

- Grade on the row the knob feeds, and read the paired column against its peers, not a bare sign. For the
  live book that is the `S+inter never-sell` DCA lane beside `SIZED`; for gates it is the passer edge and
  lift with both out-of-sample halves.
- Grade on both the point-in-time leg (`pit dead`) and the `stress` leg. A change that helps only the
  survivors is not a result.
- Read mean and median together. A mean that rises while the median falls is a few names, not a shift.
- A rank change inside an equal-weight top-N does not move the book. Grade reorders on rank slices.
- For weight searches use `backtest <set> tune`, which splits samples into a train and a test half. A
  tuned number that was never tested out of sample is an upper bound, not a forecast.

## 5. Ship or refuse

- Ship only if the pre-registered bar is met on every leg it named. A partial pass (2 of 4) is
  inconclusive: the knob stays off and the comment records the numbers.
- A shipped setting moves the goldens. Re-bless them in the same push (see `ARCHITECTURE.md`, section
  Goldens) and read the diff: only the rows the knob touches should move.
- Record the outcome beside the knob: receipt, date, the run ids, the numbers read, and the condition
  for asking again. A refusal with no stated re-open condition gets asked again by the next reader.
- Measured-dead knobs and closed questions are listed in the README under current status. Read that
  list before proposing a new one.
