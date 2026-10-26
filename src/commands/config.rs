//! `config show [--diff]` — (#1119) where the settings come from and what they resolve to: the committed
//! base, the overlay this run found, then the merged YAML every command reads. `--diff` prints only the
//! overlay values that move the base, so a leftover experiment in the private file is one command away.

/// (#669) `#[mutants::skip]` for the reason spelled out on `track::run`; `config::overlay_diff` is the
/// graded half.
#[mutants::skip]
pub fn run(args: Vec<String>) {
    let diff = match args.as_slice() {
        [s] if s == "show" => false,
        [s, d] if s == "show" && d == "--diff" => true,
        _ => {
            eprintln!("usage: folioman config show [--diff]");
            std::process::exit(2);
        }
    };
    match crate::config::show(diff) {
        Ok(out) => print!("{out}"),
        Err(e) => {
            eprintln!("folioman: {e}");
            std::process::exit(1);
        }
    }
}
