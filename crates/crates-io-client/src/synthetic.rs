//! A rustdoc document generated to order, for tests and benchmarks.
//!
//! Two things need a document larger than anything worth committing: the
//! benchmark that measures lookup at the 50 000-item ceiling, and the
//! differential parity suite, which is only as convincing as the corpus it runs
//! over. No real crate whose rustdoc JSON is small enough to keep in the
//! repository comes near that ceiling.
//!
//! It lives in the library rather than beside either caller because a benchmark
//! is a separate crate and cannot see into `src/`, while a unit test cannot see
//! into `benches/`. One definition used by both is worth more than the handful
//! of bytes it costs, and it costs nothing in practice: nothing in the shipped
//! binary references it, so it is never linked in.

use std::fmt::Write as _;

/// How many generated items share a module.
///
/// Only the shape matters: a flat crate would give every path the same length
/// and the same leading segments, which is not what a lookup scans.
pub const MODULE_SIZE: u32 = 64;

/// Every fourth item owns one inherent method. With the default path count that
/// is what takes the document to the 50 000-item ceiling: 40 000 canonical
/// paths plus 10 000 children.
pub const OWNER_STRIDE: u32 = 4;

/// The path count that produces exactly [`crate::docs::DocIndex`]'s item
/// ceiling once the generated methods are folded in.
pub const PATHS_FOR_CEILING: u32 = 40_000;

/// Build a rustdoc-shaped document declaring `paths` canonical items.
///
/// It mirrors the real schema closely enough that the parser does the same work
/// on it: `paths` holds only items with a canonical path, methods hang off an
/// inherent impl block their type points at, a share of the entries carry
/// prose, and a `use` re-exports from a named foreign crate. `paths /
/// `[`OWNER_STRIDE`] of the entries own a method, so the resulting index holds
/// `paths + paths / OWNER_STRIDE` items.
#[must_use]
pub fn rustdoc_document(paths: u32) -> String {
    // Roughly what the ceiling-sized document measures; one reservation beats a
    // few dozen reallocations of a multi-megabyte string.
    let mut out = String::with_capacity(paths as usize * 190 + 1024);
    out.push_str(r#"{"crate_version":"0.1.0","format_version":60,"paths":{"#);

    for id in 0..paths {
        if id > 0 {
            out.push(',');
        }
        let module = id / MODULE_SIZE;
        let kind = if id % OWNER_STRIDE == 0 { "struct" } else { "function" };
        let _ = write!(
            out,
            r#""{id}":{{"crate_id":0,"path":["synth","m{module}","Item{id}"],"kind":"{kind}"}}"#
        );
    }
    // One foreign item, so the re-export pass has something to resolve.
    out.push_str(
        r#","900000":{"crate_id":1,"path":["synth_core","frame","Frame"],"kind":"struct"}"#,
    );
    out.push_str(r#"},"external_crates":{"1":{"name":"synth_core"}},"index":{"#);
    // Emitted first so every entry after it can carry a leading comma.
    out.push_str(
        r#""900001":{"inner":{"use":{"source":"synth_core::frame::Frame","name":"Frame","id":900000,"is_glob":false}}}"#,
    );

    for id in 0..paths {
        if id % OWNER_STRIDE == 0 {
            // A type, its inherent impl block, and the one method inside it.
            // The ids are separated by magnitude so they cannot collide with
            // the path ids above.
            let impl_id = 1_000_000 + id;
            let method_id = 2_000_000 + id;
            let _ = write!(
                out,
                r#","{id}":{{"name":"Item{id}","docs":"Item {id} of the generated crate. It carries enough prose for the parser to copy, trim and weigh it.","inner":{{"struct":{{"impls":[{impl_id}]}}}}}}"#
            );
            let _ = write!(
                out,
                r#","{impl_id}":{{"inner":{{"impl":{{"trait":null,"items":[{method_id}]}}}}}}"#
            );
            let _ = write!(
                out,
                r#","{method_id}":{{"name":"method{id}","docs":"Does whatever item {id} does.","inner":{{"function":{{}}}}}}"#
            );
        } else if id % 2 == 0 {
            // A documented leaf: no body worth walking, but prose to carry.
            let _ = write!(
                out,
                r#","{id}":{{"name":"Item{id}","docs":"Item {id} of the generated crate.","inner":{{"function":{{}}}}}}"#
            );
        }
        // The rest have no index entry at all, which is the common case in a
        // real document.
    }

    out.push_str("}}");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docs::DocIndex;

    #[test]
    fn the_default_path_count_lands_exactly_on_the_item_ceiling() {
        let index = DocIndex::parse("synth", rustdoc_document(PATHS_FOR_CEILING).as_bytes())
            .expect("the generated document parses");

        assert_eq!(index.len(), 50_000, "the benchmark's premise is this exact number");
        assert!(!index.is_truncated(), "reaching the ceiling is not the same as passing it");
        assert_eq!(index.reexports().len(), 1, "the re-export pass has something to do");
    }

    #[test]
    fn generated_paths_are_unique_so_dedup_never_has_to_choose() {
        // `DocIndex::build` sorts by path and then deduplicates by path, so when
        // two entries share a path the survivor is decided by hash-map
        // iteration order. Uniqueness here is what keeps the parity suite
        // comparing parsers rather than comparing hash seeds.
        let index = DocIndex::parse("synth", rustdoc_document(2_048).as_bytes()).expect("parses");
        let paths: Vec<&str> = index.items().iter().map(|item| item.path.as_ref()).collect();

        let mut sorted = paths.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), paths.len(), "a generated path was emitted twice");
    }
}
