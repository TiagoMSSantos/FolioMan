//! `explain TICKER...` — (#1120) `screen --explain` without the fetch: each name's gates and SCORE
//! arithmetic off the pool the last `screen` saved (`.screen_universe.json`, the page engine's input),
//! under the settings that run ranked on. A ticker, another venue's line of a pool fund, or part of a
//! name matches, as in the page's "why isn't X in?" box.

/// (#669) `#[mutants::skip]` for the reason spelled out on `track::run`; `web::explain` is the graded half.
#[mutants::skip]
pub fn run(args: Vec<String>) {
    if args.is_empty() {
        eprintln!("usage: folioman explain TICKER...");
        std::process::exit(2);
    }
    let path = crate::config::data_path(".screen_universe.json");
    let Ok(pool) = std::fs::read_to_string(&path) else {
        eprintln!("folioman: no saved pool at {}: run `folioman screen` first", path.display());
        std::process::exit(1);
    };
    match args.iter().map(|t| crate::web::explain("", &pool, t)).collect::<Result<Vec<_>, _>>() {
        Ok(blocks) => println!("{}", blocks.join("\n\n")),
        Err(e) => {
            eprintln!("folioman: {e}");
            std::process::exit(1);
        }
    }
}
