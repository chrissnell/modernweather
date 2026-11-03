//! Pretty for humans, structured for machines -- the detection table of §5.3, as a pure
//! function of its four inputs.
//!
//! Pure on purpose. "Which format did we pick, and why" is the one thing about logging an
//! operator asks before anything is logged, and a detection that reads the environment
//! inside itself can only be tested by mutating the process environment -- which is
//! shared, which makes the test order-dependent. [`Shape::resolve`] takes the four facts
//! and returns the answer; [`crate::stderr`] is the only thing that gathers them.

/// What the operator asked for, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requested {
    /// Aligned fields for a human.
    Pretty,
    /// One JSON object per line, for journald, Docker or a pipe.
    Json,
}

impl Requested {
    /// The accepted spellings, for the error message when neither matched.
    pub const SPELLINGS: &'static str = "pretty, json";

    /// Parse a `--log-format` argument or an `MW_LOG_FORMAT` value.
    ///
    /// Returns `None` for anything else, which the caller reports rather than silently
    /// treating as "detect": a typo in `MW_LOG_FORMAT` that fell back to detection would
    /// be invisible, and the whole point of an override is that the operator is telling
    /// us the detection got it wrong.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pretty" => Some(Requested::Pretty),
            "json" => Some(Requested::Json),
            _ => None,
        }
    }
}

/// The resolved format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Aligned fields, with ANSI when the terminal and the environment both allow it.
    Pretty {
        /// Whether to emit ANSI escapes.
        ansi: bool,
    },
    /// One JSON object per line, `flatten_event(true)`.
    Json,
}

impl Shape {
    /// The chosen format in one word, for the startup line that reports it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Shape::Pretty { ansi: true } => "pretty+ansi",
            Shape::Pretty { ansi: false } => "pretty",
            Shape::Json => "json",
        }
    }

    /// §5.3's table, in the order its rows are written.
    ///
    /// The two colour variables do **not** change the shape, only whether a `Pretty`
    /// stream carries escapes. That is the narrow reading of the table and it is the
    /// useful one: `MW_LOG_FORMAT=pretty` through `less -R` with `CLICOLOR_FORCE=1` is
    /// the case the last row exists for, while `CLICOLOR_FORCE=1` on a pipe with no
    /// requested shape would otherwise have to mean "ANSI escapes inside JSON strings".
    pub fn resolve(
        requested: Option<Requested>,
        stderr_is_terminal: bool,
        no_color: bool,
        clicolor_force: bool,
    ) -> Self {
        // CLICOLOR_FORCE wins over NO_COLOR: one says "I know this is not a terminal and
        // I want colour anyway", the other says "assume it is not". The specific
        // instruction beats the general one.
        let ansi = if clicolor_force {
            true
        } else if no_color {
            false
        } else {
            stderr_is_terminal
        };
        match requested {
            Some(Requested::Pretty) => Shape::Pretty { ansi },
            Some(Requested::Json) => Shape::Json,
            None if stderr_is_terminal => Shape::Pretty { ansi },
            None => Shape::Json,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terminal_gets_pretty_with_colour_and_a_pipe_gets_json() {
        assert_eq!(
            Shape::resolve(None, true, false, false),
            Shape::Pretty { ansi: true }
        );
        assert_eq!(Shape::resolve(None, false, false, false), Shape::Json);
    }

    #[test]
    fn no_color_keeps_the_shape_and_drops_the_escapes() {
        assert_eq!(
            Shape::resolve(None, true, true, false),
            Shape::Pretty { ansi: false },
            "NO_COLOR on a terminal is pretty without ANSI, not JSON"
        );
    }

    #[test]
    fn clicolor_force_colours_a_requested_pretty_stream_that_is_not_a_terminal() {
        assert_eq!(
            Shape::resolve(Some(Requested::Pretty), false, false, true),
            Shape::Pretty { ansi: true }
        );
        // And it does not turn a pipe's JSON into escapes.
        assert_eq!(Shape::resolve(None, false, false, true), Shape::Json);
    }

    #[test]
    fn clicolor_force_beats_no_color() {
        assert_eq!(
            Shape::resolve(Some(Requested::Pretty), false, true, true),
            Shape::Pretty { ansi: true }
        );
    }

    #[test]
    fn an_explicit_request_beats_the_detection_in_both_directions() {
        assert_eq!(
            Shape::resolve(Some(Requested::Json), true, false, false),
            Shape::Json
        );
        assert_eq!(
            Shape::resolve(Some(Requested::Pretty), false, false, false),
            Shape::Pretty { ansi: false }
        );
    }

    #[test]
    fn only_the_two_documented_spellings_parse() {
        assert_eq!(Requested::parse("pretty"), Some(Requested::Pretty));
        assert_eq!(Requested::parse("json"), Some(Requested::Json));
        for bad in ["PRETTY", "compact", "", "pretty "] {
            assert_eq!(Requested::parse(bad), None, "{bad} must not parse");
        }
    }
}
