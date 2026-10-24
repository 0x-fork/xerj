//! Per-document byte ceiling for indexed JSON (#1122).
//!
//! One bulk item and one `index_document` call tokenize the whole source
//! before the process memory breaker can sample. The FTS `Token` owns a
//! `String` per word. A standalone reproduction of that ownership (not a live
//! `analyze_doc` run), measured 2026-10-23: 8,388,605 bytes of the word
//! `word ` (1,677,721 tokens) grew a `Vec<String>` from 10,160 KB RSS to
//! 101,912 KB RSS — 91,752 KB, about 56 bytes per token. It was not repeated
//! at 50 MB.
//!
//! The ceiling is a fixed constant. `limits` is already 15 settings and the
//! user-facing total is 137; a new knob would have to touch every settings
//! census for a bound that must not be raised by a config edit.

use serde_json::Value;

/// Largest JSON document source that may be parsed and analyzed.
///
/// 8 MiB is under the 50 MB files that killed a node during autoindex, and
/// above a normal record. A larger file is streamed into smaller records or
/// skipped. A single value over this limit is not indexed.
pub const MAX_INDEXED_DOCUMENT_BYTES: usize = 8 << 20;

/// Extra bytes an `_update` envelope may use beyond two documents at the ceiling.
///
/// The body can carry a partial `doc` and an `upsert`, each a full indexed
/// document, plus a script. The engine caps a script source at 64 KiB;
/// JSON escaping can double that, and the object keys sit beside the two
/// bodies. 256 KiB covers that wrapping. A larger raw body is refused
/// before `serde_json` builds a DOM (#1122).
pub const UPDATE_ENVELOPE_SLACK_BYTES: usize = 256 * 1024;

/// Largest raw `POST /{index}/_update/{id}` body that is parsed.
///
/// Bytes past this limit are refused before `serde_json`. A body at or
/// under the limit is still subject to [`MAX_INDEXED_DOCUMENT_BYTES`] on
/// the document the update would write.
pub const MAX_UPDATE_BODY_BYTES: usize =
    2 * MAX_INDEXED_DOCUMENT_BYTES + UPDATE_ENVELOPE_SLACK_BYTES;

/// `true` when an `_update` body is past [`MAX_UPDATE_BODY_BYTES`].
pub fn update_body_too_large(len: usize) -> bool {
    len > MAX_UPDATE_BODY_BYTES
}

/// Refusal text for an `_update` body of `len` bytes.
///
/// The text does not say the body was invalid JSON: the size check runs
/// before parse, including when the bytes are not JSON.
pub fn update_body_refusal(len: usize) -> String {
    format!(
        "update body is {len} bytes, over the {MAX_UPDATE_BODY_BYTES} byte limit; \
         it was not parsed and the existing document was not changed"
    )
}

/// `true` when `len` is past [`MAX_INDEXED_DOCUMENT_BYTES`].
pub fn indexed_document_too_large(len: usize) -> bool {
    len > MAX_INDEXED_DOCUMENT_BYTES
}

/// Refusal text for a document whose raw JSON is `len` bytes.
pub fn indexed_document_refusal(len: usize) -> String {
    format!(
        "document is {len} bytes, over the {MAX_INDEXED_DOCUMENT_BYTES} byte limit for one indexed JSON document; it was not indexed. Split it into smaller documents"
    )
}

/// Refusal text when a walk of an already-parsed value stopped at `seen`.
pub fn indexed_document_refusal_at_least(seen: usize) -> String {
    format!(
        "document is at least {seen} bytes, over the {MAX_INDEXED_DOCUMENT_BYTES} byte limit for one indexed JSON document; it was not indexed. Split it into smaller documents"
    )
}

/// `Some(accounted)` when a walk of `value` passes the limit.
///
/// The walk stops at the first byte past the limit, so `accounted` can be
/// smaller than the full value. Strings contribute their decoded length.
/// Numbers contribute a fixed 8 bytes (serde_json stores them as `i64`,
/// `u64`, or `f64` without `arbitrary_precision`). One byte is charged per
/// array element so a giant array of nulls still trips.
pub fn json_value_exceeds_indexed_document_limit(value: &Value) -> Option<usize> {
    let mut accounted = 0usize;
    if exceeds(value, &mut accounted) {
        Some(accounted)
    } else {
        None
    }
}

fn charge(accounted: &mut usize, n: usize) -> bool {
    *accounted = accounted.saturating_add(n);
    *accounted > MAX_INDEXED_DOCUMENT_BYTES
}

fn exceeds(value: &Value, accounted: &mut usize) -> bool {
    match value {
        Value::Null | Value::Bool(_) => charge(accounted, 4),
        Value::Number(_) => charge(accounted, 8),
        Value::String(text) => charge(accounted, text.len().saturating_add(2)),
        Value::Array(items) => {
            if charge(accounted, 2) {
                return true;
            }
            for item in items {
                if charge(accounted, 1) || exceeds(item, accounted) {
                    return true;
                }
            }
            false
        }
        Value::Object(map) => {
            if charge(accounted, 2) {
                return true;
            }
            for (key, child) in map {
                if charge(accounted, key.len().saturating_add(3)) || exceeds(child, accounted) {
                    return true;
                }
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_ceiling_is_eight_mebibytes() {
        assert_eq!(MAX_INDEXED_DOCUMENT_BYTES, 8 * 1024 * 1024);
        assert!(!indexed_document_too_large(MAX_INDEXED_DOCUMENT_BYTES));
        assert!(indexed_document_too_large(MAX_INDEXED_DOCUMENT_BYTES + 1));
    }

    #[test]
    fn an_update_envelope_fits_two_documents_and_refuses_one_byte_over() {
        assert_eq!(
            MAX_UPDATE_BODY_BYTES,
            2 * MAX_INDEXED_DOCUMENT_BYTES + 256 * 1024
        );
        assert!(!update_body_too_large(MAX_UPDATE_BODY_BYTES));
        assert!(update_body_too_large(MAX_UPDATE_BODY_BYTES + 1));
        let text = update_body_refusal(MAX_UPDATE_BODY_BYTES + 1);
        assert!(
            text.contains(&(MAX_UPDATE_BODY_BYTES + 1).to_string()),
            "{text}"
        );
        assert!(text.contains(&MAX_UPDATE_BODY_BYTES.to_string()), "{text}");
        assert!(text.contains("not parsed"), "{text}");
        assert!(text.contains("not changed"), "{text}");
        assert!(
            !text.contains("invalid JSON"),
            "the size refusal is not a parse error: {text}"
        );
    }

    #[test]
    fn refusal_names_the_size_and_the_limit() {
        let text = indexed_document_refusal(50 * 1024 * 1024);
        assert!(text.contains("52428800"));
        assert!(text.contains(&MAX_INDEXED_DOCUMENT_BYTES.to_string()));
        assert!(text.contains("not indexed"));
        let at_least = indexed_document_refusal_at_least(MAX_INDEXED_DOCUMENT_BYTES + 3);
        assert!(at_least.contains("at least"));
        assert!(at_least.contains("not indexed"));
    }

    #[test]
    fn a_small_value_is_under_the_limit_and_a_long_string_is_not() {
        assert!(json_value_exceeds_indexed_document_limit(&json!({"id": 1})).is_none());
        let huge = "x".repeat(MAX_INDEXED_DOCUMENT_BYTES + 1);
        let seen = json_value_exceeds_indexed_document_limit(&json!({"blob": huge}))
            .expect("a string past the ceiling must trip");
        assert!(seen > MAX_INDEXED_DOCUMENT_BYTES);
        // The walk stops on that string. It must not report a second full copy.
        assert!(seen < MAX_INDEXED_DOCUMENT_BYTES + 64);
    }
}
