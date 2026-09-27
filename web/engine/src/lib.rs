use wasm_bindgen::prelude::*;

/// `folioman::web::screen` for the page: `overlay` is the uploaded settings.yaml, `universe` the
/// published universe.json, and the answer is a data.json payload.
#[wasm_bindgen]
pub fn screen(overlay: &str, universe: &str) -> Result<String, JsError> {
    folioman::web::screen(overlay, universe).map_err(|e| JsError::new(&e))
}

/// (#403) `folioman::web::explain`: why `query` (a ticker, another venue's line, or part of a name) is
/// or is not in the tables that `overlay` ranks.
#[wasm_bindgen]
pub fn explain(overlay: &str, universe: &str, query: &str) -> Result<String, JsError> {
    folioman::web::explain(overlay, universe, query).map_err(|e| JsError::new(&e))
}
