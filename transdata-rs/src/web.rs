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

/// Short tag of the current assets, used as `?v=` on the asset URLs.
///
/// Why this exists: the assets are static files, and a phone that already
/// has them cached would keep using the *old* ones after the app was
/// rebuilt. New HTML + old JS is worse than useless - the old script
/// reaches for elements that no longer exist, throws, and the page renders
/// half empty. Putting the version in the URL means a rebuilt app always
/// points at URLs the browser has never seen, so it cannot serve a stale
/// copy - even if something in between still caches aggressively.
pub fn asset_version() -> &'static str {
    // FNV-1a over both assets; the result only has to change when they do.
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    fn hash(mut h: u64, bytes: &[u8]) -> u64 {
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(FNV_PRIME);
        }
        h
    }
    let mut h = hash(FNV_OFFSET, APP_CSS.as_bytes());
    h = hash(h, b"\0");
    h = hash(h, APP_JS.as_bytes());
    // Stash it in a static so we can hand out a &'static str.
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| format!("{:016x}", h))
}

/// The index page for the given mode (`app` or `pair`).
pub fn page_html(mode: &str) -> String {
    let v = asset_version();
    INDEX_HTML
        .replace(MODE_PLACEHOLDER, mode)
        .replace("\"/app.css\"", &format!("\"/app.css?v={v}\""))
        .replace("\"/app.js\"", &format!("\"/app.js?v={v}\""))
}
