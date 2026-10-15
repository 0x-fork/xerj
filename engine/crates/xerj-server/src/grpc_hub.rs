//! Corpus Hub v2 — the gRPC `XerjHub` wire mapping (plan §7,
//! `docs/CORPUS_HUB_V2_IMPLEMENTATION.md`; contract in
//! `docs/hub-v2-grpc-mcp-notes.md`).
//!
//! The one place the [`Answer`] ↔ [`pb::HubAnswer`] conversion is written.
//! The proto field numbers are frozen (`HubAnswer` 1–11, `HubHit` 1–7); this
//! module only decides the value forms, as the notes fix them:
//!
//! * enums travel as their **serde** strings — `licence_class` kebab-case
//!   (`adapt-with-attribution` | `approach-only` | `excluded`), `answered_from`
//!   and `mode` snake_case (`local_pack` | `hosted` | `local_index`;
//!   `def` | `how` | `files`) — never the human `AnsweredFrom::label()`;
//! * `Option<String>` ↔ proto3 `string` is `None` ↔ `""` (`value`,
//!   `licence_warning`);
//! * an enum string this build does not know is refused on the way in, not
//!   defaulted.
//!
//! The `XerjHub` service that uses it is wired in a follow-up change; until
//! then only the tests call it.

use serde::de::DeserializeOwned;
use serde::Serialize;

use xerj_common::hub::{Answer, Hit};

use crate::grpc::pb;

// ── the frozen mapping: Answer ↔ HubAnswer ───────────────────────────────────

/// The serde string of a unit enum variant (`approach-only`, `local_pack`, …).
fn enum_str<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        // Every hub enum is a unit enum with a string form; unreachable.
        other => unreachable!("hub enum did not serialise to a string: {other:?}"),
    }
}

fn enum_from<T: DeserializeOwned>(s: &str) -> Result<T, String> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|e| format!("[{s}]: {e}"))
}

/// `None` → `""` on the way out.
fn opt_out(v: &Option<String>) -> String {
    v.clone().unwrap_or_default()
}

/// `""` → `None` on the way in.
fn opt_in(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

/// [`Answer`] → [`pb::HubAnswer`] (fields 1–11, hits 1–7).
pub fn answer_to_pb(a: &Answer) -> pb::HubAnswer {
    pb::HubAnswer {
        corpus: a.corpus.clone(),
        pin_sha: a.pin_sha.clone(),
        pin_date: a.pin_date.clone(),
        age_days: a.age_days,
        licence_spdx: a.licence_spdx.clone(),
        licence_class: enum_str(&a.licence_class),
        answered_from: enum_str(&a.answered_from),
        mode: enum_str(&a.mode),
        hits: a
            .hits
            .iter()
            .map(|h| pb::HubHit {
                file: h.file.clone(),
                line: h.line,
                kind: h.kind.clone(),
                text: h.text.clone(),
                value: opt_out(&h.value),
                score: h.score,
                licence_warning: opt_out(&h.licence_warning),
            })
            .collect(),
        truncated: a.truncated,
        warnings: a.warnings.clone(),
    }
}

/// [`pb::HubAnswer`] → [`Answer`]: what a Rust gRPC client decodes with. An
/// enum string this build does not know is an error, not a default.
pub fn answer_from_pb(p: &pb::HubAnswer) -> Result<Answer, String> {
    Ok(Answer {
        corpus: p.corpus.clone(),
        pin_sha: p.pin_sha.clone(),
        pin_date: p.pin_date.clone(),
        age_days: p.age_days,
        licence_spdx: p.licence_spdx.clone(),
        licence_class: enum_from(&p.licence_class)?,
        answered_from: enum_from(&p.answered_from)?,
        mode: enum_from(&p.mode)?,
        hits: p
            .hits
            .iter()
            .map(|h| Hit {
                file: h.file.clone(),
                line: h.line,
                kind: h.kind.clone(),
                text: h.text.clone(),
                value: opt_in(&h.value),
                score: h.score,
                licence_warning: opt_in(&h.licence_warning),
            })
            .collect(),
        truncated: p.truncated,
        warnings: p.warnings.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use xerj_common::hub::{AnswerMode, AnsweredFrom, LicenceClass};

    fn hit(value: Option<&str>, warning: Option<&str>) -> Hit {
        Hit {
            file: "src/limits.h".into(),
            line: 5,
            kind: "define".into(),
            text: "#define MINI_LIMIT 128".into(),
            value: value.map(str::to_string),
            score: 3.25,
            licence_warning: warning.map(str::to_string),
        }
    }

    fn answer(class: LicenceClass, from: AnsweredFrom, mode: AnswerMode) -> Answer {
        Answer {
            corpus: "mini-c".into(),
            pin_sha: "0774d05537f9762f838f7ab541b7765f1a729cb5".into(),
            pin_date: "2026-10-02".into(),
            age_days: 13,
            licence_spdx: "GPL-3.0-only".into(),
            licence_class: class,
            answered_from: from,
            mode,
            hits: vec![
                hit(
                    Some("128"),
                    Some("approach-only licence (GPL-3.0-only): read, do not copy"),
                ),
                hit(None, None),
            ],
            truncated: true,
            warnings: vec!["one".into(), "two".into()],
        }
    }

    /// Every field, every enum variant, both `Option` states: out to the wire
    /// form the notes freeze and back to an identical `Answer`.
    #[test]
    fn answer_round_trips_through_the_proto_field_by_field() {
        let classes = [
            (LicenceClass::AdaptWithAttribution, "adapt-with-attribution"),
            (LicenceClass::ApproachOnly, "approach-only"),
            (LicenceClass::Excluded, "excluded"),
        ];
        let froms = [
            (AnsweredFrom::LocalPack, "local_pack"),
            (AnsweredFrom::Hosted, "hosted"),
            (AnsweredFrom::LocalIndex, "local_index"),
        ];
        let modes = [
            (AnswerMode::Def, "def"),
            (AnswerMode::How, "how"),
            (AnswerMode::Files, "files"),
        ];
        for (i, (class, class_s)) in classes.iter().enumerate() {
            let (from, from_s) = froms[i];
            let (mode, mode_s) = modes[i];
            let a = answer(*class, from, mode);
            let p = answer_to_pb(&a);

            assert_eq!(p.corpus, "mini-c");
            assert_eq!(p.pin_sha, a.pin_sha);
            assert_eq!(p.pin_date, "2026-10-02");
            assert_eq!(p.age_days, 13);
            assert_eq!(p.licence_spdx, "GPL-3.0-only");
            assert_eq!(p.licence_class, *class_s, "serde kebab-case");
            assert_eq!(p.answered_from, from_s, "serde snake_case, not label()");
            assert_ne!(p.answered_from, from.label());
            assert_eq!(p.mode, mode_s);
            assert!(p.truncated);
            assert_eq!(p.warnings, vec!["one", "two"]);
            assert_eq!(p.hits.len(), 2);
            let (h0, h1) = (&p.hits[0], &p.hits[1]);
            assert_eq!(
                (h0.file.as_str(), h0.line, h0.kind.as_str()),
                ("src/limits.h", 5, "define")
            );
            assert_eq!(h0.text, "#define MINI_LIMIT 128");
            assert_eq!(h0.value, "128");
            assert_eq!(h0.score, 3.25);
            assert_eq!(
                h0.licence_warning,
                "approach-only licence (GPL-3.0-only): read, do not copy"
            );
            assert_eq!(h1.value, "", "None -> \"\"");
            assert_eq!(h1.licence_warning, "", "None -> \"\"");

            let back = answer_from_pb(&p).expect("decodes");
            assert_eq!(
                serde_json::to_value(&back).unwrap(),
                serde_json::to_value(&a).unwrap(),
                "the round trip is lossless"
            );
            assert_eq!(back.hits[1].value, None, "\"\" -> None");
            assert_eq!(back.hits[1].licence_warning, None, "\"\" -> None");
        }
    }

    #[test]
    fn an_unknown_enum_string_is_refused_not_defaulted() {
        let mut p = answer_to_pb(&answer(
            LicenceClass::ApproachOnly,
            AnsweredFrom::Hosted,
            AnswerMode::Def,
        ));
        p.licence_class = "Approach Only".into();
        assert!(answer_from_pb(&p).is_err());
        let mut p = answer_to_pb(&answer(
            LicenceClass::ApproachOnly,
            AnsweredFrom::Hosted,
            AnswerMode::Def,
        ));
        p.answered_from = "hosted hub".into(); // the label, not the wire form
        assert!(answer_from_pb(&p).is_err());
    }
}
