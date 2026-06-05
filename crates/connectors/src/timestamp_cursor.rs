//! Shared helpers for timestamp-keyed incremental cursors that also
//! remember which records were emitted at the exact boundary instant.
//!
//! Connectors whose provider has no opaque server cursor (Zoom, Google
//! Meet) page incremental syncs off `max(timestamp)` from the previous
//! run and keep only records strictly after it. A plain `>` filter is
//! correct for the boundary record itself, but if a *second* record
//! shares the exact same sub-second timestamp and only becomes visible
//! in a later run, the strict comparison drops it forever.
//!
//! These helpers serialize the boundary instant together with the ids
//! already emitted at that instant, so a later record sharing the same
//! timestamp is still emitted — exactly once. The wire format stays
//! backward compatible with a bare RFC3339 timestamp:
//! `<rfc3339>` (no boundary ids) or `<rfc3339>|id1,id2,...`.
//!
//! The provider ids used here (Zoom meeting UUIDs, Meet resource names)
//! are base64 / path-segment strings that never contain `,` or `|`, so
//! those characters are safe delimiters.

use chrono::{DateTime, Utc};

/// A decoded incremental cursor: the high-watermark instant plus the
/// ids already emitted whose timestamp equals that instant.
pub struct WatermarkCursor {
    /// Highest record timestamp emitted so far.
    pub watermark: DateTime<Utc>,
    /// Ids of records already emitted whose timestamp equals `watermark`.
    pub seen_ids: Vec<String>,
}

impl WatermarkCursor {
    /// Whether a record at `ts` identified by `id` is new (not yet
    /// emitted). Records strictly after the watermark are always new;
    /// records exactly at the watermark are new only if their id was
    /// not already emitted at that instant; earlier records were
    /// emitted on a previous run.
    #[must_use]
    pub fn is_new(&self, ts: DateTime<Utc>, id: &str) -> bool {
        ts > self.watermark || (ts == self.watermark && !self.seen_ids.iter().any(|s| s == id))
    }
}

/// Decode a cursor string. Backward compatible with a plain RFC3339
/// timestamp produced before boundary ids were tracked.
///
/// # Errors
/// Returns the [`chrono::ParseError`] if the timestamp portion is not a
/// valid RFC3339 instant.
pub fn decode(cursor: &str) -> Result<WatermarkCursor, chrono::ParseError> {
    let (ts_part, ids_part) = cursor.split_once('|').unwrap_or((cursor, ""));
    let watermark = DateTime::parse_from_rfc3339(ts_part)?.with_timezone(&Utc);
    let seen_ids = if ids_part.is_empty() {
        Vec::new()
    } else {
        ids_part.split(',').map(str::to_string).collect()
    };
    Ok(WatermarkCursor {
        watermark,
        seen_ids,
    })
}

/// Seed a fresh cursor from the `(timestamp, id)` pairs emitted by an
/// initial sync. Returns `None` when no record carried a timestamp
/// (mirroring `max()` over an empty set), otherwise the high-watermark
/// instant plus the ids at that instant — so the first incremental run
/// neither re-emits the boundary record nor skips a later record that
/// shares its exact timestamp.
pub fn seed<'a>(emitted: impl IntoIterator<Item = (DateTime<Utc>, &'a str)>) -> Option<String> {
    let emitted: Vec<(DateTime<Utc>, &str)> = emitted.into_iter().collect();
    let watermark = emitted.iter().map(|(ts, _)| *ts).max()?;
    let mut ids: Vec<String> = Vec::new();
    for (ts, id) in &emitted {
        if *ts == watermark && !ids.iter().any(|s| s == id) {
            ids.push((*id).to_string());
        }
    }
    Some(if ids.is_empty() {
        watermark.to_rfc3339()
    } else {
        format!("{}|{}", watermark.to_rfc3339(), ids.join(","))
    })
}

/// Encode the next cursor from the previous decoded cursor and the
/// `(timestamp, id)` pairs emitted this run.
///
/// The new watermark is the maximum of the previous watermark and every
/// emitted timestamp. The boundary id set is every id whose timestamp
/// equals that new watermark, carrying forward the previous set when the
/// watermark does not advance so ties accumulate across runs.
pub fn encode<'a>(
    previous: &WatermarkCursor,
    emitted: impl IntoIterator<Item = (DateTime<Utc>, &'a str)>,
) -> String {
    let emitted: Vec<(DateTime<Utc>, &str)> = emitted.into_iter().collect();
    let new_watermark = emitted
        .iter()
        .map(|(ts, _)| *ts)
        .chain(std::iter::once(previous.watermark))
        .max()
        .unwrap_or(previous.watermark);
    let mut ids: Vec<String> = Vec::new();
    // When the watermark does not advance, the previous boundary ids are
    // still at the boundary — keep them so a tie emitted earlier is not
    // re-emitted now.
    if new_watermark == previous.watermark {
        ids.clone_from(&previous.seen_ids);
    }
    for (ts, id) in &emitted {
        if *ts == new_watermark && !ids.iter().any(|s| s == id) {
            ids.push((*id).to_string());
        }
    }
    if ids.is_empty() {
        new_watermark.to_rfc3339()
    } else {
        format!("{}|{}", new_watermark.to_rfc3339(), ids.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn decode_plain_rfc3339_has_no_seen_ids() {
        let c = decode("2024-01-01T00:00:00Z").unwrap();
        assert_eq!(c.watermark, ts("2024-01-01T00:00:00Z"));
        assert!(c.seen_ids.is_empty());
    }

    #[test]
    fn decode_with_boundary_ids() {
        let c = decode("2024-01-01T00:00:00Z|a,b").unwrap();
        assert_eq!(c.watermark, ts("2024-01-01T00:00:00Z"));
        assert_eq!(c.seen_ids, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn decode_rejects_garbage_timestamp() {
        assert!(decode("not-a-timestamp").is_err());
    }

    #[test]
    fn is_new_excludes_already_seen_boundary_id() {
        let c = decode("2024-01-01T00:00:00Z|a").unwrap();
        // Boundary record already emitted -> not new.
        assert!(!c.is_new(ts("2024-01-01T00:00:00Z"), "a"));
        // Boundary record sharing the instant but never emitted -> new.
        assert!(c.is_new(ts("2024-01-01T00:00:00Z"), "b"));
        // Strictly after -> new.
        assert!(c.is_new(ts("2024-01-01T00:00:01Z"), "a"));
        // Strictly before -> not new.
        assert!(!c.is_new(ts("2023-12-31T23:59:59Z"), "b"));
    }

    #[test]
    fn encode_advances_watermark_and_resets_boundary_ids() {
        let prev = decode("2024-01-01T00:00:00Z|a").unwrap();
        let next = encode(
            &prev,
            vec![
                (ts("2024-01-02T00:00:00Z"), "x"),
                (ts("2024-01-02T00:00:00Z"), "y"),
            ],
        );
        assert_eq!(next, "2024-01-02T00:00:00+00:00|x,y");
    }

    #[test]
    fn encode_accumulates_boundary_ids_when_watermark_unchanged() {
        // First run emitted "a" at the boundary.
        let prev = decode("2024-01-01T00:00:00Z|a").unwrap();
        // Second run emits a tie "b" at the same instant.
        let next = encode(&prev, vec![(ts("2024-01-01T00:00:00Z"), "b")]);
        assert_eq!(next, "2024-01-01T00:00:00+00:00|a,b");
    }

    #[test]
    fn encode_with_no_emissions_preserves_cursor() {
        let prev = decode("2024-01-01T00:00:00Z|a").unwrap();
        let next = encode(&prev, std::iter::empty());
        assert_eq!(next, "2024-01-01T00:00:00+00:00|a");
    }

    #[test]
    fn tie_record_in_later_run_is_emitted_exactly_once() {
        // Run 1: only record "a" exists at instant T -> emitted, cursor
        // records the tie id.
        let seed = decode("2024-01-01T00:00:00Z").unwrap();
        let after_run1 = encode(&seed, vec![(ts("2024-01-01T00:00:00Z"), "a")]);
        assert_eq!(after_run1, "2024-01-01T00:00:00+00:00|a");

        // Run 2: record "b" appears at the SAME instant T. With a plain
        // `>` cursor it would be dropped forever; here it is new.
        let c2 = decode(&after_run1).unwrap();
        assert!(!c2.is_new(ts("2024-01-01T00:00:00Z"), "a"));
        assert!(c2.is_new(ts("2024-01-01T00:00:00Z"), "b"));
        let after_run2 = encode(&c2, vec![(ts("2024-01-01T00:00:00Z"), "b")]);
        assert_eq!(after_run2, "2024-01-01T00:00:00+00:00|a,b");

        // Run 3: nothing new at T -> both stay suppressed.
        let c3 = decode(&after_run2).unwrap();
        assert!(!c3.is_new(ts("2024-01-01T00:00:00Z"), "a"));
        assert!(!c3.is_new(ts("2024-01-01T00:00:00Z"), "b"));
    }
}
