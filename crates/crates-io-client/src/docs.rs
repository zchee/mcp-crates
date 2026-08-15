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
    version::validate_version,
};

/// How many alternative items a fuzzy lookup will suggest.
const MAX_SUGGESTIONS: usize = 12;

/// Backstop on how many items one crate may contribute to an index.
const MAX_ITEMS: usize = 50_000;

/// The map the rustdoc id tables are read into.
///
/// A document carries tens of thousands of short id keys, each hashed once on
/// insert and again on every child, impl and re-export that refers to it.
/// SipHash is the standard library's default because it keeps a map usable when
/// an attacker chooses the keys; these keys are compiler-generated ids read out
/// of a document that is already being trusted enough to parse, and foldhash
/// still seeds itself randomly per process, so the trade here is a weaker
/// collision-resistance guarantee that nothing was relying on.
type FastMap<K, V> = HashMap<K, V, foldhash::fast::RandomState>;

/// Whether docs.rs managed to build documentation for a release.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[non_exhaustive]
pub struct BuildStatus {
    /// Whether the documentation build succeeded.
    ///
    /// Deliberately required: defaulting it would turn any unrecognised
    /// response, including one from a changed docs.rs schema, into a confident
    /// "the build failed" instead of a decode error.
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

/// An item this crate re-exports from another crate.
///
/// A facade crate — one whose public API is mostly `pub use` of its own
/// sub-crates — documents almost nothing itself. rustdoc records the
/// re-exported items as belonging to the crate that defines them, and does not
/// carry their documentation here, so the most useful thing this crate can say
/// about such an item is where its documentation actually lives.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Reexport {
    /// The name this crate exposes the item under.
    pub name: Box<str>,
    /// The crate that defines it, as rustdoc names it. A crates.io package name
    /// is usually the same, sometimes with `-` where rustdoc has `_`.
    pub defining_crate: Box<str>,
    /// The item's full path inside the crate that defines it.
    pub path: Box<str>,
    /// The rustdoc item kind.
    pub kind: Box<str>,
}

/// The result of looking an item up by path.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct Lookup<'a> {
    /// The item the query resolved to, if it resolved to exactly one.
    pub found: Option<&'a DocItem>,
    /// Other items that could have been meant, ordered best first.
    pub suggestions: Vec<&'a DocItem>,
    /// Matching items this crate only re-exports, whose documentation belongs
    /// to another crate. Populated only when nothing local matched.
    pub reexported: Vec<&'a Reexport>,
}

/// A crate's documentation, indexed by item path.
#[derive(Debug, PartialEq)]
pub struct DocIndex {
    crate_version: Option<String>,
    format_version: Option<u64>,
    /// Every documented item of the crate itself, ordered by path.
    items: Box<[DocItem]>,
    /// Items re-exported from other crates, ordered by exposed name.
    reexports: Box<[Reexport]>,
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
    paths: FastMap<String, PathSummary>,
    #[serde(default)]
    index: FastMap<String, IndexItem>,
    #[serde(default)]
    external_crates: FastMap<String, ExternalCrate>,
}

#[derive(Deserialize)]
struct ExternalCrate {
    #[serde(default)]
    name: String,
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
    /// The item a `use` points at.
    target: Option<u32>,
    /// The name a `use` exposes its target under.
    alias: Option<String>,
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
                "id" => body.target = map.next_value()?,
                "name" => body.alias = map.next_value()?,
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
        let root: RustdocRoot = sonic_rs::from_slice(body).map_err(|err| Error::Decode {
            url: format!("rustdoc JSON for {name}"),
            message: err.to_string(),
        })?;
        Self::build(name, root)
    }

    /// The same index, deserialized by `serde_json` instead.
    ///
    /// Exists only as the referee of the differential parity suite: everything
    /// downstream of the deserializer is the shared [`DocIndex::build`] below,
    /// so a difference between the two can only have come from the parser.
    #[cfg(test)]
    fn parse_with_serde_json(name: &str, body: &[u8]) -> Result<Self> {
        let root: RustdocRoot = serde_json::from_slice(body).map_err(|err| Error::Decode {
            url: format!("rustdoc JSON for {name}"),
            message: err.to_string(),
        })?;
        Self::build(name, root)
    }

    /// Project a deserialized document down to the index this crate keeps.
    fn build(name: &str, root: RustdocRoot) -> Result<Self> {
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

        let mut reexports = collect_reexports(&root);
        reexports.sort_by(|left, right| (&left.name, &left.path).cmp(&(&right.name, &right.path)));
        reexports.dedup_by(|left, right| left.name == right.name && left.path == right.path);

        Ok(Self {
            crate_version: root.crate_version,
            format_version: root.format_version,
            items: items.into_boxed_slice(),
            reexports: reexports.into_boxed_slice(),
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

    /// Items this crate re-exports from other crates.
    #[must_use]
    pub fn reexports(&self) -> &[Reexport] {
        &self.reexports
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
        let items = self
            .items
            .iter()
            .map(|item| {
                item.path.len()
                    + item.kind.len()
                    + item.docs.as_ref().map_or(0, |docs| docs.len())
                    + size_of::<DocItem>()
            })
            .sum::<usize>();
        // Counted too: a facade crate is almost all re-exports, so leaving them
        // out would understate exactly the crates where they dominate.
        let reexports = self
            .reexports
            .iter()
            .map(|reexport| {
                reexport.name.len()
                    + reexport.defining_crate.len()
                    + reexport.path.len()
                    + reexport.kind.len()
                    + size_of::<Reexport>()
            })
            .sum::<usize>();
        u32::try_from(items + reexports).unwrap_or(u32::MAX)
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
            return Lookup {
                found: Some(&self.items[position]),
                suggestions: Vec::new(),
                reexported: Vec::new(),
            };
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

        // Checked before the substring pass below, because a re-export match is
        // exact on the name the crate exposes, while a substring match is a
        // guess. Asking a facade crate for `Frame` should say where `Frame`
        // lives, not offer `FrameExt` because the letters appear in it.
        let reexported = self.reexported_as(query);
        if !reexported.is_empty() {
            return Lookup { found: None, suggestions: Vec::new(), reexported };
        }

        let lowered = query.to_ascii_lowercase();
        let fuzzy: Vec<&DocItem> = self
            .items
            .iter()
            .filter(|item| item.path.to_ascii_lowercase().contains(&lowered))
            .collect();
        Lookup { found: None, suggestions: shortlist(fuzzy), reexported: Vec::new() }
    }

    /// The resolution ladder written as four linear passes, kept as the oracle
    /// the equivalence suite measures the real [`DocIndex::lookup`] against.
    ///
    /// This is not a description of the intended behaviour — it *is* the
    /// behaviour, copied from the implementation that shipped before the
    /// precomputed indexes existed. Any query on which the two disagree is a
    /// behaviour change, whether or not anyone meant it.
    #[cfg(test)]
    fn lookup_linear(&self, query: &str) -> Lookup<'_> {
        let query = query.trim().trim_start_matches("::");
        if query.is_empty() {
            return Lookup::default();
        }

        if let Ok(position) = self.items.binary_search_by(|item| (*item.path).cmp(query)) {
            return Lookup {
                found: Some(&self.items[position]),
                suggestions: Vec::new(),
                reexported: Vec::new(),
            };
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

        let reexported = self.reexported_as(query);
        if !reexported.is_empty() {
            return Lookup { found: None, suggestions: Vec::new(), reexported };
        }

        let lowered = query.to_ascii_lowercase();
        let fuzzy: Vec<&DocItem> = self
            .items
            .iter()
            .filter(|item| item.path.to_ascii_lowercase().contains(&lowered))
            .collect();
        Lookup { found: None, suggestions: shortlist(fuzzy), reexported: Vec::new() }
    }

    /// Re-exports whose exposed name matches a query.
    ///
    /// The query is matched against the name this crate exposes, so both
    /// `Frame` and `ratatui::Frame` find the same item.
    fn reexported_as(&self, query: &str) -> Vec<&Reexport> {
        let wanted = query.rsplit("::").next().unwrap_or(query);
        let mut matches: Vec<&Reexport> =
            self.reexports.iter().filter(|item| item.name.eq_ignore_ascii_case(wanted)).collect();
        matches.truncate(MAX_SUGGESTIONS);
        matches
    }
}

/// Collect the items this crate re-exports from other crates.
///
/// A `use` in the index names the item it points at, so following those is what
/// separates a genuine re-export from the thousands of foreign items the path
/// table mentions merely because something references them. For `ratatui` that
/// is the difference between 125 entries and 6805.
fn collect_reexports(root: &RustdocRoot) -> Vec<Reexport> {
    // The tables are keyed by the decimal form of an id, and `HashMap<String,
    // _>` looks up by `&str`, so the digits are written into a stack buffer
    // rather than a `String` that exists only long enough to be hashed.
    let mut id = itoa::Buffer::new();
    root.index
        .values()
        .filter_map(|item| {
            let (kind, body) = item.classify()?;
            if kind != "use" {
                return None;
            }
            let target = root.paths.get(id.format(body.target?))?;
            if target.crate_id == 0 || target.path.is_empty() {
                return None;
            }
            // A crate this document does not name cannot be looked up, and the
            // whole value of a re-export entry is telling a caller where to go
            // next, so an unnamed one is dropped rather than described.
            let defining_crate = root.external_crates.get(id.format(target.crate_id))?;
            if defining_crate.name.is_empty() {
                return None;
            }
            Some(Reexport {
                name: body.alias.clone().or_else(|| item.name.clone())?.into_boxed_str(),
                defining_crate: defining_crate.name.as_str().into(),
                path: target.path.join("::").into_boxed_str(),
                kind: target.kind.clone().into_boxed_str(),
            })
        })
        .take(MAX_ITEMS)
        .collect()
}

/// Turn a set of candidates into a resolution, if there is anything to resolve.
fn resolve<'a>(candidates: &[&'a DocItem]) -> Option<Lookup<'a>> {
    match candidates {
        [] => None,
        [only] => {
            Some(Lookup { found: Some(only), suggestions: Vec::new(), reexported: Vec::new() })
        },
        many => Some(Lookup {
            found: None,
            suggestions: shortlist(many.to_vec()),
            reexported: Vec::new(),
        }),
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

    // As in `collect_reexports`: one stack buffer for the decimal id, rather
    // than a `String` allocated and dropped per impl block and per method.
    let mut id = itoa::Buffer::new();
    let children: Vec<u32> = match kind {
        "trait" => body.items.clone(),
        "struct" | "enum" | "union" => body
            .impls
            .iter()
            .filter_map(|impl_id| {
                let block = root.index.get(id.format(*impl_id))?;
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
        let Some(child) = root.index.get(id.format(child_id)) else {
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
    validate_version(version)?;
    Ok(format!("https://docs.rs/crate/{name}/{version}/status.json"))
}

/// The docs.rs rustdoc JSON URL for a release.
///
/// # Errors
///
/// Returns [`Error::InvalidCrateName`] for an invalid name.
pub fn rustdoc_url(name: &str, version: &str) -> Result<String> {
    validate_name(name)?;
    validate_version(version)?;
    Ok(format!("https://docs.rs/crate/{name}/{version}/json"))
}

/// The human-facing docs.rs page for a release.
///
/// # Errors
///
/// Returns [`Error::InvalidCrateName`] for an invalid name.
pub fn html_url(name: &str, version: &str) -> Result<String> {
    validate_name(name)?;
    validate_version(version)?;
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
            "0:98": {"crate_id": 0, "path": ["demo", "FrameExt"], "kind": "trait"},
            "0:99": {"crate_id": 0, "path": ["demo", "shout"], "kind": "macro"},
            "1:0":  {"crate_id": 1, "path": ["other", "Thing"], "kind": "struct"},
            "70":   {"crate_id": 1, "path": ["demo_core", "frame", "Frame"], "kind": "struct"},
            "71":   {"crate_id": 2, "path": ["unrelated", "Hidden"], "kind": "struct"}
        },
        "external_crates": {
            "1": {"name": "demo_core"},
            "2": {"name": "unrelated"}
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
            "50": {"name": "clone", "docs": "Clones.", "inner": {"function": {}}},
            "60": {"inner": {"use": {"source": "demo_core::frame::Frame", "name": "Frame", "id": 70, "is_glob": false}}}
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
    fn a_reexported_item_is_reported_against_the_crate_that_defines_it() {
        // A facade crate documents almost nothing itself, so "not found here"
        // is the wrong answer: the item exists, elsewhere.
        let index = index();
        let result = index.lookup("Frame");

        assert!(result.found.is_none(), "the crate does not document Frame itself");
        assert!(result.suggestions.is_empty());

        let [reexport] = result.reexported.as_slice() else {
            panic!("expected exactly one re-export, got {:?}", result.reexported);
        };
        assert_eq!(reexport.name.as_ref(), "Frame");
        assert_eq!(reexport.defining_crate.as_ref(), "demo_core");
        assert_eq!(reexport.path.as_ref(), "demo_core::frame::Frame");
        assert_eq!(reexport.kind.as_ref(), "struct");
    }

    #[test]
    fn a_reexport_is_found_through_the_name_this_crate_exposes() {
        let index = index();
        // The caller writes the path they would use, not the defining one.
        assert_eq!(index.lookup("demo::Frame").reexported.len(), 1);
        assert_eq!(index.lookup("frame").reexported.len(), 1, "matching ignores case");
    }

    #[test]
    fn a_reexport_from_a_crate_the_document_does_not_name_is_dropped() {
        // The entry exists to say where to look next. "re-exported from an
        // unnamed crate" is not something a caller can act on.
        let anonymous = r#"{
            "paths": {
                "0:0": {"crate_id": 0, "path": ["demo"], "kind": "module"},
                "80": {"crate_id": 7, "path": ["ghost", "Thing"], "kind": "struct"}
            },
            "external_crates": {},
            "index": {
                "0:0": {"docs": "The demo crate."},
                "81": {"inner": {"use": {"source": "ghost::Thing", "name": "Thing", "id": 80}}}
            }
        }"#;
        let index = DocIndex::parse("demo", anonymous.as_bytes()).expect("parses");

        assert!(index.reexports().is_empty());
        assert!(index.lookup("Thing").reexported.is_empty());
    }

    #[test]
    fn the_weight_counts_reexports_as_well_as_items() {
        let index = index();
        assert!(!index.reexports().is_empty(), "the fixture has one");

        let counted: usize = index
            .reexports()
            .iter()
            .map(|reexport| reexport.name.len() + reexport.path.len())
            .sum();
        assert!(
            index.weight() as usize > counted,
            "a facade crate is almost all re-exports; leaving them out understates it"
        );
    }

    #[test]
    fn only_genuine_reexports_are_recorded_not_every_foreign_item_mentioned() {
        // The path table names foreign items merely because something refers to
        // them; without following the `use` items, a lookup would suggest
        // unrelated internals of unrelated dependencies.
        let index = index();
        assert_eq!(index.reexports().len(), 1);
        assert!(index.lookup("Hidden").reexported.is_empty());
    }

    #[test]
    fn an_exact_reexport_beats_a_substring_match_on_an_unrelated_local_item() {
        // The crate documents `FrameExt`, whose path contains the letters of
        // "Frame", and re-exports something actually called `Frame`. The
        // re-export is what was asked for.
        let index = index();
        assert!(
            paths(&index).contains(&"demo::FrameExt"),
            "the fixture needs a local substring match for this to discriminate"
        );

        let result = index.lookup("Frame");
        assert!(
            result.suggestions.is_empty(),
            "a substring guess should not outrank an exact re-export: {:?}",
            result.suggestions
        );
        assert_eq!(result.reexported.len(), 1);
        assert_eq!(result.reexported[0].path.as_ref(), "demo_core::frame::Frame");
    }

    #[test]
    fn a_local_item_wins_over_a_reexport_of_the_same_name() {
        let index = index();
        let found = index.lookup("demo::Legacy").found.expect("resolves locally");
        assert_eq!(found.path.as_ref(), "demo::Legacy");
        assert!(index.lookup("demo::Legacy").reexported.is_empty());
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
    fn an_unrecognised_build_status_is_a_decode_error_not_a_failed_build() {
        assert!(sonic_rs::from_str::<BuildStatus>(r#"{"doc_status":true}"#).is_ok());
        assert!(
            sonic_rs::from_str::<BuildStatus>("{}").is_err(),
            "an empty object must not read as a failed build"
        );
    }

    /// One rustdoc document to run both deserializers over.
    struct ParityCase {
        /// The crate name, which only reaches error messages.
        name: &'static str,
        /// The document, already expanded.
        body: Vec<u8>,
        /// How many items the document is expected to contribute. Pinned so
        /// that a corpus entry silently shrinking to nothing — a fixture
        /// replaced, a generator changed — cannot turn into a passing test that
        /// compares almost no items.
        items: usize,
        /// How many re-exports, for the same reason.
        reexports: usize,
    }

    /// The corpus the differential parity suite runs over.
    ///
    /// Two captured documents at opposite ends of the `format_version` range
    /// docs.rs serves; the inline fixture above, which is the only one carrying
    /// a macro body that is a string rather than an object and so the only one
    /// that drives the `ItemBody` visitor down its non-map arms; and a generated
    /// document at the item ceiling, large enough that one divergent item in
    /// fifty thousand is still caught.
    fn parity_corpus() -> [(&'static str, ParityCase); 4] {
        /// Larger than any corpus entry expands to.
        const LIMIT: usize = 64 * 1024 * 1024;
        let expand = |name: &'static str, compressed: &[u8]| {
            decompress_rustdoc(name, compressed, LIMIT).expect("the committed fixture decompresses")
        };
        [
            (
                "regex 1.11.1, format_version 55",
                ParityCase {
                    name: "regex",
                    body: expand(
                        "regex",
                        include_bytes!("../fixtures/regex-1.11.1.rustdoc.json.zst"),
                    ),
                    items: 209,
                    reexports: 0,
                },
            ),
            (
                "semver 1.0.28, format_version 60",
                ParityCase {
                    name: "semver",
                    body: expand(
                        "semver",
                        include_bytes!("../fixtures/semver-1.0.28.rustdoc.json.zst"),
                    ),
                    items: 32,
                    reexports: 0,
                },
            ),
            (
                "the inline fixture",
                ParityCase {
                    name: "demo",
                    body: RUSTDOC.as_bytes().to_vec(),
                    items: 14,
                    reexports: 1,
                },
            ),
            (
                "a generated document at the item ceiling",
                ParityCase {
                    name: "synth",
                    body: crate::synthetic::rustdoc_document(crate::synthetic::PATHS_FOR_CEILING)
                        .into_bytes(),
                    items: 50_000,
                    reexports: 1,
                },
            ),
        ]
    }

    /// Assert that two indexes built from the same bytes agree completely.
    fn assert_identical(label: &str, shipped: &DocIndex, referee: &DocIndex) {
        assert_eq!(shipped.crate_version(), referee.crate_version(), "{label}: crate_version");
        assert_eq!(shipped.format_version(), referee.format_version(), "{label}: format_version");
        assert_eq!(shipped.is_truncated(), referee.is_truncated(), "{label}: truncated");

        assert_eq!(shipped.len(), referee.len(), "{label}: item count");
        for (position, (left, right)) in shipped.items().iter().zip(referee.items()).enumerate() {
            assert_eq!(left, right, "{label}: item {position}");
        }

        assert_eq!(shipped.reexports().len(), referee.reexports().len(), "{label}: re-exports");
        for (position, (left, right)) in
            shipped.reexports().iter().zip(referee.reexports()).enumerate()
        {
            assert_eq!(left, right, "{label}: re-export {position}");
        }

        // The comparisons above exist to make a failure readable: one item
        // named, not fifty thousand printed. This one exists to make the check
        // total, so that a field added to `DocIndex` later is compared whether
        // or not anyone remembers to extend this function.
        assert!(shipped == referee, "{label}: the indexes differ in a field not named above");
    }

    #[test]
    fn sonic_rs_and_serde_json_build_identical_indexes() {
        // The shipped parser and the referee share everything downstream of the
        // deserializer, so a difference here can only have come from the
        // deserializer. Zero divergence is the bar: a document this crate reads
        // wrongly is documentation reported wrongly, which is worse than
        // reporting nothing.
        for (label, case) in parity_corpus() {
            let shipped = DocIndex::parse(case.name, &case.body).expect("the shipped parser reads");
            let referee =
                DocIndex::parse_with_serde_json(case.name, &case.body).expect("the referee reads");

            assert_eq!(shipped.len(), case.items, "{label}: the corpus entry changed size");
            assert_eq!(shipped.reexports().len(), case.reexports, "{label}: re-export count");
            assert_identical(label, &shipped, &referee);
        }
    }

    #[test]
    fn parsing_the_same_document_twice_gives_the_same_index() {
        // What the parity suite above rests on. Items are collected by
        // iterating a hash map, then sorted by path and deduplicated by path,
        // so two entries sharing a path would leave the survivor to hash order
        // — and the parity comparison would be comparing seeds rather than
        // parsers. This says the corpus has no such collision.
        for (label, case) in parity_corpus() {
            let first = DocIndex::parse(case.name, &case.body).expect("parses");
            let second = DocIndex::parse(case.name, &case.body).expect("parses");
            assert_identical(label, &first, &second);
        }
    }

    /// The corpus the equivalence suite runs over: the parity corpus, parsed.
    fn equivalence_corpus() -> Vec<(&'static str, DocIndex)> {
        parity_corpus()
            .into_iter()
            .map(|(label, case)| {
                (label, DocIndex::parse(case.name, &case.body).expect("the corpus parses"))
            })
            .collect()
    }

    /// The ten query shapes the resolution ladder distinguishes, named so that a
    /// failure says which step disagreed rather than only which string did.
    ///
    /// Written against the inline fixture, which is the only corpus entry whose
    /// contents are visible here — including the one case that pins a
    /// precedence rule rather than a match: `Frame` is re-exported and
    /// `FrameExt` is a local substring match, and the re-export has to win.
    fn named_cases() -> [(&'static str, &'static str); 12] {
        [
            ("exact path", "demo::de::Deserializer"),
            ("leading separator", "::demo::Legacy"),
            ("unique suffix", "de::Deserializer"),
            ("ambiguous suffix", "Error"),
            ("bare name, case-insensitive", "deserializer"),
            ("ambiguous bare name", "error"),
            ("re-export beats a local substring", "Frame"),
            ("re-export through the exposed path", "demo::Frame"),
            ("fuzzy substring hit", "serial"),
            ("fuzzy miss", "zzqxnotpresent"),
            ("empty", ""),
            ("whitespace only", "   "),
        ]
    }

    /// Queries derived from an index's own contents.
    ///
    /// Hand-written cases only cover what the author remembered; these cover
    /// what the corpus actually holds. Each item contributes its full path, the
    /// same path with a leading separator, its final segment in three cases, a
    /// two-segment suffix, and an interior substring — which between them reach
    /// every step of the ladder, ambiguous and unique alike.
    fn derived_queries(index: &DocIndex, stride: usize) -> Vec<String> {
        let mut queries = vec![
            String::new(),
            "   ".to_owned(),
            "::".to_owned(),
            "zzqxnotpresent".to_owned(),
            "::::".to_owned(),
        ];
        for item in index.items().iter().step_by(stride.max(1)) {
            let path = item.path.as_ref();
            let short = item.short_name();
            queries.push(path.to_owned());
            queries.push(format!("::{path}"));
            queries.push(short.to_owned());
            queries.push(short.to_ascii_lowercase());
            queries.push(short.to_ascii_uppercase());

            let segments: Vec<&str> = path.split("::").collect();
            if segments.len() >= 2 {
                queries.push(segments[segments.len() - 2..].join("::"));
            }
            if short.len() > 3 {
                queries.push(short[1..short.len() - 1].to_ascii_lowercase());
            }
        }
        // Every re-export by the name the crate exposes, which is the only way
        // to reach the re-export step.
        for reexport in index.reexports() {
            queries.push(reexport.name.to_string());
            queries.push(reexport.name.to_ascii_lowercase());
        }
        queries
    }

    /// Assert two resolutions are the same answer, position by position.
    ///
    /// `DocItem` and `Reexport` both derive `PartialEq`, so this compares every
    /// field of every entry, and `Vec` comparison is positional, so it compares
    /// the ordering too — which is the part a candidate-list rewrite is most
    /// likely to get subtly wrong.
    fn assert_same_lookup(context: &str, fast: &Lookup<'_>, reference: &Lookup<'_>) {
        assert_eq!(fast.found, reference.found, "{context}: found");
        assert_eq!(fast.suggestions, reference.suggestions, "{context}: suggestions");
        assert_eq!(fast.reexported, reference.reexported, "{context}: reexported");
    }

    #[test]
    fn lookup_matches_the_linear_reference_over_the_whole_corpus() {
        let mut compared = 0_usize;
        for (label, index) in equivalence_corpus() {
            // The generated document is two orders of magnitude larger than the
            // rest, and the reference is four linear passes with an allocation
            // per item per query, so it is sampled rather than swept.
            let stride = if index.len() > 10_000 { 4_099 } else { 3 };

            let queries: Vec<String> = named_cases()
                .iter()
                .map(|(_, query)| (*query).to_owned())
                .chain(derived_queries(&index, stride))
                .collect();

            for query in &queries {
                assert_same_lookup(
                    &format!("{label}: query {query:?}"),
                    &index.lookup(query),
                    &index.lookup_linear(query),
                );
                compared += 1;
            }
        }
        // Pinned rather than bounded below: the corpus and the derivation rules
        // are both deterministic, so this number moving means one of them
        // changed, and a battery that quietly shrank would keep passing.
        assert_eq!(compared, 756, "the query battery changed size");
    }

    #[test]
    fn every_named_query_shape_reaches_the_step_it_is_named_for() {
        // A battery that silently stopped exercising a step would still pass the
        // equivalence test above, because both implementations would agree on
        // the step it did reach. These assertions pin what each case is for.
        let index = index();
        let resolved = |query: &str| index.lookup(query).found.map(|item| item.path.to_string());

        assert_eq!(resolved("demo::de::Deserializer").as_deref(), Some("demo::de::Deserializer"));
        assert_eq!(resolved("::demo::Legacy").as_deref(), Some("demo::Legacy"));
        assert_eq!(resolved("de::Deserializer").as_deref(), Some("demo::de::Deserializer"));
        assert_eq!(resolved("deserializer").as_deref(), Some("demo::de::Deserializer"));

        let ambiguous = index.lookup("Error");
        assert!(ambiguous.found.is_none() && ambiguous.suggestions.len() > 1, "ambiguous suffix");
        let ambiguous_name = index.lookup("error");
        assert!(ambiguous_name.found.is_none() && ambiguous_name.suggestions.len() > 1);

        let reexported = index.lookup("Frame");
        assert!(reexported.suggestions.is_empty(), "a re-export outranks a substring guess");
        assert_eq!(reexported.reexported.len(), 1);
        assert_eq!(index.lookup("demo::Frame").reexported.len(), 1);

        let fuzzy = index.lookup("serial");
        assert!(fuzzy.found.is_none() && !fuzzy.suggestions.is_empty(), "fuzzy hit");

        for empty in ["zzqxnotpresent", "", "   ", "::"] {
            let miss = index.lookup(empty);
            assert!(miss.found.is_none() && miss.suggestions.is_empty(), "{empty:?}");
        }
    }

    #[test]
    fn both_deserializers_agree_on_a_build_status() {
        let cases = [
            ("a successful build", r#"{"doc_status":true,"version":"1.0.0"}"#),
            ("a failed build", r#"{"doc_status":false,"version":null}"#),
            ("no version reported", r#"{"doc_status":true}"#),
            ("unknown fields are ignored", r#"{"doc_status":true,"version":"1.0","extra":[1,2]}"#),
        ];
        for (label, document) in cases {
            let shipped = sonic_rs::from_str::<BuildStatus>(document).expect(label);
            let referee = serde_json::from_str::<BuildStatus>(document).expect(label);
            assert_eq!(shipped, referee, "{label}");
        }

        // And that they agree on rejection, not just on acceptance.
        assert!(sonic_rs::from_str::<BuildStatus>("{}").is_err());
        assert!(serde_json::from_str::<BuildStatus>("{}").is_err());
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
        // `Url::join` resolves dot segments, so an unchecked version reaches a
        // different endpoint entirely.
        assert!(matches!(
            crate::api::readme_url("serde", "1.0.0/../../../owners"),
            Err(Error::InvalidVersion { .. })
        ));
        for builder in [status_url, rustdoc_url, html_url] {
            assert!(matches!(builder("serde", "1.0.0?x=1"), Err(Error::InvalidVersion { .. })));
            // `..` conforms to the semver character set but is a path segment,
            // and one that walks the URL up a level.
            assert!(matches!(builder("serde", ".."), Err(Error::InvalidVersion { .. })));
        }
        assert!(matches!(crate::api::readme_url("serde", ".."), Err(Error::InvalidVersion { .. })));
    }
}
