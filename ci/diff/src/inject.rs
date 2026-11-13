//! The negative control: make one metric wrong by a known amount and require the harness to
//! say so.
//!
//! W1 **D-10**, which M5-2's test strategy restates: *a differ that cannot be made to differ
//! proves nothing*. A clean run of (b) or (c) is only evidence if a dirty run would have been
//! red, and the only way to establish that is to dirty one on purpose.
//!
//! The injection is applied to the loaded data, **after** both sides have been read and
//! **before** the comparison -- so it passes through the real pairing, the real flattening and
//! the real rule set. A control that compared two hand-written numbers would test the
//! arithmetic in `numeric.rs` and nothing else; this one fails if the pairing silently drops
//! the reading, if the key never reaches the rule set, or if a rule is wider than it looks.
//!
//! 1% is the figure M5-2 names, and it is four orders of magnitude outside DEV-4's bound at
//! production's bucket counts -- so a rule written to excuse an accumulator's last bit cannot
//! absorb it. `--inject period_rain:1:rust` is therefore a test of the rule as well as of the
//! differ: if DEV-4's bound ever grew wide enough to swallow a 1% error, this control is what
//! would go red.

use serde_json::Value as Json;

/// Which side to spoil. The control is run both ways by `self-test.sh`, because a differ that
/// only notices the new side being wrong would pass every test and miss half the failures it
/// exists for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Go,
    Rust,
}

#[derive(Debug, Clone)]
pub struct Injection {
    /// The metric column, or the JSON key -- which are the same name on every metric the
    /// frozen contract and the hypertable share.
    pub key: String,
    /// Relative error, as a percentage.
    pub percent: f64,
    pub side: Side,
}

impl Injection {
    /// `<key>:<percent>[:go|:rust]`, e.g. `period_rain:1:rust`.
    pub fn parse(spec: &str) -> Result<Injection, String> {
        let parts: Vec<&str> = spec.split(':').collect();
        if parts.len() < 2 || parts.len() > 3 {
            return Err(format!("{spec}: expected <key>:<percent>[:go|rust]"));
        }
        let percent: f64 = parts[1]
            .parse()
            .map_err(|_| format!("{spec}: `{}` is not a percentage", parts[1]))?;
        if percent == 0.0 {
            return Err(format!(
                "{spec}: a 0% injection is not a control -- it would make a passing run pass"
            ));
        }
        let side = match parts.get(2).copied() {
            None | Some("rust") => Side::Rust,
            Some("go") => Side::Go,
            Some(other) => return Err(format!("{spec}: no such side `{other}`")),
        };
        Ok(Injection {
            key: parts[0].to_string(),
            percent,
            side,
        })
    }

    /// How the report names it.
    pub fn describe(&self) -> String {
        format!(
            "INJECTED: {} scaled by {:+}% on the {} side",
            self.key,
            self.percent,
            match self.side {
                Side::Go => "Go",
                Side::Rust => "Rust",
            }
        )
    }

    fn scale(&self) -> f64 {
        1.0 + self.percent / 100.0
    }

    /// Spoil every occurrence of the key in a response body, at any depth.
    ///
    /// Every element of a `/span` array, not just the first: an injection that touched one
    /// bucket would be a control for "the harness reports at least one difference", and the
    /// property under test is that it reports the difference wherever it is.
    /// Returns how many values actually moved. A value of zero scales to itself, so an
    /// injection into a key that is zero through the whole corpus moves nothing -- and an
    /// injection that moved nothing is not a control, which is why the count is returned
    /// rather than discarded.
    pub fn apply_json(&self, v: &mut Json) -> usize {
        let mut moved = 0;
        match v {
            Json::Object(map) => {
                for (k, child) in map.iter_mut() {
                    if *k == self.key {
                        if let Some(f) = child.as_f64() {
                            let scaled = f * self.scale();
                            if scaled != f {
                                if let Some(n) = serde_json::Number::from_f64(scaled) {
                                    *child = Json::Number(n);
                                    moved += 1;
                                    continue;
                                }
                            }
                        }
                    }
                    moved += self.apply_json(child);
                }
            }
            Json::Array(items) => {
                for child in items.iter_mut() {
                    moved += self.apply_json(child);
                }
            }
            _ => {}
        }
        moved
    }

    /// Spoil one column of a loaded reading set.
    ///
    /// A NULL is left alone. Scaling an absent metric would turn the injection into a
    /// NULL-versus-zero difference, which is the one class (b) is supposed to pass -- so the
    /// control would be testing the opposite of what it claims.
    pub fn apply_rows(&self, columns: &[String], rows: &mut [crate::readings::Reading]) -> usize {
        let Some(at) = columns.iter().position(|c| *c == self.key) else {
            return 0;
        };
        let mut touched = 0;
        for row in rows.iter_mut() {
            if let Some(f) = row.vals[at].num() {
                let scaled = f * self.scale();
                // Only a value that actually moved counts. A metric that is zero throughout
                // the window scales to itself, and a control that changed nothing has to be
                // visible in the report rather than read as a clean run.
                if scaled != f {
                    row.vals[at] = crate::value::Val::Num {
                        text: format!("{scaled}"),
                        f: scaled,
                    };
                    touched += 1;
                }
            }
        }
        touched
    }
}
