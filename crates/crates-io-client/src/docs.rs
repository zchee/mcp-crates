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

/// A slice of one of an index's text arenas.
///
/// Every path, doc comment, kind name and re-export string lives inside a
/// handful of contiguous buffers rather than in an allocation of its own. A
/// crate at the item ceiling used to mean a few hundred thousand small
/// `Box<str>`s; it now means five buffers and a flat array of records.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Span {
    start: u32,
    len: u32,
}

impl Span {
    /// The text this span names.
    ///
    /// Sound for any span this module produced: each is recorded from an
    /// arena's length immediately before appending, so both ends land on a
    /// character boundary.
    fn of(self, arena: &str) -> &str {
        &arena[self.start as usize..self.start as usize + self.len as usize]
    }

    /// Whether the span names nothing.
    ///
    /// Documentation is trimmed and empty documentation is dropped, so a
    /// zero-length doc span means "undocumented" without an `Option` around it.
    const fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// One item, as stored: four spans and a flag, with no text of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ItemRow {
    path: Span,
    kind: Span,
    docs: Span,
    deprecated: bool,
}

/// One re-export, as stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReexportRow {
    name: Span,
    defining_crate: Span,
    path: Span,
    kind: Span,
}

/// One documented item from a crate's rustdoc JSON.
///
/// A borrowed view: the item's text lives in the index that produced it, so
/// this is a pair of pointers and copying it is free.
#[derive(Clone, Copy, Debug)]
pub struct DocItem<'a> {
    index: &'a DocIndex,
    row: &'a ItemRow,
}

impl<'a> DocItem<'a> {
    /// The item's full path, such as `serde::de::Deserializer::deserialize_any`.
    #[must_use]
    pub fn path(self) -> &'a str {
        self.row.path.of(&self.index.paths)
    }

    /// The rustdoc item kind: `struct`, `trait`, `function`, `module`,
    /// `assoc_type`, and so on.
    #[must_use]
    pub fn kind(self) -> &'a str {
        self.row.kind.of(&self.index.kinds)
    }

    /// The item's documentation comment, if it has one.
    #[must_use]
    pub fn docs(self) -> Option<&'a str> {
        (!self.row.docs.is_empty()).then(|| self.row.docs.of(&self.index.prose))
    }

    /// Whether the item is marked deprecated.
    #[must_use]
    pub fn deprecated(self) -> bool {
        self.row.deprecated
    }

    /// The final segment of the item's path.
    #[must_use]
    pub fn short_name(self) -> &'a str {
        let path = self.path();
        path.rsplit("::").next().unwrap_or(path)
    }
}

/// Compared by what the item says, not by where it is stored, so that two
/// indexes built from the same document compare equal.
impl PartialEq for DocItem<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.path() == other.path()
            && self.kind() == other.kind()
            && self.docs() == other.docs()
            && self.deprecated() == other.deprecated()
    }
}

impl Eq for DocItem<'_> {}

/// An item this crate re-exports from another crate.
///
/// A facade crate — one whose public API is mostly `pub use` of its own
/// sub-crates — documents almost nothing itself. rustdoc records the
/// re-exported items as belonging to the crate that defines them, and does not
/// carry their documentation here, so the most useful thing this crate can say
/// about such an item is where its documentation actually lives.
#[derive(Clone, Copy, Debug)]
pub struct Reexport<'a> {
    index: &'a DocIndex,
    row: &'a ReexportRow,
}

impl<'a> Reexport<'a> {
    /// The name this crate exposes the item under.
    #[must_use]
    pub fn name(self) -> &'a str {
        self.row.name.of(&self.index.reexport_text)
    }

    /// The crate that defines it, as rustdoc names it. A crates.io package name
    /// is usually the same, sometimes with `-` where rustdoc has `_`.
    #[must_use]
    pub fn defining_crate(self) -> &'a str {
        self.row.defining_crate.of(&self.index.reexport_text)
    }

    /// The item's full path inside the crate that defines it.
    #[must_use]
    pub fn path(self) -> &'a str {
        self.row.path.of(&self.index.reexport_text)
    }

    /// The rustdoc item kind.
    #[must_use]
    pub fn kind(self) -> &'a str {
        self.row.kind.of(&self.index.reexport_text)
    }
}

impl PartialEq for Reexport<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.name() == other.name()
            && self.defining_crate() == other.defining_crate()
            && self.path() == other.path()
            && self.kind() == other.kind()
    }
}

impl Eq for Reexport<'_> {}

/// The result of looking an item up by path.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct Lookup<'a> {
    /// The item the query resolved to, if it resolved to exactly one.
    pub found: Option<DocItem<'a>>,
    /// Other items that could have been meant, ordered best first.
    pub suggestions: Vec<DocItem<'a>>,
    /// Matching items this crate only re-exports, whose documentation belongs
    /// to another crate. Populated only when nothing local matched.
    pub reexported: Vec<Reexport<'a>>,
}

/// A crate's documentation, indexed by item path.
#[derive(Debug, PartialEq)]
pub struct DocIndex {
    crate_version: Option<String>,
    format_version: Option<u64>,
    /// Every item path, concatenated in item order.
    paths: Box<str>,
    /// [`DocIndex::paths`], ASCII-lowercased.
    ///
    /// ASCII lowercasing is byte-for-byte length preserving, so a span means
    /// the same thing in both, and the fuzzy pass reads a lowercase path
    /// without building one.
    lowered: Box<str>,
    /// Every item's documentation, concatenated.
    prose: Box<str>,
    /// The distinct kind names, stored once each.
    kinds: Box<str>,
    /// Every re-export's name, defining crate, path and kind.
    reexport_text: Box<str>,
    /// Every documented item of the crate itself, ordered by path.
    items: Box<[ItemRow]>,
    /// Items re-exported from other crates, ordered by exposed name.
    reexports: Box<[ReexportRow]>,
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
        let decode = |message: &str| Error::Decode {
            url: format!("rustdoc JSON for {name}"),
            message: message.to_owned(),
        };

        let mut arenas = Arenas::default();
        let mut items: Vec<ItemRow> = Vec::new();
        let mut truncated = false;
        // One buffer reused for every path in the document, rather than a
        // `join` per item and a `format!` per method.
        let mut path = String::new();

        for (id, summary) in &root.paths {
            if summary.crate_id != 0 || summary.path.is_empty() {
                continue;
            }
            if items.len() >= MAX_ITEMS {
                truncated = true;
                break;
            }

            path.clear();
            for (position, segment) in summary.path.iter().enumerate() {
                if position > 0 {
                    path.push_str("::");
                }
                path.push_str(segment);
            }
            let owner_len = path.len();

            let entry = root.index.get(id);
            items.push(arenas.item(&path, &summary.kind, entry));

            // `paths` lists only items with a canonical path, which leaves out
            // every method and associated type: the answer to most questions
            // anyone actually asks. Those are reachable through the owning
            // item's body, so they are folded in here.
            if let Some(entry) = entry {
                collect_associated(
                    &root,
                    entry,
                    &mut path,
                    owner_len,
                    &mut arenas,
                    &mut items,
                    &mut truncated,
                );
            }
        }

        if items.is_empty() {
            return Err(decode("the document describes no items belonging to this crate"));
        }

        // A total order makes lookups deterministic despite the hash maps above,
        // and lets exact matches use a binary search.
        //
        // Ordering on more than the path is what makes the deduplication below
        // deterministic. Two ids can describe the same path — an item reachable
        // through more than one route, or a name a method shares with the type
        // it hangs off — and ordering on the path alone would leave the
        // survivor to be whichever the hash map happened to yield first. The
        // entry carrying documentation wins, because documentation is the thing
        // this index exists to serve; the remaining keys only have to make the
        // order total, so that the answer is the same on every run.
        items.sort_unstable_by(|left, right| {
            left.path
                .of(&arenas.paths)
                .cmp(right.path.of(&arenas.paths))
                .then_with(|| left.docs.is_empty().cmp(&right.docs.is_empty()))
                .then_with(|| left.kind.of(&arenas.kinds).cmp(right.kind.of(&arenas.kinds)))
                .then_with(|| left.docs.of(&arenas.prose).cmp(right.docs.of(&arenas.prose)))
                .then_with(|| left.deprecated.cmp(&right.deprecated))
        });
        items.dedup_by(|left, right| left.path.of(&arenas.paths) == right.path.of(&arenas.paths));

        let mut reexports = collect_reexports(&root, &mut arenas);
        // Same reasoning: `(name, path)` is what the deduplication compares, so
        // the ordering has to break ties past it or the survivor is arbitrary.
        reexports.sort_unstable_by(|left, right| {
            let text = &arenas.reexport_text;
            (
                left.name.of(text),
                left.path.of(text),
                left.defining_crate.of(text),
                left.kind.of(text),
            )
                .cmp(&(
                    right.name.of(text),
                    right.path.of(text),
                    right.defining_crate.of(text),
                    right.kind.of(text),
                ))
        });
        reexports.dedup_by(|left, right| {
            let text = &arenas.reexport_text;
            left.name.of(text) == right.name.of(text) && left.path.of(text) == right.path.of(text)
        });

        // Rewrite every arena in the order the rows are now in. Collection
        // order is hash-map order, so without this pass the text would be
        // scattered through the buffers and two parses of the same document
        // would lay it out differently — which is the difference between an
        // index that merely answers the same and one that *is* the same.
        let text = arenas.compact(&mut items, &mut reexports);
        let lowered = text.paths.to_ascii_lowercase();

        if arenas.overflowed {
            return Err(decode("the document holds more text than an index can address"));
        }

        Ok(Self {
            crate_version: root.crate_version,
            format_version: root.format_version,
            paths: text.paths.into_boxed_str(),
            lowered: lowered.into_boxed_str(),
            prose: text.prose.into_boxed_str(),
            kinds: text.kinds.into_boxed_str(),
            reexport_text: text.reexport_text.into_boxed_str(),
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
    pub fn items(&self) -> impl ExactSizeIterator<Item = DocItem<'_>> {
        (0..self.items.len()).map(|position| self.item(position))
    }

    /// Items this crate re-exports from other crates.
    pub fn reexports(&self) -> impl ExactSizeIterator<Item = Reexport<'_>> {
        (0..self.reexports.len()).map(|position| self.reexport(position))
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
    /// All of it, which is now a sum of buffer lengths rather than a walk.
    ///
    /// Counted too: the lowercase copy of the paths, which is real memory the
    /// arena layout added, and the re-export text, because a facade crate is
    /// almost all re-exports and leaving them out would understate exactly the
    /// crates where they dominate.
    #[must_use]
    pub fn weight(&self) -> u32 {
        let text = self.paths.len()
            + self.lowered.len()
            + self.prose.len()
            + self.kinds.len()
            + self.reexport_text.len();
        let rows = self.items.len() * size_of::<ItemRow>()
            + self.reexports.len() * size_of::<ReexportRow>();
        u32::try_from(text + rows).unwrap_or(u32::MAX)
    }

    /// One item as a borrowed view.
    fn item(&self, position: usize) -> DocItem<'_> {
        DocItem { index: self, row: &self.items[position] }
    }

    /// One re-export as a borrowed view.
    fn reexport(&self, position: usize) -> Reexport<'_> {
        Reexport { index: self, row: &self.reexports[position] }
    }

    /// The lowercase form of an item's path, without building one.
    fn lowered_path(&self, position: usize) -> &str {
        self.items[position].path.of(&self.lowered)
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

        if let Ok(position) = self.items.binary_search_by(|row| row.path.of(&self.paths).cmp(query))
        {
            return Lookup {
                found: Some(self.item(position)),
                suggestions: Vec::new(),
                reexported: Vec::new(),
            };
        }

        let suffix = format!("::{query}");
        let by_suffix: Vec<DocItem<'_>> = (0..self.items.len())
            .filter(|&position| self.item(position).path().ends_with(&suffix))
            .map(|position| self.item(position))
            .collect();
        if let Some(resolved) = resolve(&by_suffix) {
            return resolved;
        }

        let by_name: Vec<DocItem<'_>> = (0..self.items.len())
            .filter(|&position| self.item(position).short_name().eq_ignore_ascii_case(query))
            .map(|position| self.item(position))
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

        // The lowercase form of every path is already stored, so this compares
        // against it rather than building one per item per query.
        let lowered = query.to_ascii_lowercase();
        let fuzzy: Vec<DocItem<'_>> = (0..self.items.len())
            .filter(|&position| self.lowered_path(position).contains(&lowered))
            .map(|position| self.item(position))
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

        let all = || (0..self.items.len()).map(|position| self.item(position));

        if let Some(found) = all().find(|item| item.path() == query) {
            return Lookup { found: Some(found), suggestions: Vec::new(), reexported: Vec::new() };
        }

        let suffix = format!("::{query}");
        let by_suffix: Vec<DocItem<'_>> =
            all().filter(|item| item.path().ends_with(&suffix)).collect();
        if let Some(resolved) = resolve(&by_suffix) {
            return resolved;
        }

        let by_name: Vec<DocItem<'_>> =
            all().filter(|item| item.short_name().eq_ignore_ascii_case(query)).collect();
        if let Some(resolved) = resolve(&by_name) {
            return resolved;
        }

        let reexported = self.reexported_as_linear(query);
        if !reexported.is_empty() {
            return Lookup { found: None, suggestions: Vec::new(), reexported };
        }

        // Deliberately builds a lowercase copy per item, as the shipped code
        // did before the lowercase arena existed: the oracle has to be the old
        // behaviour, not a tidier version of it.
        let lowered = query.to_ascii_lowercase();
        let fuzzy: Vec<DocItem<'_>> =
            all().filter(|item| item.path().to_ascii_lowercase().contains(&lowered)).collect();
        Lookup { found: None, suggestions: shortlist(fuzzy), reexported: Vec::new() }
    }

    /// The re-export pass as a linear scan, for the oracle above.
    #[cfg(test)]
    fn reexported_as_linear(&self, query: &str) -> Vec<Reexport<'_>> {
        let wanted = query.rsplit("::").next().unwrap_or(query);
        let mut matches: Vec<Reexport<'_>> = (0..self.reexports.len())
            .map(|position| self.reexport(position))
            .filter(|item| item.name().eq_ignore_ascii_case(wanted))
            .collect();
        matches.truncate(MAX_SUGGESTIONS);
        matches
    }

    /// Re-exports whose exposed name matches a query.
    ///
    /// The query is matched against the name this crate exposes, so both
    /// `Frame` and `ratatui::Frame` find the same item.
    fn reexported_as(&self, query: &str) -> Vec<Reexport<'_>> {
        let wanted = query.rsplit("::").next().unwrap_or(query);
        let mut matches: Vec<Reexport<'_>> = (0..self.reexports.len())
            .map(|position| self.reexport(position))
            .filter(|item| item.name().eq_ignore_ascii_case(wanted))
            .collect();
        matches.truncate(MAX_SUGGESTIONS);
        matches
    }
}

/// The text buffers an index is assembled into.
#[derive(Default)]
struct Arenas {
    paths: String,
    prose: String,
    kinds: String,
    reexport_text: String,
    /// Where each distinct kind name landed in [`Arenas::kinds`].
    ///
    /// rustdoc names a couple of dozen kinds and repeats them across every item,
    /// so storing the string once and the span per item is the difference
    /// between one allocation and fifty thousand.
    interned_kinds: FastMap<Box<str>, Span>,
    /// Set when an arena outgrew what a [`Span`] can address, which makes every
    /// span recorded afterwards meaningless.
    overflowed: bool,
}

impl Arenas {
    /// Append text to an arena and describe where it landed.
    ///
    /// A span addresses 4 GiB, and the expanded document is capped well below
    /// that before it reaches here, so the overflow path is unreachable in
    /// practice. It is still checked rather than assumed, because a span
    /// recorded past the ceiling would quietly name the wrong text instead of
    /// failing.
    fn push(arena: &mut String, overflowed: &mut bool, text: &str) -> Span {
        let (Ok(start), Ok(len)) = (u32::try_from(arena.len()), u32::try_from(text.len())) else {
            *overflowed = true;
            return Span::default();
        };
        if start.checked_add(len).is_none() {
            *overflowed = true;
            return Span::default();
        }
        arena.push_str(text);
        Span { start, len }
    }

    /// The span of a kind name, storing it if this is the first item to use it.
    fn kind(&mut self, kind: &str) -> Span {
        if let Some(span) = self.interned_kinds.get(kind) {
            return *span;
        }
        let span = Self::push(&mut self.kinds, &mut self.overflowed, kind);
        self.interned_kinds.insert(kind.into(), span);
        span
    }

    /// Build one item's stored form from a path and the index entry describing
    /// it.
    fn item(&mut self, path: &str, kind: &str, entry: Option<&IndexItem>) -> ItemRow {
        let docs = entry
            .and_then(|item| item.docs.as_deref())
            .map(str::trim)
            .filter(|docs| !docs.is_empty());
        ItemRow {
            path: Self::push(&mut self.paths, &mut self.overflowed, path),
            kind: self.kind(kind),
            docs: docs.map_or_else(Span::default, |docs| {
                Self::push(&mut self.prose, &mut self.overflowed, docs)
            }),
            deprecated: entry.is_some_and(|item| item.deprecation.is_some()),
        }
    }

    /// Rewrite every arena in row order, repointing each row as it goes.
    ///
    /// Returns new buffers rather than replacing the old ones in place, because
    /// copying a buffer onto itself is exactly what cannot be done while it is
    /// being read.
    ///
    /// Two things come out of this. A scan of the path arena now walks forwards
    /// through memory instead of jumping around it, and the layout is a
    /// function of the sorted rows alone — so the same document produces the
    /// same bytes, however the hash maps it was collected through were seeded.
    fn compact(&mut self, items: &mut [ItemRow], reexports: &mut [ReexportRow]) -> Text {
        let mut text = Text {
            paths: String::with_capacity(self.paths.len()),
            prose: String::with_capacity(self.prose.len()),
            kinds: String::new(),
            reexport_text: String::with_capacity(self.reexport_text.len()),
        };
        // Kinds are re-interned in first-seen order over the sorted rows, which
        // is deterministic where their collection order was not.
        let mut kinds: FastMap<Box<str>, Span> = FastMap::default();

        for row in items {
            let path = Self::push(&mut text.paths, &mut self.overflowed, row.path.of(&self.paths));
            let docs = if row.docs.is_empty() {
                Span::default()
            } else {
                Self::push(&mut text.prose, &mut self.overflowed, row.docs.of(&self.prose))
            };
            let name = row.kind.of(&self.kinds);
            let kind = match kinds.get(name) {
                Some(span) => *span,
                None => {
                    let span = Self::push(&mut text.kinds, &mut self.overflowed, name);
                    kinds.insert(name.into(), span);
                    span
                },
            };
            row.path = path;
            row.docs = docs;
            row.kind = kind;
        }

        for row in reexports {
            let source = &self.reexport_text;
            let target = &mut text.reexport_text;
            let overflowed = &mut self.overflowed;
            let name = Self::push(target, overflowed, row.name.of(source));
            let defining_crate = Self::push(target, overflowed, row.defining_crate.of(source));
            let path = Self::push(target, overflowed, row.path.of(source));
            let kind = Self::push(target, overflowed, row.kind.of(source));
            *row = ReexportRow { name, defining_crate, path, kind };
        }

        text
    }
}

/// The arenas an index keeps, once compacted.
struct Text {
    paths: String,
    prose: String,
    kinds: String,
    reexport_text: String,
}

/// Collect the items this crate re-exports from other crates.
///
/// A `use` in the index names the item it points at, so following those is what
/// separates a genuine re-export from the thousands of foreign items the path
/// table mentions merely because something references them. For `ratatui` that
/// is the difference between 125 entries and 6805.
fn collect_reexports(root: &RustdocRoot, arenas: &mut Arenas) -> Vec<ReexportRow> {
    // The tables are keyed by the decimal form of an id, and `HashMap<String,
    // _>` looks up by `&str`, so the digits are written into a stack buffer
    // rather than a `String` that exists only long enough to be hashed.
    let mut id = itoa::Buffer::new();
    // As for item paths: one reusable buffer instead of a `join` per re-export.
    let mut path = String::new();
    let mut rows: Vec<ReexportRow> = Vec::new();

    for item in root.index.values() {
        if rows.len() >= MAX_ITEMS {
            break;
        }
        let Some((kind, body)) = item.classify() else {
            continue;
        };
        if kind != "use" {
            continue;
        }
        let Some(target) = body.target.and_then(|target| root.paths.get(id.format(target))) else {
            continue;
        };
        if target.crate_id == 0 || target.path.is_empty() {
            continue;
        }
        // A crate this document does not name cannot be looked up, and the
        // whole value of a re-export entry is telling a caller where to go
        // next, so an unnamed one is dropped rather than described.
        let Some(defining_crate) = root.external_crates.get(id.format(target.crate_id)) else {
            continue;
        };
        if defining_crate.name.is_empty() {
            continue;
        }
        let Some(name) = body.alias.as_deref().or(item.name.as_deref()) else {
            continue;
        };

        path.clear();
        for (position, segment) in target.path.iter().enumerate() {
            if position > 0 {
                path.push_str("::");
            }
            path.push_str(segment);
        }

        let text = &mut arenas.reexport_text;
        let overflowed = &mut arenas.overflowed;
        rows.push(ReexportRow {
            name: Arenas::push(text, overflowed, name),
            defining_crate: Arenas::push(text, overflowed, &defining_crate.name),
            path: Arenas::push(text, overflowed, &path),
            kind: Arenas::push(text, overflowed, &target.kind),
        });
    }
    rows
}

/// Turn a set of candidates into a resolution, if there is anything to resolve.
fn resolve<'a>(candidates: &[DocItem<'a>]) -> Option<Lookup<'a>> {
    match candidates {
        [] => None,
        [only] => {
            Some(Lookup { found: Some(*only), suggestions: Vec::new(), reexported: Vec::new() })
        },
        many => Some(Lookup {
            found: None,
            suggestions: shortlist(many.to_vec()),
            reexported: Vec::new(),
        }),
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
    path: &mut String,
    owner_len: usize,
    arenas: &mut Arenas,
    items: &mut Vec<ItemRow>,
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
        // The owner's path is already in the buffer; each child only appends
        // its own segment and then hands the buffer back.
        path.truncate(owner_len);
        path.push_str("::");
        path.push_str(name);
        items.push(arenas.item(path, child_kind, Some(child)));
    }
}

/// Keep suggestion lists short enough to be read, preferring shorter paths,
/// which are the more likely intent.
fn shortlist(mut items: Vec<DocItem<'_>>) -> Vec<DocItem<'_>> {
    items.sort_by(|left, right| {
        (left.path().len(), left.path()).cmp(&(right.path().len(), right.path()))
    });
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
        index.items().map(|item| item.path()).collect()
    }

    #[test]
    fn only_items_belonging_to_the_crate_are_indexed() {
        let index = index();
        assert_eq!(index.crate_version(), Some("1.2.3"));
        assert_eq!(index.format_version(), Some(60));
        assert!(!index.is_truncated());
        assert!(
            index.items().all(|item| item.path().starts_with("demo")),
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
        assert_eq!(method.kind(), "function");
        assert_eq!(method.docs(), Some("Deserialize anything."));

        let assoc = index.lookup("demo::de::Deserializer::Error").found.expect("resolves");
        assert_eq!(assoc.kind(), "assoc_type");
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
        assert_eq!(found.path(), "demo::value::Value::as_str");
    }

    #[test]
    fn a_macro_body_that_is_not_an_object_does_not_break_parsing() {
        let index = index();
        let found = index.lookup("demo::shout").found.expect("resolves");
        assert_eq!(found.kind(), "macro");
        assert_eq!(found.docs(), Some("Shout."));
    }

    #[test]
    fn documentation_is_trimmed_and_absent_docs_stay_absent() {
        let index = index();
        let found = index.lookup("demo::de::Deserializer").found.expect("resolves");
        assert_eq!(found.docs(), Some("A data format that can deserialize."));
        assert_eq!(found.kind(), "trait");

        let undocumented = index.lookup("demo::ser::Serializer").found.expect("resolves");
        assert_eq!(undocumented.docs(), None);
    }

    #[test]
    fn deprecation_is_recorded() {
        let index = index();
        assert!(index.lookup("demo::Legacy").found.expect("resolves").deprecated());
        assert!(!index.lookup("demo::de::Deserializer").found.expect("resolves").deprecated());
    }

    #[test]
    fn lookup_resolves_a_unique_path_suffix() {
        let index = index();
        let found = index.lookup("de::Deserializer").found.expect("resolves");
        assert_eq!(found.path(), "demo::de::Deserializer");
    }

    #[test]
    fn lookup_resolves_a_unique_bare_type_name_case_insensitively() {
        let index = index();
        let found = index.lookup("deserializer").found.expect("resolves");
        assert_eq!(found.path(), "demo::de::Deserializer");
    }

    #[test]
    fn an_ambiguous_query_suggests_rather_than_guesses() {
        let index = index();
        let result = index.lookup("Error");
        assert!(result.found.is_none(), "three items named Error must not resolve to one");
        let suggested: Vec<&str> = result.suggestions.iter().map(|item| item.path()).collect();
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
        let suggested: Vec<&str> = result.suggestions.iter().map(|item| item.path()).collect();
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
        assert_eq!(reexport.name(), "Frame");
        assert_eq!(reexport.defining_crate(), "demo_core");
        assert_eq!(reexport.path(), "demo_core::frame::Frame");
        assert_eq!(reexport.kind(), "struct");
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

        assert_eq!(index.reexports().len(), 0);
        assert!(index.lookup("Thing").reexported.is_empty());
    }

    #[test]
    fn the_weight_counts_reexports_as_well_as_items() {
        let index = index();
        assert_eq!(index.reexports().len(), 1, "the fixture has one");

        let counted: usize =
            index.reexports().map(|reexport| reexport.name().len() + reexport.path().len()).sum();
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
        assert_eq!(result.reexported[0].path(), "demo_core::frame::Frame");
    }

    #[test]
    fn a_local_item_wins_over_a_reexport_of_the_same_name() {
        let index = index();
        let found = index.lookup("demo::Legacy").found.expect("resolves locally");
        assert_eq!(found.path(), "demo::Legacy");
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
        for (position, (left, right)) in shipped.items().zip(referee.items()).enumerate() {
            assert_eq!(left, right, "{label}: item {position}");
        }

        assert_eq!(shipped.reexports().len(), referee.reexports().len(), "{label}: re-exports");
        for (position, (left, right)) in shipped.reexports().zip(referee.reexports()).enumerate() {
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
        for item in index.items().step_by(stride.max(1)) {
            let path = item.path();
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
            queries.push(reexport.name().to_owned());
            queries.push(reexport.name().to_ascii_lowercase());
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
        let resolved = |query: &str| index.lookup(query).found.map(|item| item.path().to_owned());

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

    /// Two ids describing the same path, one documented and one not, plus a
    /// pair of re-exports that agree on `(name, path)` and differ past it.
    const COLLIDING: &str = r#"{
        "paths": {
            "1":  {"crate_id": 0, "path": ["demo", "Thing"], "kind": "struct"},
            "2":  {"crate_id": 0, "path": ["demo", "Thing"], "kind": "enum"},
            "3":  {"crate_id": 0, "path": ["demo"], "kind": "module"},
            "90": {"crate_id": 1, "path": ["dep", "Shared"], "kind": "struct"},
            "91": {"crate_id": 2, "path": ["dep", "Shared"], "kind": "struct"}
        },
        "external_crates": {"1": {"name": "alpha"}, "2": {"name": "beta"}},
        "index": {
            "2":  {"docs": "The documented one."},
            "80": {"inner": {"use": {"name": "Shared", "id": 90}}},
            "81": {"inner": {"use": {"name": "Shared", "id": 91}}}
        }
    }"#;

    #[test]
    fn a_path_reached_by_two_ids_resolves_to_the_documented_one_every_time() {
        // Items are collected by iterating a hash map, so before the ordering
        // broke ties past the path, which of two same-path entries survived
        // deduplication was decided by the hash seed — a different answer from
        // the same bytes across runs of the same binary.
        let first = DocIndex::parse("demo", COLLIDING.as_bytes()).expect("parses");

        let thing = first.lookup("demo::Thing").found.expect("resolves");
        assert_eq!(thing.docs(), Some("The documented one."));
        assert_eq!(thing.kind(), "enum", "the documented entry brings its own kind");

        // Repeated because the failure this guards against is probabilistic:
        // one parse could agree with the rule by luck.
        for attempt in 1..32 {
            let again = DocIndex::parse("demo", COLLIDING.as_bytes()).expect("parses");
            assert_eq!(again, first, "parse {attempt} disagreed with parse 0");
        }
    }

    #[test]
    fn two_reexports_agreeing_on_name_and_path_resolve_the_same_way_every_time() {
        let first = DocIndex::parse("demo", COLLIDING.as_bytes()).expect("parses");
        assert_eq!(first.reexports().len(), 1, "one survivor, not two");
        let reexport = first.reexports().next().expect("the survivor");
        // `alpha` and `beta` both expose `Shared` at `dep::Shared`; the
        // deduplication compares only `(name, path)`, so the ordering has to
        // decide, and it decides on the defining crate.
        assert_eq!(reexport.defining_crate(), "alpha");

        for attempt in 1..32 {
            let again = DocIndex::parse("demo", COLLIDING.as_bytes()).expect("parses");
            assert_eq!(again, first, "parse {attempt} disagreed with parse 0");
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
