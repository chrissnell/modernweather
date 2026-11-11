//! [`Account`] — the identity and credential half the two HTTP uploaders share.
//!
//! `ID`, `PASSWORD`, `dateutc`, `action` and `softwaretype` are the same five parameters in
//! the same places for Weather Underground and PWS Weather, because both are the same
//! WeatherUnderground-derived protocol. They are shared and the **measurements are not**:
//! `docs/service-architecture.md` §9.3 names the Go defect as "pwsweather and wunderground
//! are line-for-line identical because there was no shared shape for *a thing that uploads
//! a windowed summary on a timer*", and the shared shape is `crate::Uploader` plus this —
//! not a common field list, because the field lists genuinely differ (`_avg2m` is Weather
//! Underground's and `monthrainin` is PWS Weather's).
//!
//! # `dateutc` is the resolution instant, not the wall clock
//!
//! Both Go uploaders send `time.Now()`. What they are sending is a window, so the honest
//! timestamp is the instant `window:rollup` last refreshed — `Resolved::as_of`, which
//! `crate::encoder::Observation::as_of` carries. The two differ by up to one rollup period,
//! and sending the wall clock tells the service a one-minute-old average was measured now.

use chrono::{DateTime, Utc};
use secrecy::{ExposeSecret, SecretString};

use crate::form::Form;

/// The format both services document for `dateutc`: `YYYY-MM-DD HH:MM:SS`, UTC.
///
/// The space is percent-encoded by `Form`, so it reaches the wire as `%20`.
const DATEUTC: &str = "%Y-%m-%d %H:%M:%S";

/// One station's identity and credential at one upload service.
///
/// `station_id` is the **pairing's** `station_service.remote_station_id`, not the account's:
/// one account can carry several stations, each with its own identifier at the far end
/// (that column's own comment in `migrations/config/`). The credential is the account's.
#[derive(Debug, Clone)]
pub struct Account {
    station_id: String,
    api_key: SecretString,
    software: String,
}

impl Account {
    /// Build an account. `crate::plan` is the only caller outside tests.
    pub fn new(
        station_id: impl Into<String>,
        api_key: SecretString,
        software: impl Into<String>,
    ) -> Self {
        Self {
            station_id: station_id.into(),
            api_key,
            software: software.into(),
        }
    }

    /// This station's identifier at the far end. Not a credential — it is what the service
    /// shows on a public page.
    pub fn station_id(&self) -> &str {
        &self.station_id
    }

    /// Write the five shared parameters that precede the measurements.
    ///
    /// The credential is exposed **here and nowhere else**: `Form`'s own `Debug` redacts
    /// (**D-33**), so the one place the secret leaves `secrecy` is the line below and the
    /// value it produces cannot be printed by accident afterwards.
    pub fn preamble(&self, form: &mut Form, as_of: DateTime<Utc>) {
        form.set("ID", &self.station_id);
        form.set("PASSWORD", self.api_key.expose_secret());
        form.set("dateutc", &as_of.format(DATEUTC).to_string());
        // Both services document `updateraw` as the only action for a station upload. The
        // Go uploaders also send `realtime=1&rtfreq=2.5`, which claims a 2.5-second update
        // rate on a component whose floor is 60 s -- so the claim is false by between one
        // and two orders of magnitude, and Weather Underground's rapid-fire endpoint is a
        // different host from the one the configuration names. Not carried forward.
        form.set("action", "updateraw");
    }

    /// Write the software identifier, which goes after the measurements.
    pub fn software(&self, form: &mut Form) {
        form.set("softwaretype", &self.software);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("an RFC 3339 instant")
    }

    fn account() -> Account {
        Account::new(
            "KWABACKYARD",
            SecretString::from("hunter2"),
            "modernweather-1.0",
        )
    }

    #[test]
    fn the_preamble_is_the_five_parameters_both_services_document() {
        let mut form = Form::new();
        let a = account();
        a.preamble(&mut form, at("2026-11-11T05:00:00Z"));
        a.software(&mut form);
        assert_eq!(
            form.as_str(),
            "ID=KWABACKYARD&PASSWORD=hunter2&dateutc=2026-11-11%2005%3A00%3A00\
             &action=updateraw&softwaretype=modernweather-1.0"
        );
        // No measurements: the preamble is identity, not data, which is what makes
        // `EncodeError::NothingToSend` decidable.
        assert_eq!(form.measurements(), 0);
        // The Go uploaders' false real-time claim is not here.
        assert!(!form.as_str().contains("realtime"), "{}", form.as_str());
        assert!(!form.as_str().contains("rtfreq"), "{}", form.as_str());
    }

    /// The credential leaves `secrecy` in exactly one place and cannot be printed
    /// afterwards.
    #[test]
    fn the_credential_is_not_printable_once_it_is_in_the_form() {
        let mut form = Form::new();
        account().preamble(&mut form, at("2026-11-11T05:00:00Z"));
        assert!(form.as_str().contains("PASSWORD=hunter2"));
        assert!(!format!("{form:?}").contains("hunter2"));
        assert!(!format!("{:?}", account()).contains("hunter2"));
    }

    /// `dateutc` is the instant the windows are as of, so a caller cannot pass a clock.
    #[test]
    fn dateutc_is_the_resolution_instant() {
        let mut form = Form::new();
        account().preamble(&mut form, at("2026-03-08T02:30:00Z"));
        assert!(
            form.as_str().contains("dateutc=2026-03-08%2002%3A30%3A00"),
            "{}",
            form.as_str()
        );
    }
}
