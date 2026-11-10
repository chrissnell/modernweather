//! The JavaScript half of `html/template`'s contextual escaping, which the page needs
//! because it carries a `<script type="application/json">` block.
//!
//! # Why this exists at all, when §7.1 only asked for one escaper
//!
//! `html/template` is **contextual**. §7.1 names one table and calls it `GoHtmlEscaper`,
//! which is right for the five interpolations in HTML text, a quoted attribute and
//! `<title>` -- and wrong for the three inside `<script id="radar-config"
//! type="application/json">`, which Go escapes as JavaScript:
//!
//! ```text
//! {"lat": {{ .Latitude }}, "lon": {{ .Longitude }}, "token": "{{ .RadarToken }}", "widthMiles": 200}
//! ```
//!
//! renders as
//!
//! ```text
//! {"lat":  39.9 , "lon":  -105.5 , "token": "contract-radar-token", "widthMiles": 200}
//! ```
//!
//! -- note the **padding spaces** around the two numbers. `jsValEscaper` marshals the value
//! as JSON and surrounds it with spaces whenever the first or last rune is a JavaScript
//! identifier character, so that a number cannot run into an adjacent keyword
//! (`go1.26/src/html/template/js.go:219`). Reproducing the recorded body therefore needs
//! that padding, and an HTML escaper would never produce it.
//!
//! The byte diff is what surfaced this: §7.1's single-escaper account is incomplete rather
//! than wrong, and the missing part is recorded here instead of being worked around with
//! `|safe`, which the layout gate forbids and which would also turn off escaping for a
//! value an operator controls.
//!
//! # Composition with [`crate::escape::GoHtmlEscaper`] is a no-op, by construction
//!
//! These functions are called in the template context struct and their results are
//! interpolated like any other field, so askama's escaper runs over them. That is safe
//! because [`js_string`]'s table maps every HTML-special byte -- `"`, `'`, `&`, `<`, `>`,
//! `+` -- to a `\uXXXX` escape, and [`js_number`]'s output is digits, `-`, `.`, `e`, `+`
//! and spaces... with one exception that matters: an exponent's `+`. `1e+21` would be
//! escaped to `1e&#43;21`.
//!
//! **A latitude or longitude can never reach that form** -- Go switches to the exponent
//! spelling only outside `[1e-6, 1e21)`, and a coordinate is in `[-180, 180]` -- but
//! "can never" is not a thing to leave to a comment, so [`js_number`] returns the padded
//! token and [`JsNumber`] is a wrapper whose `Display` is `HtmlSafe`. `tests/escaping.rs`
//! asserts the composition over every value in the golden table, exponent forms included.
//!
//! # The digits are Go's, which is `ryu`
//!
//! `encoding/json` formats a float with `strconv.AppendFloat(b, f, fmt, -1, 64)`, which is
//! ryu and breaks a shortest-representation tie **round-half-to-even**; Rust's `Display` is
//! Grisu/Dragon and breaks it **round-half-up**. `mw_api_legacy::dto` has the same problem
//! and the same answer for the frozen surface's numbers; this is the website's, and
//! `crates/modernweather/tests/go_numbers.rs` asserts the two implementations agree over a
//! sweep, because two answers to "what are Go's digits" is one answer too many.

use std::fmt;

use askama::filters::HtmlSafe;

/// Go's `jsStrReplacementTable` (`go1.26/src/html/template/js.go:333`), plus the low-Unicode
/// table `replace` consults first (`:319`).
///
/// Every HTML-special byte becomes a `\uXXXX` escape, which is what
/// `jsStrReplacementTable`'s own comment says it is for: "Encode HTML specials as hex so the
/// output can be embedded in HTML attributes without further encoding".
///
/// The two tables are **one** function here because Go consults them in a fixed order --
/// `lowUnicodeReplacementTable` wins for every rune below `0x20` -- so a reader who needs to
/// check one byte against Go reads one `match` instead of two.
fn push_js_escaped(out: &mut String, c: char) {
    let named = match c {
        '\t' => Some(r"\t"),
        '\n' => Some(r"\n"),
        '\u{c}' => Some(r"\f"),
        '\r' => Some(r"\r"),
        '"' => Some(r"\u0022"),
        '`' => Some(r"\u0060"),
        '&' => Some(r"\u0026"),
        '\'' => Some(r"\u0027"),
        '+' => Some(r"\u002b"),
        '/' => Some(r"\/"),
        '<' => Some(r"\u003c"),
        '>' => Some(r"\u003e"),
        '\\' => Some(r"\\"),
        // `replace` escapes the two line terminators that are legal in a JSON string and
        // illegal in a JavaScript string literal.
        '\u{2028}' => Some(r"\u2028"),
        '\u{2029}' => Some(r"\u2029"),
        _ => None,
    };
    if let Some(named) = named {
        out.push_str(named);
        return;
    }
    // Every remaining control character below `0x20`. Go spells them `\u000e` and so on,
    // lower-case and four digits, which is what `lowUnicodeReplacementTable` holds for the
    // ones it does not name.
    if (c as u32) < 0x20 {
        out.push_str(&format!("\\u{:04x}", c as u32));
        return;
    }
    out.push(c);
}

/// Go's `jsStrEscaper`: a value safe between quotes in a JavaScript string literal.
///
/// The output carries no HTML-special byte, which is what makes composing it with
/// [`crate::escape::GoHtmlEscaper`] a no-op. It is nonetheless returned as a plain `String`
/// rather than a `HtmlSafe` wrapper, so that the escaper *does* run over it: an escaper that
/// is skipped is one that cannot catch a mistake in this table.
pub fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        push_js_escaped(&mut out, c);
    }
    out
}

/// A JavaScript value token, already padded the way `jsValEscaper` pads one.
///
/// [`HtmlSafe`] because an exponent's `+` would otherwise be escaped to `&#43;` by the
/// page's escaper -- see the module documentation. This is the **only** `HtmlSafe` type in
/// this crate, and it is a number: there is no string path through it, so marking it cannot
/// let an operator-authored value reach a page unescaped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsNumber(String);

impl fmt::Display for JsNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl HtmlSafe for JsNumber {}

impl JsNumber {
    /// The token, for a test and for a caller building JSON by hand.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether a byte is a JavaScript identifier part, which is what decides the padding.
///
/// Go's `isJSIdentPart` (`js.go:52`): a letter, a digit, `$` or `_`. Only the first and
/// last runes of the marshalled value are tested, and for a number those are ASCII, so a
/// byte test is exact here.
fn is_js_ident_part(c: char) -> bool {
    c == '$' || c == '_' || c.is_alphanumeric()
}

fn pad(json: String) -> JsNumber {
    let first = json.chars().next();
    let last = json.chars().next_back();
    let padded = match (first, last) {
        (Some(f), Some(l)) if is_js_ident_part(f) || is_js_ident_part(l) => {
            format!(" {json} ")
        }
        // `jsValEscaper` returns " null " for an empty marshalling, which cannot happen
        // here: `json` is always at least one digit.
        _ => json,
    };
    JsNumber(padded)
}

/// Go's `jsValEscaper` over a `float64`: `json.Marshal` then the padding.
pub fn js_number(value: f64) -> JsNumber {
    pad(json_f64(value))
}

/// Go's `jsValEscaper` over an integer.
pub fn js_integer(value: i64) -> JsNumber {
    pad(value.to_string())
}

/// `encoding/json`'s rendering of a `float64`.
///
/// Go switches to the exponent spelling when the magnitude is nonzero and outside
/// `[1e-6, 1e21)`, and spells the exponent with a sign and no leading zero: `1e+21`,
/// `1e-7`. Only the *digits* come from `ryu`, which picks whichever of the two spellings is
/// shorter and would write `1e21`.
///
/// A non-finite value cannot reach here: `json.Marshal` fails on one, and the two callers
/// are a latitude and a longitude read from a `REAL` column that the configuration reader
/// has already turned into an `Option<f64>`. [`json_f64`] renders one as `0` rather than
/// panicking, which is the same choice **DEV-11** made one surface over for the same reason:
/// a half-written page is worse than a zero.
pub fn json_f64(value: f64) -> String {
    if !value.is_finite() {
        return "0".to_string();
    }
    let mut buf = ryu::Buffer::new();
    let shortest = buf.format_finite(value);
    let abs = value.abs();
    let exponent_form = abs != 0.0 && !(1e-6..1e21).contains(&abs);
    respell(shortest, exponent_form)
}

/// Re-spell `ryu`'s output the way Go's `'f'` or `'e'` verb would.
fn respell(shortest: &str, exponent_form: bool) -> String {
    let (sign, rest) = match shortest.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", shortest),
    };
    // ryu always writes a decimal point and may write an exponent: `39.9`, `1e21`,
    // `1.0E0` is not a form it produces. Split into digits and the decimal exponent of the
    // first digit.
    let (mantissa, exp) = match rest.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i32>().unwrap_or(0)),
        None => (rest, 0),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
    let mut digits: String = int_part.to_string();
    digits.push_str(frac_part);
    // `point` is how many digits belong before the decimal point.
    let mut point = int_part.len() as i32 + exp;
    // Strip leading zeros, which `0.0001` has and which move the point.
    let leading = digits.len() - digits.trim_start_matches('0').len();
    if leading == digits.len() {
        // The value is zero. `ryu` writes `0.0`; Go writes `0`.
        return format!("{sign}0");
    }
    digits = digits[leading..].to_string();
    point -= leading as i32;
    // Strip trailing zeros, which do not change the value and which Go's shortest form
    // never carries.
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };

    if exponent_form {
        let e = point - 1;
        let mut out = String::with_capacity(digits.len() + 6);
        out.push_str(sign);
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if e < 0 { '-' } else { '+' });
        out.push_str(&e.abs().to_string());
        return out;
    }

    let mut out = String::with_capacity(digits.len() + 8);
    out.push_str(sign);
    if point <= 0 {
        out.push_str("0.");
        for _ in 0..-point {
            out.push('0');
        }
        out.push_str(digits);
    } else if (point as usize) >= digits.len() {
        out.push_str(digits);
        for _ in 0..(point as usize - digits.len()) {
            out.push('0');
        }
    } else {
        out.push_str(&digits[..point as usize]);
        out.push('.');
        out.push_str(&digits[point as usize..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two values the recorded `radar-config` block carries, padding included.
    #[test]
    fn the_radar_config_numbers() {
        assert_eq!(js_number(39.900).as_str(), " 39.9 ");
        assert_eq!(js_number(-105.500).as_str(), " -105.5 ");
        assert_eq!(js_integer(4).as_str(), " 4 ");
        assert_eq!(js_integer(-1).as_str(), " -1 ");
    }

    #[test]
    fn gos_exponent_thresholds_and_spelling() {
        assert_eq!(json_f64(1e21), "1e+21");
        assert_eq!(json_f64(1e-7), "1e-7");
        assert_eq!(
            json_f64(1e-6),
            "0.000001",
            "the interval is half-open at 1e-6"
        );
        assert_eq!(json_f64(-1e-6), "-0.000001");
        assert_eq!(json_f64(0.0), "0");
        assert_eq!(
            json_f64(-0.0),
            "-0",
            "Go writes the sign of a negative zero"
        );
        assert_eq!(json_f64(1.0), "1", "no trailing `.0`");
        assert_eq!(json_f64(123456789012345680000.0), "123456789012345680000");
        assert_eq!(json_f64(std::f64::consts::PI), "3.141592653589793");
    }

    /// `ryu`'s tie rule rather than `Display`'s, which is the whole reason `ryu` is here.
    #[test]
    fn the_digits_are_ryus() {
        assert_eq!(json_f64(2956007.25), "2956007.25");
        assert_eq!(json_f64(46.6328125), "46.6328125");
    }

    #[test]
    fn a_non_finite_value_renders_as_zero_rather_than_panicking() {
        assert_eq!(json_f64(f64::NAN), "0");
        assert_eq!(json_f64(f64::INFINITY), "0");
        assert_eq!(json_f64(f64::NEG_INFINITY), "0");
    }

    #[test]
    fn js_strings_escape_every_html_special() {
        assert_eq!(
            js_string("Bob's \"Weather\" & Co"),
            r"Bob\u0027s \u0022Weather\u0022 \u0026 Co"
        );
        assert_eq!(js_string("</script>"), r"\u003c\/script\u003e");
        assert_eq!(js_string("a+b"), r"a\u002bb");
        assert_eq!(js_string("a\\b"), r"a\\b");
        assert_eq!(js_string("a/b"), r"a\/b");
        assert_eq!(js_string("a`b"), r"a\u0060b");
        assert_eq!(js_string("tab\there"), r"tab\there");
        assert_eq!(js_string("contract-radar-token"), "contract-radar-token");
        assert_eq!(js_string("\u{0}\u{1b}"), r"\u0000\u001b");
        assert_eq!(js_string("\u{2028}\u{2029}"), r"\u2028\u2029");
    }

    /// The property that makes the composition in the template safe: nothing the page's
    /// escaper would touch survives.
    #[test]
    fn a_js_string_carries_no_html_special_byte() {
        for s in [
            "Bob's \"Weather\" & Co",
            "</script>",
            "'; alert(1); //",
            "a+b",
            "\u{0}\u{1f}\u{2028}",
            "café",
        ] {
            let escaped = js_string(s);
            assert!(
                !escaped.contains(['"', '\'', '&', '<', '>', '+', '\0']),
                "{s:?} -> {escaped:?}"
            );
            assert_eq!(
                crate::escape::escape(&escaped),
                escaped,
                "the page's escaper must be a no-op over a JS string"
            );
        }
    }
}
