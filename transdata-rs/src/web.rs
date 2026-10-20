pub const INDEX_HTML: &str = include_str!("../web/index.html");

/// Placeholder in `web/index.html` that is replaced with the per-run session
/// token. The page is served from the same origin as the API, so the token
/// only needs to defeat *other* websites that try to talk to this server
/// from a browser (see `Config::token`).
pub const TOKEN_PLACEHOLDER: &str = "__TD_TOKEN__";

/// The index page with the session token injected.
pub fn page_html(token: &str) -> String {
    INDEX_HTML.replace(TOKEN_PLACEHOLDER, token)
}
