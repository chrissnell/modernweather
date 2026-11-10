//! [`GoHtmlEscaper`] -- `html/template`'s escaping, so that the byte diff of §7.1 is a real
//! test rather than an aspiration.
//!
//! `askama` is registered to use this escaper for the `html` extension through
//! `crates/mw-web/askama.toml`, which is the *only* escaper configured. `cargo xtask gate
//! layout` rejects the `safe` filter and the `escape` filter's `none` argument anywhere
//! under `templates/`, and the same `none` as a `#[template(...)]` attribute anywhere in
//! this crate's Rust, so a template that wants raw HTML fails the build (**D-81**). The
//! literal spellings are in `ci/layers.toml` and deliberately not repeated here: this
//! file is scanned by the same rule, and a doc comment that quoted them would be a
//! violation.
//!
//! # The table is Go's, and §7.1's prose version of it is missing two entries
//!
//! `docs/service-architecture.md` §7.1 writes the table as *`&` → `&amp;`, `<` → `&lt;`,
//! `>` → `&gt;`, `"` → `&#34;`, `'` → `&#39;`, NUL → `&#0;`*. Go's actual table
//! (`go1.26/src/html/template/html.go:54`, `htmlReplacementTable`) is:
//!
//! | Byte | Go | §7.1 |
//! |---|---|---|
//! | `"` | `&#34;` | same |
//! | `&` | `&amp;` | same |
//! | `'` | `&#39;` | same |
//! | **`+`** | **`&#43;`** | **absent** |
//! | `<` | `&lt;` | same |
//! | `>` | `&gt;` | same |
//! | **NUL** | **`U+FFFD`** | `&#0;` |
//!
//! Both differences are reproduced **as Go has them**, not as the prose has them, and that
//! is the decision rather than an oversight: §7.1's own argument for a custom escaper is
//! that the acceptance test is a byte diff against recorded bodies, and an escaper that
//! follows the prose would differ from the recording on any value containing `+` -- an
//! Aeris station name, a `+`-suffixed callsign, a radar token. `tests/escaping.rs` asserts
//! every byte of the table against output produced by **running** Go's `html/template`
//! (`crates/mw-web/tests/golden/go-escapes.tsv`), which is how M4-2 pinned the enrich
//! formulae and is the only way this can be checked rather than believed.
//!
//! # One table covers all three HTML contexts, and that is measured
//!
//! `html/template` is contextual: text, a quoted attribute and RCDATA (`<title>`) go
//! through `htmlEscaper`, `attrEscaper` and `rcdataEscaper` respectively. For a plain
//! `string` all three reduce to `htmlReplacer(s, htmlReplacementTable, true)`, so one
//! escaper is correct for every interpolation in the page -- and the golden table contains
//! all three contexts over the same inputs, so the claim is checked rather than asserted.
//!
//! The one context that does **not** reduce to this table is JavaScript, which the page
//! enters inside `<script type="application/json">`. That is [`crate::gojs`]'s, and its
//! output carries no HTML-special byte, so this escaper is a no-op over it -- also asserted.
//!
//! `badRunes` is `true` in every call site Go makes for these three contexts, which means
//! the surrogate and non-character ranges pass through unescaped. So this escaper's rule is
//! exactly "replace these seven bytes, copy everything else".

use std::fmt::{self, Write};

/// Go's `html/template` escaping, byte for byte.
#[derive(Debug, Clone, Copy, Default)]
pub struct GoHtmlEscaper;

/// `U+FFFD REPLACEMENT CHARACTER`, which is what Go substitutes for NUL -- "U+0000 NULL
/// Parse error. Append a U+FFFD REPLACEMENT CHARACTER character to the current attribute's
/// value", quoting the HTML5 spec in `htmlReplacementTable`'s own comment.
const NUL: &str = "\u{FFFD}";

/// The replacement for one byte, or `None` to copy it through.
///
/// A function over `u8` rather than a 256-entry table: every entry is ASCII, so a
/// multi-byte character can never match part way and the byte-wise scan below is correct.
const fn replacement(b: u8) -> Option<&'static str> {
    match b {
        0 => Some(NUL),
        b'"' => Some("&#34;"),
        b'&' => Some("&amp;"),
        b'\'' => Some("&#39;"),
        b'+' => Some("&#43;"),
        b'<' => Some("&lt;"),
        b'>' => Some("&gt;"),
        _ => None,
    }
}

/// [`GoHtmlEscaper`] as a function, for a caller that is not a template.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    write_escaped(&mut out, s).expect("writing into a String cannot fail");
    out
}

fn write_escaped<W: Write>(mut dest: W, s: &str) -> fmt::Result {
    let bytes = s.as_bytes();
    let mut written = 0;
    for (i, b) in bytes.iter().enumerate() {
        if let Some(repl) = replacement(*b) {
            // `s[written..i]` is on a character boundary because every byte this matches is
            // ASCII, so it can never be a continuation byte of a multi-byte character.
            dest.write_str(&s[written..i])?;
            dest.write_str(repl)?;
            written = i + 1;
        }
    }
    dest.write_str(&s[written..])
}

impl askama::filters::Escaper for GoHtmlEscaper {
    fn write_escaped_str<W: Write>(&self, dest: W, string: &str) -> fmt::Result {
        write_escaped(dest, string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seven_replacements() {
        assert_eq!(escape("a\"b"), "a&#34;b");
        assert_eq!(escape("a&b"), "a&amp;b");
        assert_eq!(escape("a'b"), "a&#39;b");
        assert_eq!(escape("a+b"), "a&#43;b");
        assert_eq!(escape("a<b"), "a&lt;b");
        assert_eq!(escape("a>b"), "a&gt;b");
        assert_eq!(escape("a\0b"), "a\u{FFFD}b");
    }

    /// What askama's own `Html` escaper would have produced, and why it is not used: the
    /// recorded bodies carry Go's spellings.
    #[test]
    fn the_two_spellings_askamas_default_gets_wrong() {
        assert_eq!(escape("Bob's"), "Bob&#39;s", "askama writes &#x27;");
        assert_eq!(escape("\"q\""), "&#34;q&#34;", "askama writes &quot;");
    }

    #[test]
    fn everything_else_is_copied_through() {
        for s in [
            "",
            "Suncrest Live Weather",
            "contract-radar-token",
            "café",
            "a/b",
            "a\\b",
            "a`b",
            "a=b",
            "emoji\u{1F300}",
            "\u{FDD0}\u{FFFE}",
            "tab\there\nnewline",
        ] {
            assert_eq!(escape(s), s, "{s:?} needs no escaping");
        }
    }

    /// A multi-byte character immediately around a replaced byte, which is where a
    /// byte-index slice would panic if the scan were wrong.
    #[test]
    fn a_replacement_next_to_a_multibyte_character() {
        assert_eq!(escape("é<é"), "é&lt;é");
        assert_eq!(escape("\u{1F300}+\u{1F300}"), "\u{1F300}&#43;\u{1F300}");
    }

    #[test]
    fn the_escaper_is_what_askama_calls() {
        use askama::filters::Escaper;
        let mut out = String::new();
        GoHtmlEscaper
            .write_escaped_str(&mut out, "<a href='x'>&")
            .expect("a String accepts writes");
        assert_eq!(out, "&lt;a href=&#39;x&#39;&gt;&amp;");
    }
}
