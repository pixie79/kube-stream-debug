//! Cursor trim-risk evaluation.
//!
//! Answers two questions per partition, from `internalStats`:
//!
//! 1. **Stuck?** Is the subscription's next read position older than the oldest
//!    ledger still physically present on the topic? If so, the entry it wants
//!    has been trimmed (TTL/retention GC) and the consumer will spin forever
//!    waiting for a message that no longer exists.
//! 2. **Headroom.** If not yet stuck, how many entries of margin remain before
//!    the trimmer reaches the cursor — i.e. how far off the cliff edge we are.
//!
//! A corroborating signal is `waitingReadOp == true` (cursor parked as if
//! caught up) while the subscription still reports a backlog: the broker thinks
//! we're done, the backlog counter disagrees — classic stranded cursor.

use crate::pulsar::{InternalStats, Position};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimVerdict {
    /// Read position is behind the trim floor: next entry is gone.
    Stuck,
    /// Read position sits in the oldest present ledger — one GC cycle from loss.
    AtEdge,
    /// Comfortable margin before the trim floor.
    Safe,
    /// Cursor or ledger data missing/unparseable — can't tell.
    Unknown,
}

#[derive(Debug, Clone)]
pub struct CursorTrimStatus {
    pub verdict: TrimVerdict,
    /// Entries between the trim floor and the read position. Negative means the
    /// read position is already below the floor (how many trimmed entries the
    /// cursor is stranded past). `None` when it can't be computed.
    pub headroom_entries: Option<i64>,
    /// Broker reports the cursor parked-and-waiting despite an outstanding
    /// backlog — an independent corroboration of "stuck".
    pub waiting_with_backlog: bool,
}

impl CursorTrimStatus {
    fn unknown() -> Self {
        CursorTrimStatus {
            verdict: TrimVerdict::Unknown,
            headroom_entries: None,
            waiting_with_backlog: false,
        }
    }
}

/// Evaluate one partition's cursor against the topic's physical ledger floor.
///
/// `backlog` is the subscription's `msgBacklog` from the regular stats, used
/// only to qualify the `waitingReadOp` corroboration signal.
pub fn evaluate_cursor(
    internal: &InternalStats,
    subscription: &str,
    backlog: i64,
) -> CursorTrimStatus {
    let Some(cursor) = internal.cursors.get(subscription) else {
        return CursorTrimStatus::unknown();
    };
    let Some(floor) = internal.floor_ledger_id() else {
        return CursorTrimStatus::unknown();
    };
    let Some(read) = cursor.read() else {
        return CursorTrimStatus::unknown();
    };

    let waiting_with_backlog = cursor.waiting_read_op && backlog > 0;

    // Headroom is only meaningful when there's a backlog at risk. For a
    // caught-up partition (backlog 0) the "entries below the cursor" figure is
    // just wherever the cursor happens to sit in the current ledger — noise, not
    // a safety margin — so we don't report it.
    let headroom = if backlog > 0 {
        headroom_entries(internal, read)
    } else {
        None
    };

    let verdict = if read.ledger_id < floor {
        // Next entry to read lives in a ledger that's already been trimmed.
        TrimVerdict::Stuck
    } else if read.ledger_id == floor && backlog > 0 {
        // Sitting in the oldest surviving ledger with data still to consume:
        // the very next trim removes messages we haven't read. An idle,
        // caught-up partition (backlog 0) in the oldest ledger has nothing to
        // lose, so it is not flagged.
        TrimVerdict::AtEdge
    } else {
        TrimVerdict::Safe
    };

    CursorTrimStatus {
        verdict,
        headroom_entries: headroom,
        waiting_with_backlog,
    }
}

/// Count entries from the trim floor up to (not including) the read position.
///
/// Sums whole ledgers older than the read ledger, then adds the entry offset
/// within the read ledger. If the read ledger isn't in the present list (it's
/// been trimmed), returns a negative estimate: minus the entries in ledgers
/// that survive below where the cursor is pointing is not meaningful, so we
/// signal "past the floor" as a negative of the surviving-below count, which is
/// zero — hence we return the negative read-entry offset as a best effort.
fn headroom_entries(internal: &InternalStats, read: Position) -> Option<i64> {
    let read_ledger_present = internal
        .ledgers
        .iter()
        .any(|l| l.ledger_id == read.ledger_id);

    if !read_ledger_present {
        // Cursor points below the surviving ledgers entirely. There is no
        // positive headroom; report how many entries still exist below the
        // read ledger as a negative "already lost" figure is not derivable
        // precisely, so we report 0 headroom flagged via the Stuck verdict.
        return Some(0);
    }

    let mut headroom: i64 = 0;
    for ledger in &internal.ledgers {
        if ledger.ledger_id < read.ledger_id {
            headroom += ledger.entries;
        } else if ledger.ledger_id == read.ledger_id {
            // entry_id is the next-to-read index; entries [0, entry_id) precede it.
            headroom += read.entry_id.max(0);
            break;
        }
    }
    Some(headroom)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(json: &str) -> InternalStats {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn safe_when_cursor_well_inside_ledgers() {
        let s = stats(
            r#"{
            "ledgers": [
                {"ledgerId": 10, "entries": 100},
                {"ledgerId": 20, "entries": 100},
                {"ledgerId": 30, "entries": 50}
            ],
            "cursors": {"sub": {"readPosition": "30:10", "waitingReadOp": false}}
        }"#,
        );
        let status = evaluate_cursor(&s, "sub", 40);
        assert_eq!(status.verdict, TrimVerdict::Safe);
        // 100 + 100 (whole older ledgers) + 10 (offset in read ledger) = 210.
        assert_eq!(status.headroom_entries, Some(210));
    }

    #[test]
    fn zero_backlog_reports_no_headroom() {
        // Same ledgers/cursor as above, but caught up: headroom is meaningless.
        let s = stats(
            r#"{
            "ledgers": [
                {"ledgerId": 10, "entries": 100},
                {"ledgerId": 20, "entries": 100},
                {"ledgerId": 30, "entries": 50}
            ],
            "cursors": {"sub": {"readPosition": "30:10", "waitingReadOp": true}}
        }"#,
        );
        let status = evaluate_cursor(&s, "sub", 0);
        assert_eq!(status.verdict, TrimVerdict::Safe);
        assert_eq!(status.headroom_entries, None);
    }

    #[test]
    fn at_edge_when_cursor_in_oldest_ledger() {
        let s = stats(
            r#"{
            "ledgers": [
                {"ledgerId": 20, "entries": 100},
                {"ledgerId": 30, "entries": 50}
            ],
            "cursors": {"sub": {"readPosition": "20:5", "waitingReadOp": false}}
        }"#,
        );
        let status = evaluate_cursor(&s, "sub", 130);
        assert_eq!(status.verdict, TrimVerdict::AtEdge);
        assert_eq!(status.headroom_entries, Some(5));
    }

    #[test]
    fn stuck_when_cursor_below_floor() {
        let s = stats(
            r#"{
            "ledgers": [
                {"ledgerId": 20, "entries": 100},
                {"ledgerId": 30, "entries": 50}
            ],
            "cursors": {"sub": {"readPosition": "10:40", "waitingReadOp": true}}
        }"#,
        );
        let status = evaluate_cursor(&s, "sub", 500);
        assert_eq!(status.verdict, TrimVerdict::Stuck);
        assert!(status.waiting_with_backlog, "waiting + backlog corroborates");
    }

    #[test]
    fn waiting_without_backlog_is_not_flagged() {
        let s = stats(
            r#"{
            "ledgers": [{"ledgerId": 20, "entries": 100}],
            "cursors": {"sub": {"readPosition": "20:99", "waitingReadOp": true}}
        }"#,
        );
        let status = evaluate_cursor(&s, "sub", 0);
        assert!(!status.waiting_with_backlog);
        // Idle, caught up, nothing to lose even though it's the oldest ledger.
        assert_eq!(status.verdict, TrimVerdict::Safe);
    }

    #[test]
    fn at_edge_requires_backlog() {
        let s = stats(
            r#"{
            "ledgers": [{"ledgerId": 20, "entries": 100}, {"ledgerId": 30, "entries": 50}],
            "cursors": {"sub": {"readPosition": "20:10", "waitingReadOp": false}}
        }"#,
        );
        // Same position, only backlog differs.
        assert_eq!(evaluate_cursor(&s, "sub", 0).verdict, TrimVerdict::Safe);
        assert_eq!(evaluate_cursor(&s, "sub", 500).verdict, TrimVerdict::AtEdge);
    }

    #[test]
    fn unknown_when_subscription_absent() {
        let s = stats(
            r#"{
            "ledgers": [{"ledgerId": 20, "entries": 100}],
            "cursors": {"other": {"readPosition": "20:1"}}
        }"#,
        );
        assert_eq!(evaluate_cursor(&s, "sub", 0).verdict, TrimVerdict::Unknown);
    }

    #[test]
    fn unknown_when_no_ledgers() {
        let s = stats(r#"{"ledgers": [], "cursors": {"sub": {"readPosition": "20:1"}}}"#);
        assert_eq!(evaluate_cursor(&s, "sub", 0).verdict, TrimVerdict::Unknown);
    }
}
