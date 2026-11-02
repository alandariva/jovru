//! Runtime UI localization.
//!
//! English source text is the message ID: `tr!("Run query")` returns the
//! English text unless the active locale's catalog translates it. Catalogs are
//! flat JSON objects in `locales/<tag>.json`, embedded at build time through
//! [`CATALOGS`]. Placeholders are positional: `tr!("Query {0}: {1}", n, title)`.
//! `cargo xtask i18n <tag>` creates or refreshes a catalog from the sources.
use std::{collections::HashMap, fmt::Display, sync::OnceLock};

/// Source language of every message.
pub const SOURCE_LOCALE: &str = "en";

/// Embedded catalogs as `(BCP 47 tag, JSON)`. To ship a language, generate
/// `locales/<tag>.json` and add `("<tag>", include_str!("../locales/<tag>.json"))`.
const CATALOGS: &[(&str, &str)] = &[("pt-BR", include_str!("../locales/pt-BR.json"))];

/// Translates a message, or formats it with positional arguments.
#[macro_export]
macro_rules! tr {
    ($msg:literal) => {
        $crate::i18n::text($msg)
    };
    ($msg:literal, $($arg:expr),+ $(,)?) => {
        $crate::i18n::format($msg, &[$(&$arg as &dyn ::std::fmt::Display),+])
    };
}

struct Active {
    tag: &'static str,
    messages: HashMap<&'static str, &'static str>,
}

fn active() -> &'static Active {
    static ACTIVE: OnceLock<Active> = OnceLock::new();
    ACTIVE.get_or_init(|| {
        let requested = std::env::var("JYKRE_LOCALE")
            .ok()
            .filter(|tag| !tag.trim().is_empty())
            .or_else(sys_locale::get_locale);
        let tags: Vec<_> = CATALOGS.iter().map(|(tag, _)| *tag).collect();
        let tag = resolve(requested.as_deref(), &tags);
        let messages = CATALOGS
            .iter()
            .find(|(candidate, _)| *candidate == tag)
            // A broken catalog is caught by the tests; at runtime fall back to English.
            .and_then(|(_, json)| parse_catalog(json).ok())
            .unwrap_or_default();
        Active { tag, messages }
    })
}

/// The locale in use: a catalog tag or [`SOURCE_LOCALE`].
pub fn locale() -> &'static str {
    active().tag
}

/// Locales that can be selected with `JYKRE_LOCALE`.
pub fn available_locales() -> impl Iterator<Item = &'static str> {
    std::iter::once(SOURCE_LOCALE).chain(CATALOGS.iter().map(|(tag, _)| *tag))
}

pub fn text(message: &'static str) -> &'static str {
    active().messages.get(message).copied().unwrap_or(message)
}

pub fn format(message: &'static str, args: &[&dyn Display]) -> String {
    fill(text(message), args)
}

/// Picks the catalog for a requested tag: exact match, then the same primary
/// language (`pt-PT` → `pt-BR`), then the source locale. Case and `_` are ignored.
fn resolve(requested: Option<&str>, available: &[&'static str]) -> &'static str {
    let Some(requested) = requested else {
        return SOURCE_LOCALE;
    };
    let normalize = |tag: &str| tag.trim().replace('_', "-").to_ascii_lowercase();
    let requested = normalize(requested);
    let language = |tag: &str| tag.split('-').next().unwrap_or_default().to_string();
    available
        .iter()
        .find(|tag| normalize(tag) == requested)
        .or_else(|| {
            available
                .iter()
                .find(|tag| language(&normalize(tag)) == language(&requested))
        })
        .copied()
        .unwrap_or(SOURCE_LOCALE)
}

/// Parses a catalog. Empty translations are skipped so they fall back to English.
fn parse_catalog(json: &str) -> Result<HashMap<&'static str, &'static str>, String> {
    let entries: HashMap<String, String> =
        serde_json::from_str(json).map_err(|error| error.to_string())?;
    let mut messages = HashMap::new();
    for (source, translation) in entries {
        if translation.is_empty() {
            continue;
        }
        if placeholders(&source) != placeholders(&translation) {
            return Err(format!("placeholders differ for {source:?}"));
        }
        // Catalogs load once per process, so leaking gives `&'static` lookups.
        messages.insert(
            &*Box::leak(source.into_boxed_str()),
            &*Box::leak(translation.into_boxed_str()),
        );
    }
    Ok(messages)
}

fn placeholders(template: &str) -> Vec<usize> {
    let mut found: Vec<usize> = template
        .split('{')
        .skip(1)
        .filter_map(|part| part.split_once('}')?.0.parse().ok())
        .collect();
    found.sort_unstable();
    found
}

/// Replaces `{N}` with the N-th argument; unknown placeholders stay as written.
fn fill(template: &str, args: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after
            .split_once('}')
            .and_then(|(index, tail)| Some((args.get(index.parse::<usize>().ok()?)?, tail)))
        {
            Some((arg, tail)) => {
                out.push_str(&arg.to_string());
                rest = tail;
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalogs_parse_and_keep_placeholders() {
        for (tag, json) in CATALOGS {
            assert_ne!(*tag, SOURCE_LOCALE);
            parse_catalog(json).unwrap_or_else(|error| panic!("{tag}: {error}"));
        }
    }

    #[test]
    fn resolves_exact_language_and_fallback() {
        let available = ["pt-BR", "es"];
        assert_eq!(resolve(Some("pt_BR"), &available), "pt-BR");
        assert_eq!(resolve(Some("PT-pt"), &available), "pt-BR");
        assert_eq!(resolve(Some("es-MX"), &available), "es");
        assert_eq!(resolve(Some("de-DE"), &available), SOURCE_LOCALE);
        assert_eq!(resolve(None, &available), SOURCE_LOCALE);
        assert_eq!(resolve(Some("en-US"), &[]), SOURCE_LOCALE);
    }

    #[test]
    fn fills_positional_placeholders_in_any_order() {
        assert_eq!(fill("{1} of {0}", &[&3, &"two"]), "two of 3");
        assert_eq!(fill("{0}{0}", &[&"a"]), "aa");
        assert_eq!(fill("{2} {x} {", &[&1]), "{2} {x} {");
    }

    #[test]
    fn rejects_mismatched_placeholders_and_skips_empty_entries() {
        assert!(parse_catalog(r#"{"Query {0}": "Consulta"}"#).is_err());
        assert!(parse_catalog("[]").is_err());
        let messages =
            parse_catalog(r#"{"Run": "", "Query {0}: {1}": "{1} · consulta {0}"}"#).unwrap();
        assert!(!messages.contains_key("Run"));
        assert_eq!(messages["Query {0}: {1}"], "{1} · consulta {0}");
    }

    #[test]
    fn untranslated_messages_return_the_source_text() {
        assert_eq!(tr!("Run query"), "Run query");
        assert_eq!(tr!("Query {0}: {1}", 2, "Sales"), "Query 2: Sales");
    }
}
