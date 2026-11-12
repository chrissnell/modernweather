//! The alerts response, and the rows it becomes.
//!
//! # Why this does not reproduce the Go struct, where [`super::forecast`] does
//!
//! The two modules make opposite calls about the same question and the reason is which
//! contract reads the result.
//!
//! `forecasts.data` is served **verbatim** by `/forecast` (**D-39**), so its key set and
//! its number formatting are a frozen response body and the Go struct's eighteen fields are
//! the specification. `alerts.data` is read by **nothing**: `docs/reconciliation.md` §6.3
//! retired `/alerts` and **A-M6-3-1** confirms that only the components are in scope. So
//! there is no body to be faithful to, and the choice is between the Go struct's subset and
//! the payload as fetched.
//!
//! **The payload as fetched wins, because the other direction is irreversible.** Go's
//! `AerisAlertData` declares ten nested structs and drops every upstream field they do not
//! name; among the fields it drops are `includes.counties`, `place.name` and the polygon —
//! the three a management UI showing "which of your counties is under this warning" would
//! want first. Storing them costs a `jsonb` column that is already there and nothing else;
//! not storing them means the data is gone for every alert that was fetched before someone
//! asked. Reproducing a subset to match a table nobody reads would be fidelity to an
//! accident.
//!
//! What **is** reproduced exactly is the seven promoted columns, because those are the ones
//! a reader would compare against the Go table: the four `details` strings and the three
//! timestamps, each with Go's own `> 0` guard so an absent timestamp is a NULL and not the
//! epoch.

use mw_component::Alert;

use crate::error::AerisError;

/// Turn one alerts response into the rows `AlertStore::store_alerts` takes.
///
/// An **empty list is a success**, and the ordinary one: most stations have no active alert
/// most of the time. The trait's own documentation has why that must not be a delete.
///
/// Unlike the forecast endpoint, `response` here is the array of alerts itself rather than a
/// one-element array of groups — which is the vendor's shape and the reason the two
/// modules do not share an envelope type. A `success: true` with no `response` key at all is
/// the no-alerts answer and is not an error.
pub fn alerts(body: &[u8]) -> Result<Vec<Alert>, AerisError> {
    let envelope: serde_json::Value =
        serde_json::from_slice(body).map_err(|source| AerisError::Decode { source })?;
    if envelope.get("success") != Some(&serde_json::Value::Bool(true)) {
        return Err(AerisError::Refused {
            detail: super::forecast::render_error(envelope.get("error")),
        });
    }
    let found = match envelope.get("response") {
        // No key, or an explicit null: no alerts. The vendor sends both.
        None | Some(serde_json::Value::Null) => return Ok(Vec::new()),
        Some(serde_json::Value::Array(items)) => items,
        Some(other) => {
            return Err(AerisError::Shape {
                what: "the alerts response's `response` field",
                found: kind(other),
            })
        }
    };
    let mut out: Vec<Alert> = Vec::with_capacity(found.len());
    for item in found {
        // An element that cannot be **keyed** is dropped and the rest are kept: a warning
        // missing its `id` could not be upserted and would be re-inserted on every fetch.
        // An element that is not an object at all fails the fetch, because that is a
        // disagreement about the schema rather than one bad row. Neither is counted
        // anywhere, which the caller's log line is honest about: it reports what was
        // written.
        let Some(alert) = row(item)? else {
            continue;
        };
        // **Last occurrence wins, and the de-duplication is load-bearing.**
        // `AlertStore::store_alerts` writes the whole list as one
        // `INSERT ... SELECT ... FROM UNNEST(...) ... ON CONFLICT (station_id, alert_id)
        // DO UPDATE`, and PostgreSQL raises `21000: ON CONFLICT DO UPDATE command cannot
        // affect row a second time` when one statement's own source rows collide on the
        // arbiter -- which no retry recovers. The vendor's `id` is the vendor's, and a
        // point inside two overlapping wxzones for one warning is how it repeats; Go wrote
        // per alert in a loop, so a repeat was simply an update. Last-wins reproduces that
        // loop's outcome.
        match out.iter_mut().find(|held| held.alert_id == alert.alert_id) {
            Some(held) => *held = alert,
            None => out.push(alert),
        }
    }
    Ok(out)
}

/// One `response[]` element as a row, or `None` for one that cannot be keyed.
fn row(item: &serde_json::Value) -> Result<Option<Alert>, AerisError> {
    let serde_json::Value::Object(_) = item else {
        return Err(AerisError::Shape {
            what: "an element of the alerts response",
            found: kind(item),
        });
    };
    let Some(alert_id) = item.get("id").and_then(serde_json::Value::as_str) else {
        // No `id`, so no natural key: the row could not be upserted and would be inserted
        // afresh on every fetch. Dropped, and the caller's count is what makes it visible.
        return Ok(None);
    };
    if alert_id.is_empty() {
        return Ok(None);
    }
    let details = item.get("details");
    let stamps = item.get("timestamps");
    Ok(Some(Alert {
        alert_id: alert_id.to_string(),
        issued_at: stamp(stamps, "issued"),
        begins_at: stamp(stamps, "begins"),
        expires_at: stamp(stamps, "expires"),
        name: string(details, "name"),
        color: string(details, "color"),
        body: string(details, "body"),
        body_full: string(details, "bodyFull"),
        // The whole element, re-rendered. Re-rendered rather than sliced out of the input
        // because the input is one array and this is one element of it; the numeric
        // formatting that `super::forecast` is careful about does not matter here, for the
        // reason the module documentation gives.
        data: item.to_string(),
    }))
}

/// One `timestamps.<field>`, as Go reads it: Unix **seconds**, and only when positive.
///
/// `if alertData.Timestamps.Issued > 0 { t := time.Unix(...); issuedAt = &t }`. The guard
/// is the whole of why this is not `From<i64>`: the field is a non-pointer `int64` in Go, so
/// an absent key is `0`, and `0` is 1 January 1970 — a NULL and an alert issued at the epoch
/// are the same bytes on the wire and very different facts in the table.
///
/// The ISO sibling fields (`issuedISO`, `expiresISO`) are deliberately not read. They carry
/// the same instant in the alert's local zone, so reading both would be two answers to one
/// question, and the epoch form is the one Go stores.
fn stamp(stamps: Option<&serde_json::Value>, field: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let seconds = stamps?.get(field)?.as_i64()?;
    if seconds <= 0 {
        return None;
    }
    chrono::DateTime::from_timestamp(seconds, 0)
}

/// One `details.<field>`, or `""` — which is Go's zero value for the same absent key.
fn string(details: Option<&serde_json::Value>, field: &str) -> String {
    details
        .and_then(|d| d.get(field))
        .and_then(serde_json::Value::as_str)
        .map_or_else(String::new, str::to_string)
}

/// A JSON value's type name, for a refusal that says what it found.
fn kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE_ALERT: &str = r#"{"success":true,"error":null,"response":[{
        "id":"a1b2c3","dataSource":"NWS",
        "details":{"type":"WS.W","name":"Winter Storm Warning","color":"FF69B4",
                   "body":"Heavy snow expected.","bodyFull":"...the whole product..."},
        "timestamps":{"issued":1762900000,"issuedISO":"2026-11-11T17:00:00-07:00",
                      "begins":1762920000,"expires":1763000000,"updated":0},
        "includes":{"counties":["Boulder"]},
        "place":{"name":"boulder","state":"co"}}]}"#;

    #[test]
    fn the_seven_promoted_fields_come_off_the_payload() {
        let rows = alerts(ONE_ALERT.as_bytes()).expect("a successful response");
        assert_eq!(rows.len(), 1);
        let a = &rows[0];
        assert_eq!(a.alert_id, "a1b2c3");
        assert_eq!(a.name, "Winter Storm Warning");
        assert_eq!(a.color, "FF69B4");
        assert_eq!(a.body, "Heavy snow expected.");
        assert_eq!(a.body_full, "...the whole product...");
        assert_eq!(a.issued_at, chrono::DateTime::from_timestamp(1762900000, 0));
        assert_eq!(a.begins_at, chrono::DateTime::from_timestamp(1762920000, 0));
        assert_eq!(
            a.expires_at,
            chrono::DateTime::from_timestamp(1763000000, 0)
        );
    }

    /// The module's one deliberate departure from the Go row: the fields Go's struct drops
    /// are kept, because nothing reads this column and losing them is the irreversible
    /// direction.
    #[test]
    fn the_stored_payload_keeps_what_the_go_struct_would_have_dropped() {
        let rows = alerts(ONE_ALERT.as_bytes()).expect("a successful response");
        let data: serde_json::Value =
            serde_json::from_str(&rows[0].data).expect("the stored text is JSON");
        assert_eq!(data["includes"]["counties"][0], "Boulder");
        assert_eq!(data["place"]["name"], "boulder");
        assert!(
            data.get("success").is_none(),
            "the element, not the envelope"
        );
    }

    /// Go's `> 0` guard: an absent timestamp is a NULL and not 1 January 1970.
    #[test]
    fn a_zero_timestamp_is_absent_rather_than_the_epoch() {
        let body = r#"{"success":true,"response":[{"id":"x",
            "timestamps":{"issued":0,"begins":0,"expires":0}}]}"#;
        let rows = alerts(body.as_bytes()).expect("a successful response");
        assert_eq!(rows[0].issued_at, None);
        assert_eq!(rows[0].begins_at, None);
        assert_eq!(rows[0].expires_at, None, "and so is never swept");
    }

    #[test]
    fn no_alerts_is_a_success_and_not_an_error() {
        for body in [
            r#"{"success":true,"response":[]}"#,
            r#"{"success":true,"response":null}"#,
            r#"{"success":true}"#,
        ] {
            let rows = alerts(body.as_bytes()).expect("a successful response");
            assert!(rows.is_empty(), "{body}");
        }
    }

    /// A repeated `id` in one response is one row, not a statement PostgreSQL refuses.
    ///
    /// The whole list becomes one `INSERT ... ON CONFLICT ... DO UPDATE`, and a source-row
    /// collision on the arbiter is `21000`, unrecoverable by retry — so every alert in that
    /// fetch would be lost, every fifteen minutes, for as long as the warning was active.
    #[test]
    fn a_repeated_alert_id_is_one_row_with_the_last_values() {
        let body = r#"{"success":true,"response":[
            {"id":"dup","details":{"name":"first"}},
            {"id":"other","details":{"name":"unaffected"}},
            {"id":"dup","details":{"name":"second"}}]}"#;
        let rows = alerts(body.as_bytes()).expect("a successful response");
        let ids: Vec<&str> = rows.iter().map(|a| a.alert_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["dup", "other"],
            "one row per id, in first-seen order"
        );
        assert_eq!(rows[0].name, "second", "the last occurrence wins");
    }

    #[test]
    fn an_alert_with_no_id_is_dropped_and_the_rest_are_kept() {
        let body = r#"{"success":true,"response":[
            {"details":{"name":"no id"}},
            {"id":"","details":{"name":"blank id"}},
            {"id":"keeps","details":{"name":"fine"}}]}"#;
        let rows = alerts(body.as_bytes()).expect("a successful response");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].alert_id, "keeps");
    }

    #[test]
    fn a_successless_body_is_refused_with_the_upstream_s_reason() {
        let e = alerts(br#"{"success":false,"error":"no soup for you"}"#).expect_err("refused");
        let AerisError::Refused { detail } = &e else {
            panic!("expected a refusal, got {e:?}");
        };
        assert_eq!(detail, "no soup for you");
    }

    #[test]
    fn a_response_that_is_not_an_array_is_a_shape_failure() {
        let e = alerts(br#"{"success":true,"response":{"id":"x"}}"#).expect_err("not an array");
        assert!(matches!(e, AerisError::Shape { .. }), "{e:?}");
    }
}
