//! (#538) Buy heuristics that run BESIDE the 20y one in `picks`. Each owns its config section and
//! reads `buy_heuristic` only through its own resolved copy, so none of them can move a lane, BUY NOW
//! or the backtest.
//!
//! Adding one: a new file here with plain fns and its own tests, its own top-level config section that
//! defaults to off, and its own page table. It reads quotes or charts and never writes a score the
//! lanes read, so the 20y goldens stay byte-identical.

pub mod intraday;
pub mod social;
