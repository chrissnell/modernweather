//! A cursor: one consumer's position in the log, owned by exactly one task.
//!
//! **There is no shared collection of sinks, and that is the fix for three of the four
//! defects W3 §2.4 measures.** The Go distributor holds `s.Engines`, mutates it without a
//! mutex while iterating it, and `RemoveEngine` closes a channel the distributor may be
//! sending on. Here, `Spool::open_cursor(id)` hands out a [`Cursor`] that one task owns;
//! removal is `Drop`, which releases the exclusive hold and leaves the position on disk;
//! and nobody closes anything anybody else holds.
//!
//! **The position survives `Drop`**, and that is deliberate rather than incidental: a
//! component that failed and is in `Backoff` gets a new `Cursor` on its next start attempt
//! and must resume where it was, because the cursor not advancing is what makes the backoff
//! a retry of the same batch rather than a loss (W3 §2.3).

use std::sync::Arc;

use mw_component::ComponentId;
use mw_types::{Envelope, Seq};

use super::format::CursorState;
use super::{Spool, SpoolError, READ_AHEAD};

/// What a cursor file says, for a metric or an assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPosition {
    /// Everything at or below this local position is durably handled.
    pub acked_seq: Seq,
    /// When that was recorded, unix milliseconds.
    pub acked_at: i64,
    /// Active, or abandoned past `max_lag`.
    pub state: CursorState,
}

/// What one read from the spool returned.
#[derive(Debug, Clone)]
pub struct Read {
    /// The records, oldest first, with their local positions.
    pub envelopes: Vec<(Seq, Envelope)>,
    /// The highest local position the read covered.
    ///
    /// **Not** `envelopes.last()`: a filter applied after the read drops envelopes that
    /// still have to advance the cursor (W7 **D-9**), and this is the number that does it.
    pub through: Seq,
}

/// One consumer's position in the spool.
#[derive(Debug)]
pub struct Cursor {
    spool: Arc<Spool>,
    id: ComponentId,
    /// Where reading resumes. Ahead of the durable `acked_seq` for exactly as long as a
    /// batch is in flight.
    position: Seq,
    acked: Seq,
    state: CursorState,
}

impl Cursor {
    pub(super) fn new(
        spool: Arc<Spool>,
        id: ComponentId,
        position: Seq,
        state: CursorState,
    ) -> Self {
        Self {
            spool,
            id,
            position,
            acked: position,
            state,
        }
    }

    /// Which cursor this is.
    pub fn id(&self) -> &ComponentId {
        &self.id
    }

    /// Whether `max_lag` has fired on this cursor.
    ///
    /// An abandoned cursor's component belongs in `Failed { CursorAbandoned }`: its
    /// segments have been reclaimed, so there is nothing for it to read and no amount of
    /// waiting produces any.
    ///
    /// Reads through to the spool rather than returning a field, because `max_lag` fires in
    /// the spool's own maintenance and a handle that answered from a cached copy would
    /// report `Active` for as long as the subscription was parked — which is precisely the
    /// case `max_lag` exists for.
    pub fn is_abandoned(&self) -> bool {
        self.spool
            .cursor_position(&self.id)
            .map_or(self.state == CursorState::Abandoned, |p| {
                p.state == CursorState::Abandoned
            })
    }

    /// The durable position.
    pub fn acked_seq(&self) -> Seq {
        self.acked
    }

    /// Read the next records after the durable position, up to [`READ_AHEAD`].
    ///
    /// Returns an empty [`Read`] when there is nothing new, which is the signal to await
    /// the tail rather than to spin.
    pub fn read(&mut self) -> Result<Read, SpoolError> {
        self.refresh_state();
        let read = self.spool.read_after(self.position, READ_AHEAD)?;
        if !read.envelopes.is_empty() {
            self.position = read.through;
        }
        Ok(read)
    }

    /// Wait until there is something after the durable position.
    ///
    /// Cancel-safe: it holds nothing and takes nothing from a queue, so a dropped future
    /// loses no envelope. That is the property `ReadingSubscription::next_batch` requires.
    pub async fn wait_for_records(&self) {
        let mut tail = self.spool.watch_tail();
        let _ = tail.wait_for(|t| *t > self.position).await;
    }

    /// Acknowledge everything up to `through`, durably.
    ///
    /// **The only place an offset advances.** `through` is a *local* position, which is why
    /// a filtered envelope can advance it without having been sent (W7 **D-9**): the cursor
    /// is a position in this node's log, not a count of what a consumer liked.
    pub fn ack(&mut self, through: Seq) -> Result<(), SpoolError> {
        if through <= self.acked {
            return Ok(());
        }
        self.spool.ack(&self.id, through)?;
        self.acked = through;
        self.position = self.position.max(through);
        Ok(())
    }

    /// Put the read position back to the durable one, so the unacknowledged batch replays.
    ///
    /// Called when a delivery failed. Without it a failed batch would be skipped on the
    /// next read, which is the one thing a durable sink may not do.
    pub fn rewind(&mut self) {
        self.position = self.acked;
    }

    fn refresh_state(&mut self) {
        if let Some(position) = self.spool.cursor_position(&self.id) {
            self.state = position.state;
            if position.acked_seq > self.acked {
                // Clamped forward by `max_lag` reclamation while this handle was alive.
                self.acked = position.acked_seq;
                self.position = self.position.max(position.acked_seq);
            }
        }
    }
}

impl Drop for Cursor {
    fn drop(&mut self) {
        self.spool.detach(&self.id);
    }
}

/// Where a cursor's file lives.
///
/// A component id like `writer:timescale` becomes `writer-timescale.cur`, because a colon
/// is legal in a component id and awkward in a path.
pub(super) fn file_for(spool: &std::path::Path, id: &ComponentId) -> std::path::PathBuf {
    spool
        .join("cursors")
        .join(format!("{}.cur", id.as_str().replace(':', "-")))
}

/// The inverse of [`file_for`], for recovery.
///
/// Lossy in principle — a component id containing a literal `-` is indistinguishable from
/// one containing a `:` — and that is why the recovered id is only ever used as a **key**:
/// `open_cursor` looks the registry up by the id the component declares, and a recovered
/// row whose spelling does not match is a cursor nobody opens, which pins segments until
/// `max_lag` abandons it rather than advancing someone else's position.
pub(super) fn id_from_file(path: &std::path::Path) -> Option<ComponentId> {
    let stem = path.file_stem()?.to_str()?;
    ComponentId::new(stem.replacen('-', ":", 1)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_component_id_round_trips_through_its_cursor_file_name() {
        let dir = std::path::Path::new("/var/lib/modernweather/spool");
        for text in ["writer:timescale", "uplink:suncrest"] {
            let id = ComponentId::new(text).expect("a valid component id");
            let path = file_for(dir, &id);
            assert_eq!(
                path.file_name().and_then(|n| n.to_str()),
                Some(format!("{}.cur", text.replace(':', "-")).as_str())
            );
            assert_eq!(id_from_file(&path), Some(id));
        }
    }

    /// A stray file in `cursors/` is skipped rather than adopted as somebody's position.
    /// `ComponentId::new` rejects an upper-case character, which is what makes this
    /// answerable at all -- the file-name mapping is not injective, so the only safe
    /// reading of a name is "a key, or nothing".
    #[test]
    fn a_file_that_is_not_a_component_id_is_not_mistaken_for_one() {
        for name in ["README.cur", "Writer-Timescale.cur", "writer timescale.cur"] {
            let path = std::path::PathBuf::from("/spool/cursors").join(name);
            assert_eq!(id_from_file(&path), None, "{name}");
        }
    }
}
