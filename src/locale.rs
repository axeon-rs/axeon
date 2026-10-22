//! Language negotiation: the best of your supported languages for each
//! request, from `Accept-Language` (or an explicit choice).
//!
//! ```rust
//! use axeon::locale::Locales;
//! use axeon::Server;
//!
//! let mut app = Server::new();
//! // The first language is the default.
//! app.middleware(Locales::new(["en", "zh-CN"]).query_param("lang"));
//! app.get("/hello", |req| async move {
//!     match req.locale().map(|l| l.as_str()) {
//!         Some("zh-CN") => "你好",
//!         _ => "Hello",
//!     }
//! });
//! ```

use crate::message::{Request, Response};
use crate::middleware::{Middleware, MiddlewareResult, Next};
use std::sync::Arc;

/// The language chosen for a request, as a request extension: one of the
/// supported tags, exactly as given to [`Locales::new`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Locale(Arc<str>);

impl Locale {
    /// The tag, e.g. `zh-CN`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The primary language subtag, e.g. `zh` for `zh-CN`.
    pub fn language(&self) -> &str {
        self.0.split('-').next().unwrap_or(&self.0)
    }
}

impl std::fmt::Display for Locale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Request {
    /// The language [`Locales`] chose for this request.
    pub fn locale(&self) -> Option<&Locale> {
        self.extension::<Locale>()
    }
}

/// The languages an app supports, and the middleware that picks one per
/// request.
///
/// The choice, in order: the [query parameter](Locales::query_param) or
/// [cookie](Locales::cookie) if set to a supported language, then the best
/// match for `Accept-Language`, then the default (the first language).
/// Matching ignores case and follows RFC 4647 lookup: a request for
/// `zh-Hant-TW` falls back to `zh-Hant`, then `zh`; a request for `zh`
/// matches a supported `zh-CN`; ranges with `q=0` are never chosen.
///
/// As middleware it attaches the [`Locale`] and sets `Content-Language`
/// (unless the handler did) and `Vary: Accept-Language`, so caches keep
/// the languages apart.
#[derive(Clone, Debug)]
pub struct Locales {
    supported: Arc<[Locale]>,
    query_param: Option<String>,
    cookie: Option<String>,
}

impl Locales {
    /// Supports these language tags; the first is the default.
    ///
    /// # Panics
    ///
    /// With no tags.
    pub fn new<I, S>(supported: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let supported: Vec<Locale> = supported.into_iter().map(|s| Locale(Arc::from(s.as_ref().trim()))).collect();
        assert!(!supported.is_empty(), "Locales needs at least one language");
        Self { supported: supported.into(), query_param: None, cookie: None }
    }

    /// Lets a query parameter (e.g. `?lang=zh-CN`) choose the language,
    /// for language switchers and shared links.
    pub fn query_param(mut self, name: impl Into<String>) -> Self {
        self.query_param = Some(name.into());
        self
    }

    /// Lets a cookie choose the language, e.g. one your language switcher
    /// sets. Checked after the query parameter.
    pub fn cookie(mut self, name: impl Into<String>) -> Self {
        self.cookie = Some(name.into());
        self
    }

    /// The default language.
    pub fn default_locale(&self) -> &Locale {
        &self.supported[0]
    }

    /// The supported language matching `tag` (ignoring case, with
    /// fallback), if any.
    pub fn find(&self, tag: &str) -> Option<&Locale> {
        let tag = tag.trim();
        if tag.is_empty() || tag == "*" {
            return None;
        }
        // Exact, then shorter forms of the requested tag (zh-Hant-TW,
        // zh-Hant, zh), then a more specific supported tag (zh → zh-CN).
        let mut prefix = tag;
        loop {
            if let Some(found) = self.supported.iter().find(|s| s.as_str().eq_ignore_ascii_case(prefix)) {
                return Some(found);
            }
            match prefix.rfind('-') {
                Some(cut) => prefix = &prefix[..cut],
                None => break,
            }
        }
        self.supported.iter().find(|s| {
            let s = s.as_str();
            s.len() > tag.len() && s.as_bytes()[tag.len()] == b'-' && s[..tag.len()].eq_ignore_ascii_case(tag)
        })
    }

    /// The best supported language for an `Accept-Language` value, or the
    /// default.
    pub fn negotiate(&self, accept_language: Option<&str>) -> &Locale {
        let Some(header) = accept_language else { return self.default_locale() };
        let mut ranges: Vec<(&str, f32)> = header
            .split(',')
            .filter_map(|part| {
                let mut pieces = part.split(';');
                let tag = pieces.next()?.trim();
                let q = pieces
                    .filter_map(|p| p.trim().strip_prefix("q=").or_else(|| p.trim().strip_prefix("Q=")))
                    .find_map(|q| q.trim().parse::<f32>().ok())
                    .unwrap_or(1.0);
                (!tag.is_empty() && q.is_finite()).then_some((tag, q.clamp(0.0, 1.0)))
            })
            .collect();
        // Highest quality first; equal ones keep their order.
        ranges.sort_by(|a, b| b.1.total_cmp(&a.1));
        let refused: Vec<&Locale> = ranges.iter().filter(|(_, q)| *q == 0.0).filter_map(|(tag, _)| self.find(tag)).collect();
        for (tag, q) in &ranges {
            if *q == 0.0 {
                break;
            }
            if *tag == "*" {
                // Anything: the first supported language not refused.
                if let Some(any) = self.supported.iter().find(|s| !refused.contains(s)) {
                    return any;
                }
                continue;
            }
            if let Some(found) = self.find(tag).filter(|f| !refused.contains(f)) {
                return found;
            }
        }
        self.default_locale()
    }

    /// The language for a request; see [`Locales`] for the order.
    pub fn choose(&self, req: &Request) -> Locale {
        let explicit = self
            .query_param
            .as_deref()
            .and_then(|name| req.query_param(name))
            .and_then(|tag| self.find(tag))
            .or_else(|| self.cookie.as_deref().and_then(|name| cookie(req, name)).and_then(|tag| self.find(tag)));
        explicit.unwrap_or_else(|| self.negotiate(req.get_header("accept-language"))).clone()
    }
}

/// A cookie's value from the `Cookie` header.
fn cookie<'r>(req: &'r Request, name: &str) -> Option<&'r str> {
    req.get_header("cookie")?
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.trim_matches('"'))
}

impl Middleware for Locales {
    fn call(&self, mut req: Request, next: Next) -> MiddlewareResult {
        let locale = self.choose(&req);
        req.insert_extension(locale.clone());
        Box::pin(async move {
            let mut response: Response = next.handle(req).await?;
            if response.get_header("content-language").is_none() {
                response.header("Content-Language", locale.as_str());
            }
            let vary = response.get_header("vary").map(str::to_string);
            match vary {
                None => {
                    response.header("Vary", "Accept-Language");
                }
                Some(vary) if !vary.split(',').any(|v| v.trim().eq_ignore_ascii_case("accept-language")) => {
                    response.header("Vary", format!("{}, Accept-Language", vary));
                }
                Some(_) => {}
            }
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick<'a>(locales: &'a Locales, header: &str) -> &'a str {
        locales.negotiate(Some(header)).as_str()
    }

    #[test]
    fn negotiates() {
        let locales = Locales::new(["en", "zh-CN", "zh-Hant", "fr"]);
        assert_eq!(locales.negotiate(None).as_str(), "en");
        assert_eq!(pick(&locales, "fr"), "fr");
        assert_eq!(pick(&locales, "FR-ca"), "fr", "fallback to the language, ignoring case");
        assert_eq!(pick(&locales, "zh-Hant-TW,en;q=0.5"), "zh-Hant");
        assert_eq!(pick(&locales, "zh"), "zh-CN", "a bare language matches a regional tag");
        assert_eq!(pick(&locales, "de, fr;q=0.8, en;q=0.9"), "en", "by quality");
        assert_eq!(pick(&locales, "de, ja"), "en", "nothing supported: the default");
        assert_eq!(pick(&locales, "*"), "en");
        assert_eq!(pick(&locales, "en;q=0, *"), "zh-CN", "q=0 refuses, even through *");
        assert_eq!(pick(&locales, "fr;q=0"), "en");
        assert_eq!(pick(&locales, ""), "en");
        assert_eq!(pick(&locales, "fr;q=abc, zh-CN;q=0.5"), "fr", "a bad q counts as 1");
        assert_eq!(pick(&locales, ",,;q=1, zh-cn ; q=0.7"), "zh-CN");
        assert_eq!(locales.find("zh-cn").map(|l| l.language()), Some("zh"));
    }

    #[test]
    #[should_panic(expected = "at least one language")]
    fn needs_languages() {
        let _ = Locales::new(Vec::<String>::new());
    }
}
