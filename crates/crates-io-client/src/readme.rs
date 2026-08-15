//! Turning a rendered README back into Markdown.
//!
//! crates.io stores READMEs pre-rendered as HTML. HTML is a poor format to hand
//! to a language model: the markup is most of the bytes and none of the
//! meaning. Converting back to Markdown typically removes well over half the
//! payload while making headings, lists and code fences legible again.

use htmd::options::{BulletListMarker, LinkStyle, Options};

/// Longest README returned by default, in characters.
pub const DEFAULT_MAX_CHARS: usize = 40_000;

/// Marker appended to a document that was cut short.
const TRUNCATION_MARKER: &str = "\n\n[README truncated]";

/// Convert a rendered README to Markdown.
///
/// Deliberately does not truncate: the result is memoized against the cached
/// response, and folding a caller's length budget into it would pin one
/// caller's budget onto every later reader of the same document. Truncation is
/// [`truncate`], applied per call.
#[must_use]
pub fn to_markdown(html: &str) -> String {
    let converter = htmd::HtmlToMarkdown::builder()
        .options(Options {
            // Dashes with single spacing are the conventional Rust-ecosystem
            // style and cost fewer characters than the default `*` plus three.
            bullet_list_marker: BulletListMarker::Dash,
            ul_bullet_spacing: 1,
            // Collapses `[https://x](https://x)` down to `<https://x>`.
            link_style: LinkStyle::InlinedPreferAutolinks,
            ..Options::default()
        })
        // These carry no prose, and `script` in particular would otherwise be
        // emitted as body text.
        .skip_tags(vec!["script", "style", "noscript", "iframe"])
        // A reader that cannot see an image gains nothing from its URL, and
        // badge walls at the top of a README are almost all image. Dropping
        // them leaves the surrounding link empty, which the pass below removes.
        .skip_tags(vec!["img", "svg", "picture"])
        .build();

    let markdown = converter.convert(html).unwrap_or_else(|_| strip_tags(html));
    let without_empty_links = remove_empty_links(&markdown);
    let inert = neutralize_unsafe_links(&without_empty_links);
    collapse_blank_lines(inert.trim())
}

/// Schemes a link destination may use and still be presented as a link.
const SAFE_SCHEMES: [&str; 3] = ["http", "https", "mailto"];

/// Find the `)` that closes a link destination starting at `from`.
///
/// Parentheses nest: `javascript:alert(1)` and plenty of ordinary URLs contain
/// their own, and stopping at the first `)` would cut a destination in half and
/// leave its tail behind as text.
fn destination_end(text: &str, from: usize) -> Option<usize> {
    let mut depth = 1_usize;
    for (offset, character) in text[from..].char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(from + offset);
                }
            },
            _ => {},
        }
    }
    None
}

/// Whether a link destination is one worth presenting as a link.
///
/// A destination with no scheme is relative or an anchor and carries nothing.
fn is_safe_destination(destination: &str) -> bool {
    let destination = destination.trim();
    let Some(colon) = destination.find(':') else {
        return true;
    };
    let scheme = &destination[..colon];
    // Anything before a path, query or fragment separator is not a scheme.
    if scheme.contains(['/', '?', '#']) {
        return true;
    }
    SAFE_SCHEMES.iter().any(|safe| scheme.eq_ignore_ascii_case(safe))
}

/// Demote links whose destination uses an unexpected scheme to plain text.
///
/// crates.io sanitizes the HTML it renders, so this is a second line rather
/// than the only one. It costs a scan and removes the question of whether a
/// `javascript:` destination could reach a consumer that renders what it is
/// given.
fn neutralize_unsafe_links(markdown: &str) -> String {
    let mut output = String::with_capacity(markdown.len());

    for line in markdown.lines() {
        let mut rest = line;
        while let Some(open) = rest.find('[') {
            let Some(separator) = rest[open..].find("](").map(|offset| open + offset) else {
                break;
            };
            let Some(close) = destination_end(rest, separator + 2) else {
                break;
            };

            output.push_str(&rest[..open]);
            if is_safe_destination(&rest[separator + 2..close]) {
                output.push_str(&rest[open..=close]);
            } else {
                // Keep what the reader was meant to see, drop where it pointed.
                output.push_str(&rest[open + 1..separator]);
            }
            rest = &rest[close + 1..];
        }
        output.push_str(rest);
        output.push('\n');
    }

    output.truncate(output.trim_end().len());
    output
}

/// Cut a document to a character budget, appending a marker so a consumer can
/// tell a shortened document from a complete one.
///
/// Whole lines are kept where possible. A first line that is itself over budget
/// is cut mid-line instead of discarded: rendered Markdown puts each paragraph
/// on one line, so a small budget would otherwise return nothing but the
/// marker.
#[must_use]
pub fn truncate(text: &str, max_chars: usize) -> String {
    let mut kept_chars = 0;
    let mut kept_bytes = 0;
    // `split_inclusive` keeps the newline on the line it terminates, so byte
    // offsets accumulate without having to re-add separators.
    for line in text.split_inclusive('\n') {
        let line_chars = line.chars().count();
        if kept_chars + line_chars > max_chars {
            break;
        }
        kept_chars += line_chars;
        kept_bytes += line.len();
    }

    if kept_bytes == 0 {
        kept_bytes = text.char_indices().nth(max_chars).map_or(text.len(), |(offset, _)| offset);
    }

    // Sound because `kept_bytes` is only ever a sum of whole line lengths or a
    // `char_indices` offset, both of which land on character boundaries. An
    // offset computed from a character count would not.
    let head = text[..kept_bytes].trim_end();
    let mut output = String::with_capacity(head.len() + TRUNCATION_MARKER.len());
    output.push_str(head);
    output.push_str(TRUNCATION_MARKER);
    output
}

/// Drop links whose text is empty.
///
/// These come from two places: the anchor links crates.io injects into every
/// heading, and badge images that have just been removed. Neither leaves
/// anything a reader can use.
///
/// Fenced code blocks are left alone, since bracket-paren sequences inside them
/// are code rather than markup.
fn remove_empty_links(markdown: &str) -> String {
    let mut output = String::with_capacity(markdown.len());
    let mut in_fence = false;

    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if in_fence {
            output.push_str(line);
            output.push('\n');
            continue;
        }

        let mut rest = line;
        while let Some(start) = rest.find("[](") {
            let Some(end) = destination_end(rest, start + 3) else {
                break;
            };
            output.push_str(&rest[..start]);
            rest = &rest[end + 1..];
        }
        output.push_str(rest);
        output.push('\n');
    }

    output.truncate(output.trim_end().len());
    output
}

/// Collapse runs of blank lines down to one.
///
/// HTML-to-Markdown conversion leaves long vertical gaps where the source had
/// layout markup; they cost tokens and carry nothing.
fn collapse_blank_lines(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut blank_run = 0_u32;
    for line in text.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        output.push_str(line.trim_end());
        output.push('\n');
    }
    output.truncate(output.trim_end().len());
    output
}

/// Last-resort tag stripper, used only if the HTML parser fails outright.
fn strip_tags(html: &str) -> String {
    let mut output = String::with_capacity(html.len() / 2);
    let mut inside_tag = false;
    for character in html.chars() {
        match character {
            '<' => inside_tag = true,
            '>' => inside_tag = false,
            _ if !inside_tag => output.push(character),
            _ => {},
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_lists_and_code_survive_the_round_trip() {
        let html = "<h1>Demo</h1><p>A <strong>tiny</strong> \
                    crate.</p><ul><li>fast</li><li>small</li></ul><pre><code>let x = \
                    1;</code></pre>";
        let markdown = to_markdown(html);

        assert!(markdown.contains("# Demo"), "{markdown}");
        assert!(markdown.contains("**tiny**"), "{markdown}");
        assert!(markdown.contains("- fast"), "{markdown}");
        assert!(markdown.contains("let x = 1;"), "{markdown}");
    }

    #[test]
    fn script_and_style_content_is_not_emitted_as_prose() {
        let html = "<style>body{color:red}</style><script>alert(1)</script><p>Real text.</p>";
        let markdown = to_markdown(html);

        assert!(markdown.contains("Real text."));
        assert!(!markdown.contains("alert(1)"), "{markdown}");
        assert!(!markdown.contains("color:red"), "{markdown}");
    }

    #[test]
    fn runs_of_blank_lines_are_collapsed() {
        assert_eq!(collapse_blank_lines("a\n\n\n\n\nb"), "a\n\nb");
        assert_eq!(collapse_blank_lines("a\nb"), "a\nb");
        assert_eq!(collapse_blank_lines("a   \n\n\nb"), "a\n\nb", "trailing spaces go too");
    }

    #[test]
    fn truncation_stops_on_a_line_boundary_and_says_so() {
        let text = "line one\nline two\nline three\nline four";
        let cut = truncate(text, 20);

        assert!(cut.contains("line one"), "{cut}");
        assert!(!cut.contains("line four"), "{cut}");
        assert!(cut.ends_with("[README truncated]"), "{cut}");
        assert!(cut.lines().any(|line| line == "line two"), "a kept line is kept whole: {cut}");
    }

    #[test]
    fn a_first_line_over_budget_is_cut_rather_than_discarded() {
        // Rendered Markdown puts a whole paragraph on one line, so a document
        // whose opening paragraph exceeds the budget is the common case, not a
        // corner one. Returning only the marker would lose everything.
        let text = "a paragraph far longer than the budget allows\nsecond line";
        let cut = truncate(text, 20);

        assert!(cut.starts_with("a paragraph far"), "{cut}");
        assert!(cut.ends_with("[README truncated]"), "{cut}");
        assert!(cut.len() > TRUNCATION_MARKER.len(), "content was kept, not just the marker");
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        // Each of these is three bytes but one character; a byte-based budget
        // would cut this document to a third of its intended length.
        let text = "\u{3042}\u{3044}\u{3046}\n\u{3048}\u{304a}";
        let cut = truncate(text, 4);

        assert!(cut.starts_with("\u{3042}\u{3044}\u{3046}"), "{cut}");
    }

    #[test]
    fn truncation_never_splits_a_multi_byte_character() {
        // Budget lands in the middle of a three-byte character.
        let text = "\u{3042}\u{3044}\u{3046}\u{3048}\u{304a}";
        let cut = truncate(text, 2);

        assert!(cut.starts_with("\u{3042}\u{3044}"), "{cut}");
        assert!(!cut.contains('\u{3046}'), "{cut}");
    }

    #[test]
    fn badge_walls_and_heading_anchors_are_removed() {
        // crates.io renders headings with an injected anchor link, and READMEs
        // commonly open with a row of badge images wrapped in links.
        let html = "<h1><a href=\"#demo\"></a>Demo</h1>\
                    <p><a href=\"https://ci.example\"><img src=\"https://img.example/b.svg\" \
                    alt=\"build\"></a></p><p>Real prose.</p>";
        let markdown = to_markdown(html);

        assert!(markdown.contains("# Demo"), "{markdown}");
        assert!(markdown.contains("Real prose."), "{markdown}");
        assert!(!markdown.contains("img.example"), "{markdown}");
        assert!(!markdown.contains("ci.example"), "{markdown}");
    }

    #[test]
    fn empty_links_are_removed_but_real_ones_are_kept() {
        assert_eq!(remove_empty_links("see [](#anchor)the docs"), "see the docs");
        assert_eq!(remove_empty_links("see [the docs](https://x)"), "see [the docs](https://x)");
        assert_eq!(remove_empty_links("a [](#one) b [](#two) c"), "a  b  c");
    }

    #[test]
    fn bracket_sequences_inside_code_fences_are_left_alone() {
        let source = "text\n```rust\nlet v: Vec<u8> = [](); // odd but code\n```\nmore";
        assert_eq!(remove_empty_links(source), source);
    }

    #[test]
    fn a_destination_containing_parentheses_is_consumed_whole() {
        // Both link passes have to agree on where a destination ends, or one
        // leaves the tail of it behind as visible text.
        assert_eq!(remove_empty_links("a [](https://x.example/a(b)c) b"), "a  b");
        assert_eq!(neutralize_unsafe_links("[t](javascript:f(1)) tail"), "t tail");
        assert_eq!(destination_end("(a(b)c) rest", 1), Some(6));
        assert_eq!(destination_end("unterminated", 0), None);
    }

    #[test]
    fn link_destinations_are_limited_to_schemes_worth_following() {
        for safe in ["https://x.example", "http://x.example", "mailto:a@b.example"] {
            assert!(is_safe_destination(safe), "{safe}");
        }
        for safe in ["#anchor", "./relative", "path/to:thing", "", "/abs"] {
            assert!(is_safe_destination(safe), "{safe} has no scheme");
        }
        for unsafe_destination in ["javascript:alert(1)", "JavaScript:x", "data:text/html,x"] {
            assert!(!is_safe_destination(unsafe_destination), "{unsafe_destination}");
        }
    }

    #[test]
    fn an_unsafe_link_keeps_its_text_and_loses_its_destination() {
        assert_eq!(
            neutralize_unsafe_links("see [the docs](javascript:alert(1)) now"),
            "see the docs now"
        );
        assert_eq!(
            neutralize_unsafe_links("see [the docs](https://x.example)"),
            "see [the docs](https://x.example)"
        );
    }

    #[test]
    fn a_line_with_several_links_neutralizes_only_the_unsafe_ones() {
        let line = "[a](https://x.example) and [b](data:text/html,x) and [c](#anchor)";
        assert_eq!(neutralize_unsafe_links(line), "[a](https://x.example) and b and [c](#anchor)");
    }

    #[test]
    fn the_fallback_stripper_removes_markup() {
        assert_eq!(strip_tags("<p>hello <b>world</b></p>"), "hello world");
    }
}
