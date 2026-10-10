//! (#538) Buy heuristics that run BESIDE the 20y one in `picks`. Each owns its config section and
//! reads `buy_heuristic` only through its own resolved copy, so none of them can move a lane, BUY NOW
//! or the backtest.

pub mod social;
