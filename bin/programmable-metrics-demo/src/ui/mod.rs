//! The UI: the page, the language reference, their scripts, and the stylesheet they share, embedded in the binary.
//! There is no build step and nothing is loaded from elsewhere.

use axum::{http::header, response::IntoResponse};

const INDEX_HTML: &str = include_str!("index.html");
const APP_JS: &str = include_str!("app.js");
const APP_CSS: &str = include_str!("app.css");
const REFERENCE_HTML: &str = include_str!("reference.html");
const REFERENCE_JS: &str = include_str!("reference.js");

/// Serves the page.
pub async fn index() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        INDEX_HTML,
    )
}

/// Serves the page's script.
pub async fn script() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        APP_JS,
    )
}

/// Serves the page's stylesheet.
pub async fn stylesheet() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        APP_CSS,
    )
}

/// Serves the language reference.
pub async fn reference() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        REFERENCE_HTML,
    )
}

/// Serves the language reference's script.
pub async fn reference_script() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        REFERENCE_JS,
    )
}
