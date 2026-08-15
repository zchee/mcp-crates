//! docs.rs integration: build status and rustdoc JSON.
//!
//! docs.rs publishes the rustdoc JSON it generated for each release. That
//! document holds the real item-level documentation — the prose attached to
//! every public type, trait, function and method — which no crates.io endpoint
//! exposes. Reading it turns "here is a README" into "here is what
//! `Deserializer::deserialize_any` actually does".
//!
//! The document is large: a few hundred kilobytes compressed, a few megabytes
//! expanded. It is therefore reduced to a compact path-to-documentation index,
//! and only that projection is retained; the raw JSON is dropped immediately.

use std::{borrow::Cow, collections::HashMap, fmt};

use serde::{
    Deserialize, Deserializer,
    de::{IgnoredAny, MapAccess, SeqAccess, Visitor},
};

use crate::{
    error::{Error, Result},
    index::validate_name,
};

/// How many alternative items a fuzzy lookup will suggest.
const MAX_SUGGESTIONS: usize = 12;

/// Backstop on how many items one crate may contribute to an index.
const MAX_ITEMS: usize = 50_000;

/// Whether docs.rs managed to build documentation for a release.
#[derive(Clone, Debug, Deserialize)]
#[non_exhaustive]
pub struct BuildStatus {
    /// Whether the documentation build succeeded.
    #[serde(default)]
    pub doc_status: bool,
    /// The version docs.rs resolved the request to.
    #[serde(default)]
    pub version: Option<String>,
}

/// One documented item from a crate's rustdoc JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DocItem {
    /// The item's full path, such as `serde::de::Deserializer::deserialize_any`.
    pub path: Box<str>,
    /// The rustdoc item kind: `struct`, `trait`, `function`, `module`,
    /// `assoc_type`, and so on.
    pub kind: Box<str>,
    /// The item's documentation comment, if it has one.
    pub docs: Option<Box<str>>,
    /// Whether the item is marked deprecated.
    pub deprecated: bool,
}

impl DocItem {
    /// The final segment of the item's path.
    #[must_use]
    pub fn short_name(&self) -> &str {
        self.path.rsplit("::").next().unwrap_or(&self.path)
    }
}

/// The result of looking an item up by path.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct Lookup<'a> {
    /// The item the query resolved to, if it resolved to exactly one.
    pub found: Option<&'a DocItem>,
    /// Other items that could have been meant, ordered best first.
    pub suggestions: Vec<&'a DocItem>,
}

/// A crate's documentation, indexed by item path.
#[derive(Debug)]
pub struct DocIndex {
    crate_version: Option<String>,
    format_version: Option<u64>,
    /// Every documented item of the crate itself, ordered by path.
    items: Box<[DocItem]>,
    truncated: bool,
}

/// Only the parts of the rustdoc JSON schema this crate reads.
///
/// Deserializing into narrow types rather than a generic value keeps a
/// multi-megabyte document from being materialized field by field.
#[derive(Deserialize)]
struct RustdocRoot {
    #[serde(default)]
    crate_version: Option<String>,
    #[serde(default)]
    format_version: Option<u64>,
    #[serde(default)]
    paths: HashMap<String, PathSummary>,
    #[serde(default)]
    index: HashMap<String, IndexItem>,
}

#[derive(Deserialize)]
struct PathSummary {
    /// `0` is the crate being documented; anything else is a dependency.
    #[serde(default)]
    crate_id: u32,
    #[serde(default)]
    path: Vec<String>,
    #[serde(default)]
    kind: String,
}

#[derive(Deserialize)]
struct IndexItem {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    docs: Option<String>,
    #[serde(default)]
    deprecation: Option<IgnoredAny>,
    /// A single-entry map of item kind to body, such as `{"trait": {...}}`.
    #[serde(default)]
    inner: Option<HashMap<String, ItemBody>>,
}

impl IndexItem {
    /// The item's kind and body.
    fn classify(&self) -> Option<(&str, &ItemBody)> {
        let inner = self.inner.as_ref()?;
        inner.iter().next().map(|(kind, body)| (kind.as_str(), body))
    }
}

/// The fields of a rustdoc item body that this index needs.
///
/// Bodies vary by kind and are not all objects — a macro's body is its source
/// text — so anything that is not a map is accepted and contributes nothing.
#[derive(Default)]
struct ItemBody {
    /// Child item ids, on a trait or an impl block.
    items: Vec<u32>,
    /// Impl block ids, on a struct, enum or union.
    impls: Vec<u32>,
    /// Whether an impl block implements a trait, as opposed to being inherent.
    is_trait_impl: bool,
}

impl<'de> Deserialize<'de> for ItemBody {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        deserializer.deserialize_any(ItemBodyVisitor)
    }
}

struct ItemBodyVisitor;

impl<'de> Visitor<'de> for ItemBodyVisitor {
    type Value = ItemBody;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a rustdoc item body")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<ItemBody, A::Error> {
        let mut body = ItemBody::default();
        while let Some(key) = map.next_key::<Cow<'_, str>>()? {
            match key.as_ref() {
                "items" => body.items = map.next_value()?,
                "impls" => body.impls = map.next_value()?,
                "trait" => {
                    body.is_trait_impl = map.next_value::<Option<IgnoredAny>>()?.is_some();
                },
                _ => {
                    map.next_value::<IgnoredAny>()?;
                },
            }
        }
        Ok(body)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<ItemBody, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(ItemBody::default())
    }

    fn visit_str<E>(self, _value: &str) -> std::result::Result<ItemBody, E> {
        Ok(ItemBody::default())
    }

    fn visit_bool<E>(self, _value: bool) -> std::result::Result<ItemBody, E> {
        Ok(ItemBody::default())
    }

    fn visit_i64<E>(self, _value: i64) -> std::result::Result<ItemBody, E> {
        Ok(ItemBody::default())
    }

    fn visit_u64<E>(self, _value: u64) -> std::result::Result<ItemBody, E> {
        Ok(ItemBody::default())
    }

    fn visit_f64<E>(self, _value: f64) -> std::result::Result<ItemBody, E> {
        Ok(ItemBody::default())
    }

    fn visit_unit<E>(self) -> std::result::Result<ItemBody, E> {
        Ok(ItemBody::default())
    }

    fn visit_none<E>(self) -> std::result::Result<ItemBody, E> {
        Ok(ItemBody::default())
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> std::result::Result<ItemBody, D::Error> {
        d.deserialize_any(self)
    }
}

impl DocIndex {
    /// Build an index from a rustdoc JSON document.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] if the document is not rustdoc JSON, or
    /// describes no items belonging to this crate.
    pub fn parse(name: &str, body: &[u8]) -> Result<Self> {
        let root: RustdocRoot = serde_json::from_slice(body).map_err(|err| Error::Decode {
            url: format!("rustdoc JSON for {name}"),
            message: err.to_string(),
        })?;

        let mut items: Vec<DocItem> = Vec::new();
        let mut truncated = false;

        for (id, summary) in &root.paths {
            if summary.crate_id != 0 || summary.path.is_empty() {
                continue;
            }
            if items.len() >= MAX_ITEMS {
                truncated = true;
                break;
            }

            let path = summary.path.join("::");
            let entry = root.index.get(id);
            items.push(doc_item(path.clone(), summary.kind.clone(), entry));

            // `paths` lists only items with a canonical path, which leaves out
            // every method and associated type: the answer to most questions
            // anyone actually asks. Those are reachable through the owning
            // item's body, so they are folded in here.
            if let Some(entry) = entry {
                collect_associated(&root, entry, &path, &mut items, &mut truncated);
            }
        }

        if items.is_empty() {
            return Err(Error::Decode {
                url: format!("rustdoc JSON for {name}"),
                message: "the document describes no items belonging to this crate".to_owned(),
            });
        }

        // A stable order makes lookups deterministic despite the hash maps
        // above, and lets exact matches use a binary search.
        items.sort_by(|left, right| left.path.cmp(&right.path));
        items.dedup_by(|left, right| left.path == right.path);

        Ok(Self {
            crate_version: root.crate_version,
            format_version: root.format_version,
            items: items.into_boxed_slice(),
            truncated,
        })
    }

    /// The crate version the documentation was generated from.
    #[must_use]
    pub fn crate_version(&self) -> Option<&str> {
        self.crate_version.as_deref()
    }

    /// The rustdoc JSON schema version of the source document.
    #[must_use]
    pub fn format_version(&self) -> Option<u64> {
        self.format_version
    }

    /// Every documented item, ordered by path.
    #[must_use]
    pub fn items(&self) -> &[DocItem] {
        &self.items
    }

    /// How many items are indexed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the index is empty. Never true for an index that parsed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Whether indexing stopped early because the crate has an implausible
    /// number of items.
    #[must_use]
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Approximate heap cost of this index, used to bound the cache by bytes.
    ///
    /// Documentation prose dominates and varies by orders of magnitude between
    /// crates, so a cache bounded by entry count would be a poor proxy for
    /// memory use.
    #[must_use]
    pub fn weight(&self) -> u32 {
        let bytes = self
            .items
            .iter()
            .map(|item| {
                item.path.len()
                    + item.kind.len()
                    + item.docs.as_ref().map_or(0, |docs| docs.len())
                    + size_of::<DocItem>()
            })
            .sum::<usize>();
        u32::try_from(bytes).unwrap_or(u32::MAX)
    }

    /// Look an item up by path.
    ///
    /// Resolution widens in steps, so that a caller who knows the exact path
    /// gets it, and a caller who remembers only a type name still gets
    /// somewhere useful:
    ///
    /// 1. the exact full path;
    /// 2. a path suffix on a `::` boundary, so `de::Deserializer` finds
    ///    `serde::de::Deserializer` even though the crate root is missing, and
    ///    `Value::as_str` finds `serde_json::value::Value::as_str` even though the item
    ///    is re-exported from a different module than it lives in;
    /// 3. a case-insensitive match on the final segment;
    /// 4. a case-insensitive substring of the path.
    ///
    /// A query that lands on several items returns no single result and reports
    /// them all as suggestions, rather than silently picking one.
    #[must_use]
    pub fn lookup(&self, query: &str) -> Lookup<'_> {
        let query = query.trim().trim_start_matches("::");
        if query.is_empty() {
            return Lookup::default();
        }

        if let Ok(position) = self.items.binary_search_by(|item| (*item.path).cmp(query)) {
            return Lookup { found: Some(&self.items[position]), suggestions: Vec::new() };
        }

        let suffix = format!("::{query}");
        let by_suffix: Vec<&DocItem> =
            self.items.iter().filter(|item| item.path.ends_with(&suffix)).collect();
        if let Some(resolved) = resolve(&by_suffix) {
            return resolved;
        }

        let by_name: Vec<&DocItem> = self
            .items
            .iter()
            .filter(|item| item.short_name().eq_ignore_ascii_case(query))
            .collect();
        if let Some(resolved) = resolve(&by_name) {
            return resolved;
        }

        let lowered = query.to_ascii_lowercase();
        let fuzzy: Vec<&DocItem> = self
            .items
            .iter()
            .filter(|item| item.path.to_ascii_lowercase().contains(&lowered))
            .collect();
        Lookup { found: None, suggestions: shortlist(fuzzy) }
    }
}

/// Turn a set of candidates into a resolution, if there is anything to resolve.
fn resolve<'a>(candidates: &[&'a DocItem]) -> Option<Lookup<'a>> {
    match candidates {
        [] => None,
        [only] => Some(Lookup { found: Some(only), suggestions: Vec::new() }),
        many => Some(Lookup { found: None, suggestions: shortlist(many.to_vec()) }),
    }
}

/// Build one [`DocItem`] from a path and the index entry describing it.
fn doc_item(path: String, kind: String, entry: Option<&IndexItem>) -> DocItem {
    DocItem {
        path: path.into_boxed_str(),
        kind: kind.into_boxed_str(),
        docs: entry
            .and_then(|item| item.docs.as_deref())
            .map(str::trim)
            .filter(|docs| !docs.is_empty())
            .map(Box::from),
        deprecated: entry.is_some_and(|item| item.deprecation.is_some()),
    }
}

/// Add an item's methods and associated types to the index.
///
/// A trait declares its members directly. A struct, enum or union holds them in
/// impl blocks, of which only the inherent ones are followed: pulling in every
/// trait impl would bury the crate's own API under thousands of `clone`,
/// `fmt` and `eq` entries derived from elsewhere.
fn collect_associated(
    root: &RustdocRoot,
    owner: &IndexItem,
    owner_path: &str,
    items: &mut Vec<DocItem>,
    truncated: &mut bool,
) {
    let Some((kind, body)) = owner.classify() else {
        return;
    };

    let children: Vec<u32> = match kind {
        "trait" => body.items.clone(),
        "struct" | "enum" | "union" => body
            .impls
            .iter()
            .filter_map(|impl_id| {
                let block = root.index.get(&impl_id.to_string())?;
                let (_, impl_body) = block.classify()?;
                (!impl_body.is_trait_impl).then(|| impl_body.items.clone())
            })
            .flatten()
            .collect(),
        _ => return,
    };

    for child_id in children {
        if items.len() >= MAX_ITEMS {
            *truncated = true;
            return;
        }
        let Some(child) = root.index.get(&child_id.to_string()) else {
            continue;
        };
        let Some(name) = child.name.as_deref() else {
            continue;
        };
        let child_kind = child.classify().map_or("item", |(kind, _)| kind);
        items.push(doc_item(format!("{owner_path}::{name}"), child_kind.to_owned(), Some(child)));
    }
}

/// Keep suggestion lists short enough to be read, preferring shorter paths,
/// which are the more likely intent.
fn shortlist(mut items: Vec<&DocItem>) -> Vec<&DocItem> {
    items
        .sort_by(|left, right| (left.path.len(), &left.path).cmp(&(right.path.len(), &right.path)));
    items.truncate(MAX_SUGGESTIONS);
    items
}

/// Expand a zstd-compressed rustdoc document, refusing to exceed a ceiling.
///
/// docs.rs serves these documents zstd-compressed, and the compression ratio is
/// high enough that a modest download expands into a large allocation. The
/// ceiling is enforced during decompression rather than after it, so a
/// pathological ratio cannot exhaust memory before it is noticed.
///
/// # Errors
///
/// Returns [`Error::BodyTooLarge`] if the document expands past `limit`, and
/// [`Error::Decode`] if it is not valid zstd.
pub fn decompress_rustdoc(name: &str, body: &[u8], limit: usize) -> Result<Vec<u8>> {
    use std::io::Read;

    let mut decoder = zstd::stream::read::Decoder::new(body)
        .map_err(|err| Error::Decode {
            url: format!("rustdoc JSON for {name}"),
            message: format!("could not start zstd decompression: {err}"),
        })?
        // Reading one byte past the ceiling is what distinguishes "exactly at
        // the limit" from "over it".
        .take(limit as u64 + 1);

    let mut expanded = Vec::with_capacity(body.len().saturating_mul(4).min(limit));
    decoder.read_to_end(&mut expanded).map_err(|err| Error::Decode {
        url: format!("rustdoc JSON for {name}"),
        message: format!("could not decompress the document: {err}"),
    })?;

    if expanded.len() > limit {
        return Err(Error::BodyTooLarge { url: format!("rustdoc JSON for {name}"), limit });
    }
    Ok(expanded)
}

/// The docs.rs build status URL for a release.
///
/// # Errors
///
/// Returns [`Error::InvalidCrateName`] for an invalid name.
pub fn status_url(name: &str, version: &str) -> Result<String> {
    validate_name(name)?;
    Ok(format!("https://docs.rs/crate/{name}/{version}/status.json"))
}

/// The docs.rs rustdoc JSON URL for a release.
///
/// # Errors
///
/// Returns [`Error::InvalidCrateName`] for an invalid name.
pub fn rustdoc_url(name: &str, version: &str) -> Result<String> {
    validate_name(name)?;
    Ok(format!("https://docs.rs/crate/{name}/{version}/json"))
}

/// The human-facing docs.rs page for a release.
///
/// # Errors
///
/// Returns [`Error::InvalidCrateName`] for an invalid name.
pub fn html_url(name: &str, version: &str) -> Result<String> {
    validate_name(name)?;
    Ok(format!("https://docs.rs/{name}/{version}/{}/", name.replace('-', "_")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors the real schema: `paths` lists only items with a canonical path,
    /// trait members hang off the trait, and inherent methods hang off an impl
    /// block that the type points at.
    const RUSTDOC: &str = r#"{
        "root": "0:0",
        "crate_version": "1.2.3",
        "format_version": 60,
        "paths": {
            "0:0":  {"crate_id": 0, "path": ["demo"], "kind": "module"},
            "0:1":  {"crate_id": 0, "path": ["demo", "de"], "kind": "module"},
            "0:2":  {"crate_id": 0, "path": ["demo", "de", "Deserializer"], "kind": "trait"},
            "0:3":  {"crate_id": 0, "path": ["demo", "ser", "Serializer"], "kind": "trait"},
            "0:4":  {"crate_id": 0, "path": ["demo", "Legacy"], "kind": "struct"},
            "0:5":  {"crate_id": 0, "path": ["demo", "de", "Error"], "kind": "enum"},
            "0:6":  {"crate_id": 0, "path": ["demo", "ser", "Error"], "kind": "enum"},
            "0:7":  {"crate_id": 0, "path": ["demo", "value", "Value"], "kind": "struct"},
            "0:99": {"crate_id": 0, "path": ["demo", "shout"], "kind": "macro"},
            "1:0":  {"crate_id": 1, "path": ["other", "Thing"], "kind": "struct"}
        },
        "index": {
            "0:0": {"docs": "The demo crate.", "inner": {"module": {"items": []}}},
            "0:2": {
                "docs": "  A data format that can deserialize.  ",
                "inner": {"trait": {"items": [20, 21]}}
            },
            "0:3": {"docs": null, "inner": {"trait": {"items": []}}},
            "0:4": {"docs": "Old.", "deprecation": {"since": "1.0.0"}},
            "0:7": {"docs": "A value.", "inner": {"struct": {"impls": [30, 31]}}},
            "0:99": {"docs": "Shout.", "inner": {"macro": "macro_rules! shout { () => {} }"}},
            "20": {"name": "deserialize_any", "docs": "Deserialize anything.", "inner": {"function": {}}},
            "21": {"name": "Error", "docs": "The error type.", "inner": {"assoc_type": {}}},
            "30": {"inner": {"impl": {"trait": null, "items": [40, 41]}}},
            "31": {"inner": {"impl": {"trait": {"path": "Clone"}, "items": [50]}}},
            "40": {"name": "as_str", "docs": "Borrow as a string.", "inner": {"function": {}}},
            "41": {"name": "is_null", "docs": "Whether this is null.", "inner": {"function": {}}},
            "50": {"name": "clone", "docs": "Clones.", "inner": {"function": {}}}
        }
    }"#;

    fn index() -> DocIndex {
        DocIndex::parse("demo", RUSTDOC.as_bytes()).expect("parses")
    }

    fn paths(index: &DocIndex) -> Vec<&str> {
        index.items().iter().map(|item| item.path.as_ref()).collect()
    }

    #[test]
    fn only_items_belonging_to_the_crate_are_indexed() {
        let index = index();
        assert_eq!(index.crate_version(), Some("1.2.3"));
        assert_eq!(index.format_version(), Some(60));
        assert!(!index.is_truncated());
        assert!(
            index.items().iter().all(|item| item.path.starts_with("demo")),
            "a dependency's items are not this crate's documentation"
        );
    }

    #[test]
    fn trait_methods_and_associated_types_are_indexed() {
        // The path table has no entry for either, so without folding in the
        // trait's own members neither would be findable at all.
        let index = index();
        let method =
            index.lookup("demo::de::Deserializer::deserialize_any").found.expect("resolves");
        assert_eq!(method.kind.as_ref(), "function");
        assert_eq!(method.docs.as_deref(), Some("Deserialize anything."));

        let assoc = index.lookup("demo::de::Deserializer::Error").found.expect("resolves");
        assert_eq!(assoc.kind.as_ref(), "assoc_type");
    }

    #[test]
    fn inherent_methods_are_indexed_but_trait_impl_methods_are_not() {
        let index = index();
        assert!(index.lookup("demo::value::Value::as_str").found.is_some());
        assert!(index.lookup("demo::value::Value::is_null").found.is_some());
        assert!(
            !paths(&index).contains(&"demo::value::Value::clone"),
            "derived trait impls would bury the crate's own API"
        );
    }

    #[test]
    fn a_method_is_found_by_its_type_and_name_alone() {
        // The common case: a caller knows `Value::as_str` but not that `Value`
        // is canonically `demo::value::Value`.
        let index = index();
        let found = index.lookup("Value::as_str").found.expect("resolves");
        assert_eq!(found.path.as_ref(), "demo::value::Value::as_str");
    }

    #[test]
    fn a_macro_body_that_is_not_an_object_does_not_break_parsing() {
        let index = index();
        let found = index.lookup("demo::shout").found.expect("resolves");
        assert_eq!(found.kind.as_ref(), "macro");
        assert_eq!(found.docs.as_deref(), Some("Shout."));
    }

    #[test]
    fn documentation_is_trimmed_and_absent_docs_stay_absent() {
        let index = index();
        let found = index.lookup("demo::de::Deserializer").found.expect("resolves");
        assert_eq!(found.docs.as_deref(), Some("A data format that can deserialize."));
        assert_eq!(found.kind.as_ref(), "trait");

        let undocumented = index.lookup("demo::ser::Serializer").found.expect("resolves");
        assert_eq!(undocumented.docs, None);
    }

    #[test]
    fn deprecation_is_recorded() {
        let index = index();
        assert!(index.lookup("demo::Legacy").found.expect("resolves").deprecated);
        assert!(!index.lookup("demo::de::Deserializer").found.expect("resolves").deprecated);
    }

    #[test]
    fn lookup_resolves_a_unique_path_suffix() {
        let index = index();
        let found = index.lookup("de::Deserializer").found.expect("resolves");
        assert_eq!(found.path.as_ref(), "demo::de::Deserializer");
    }

    #[test]
    fn lookup_resolves_a_unique_bare_type_name_case_insensitively() {
        let index = index();
        let found = index.lookup("deserializer").found.expect("resolves");
        assert_eq!(found.path.as_ref(), "demo::de::Deserializer");
    }

    #[test]
    fn an_ambiguous_query_suggests_rather_than_guesses() {
        let index = index();
        let result = index.lookup("Error");
        assert!(result.found.is_none(), "three items named Error must not resolve to one");
        let suggested: Vec<&str> =
            result.suggestions.iter().map(|item| item.path.as_ref()).collect();
        assert_eq!(
            suggested,
            ["demo::de::Error", "demo::ser::Error", "demo::de::Deserializer::Error"],
            "shorter paths are the more likely intent"
        );
    }

    #[test]
    fn an_unknown_query_falls_back_to_substring_suggestions() {
        let index = index();
        // "serial" is a substring of both "Serializer" and "Deserializer", so
        // neither is a unique answer; the trait's own members match too,
        // because their paths carry the trait name. All are offered, shortest
        // first.
        let result = index.lookup("serial");
        assert!(result.found.is_none());
        let suggested: Vec<&str> =
            result.suggestions.iter().map(|item| item.path.as_ref()).collect();
        assert_eq!(
            suggested,
            [
                "demo::ser::Serializer",
                "demo::de::Deserializer",
                "demo::de::Deserializer::Error",
                "demo::de::Deserializer::deserialize_any",
            ]
        );

        assert!(index.lookup("nothing_like_this").suggestions.is_empty());
        assert!(index.lookup("   ").found.is_none());
    }

    #[test]
    fn leading_path_separators_are_ignored() {
        let index = index();
        assert!(index.lookup("::demo::Legacy").found.is_some());
    }

    #[test]
    fn a_document_with_no_items_of_its_own_is_rejected() {
        let foreign =
            r#"{"paths":{"1:0":{"crate_id":1,"path":["other"],"kind":"module"}},"index":{}}"#;
        assert!(matches!(DocIndex::parse("demo", foreign.as_bytes()), Err(Error::Decode { .. })));
        assert!(matches!(DocIndex::parse("demo", b"not json"), Err(Error::Decode { .. })));
    }

    #[test]
    fn decompression_refuses_a_document_that_expands_past_the_ceiling() {
        let payload = vec![b'x'; 4096];
        let compressed = zstd::stream::encode_all(payload.as_slice(), 3).expect("compresses");

        let expanded = decompress_rustdoc("demo", &compressed, 8192).expect("fits");
        assert_eq!(expanded.len(), 4096);

        assert!(matches!(
            decompress_rustdoc("demo", &compressed, 1024),
            Err(Error::BodyTooLarge { .. })
        ));
        assert!(matches!(decompress_rustdoc("demo", b"not zstd", 1024), Err(Error::Decode { .. })));
    }

    #[test]
    fn docs_rs_urls_use_the_underscored_module_name() {
        assert_eq!(
            html_url("tokio-util", "0.7.0").expect("builds"),
            "https://docs.rs/tokio-util/0.7.0/tokio_util/"
        );
        assert_eq!(
            status_url("serde", "1.0.0").expect("builds"),
            "https://docs.rs/crate/serde/1.0.0/status.json"
        );
        assert!(matches!(rustdoc_url("../evil", "1.0.0"), Err(Error::InvalidCrateName { .. })));
    }
}
