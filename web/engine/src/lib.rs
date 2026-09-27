use wasm_bindgen::prelude::*;

/// `folioman::web::screen` for the page: `overlay` is the uploaded settings.yaml, `universe` the
/// published universe.json, and the answer is a data.json payload.
#[wasm_bindgen]
pub fn screen(overlay: &str, universe: &str) -> Result<String, JsError> {
    folioman::web::screen(overlay, universe).map_err(|e| JsError::new(&e))
}
