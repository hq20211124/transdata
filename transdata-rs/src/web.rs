pub const INDEX_HTML: &str = include_str!("../web/index.html");
pub const APP_CSS: &str = include_str!("../web/app.css");
pub const APP_JS: &str = include_str!("../web/app.js");

/// Placeholder in `web/index.html` that is replaced with the page mode:
/// `app` (already paired) or `pair` (needs a pairing code).
///
/// Note what is *not* here any more: the old build injected the session
/// token straight into the HTML, which meant that anybody who could reach
/// the port also got full access. The page now only learns whether it is
/// paired; the actual credential lives in an `HttpOnly` cookie.
pub const MODE_PLACEHOLDER: &str = "__TD_MODE__";

/// The index page for the given mode (`app` or `pair`).
pub fn page_html(mode: &str) -> String {
    INDEX_HTML.replace(MODE_PLACEHOLDER, mode)
}
