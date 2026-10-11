//! XLS — legacy Excel 97-2003 workbooks (BIFF8 inside an OLE2 compound file),
//! read as tables on the same record model as [`super::xlsx`].
//!
//! Each worksheet is its own dataset (`RawRecord.group` = sheet name) and each
//! row below the sheet's header row is one record keyed by the header cells,
//! with values carrying the type the cell stored: numbers stay numbers,
//! booleans stay booleans, and a date/time cell becomes an ISO-8601 string so
//! type inference elects `date` rather than seeing a serial number. The header
//! is the first row (among the first [`HEADER_SCAN_ROWS`] non-empty rows) with
//! at least two cells, all text or date and at least one text, that is not
//! immediately followed by a wider row of the same kind; rows above it are not
//! indexed. A sheet with no such header is indexed as a document, one line per
//! row with cells joined by ` | `.
//!
//! Reading is delegated to calamine, which ships its own OLE2/BIFF reader. The
//! heavy lifting of BIFF record parsing, the shared-string table and the
//! legacy 8-bit cell codepage all live there; this module only maps calamine's
//! `Data` cells onto the extractor's typed record model.
//!
//! Differences from the XLSX extractor, by design: calamine materialises a
//! whole worksheet as a `Range` (the BIFF format caps a sheet at 65,536 rows ×
//! 256 columns, and calamine reads it in one pass), so there is no streaming
//! row cap of our own — the per-sheet sample limit bounds how many rows we
//! EMIT, and [`MAX_ROWS_PER_SHEET`] bounds how many we even look at against a
//! crafted dimension record. Vertical merge fill-down is not applied (`.xls`
//! merge ranges are not surfaced on calamine's `Range`); a category label
//! merged down a column therefore appears only on its first row.

use super::{
    emit_document_with_fields, sanitize_field_name, ExtractStats, FieldOrigin, RawRecord, Sink,
    MAX_FIELDS_PER_RECORD,
};
use anyhow::Result;
use calamine::{Data, DataType, Reader, Xls};
use chrono::Timelike;
use serde_json::{Map, Value};
use std::io::Read;
use std::path::Path;

/// Non-empty rows searched for a header before the sheet is treated as a
/// document (mirrors the XLSX extractor).
const HEADER_SCAN_ROWS: usize = 20;

/// Defensive cap on rows inspected per sheet, in case a crafted BIFF dimension
/// record claims a range far larger than the format's own 65,536-row ceiling.
const MAX_ROWS_PER_SHEET: usize = 1 << 20;

/// Text kept from one sheet indexed as a document.
const MAX_DOC_SHEET_BYTES: usize = 16 << 20;

/// BIFF8 worksheet ceiling: 65,536 rows × 256 columns. A `DIMENSIONS` record
/// (`0x0200`) claiming more than this is malformed or crafted — and it matters
/// because calamine materialises a *dense* `Range` sized from that record
/// before we ever iterate a row (`MAX_ROWS_PER_SHEET` on `range.rows()` is
/// applied too late). An inflated dimension therefore drives an unbounded
/// allocation that aborts the whole scan (release builds are `panic = abort`,
/// and nothing wraps the extractor in `catch_unwind`). So we read the
/// dimensions ourselves and refuse the file before handing it to calamine.
const BIFF_MAX_ROWS: u32 = 65_536;
const BIFF_MAX_COLS: u32 = 256;

/// Upper bound on the Workbook stream bytes read for the dimension pre-flight.
/// A legitimate `.xls` Workbook stream is far below this; the `take()` cap also
/// bounds the pre-flight's own allocation regardless of what the container
/// claims the stream length to be.
const MAX_WORKBOOK_STREAM: u64 = 512 << 20;

/// `false` ⇒ refuse the file: a `DIMENSIONS` record exceeds the BIFF8 ceiling,
/// so calamine would over-allocate. `true` ⇒ no crafted dimension found;
/// calamine may read it. Never panics — any CFB/IO failure returns `true` (not
/// our gate; `Xls::new` rejects a genuinely broken file cleanly), and the read
/// is `take()`-bounded so it cannot itself be driven to over-allocate.
fn xls_dimensions_within_limits(path: &Path) -> bool {
    let Ok(mut comp) = cfb::open(path) else {
        return true;
    };
    let name = if comp.is_stream("Workbook") {
        "Workbook"
    } else if comp.is_stream("Book") {
        "Book"
    } else {
        return true;
    };
    let Ok(stream) = comp.open_stream(name) else {
        return true;
    };
    let mut buf = Vec::new();
    if stream.take(MAX_WORKBOOK_STREAM).read_to_end(&mut buf).is_err() {
        return true;
    }
    dimensions_sane(&buf)
}

/// Walk the BIFF record stream and refuse if any `DIMENSIONS` record (`0x0200`)
/// claims a range outside the BIFF8 ceiling. Linear, bounds-checked, and
/// terminating: every step advances past a `u16` length, so a crafted length
/// cannot loop. Records other than `DIMENSIONS` are skipped by their length.
fn dimensions_sane(stream: &[u8]) -> bool {
    let mut pos = 0usize;
    while pos + 4 <= stream.len() {
        let rectype = u16::from_le_bytes([stream[pos], stream[pos + 1]]);
        let reclen = u16::from_le_bytes([stream[pos + 2], stream[pos + 3]]) as usize;
        let body = pos + 4;
        let Some(end) = body.checked_add(reclen) else {
            break;
        };
        if end > stream.len() {
            break;
        }
        if rectype == 0x0200 && reclen >= 14 {
            let d = &stream[body..body + 14];
            let rw_mic = u32::from_le_bytes([d[0], d[1], d[2], d[3]]);
            let rw_mac = u32::from_le_bytes([d[4], d[5], d[6], d[7]]);
            let col_mic = u16::from_le_bytes([d[8], d[9]]) as u32;
            let col_mac = u16::from_le_bytes([d[10], d[11]]) as u32;
            if rw_mac > BIFF_MAX_ROWS
                || col_mac > BIFF_MAX_COLS
                || rw_mac < rw_mic
                || col_mac < col_mic
            {
                return false;
            }
        }
        pos = end;
    }
    true
}

/// `name` is the file as the corpus names it (under durable preparation this is
/// not `path`, a sealed snapshot blob); document sheets are titled from it.
/// `per_sheet_limit` caps the rows emitted from each sheet (phase-A sampling);
/// `None` reads every row.
pub fn extract(
    path: &Path,
    name: &Path,
    per_sheet_limit: Option<u64>,
    sink: Sink,
) -> Result<ExtractStats> {
    let mut stats = ExtractStats::default();
    // DoS gate: calamine allocates a dense `Range` from the sheet's declared
    // `DIMENSIONS` before any row is read, so a crafted record makes it attempt
    // a multi-terabyte allocation and abort the process (release = panic=abort,
    // no catch_unwind around the scan). Refuse such a file up front; the read
    // here is bounded by `cfb`'s FAT chain, so the gate cannot itself be driven
    // to over-allocate.
    if !xls_dimensions_within_limits(path) {
        stats.junk += 1;
        return Ok(stats);
    }
    let f = std::fs::File::open(path)?;
    // A malformed/truncated workbook that still clears the dimension gate is
    // treated as junk rather than propagating — a single unreadable legacy file
    // must not abort a folder scan. (calamine *returns* an error for most such
    // files, but is not panic-free on hostile input, which is why the gate
    // above runs first.)
    let mut wb = match Xls::new(f) {
        Ok(wb) => wb,
        Err(_) => {
            stats.junk += 1;
            return Ok(stats);
        }
    };

    let stem = name
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "untitled".into());

    let sheet_names = wb.sheet_names().to_vec();
    for sheet_name in sheet_names {
        let Ok(range) = wb.worksheet_range(&sheet_name) else {
            continue;
        };
        let start_row = range.start().map(|(r, _)| r).unwrap_or(0);
        let start_col = range.start().map(|(_, c)| c).unwrap_or(0);

        // Build the non-empty rows, each carrying 1-based column numbers so the
        // header/column machinery matches the XLSX extractor's.
        let mut rows: Vec<Row> = Vec::new();
        for (i, cells) in range.rows().take(MAX_ROWS_PER_SHEET).enumerate() {
            let mut out = Vec::new();
            for (j, data) in cells.iter().enumerate() {
                if let Some(cell) = convert_cell(start_col + j as u32 + 1, data) {
                    out.push(cell);
                }
            }
            if !out.is_empty() {
                rows.push(Row {
                    num: start_row + i as u32 + 1,
                    cells: out,
                });
            }
        }
        if rows.is_empty() {
            continue;
        }

        if let Some(h) = find_header(&rows) {
            if !emit_table(&sheet_name, &rows, h, per_sheet_limit, sink, &mut stats) {
                break;
            }
        } else if !emit_document_sheet(&sheet_name, &stem, &rows, sink, &mut stats) {
            break;
        }
    }

    if stats.records == 0 {
        stats.junk += 1;
    }
    Ok(stats)
}

/// A cell's typed kind, mirroring the XLSX extractor so header detection and
/// type inference agree across the two formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Number,
    Bool,
    Date,
}

#[derive(Debug, Clone)]
struct Cell {
    /// 1-based column (A = 1).
    col: u32,
    value: Value,
    kind: Kind,
}

impl Cell {
    fn display(&self) -> String {
        match &self.value {
            Value::String(s) => s.clone(),
            Value::Bool(true) => "TRUE".into(),
            Value::Bool(false) => "FALSE".into(),
            v => v.to_string(),
        }
    }
}

#[derive(Debug)]
struct Row {
    /// 1-based row number, as Excel shows it.
    num: u32,
    cells: Vec<Cell>,
}

/// Map one calamine cell onto the typed model; `None` for an empty cell or an
/// error value (`#N/A`, `#DIV/0!`), exactly as the XLSX extractor drops them.
fn convert_cell(col: u32, data: &Data) -> Option<Cell> {
    match data {
        Data::Empty | Data::Error(_) => None,
        Data::String(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| Cell {
                col,
                value: Value::String(s.to_string()),
                kind: Kind::Text,
            })
        }
        Data::Int(i) => Some(Cell {
            col,
            value: Value::Number((*i).into()),
            kind: Kind::Number,
        }),
        Data::Float(f) => Some(Cell {
            col,
            value: number_value(*f)?,
            kind: Kind::Number,
        }),
        Data::Bool(b) => Some(Cell {
            col,
            value: Value::Bool(*b),
            kind: Kind::Bool,
        }),
        Data::DateTimeIso(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| Cell {
                col,
                value: Value::String(s.to_string()),
                kind: Kind::Date,
            })
        }
        Data::DurationIso(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| Cell {
                col,
                value: Value::String(s.to_string()),
                kind: Kind::Text,
            })
        }
        Data::DateTime(_) => date_cell(col, data),
    }
}

/// A calamine `Data::DateTime` rendered to the same ISO-8601 strings the XLSX
/// extractor emits: a bare date at midnight, a `T`-joined datetime otherwise,
/// and `HH:MM:SS` for a duration/time-of-day. Falls back to the raw serial
/// number if calamine cannot resolve it to a chrono value.
fn date_cell(col: u32, data: &Data) -> Option<Cell> {
    // A time of day (fraction of a day with no date part) resolves to a
    // `NaiveTime` but not a sensible `NaiveDateTime`; check it first.
    if let Some(dt) = data.as_datetime() {
        let iso = if dt.num_seconds_from_midnight() == 0 && dt.nanosecond() == 0 {
            dt.format("%Y-%m-%d").to_string()
        } else if dt.nanosecond() == 0 {
            dt.format("%Y-%m-%dT%H:%M:%S").to_string()
        } else {
            dt.format("%Y-%m-%dT%H:%M:%S%.3f").to_string()
        };
        return Some(Cell {
            col,
            value: Value::String(iso),
            kind: Kind::Date,
        });
    }
    if let Some(t) = data.as_time() {
        return Some(Cell {
            col,
            value: Value::String(t.format("%H:%M:%S").to_string()),
            kind: Kind::Date,
        });
    }
    // Could not resolve a calendar value: keep the serial number as a number so
    // nothing is dropped.
    let f = data.as_f64()?;
    Some(Cell {
        col,
        value: number_value(f)?,
        kind: Kind::Number,
    })
}

/// Integral floats inside the i64-exact range become integers (so `1250.0`
/// indexes as `1250`), matching the XLSX extractor's `number_value`.
fn number_value(f: f64) -> Option<Value> {
    if !f.is_finite() {
        return None;
    }
    if f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 {
        Some(Value::Number((f as i64).into()))
    } else {
        serde_json::Number::from_f64(f).map(Value::Number)
    }
}

/// Column letters of a column number (28 → `AB`).
fn column_letters(mut col: u32) -> String {
    let mut out = Vec::new();
    while col > 0 {
        let rem = (col - 1) % 26;
        out.push(b'A' + rem as u8);
        col = (col - 1) / 26;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// Whether a row can be a header: two or more cells, some text, and no numbers
/// or booleans (date cells are allowed, for month-column headers).
fn is_header_candidate(row: &Row) -> bool {
    row.cells.len() >= 2
        && row
            .cells
            .iter()
            .all(|c| matches!(c.kind, Kind::Text | Kind::Date))
        && row.cells.iter().any(|c| c.kind == Kind::Text)
}

/// Index of the header row. A candidate followed by a WIDER candidate is a
/// title line above the real header and is passed over. Only the first
/// [`HEADER_SCAN_ROWS`] non-empty rows are considered.
fn find_header(rows: &[Row]) -> Option<usize> {
    let scan = rows.len().min(HEADER_SCAN_ROWS);
    rows[..scan].windows(2).position(|w| {
        is_header_candidate(&w[0])
            && !(is_header_candidate(&w[1]) && w[1].cells.len() > w[0].cells.len())
    })
}

/// Emit every data row under `header_idx` as a typed record keyed by the header
/// cells. Returns false if the sink asked to stop the whole file.
fn emit_table(
    sheet: &str,
    rows: &[Row],
    header_idx: usize,
    limit: Option<u64>,
    sink: Sink,
    stats: &mut ExtractStats,
) -> bool {
    let header = &rows[header_idx];
    let mut seen = std::collections::HashSet::new();
    let mut names: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    for c in &header.cells {
        let mut name = sanitize_field_name(&c.display());
        while !seen.insert(name.clone()) {
            name.push_str("_2");
        }
        names.insert(c.col, name);
    }
    let header_cells: Vec<(u32, String)> =
        header.cells.iter().map(|c| (c.col, c.display())).collect();

    let mut emitted = 0u64;
    for row in &rows[header_idx + 1..] {
        // A repeated header row (common when a sheet stacks sections) is not a
        // data record.
        let row_text: Vec<(u32, String)> = row.cells.iter().map(|c| (c.col, c.display())).collect();
        if row_text == header_cells {
            continue;
        }
        let mut fields = Map::new();
        for c in &row.cells {
            if fields.len() >= MAX_FIELDS_PER_RECORD {
                break;
            }
            let name = names
                .get(&c.col)
                .cloned()
                .unwrap_or_else(|| format!("col_{}", column_letters(c.col)));
            fields.insert(name, c.value.clone());
        }
        stats.records += 1;
        if !sink(RawRecord {
            fields,
            locator: format!("{sheet}!r{}", row.num),
            group: Some(sheet.to_string()),
            origin: FieldOrigin::Data,
        }) {
            return false;
        }
        emitted += 1;
        if limit.is_some_and(|lim| emitted >= lim) {
            break;
        }
    }
    true
}

/// Emit a sheet with no detectable header as a document: one line per row,
/// cells joined by ` | `. Returns false if the sink asked to stop.
fn emit_document_sheet(
    sheet: &str,
    stem: &str,
    rows: &[Row],
    sink: Sink,
    stats: &mut ExtractStats,
) -> bool {
    let mut body = String::new();
    let mut last_row = 0u32;
    for row in rows {
        let line = row
            .cells
            .iter()
            .map(Cell::display)
            .collect::<Vec<_>>()
            .join(" | ");
        if body.len() + line.len() + 2 > MAX_DOC_SHEET_BYTES {
            stats.truncated = true;
            break;
        }
        if !body.is_empty() {
            // A gap in row numbers is a paragraph break, where section
            // splitting prefers to cut.
            body.push_str(if row.num > last_row + 1 { "\n\n" } else { "\n" });
        }
        body.push_str(&line);
        last_row = row.num;
    }
    if body.trim().is_empty() {
        return true;
    }
    let mut base = Map::new();
    base.insert("sheet".into(), Value::String(sheet.to_string()));
    emit_document_with_fields(
        &base,
        &format!("{stem} — {sheet}"),
        &body,
        sheet,
        sink,
        stats,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields(r: &RawRecord) -> Value {
        Value::Object(r.fields.clone())
    }

    /// A BIFF8 `DIMENSIONS` record (type 0x0200, len 14): rwMic, rwMac, colMic,
    /// colMac (each last-index-plus-one), reserved.
    fn dimensions_record(rw_mic: u32, rw_mac: u32, col_mic: u16, col_mac: u16) -> Vec<u8> {
        let mut r = vec![0x00, 0x02, 14, 0x00]; // type 0x0200, length 14
        r.extend_from_slice(&rw_mic.to_le_bytes());
        r.extend_from_slice(&rw_mac.to_le_bytes());
        r.extend_from_slice(&col_mic.to_le_bytes());
        r.extend_from_slice(&col_mac.to_le_bytes());
        r.extend_from_slice(&[0x00, 0x00]); // reserved
        r
    }

    #[test]
    fn a_real_dimension_is_accepted() {
        // A 100-row × 10-column sheet is well within the BIFF8 ceiling.
        assert!(dimensions_sane(&dimensions_record(0, 100, 0, 10)));
        // And the format's own maximum is still accepted.
        assert!(dimensions_sane(&dimensions_record(0, 65_536, 0, 256)));
    }

    #[test]
    fn a_crafted_dimension_past_the_biff_ceiling_is_refused() {
        // The shape of the 36 KB proof-of-concept: a DIMENSIONS claiming a vast
        // row range, which drove calamine to attempt a ~2.1 TB Range alloc.
        assert!(!dimensions_sane(&dimensions_record(0, 0x7FFF_FFFF, 0, 256)));
        // An inflated column count is refused the same way.
        assert!(!dimensions_sane(&dimensions_record(0, 100, 0, 60_000)));
        // Inverted ranges (mac < mic) are malformed and refused.
        assert!(!dimensions_sane(&dimensions_record(500, 100, 0, 10)));
    }

    #[test]
    fn a_truncated_or_dimensionless_stream_is_not_mistaken_for_crafted() {
        // No DIMENSIONS record at all: nothing to object to, let calamine try.
        assert!(dimensions_sane(b"\x09\x08\x04\x00\x00\x00\x00\x00")); // a BOF-ish record, no 0x0200
        // A record header whose declared length runs past the buffer end stops
        // the walk without a panic and without a false refusal.
        assert!(dimensions_sane(&[0x00, 0x02, 0xFF, 0xFF, 0x01]));
        // Empty stream.
        assert!(dimensions_sane(&[]));
    }

    /// Build a one-sheet `.xls` in a tempdir with `rustexcel`-free, synthetic
    /// content by driving calamine's own writer? calamine has no writer, so the
    /// unit tests here exercise the typed-model mapping and the table/document
    /// routing directly on synthetic rows. The end-to-end round-trip through a
    /// real BIFF file is covered by `extract_real_xls_fixture` (gated on a
    /// generated fixture), so these pin the behaviour deterministically.
    fn row(num: u32, cells: &[Cell]) -> Row {
        Row {
            num,
            cells: cells.to_vec(),
        }
    }
    fn txt(col: u32, s: &str) -> Cell {
        Cell {
            col,
            value: Value::String(s.into()),
            kind: Kind::Text,
        }
    }
    fn num(col: u32, v: i64) -> Cell {
        Cell {
            col,
            value: Value::Number(v.into()),
            kind: Kind::Number,
        }
    }
    fn boolean(col: u32, v: bool) -> Cell {
        Cell {
            col,
            value: Value::Bool(v),
            kind: Kind::Bool,
        }
    }

    fn collect_table(sheet: &str, rows: &[Row]) -> (ExtractStats, Vec<RawRecord>) {
        let h = find_header(rows).expect("a header should be found");
        let mut recs = Vec::new();
        let mut stats = ExtractStats::default();
        emit_table(
            sheet,
            rows,
            h,
            None,
            &mut |r| {
                recs.push(r);
                true
            },
            &mut stats,
        );
        (stats, recs)
    }

    #[test]
    fn convert_cell_types_mirror_the_xlsx_model() {
        assert!(convert_cell(1, &Data::Empty).is_none());
        assert!(convert_cell(1, &Data::Error(calamine::CellErrorType::Div0)).is_none());
        // An integral float indexes as an integer, not 1250.0.
        assert_eq!(
            convert_cell(3, &Data::Float(1250.0)).unwrap().value,
            json!(1250)
        );
        assert_eq!(
            convert_cell(3, &Data::Float(980.5)).unwrap().value,
            json!(980.5)
        );
        assert_eq!(convert_cell(4, &Data::Bool(true)).unwrap().kind, Kind::Bool);
        // A whitespace-only string is dropped like an empty cell.
        assert!(convert_cell(1, &Data::String("   ".into())).is_none());
    }

    #[test]
    fn a_title_row_is_skipped_and_each_row_under_the_header_is_a_typed_record() {
        // Row 1 is a single-cell title; row 3 the header; 4-5 the data.
        let rows = vec![
            row(1, &[txt(1, "Q3 Sales Report")]),
            row(
                3,
                &[
                    txt(1, "Region"),
                    txt(2, "Month"),
                    txt(3, "Revenue"),
                    txt(4, "Closed"),
                ],
            ),
            row(
                4,
                &[
                    txt(1, "EMEA"),
                    txt(2, "July"),
                    num(3, 1250),
                    boolean(4, true),
                ],
            ),
            row(
                5,
                &[
                    txt(1, "APAC"),
                    txt(2, "August"),
                    num(3, 980),
                    boolean(4, false),
                ],
            ),
        ];
        let (stats, recs) = collect_table("Sales", &rows);
        assert_eq!((stats.records, stats.junk), (2, 0));
        assert_eq!(recs[0].group.as_deref(), Some("Sales"));
        assert_eq!(recs[0].locator, "Sales!r4");
        assert_eq!(recs[0].origin, FieldOrigin::Data);
        assert_eq!(
            fields(&recs[0]),
            json!({"Region": "EMEA", "Month": "July", "Revenue": 1250, "Closed": true})
        );
        assert_eq!(
            fields(&recs[1]),
            json!({"Region": "APAC", "Month": "August", "Revenue": 980, "Closed": false})
        );
    }

    #[test]
    fn a_repeated_header_row_is_not_a_data_record() {
        let rows = vec![
            row(1, &[txt(1, "Name"), txt(2, "Score")]),
            row(2, &[txt(1, "Ann"), num(2, 10)]),
            row(3, &[txt(1, "Name"), txt(2, "Score")]), // stacked header again
            row(4, &[txt(1, "Bob"), num(2, 20)]),
        ];
        let (stats, recs) = collect_table("S", &rows);
        assert_eq!(stats.records, 2);
        assert_eq!(fields(&recs[0]), json!({"Name": "Ann", "Score": 10}));
        assert_eq!(fields(&recs[1]), json!({"Name": "Bob", "Score": 20}));
    }

    #[test]
    fn duplicate_header_names_are_disambiguated() {
        let rows = vec![
            row(1, &[txt(1, "Amount"), txt(2, "Amount")]),
            row(2, &[num(1, 1), num(2, 2)]),
        ];
        let (_stats, recs) = collect_table("S", &rows);
        let obj = recs[0].fields.clone();
        assert!(obj.contains_key("Amount") && obj.contains_key("Amount_2"));
    }

    #[test]
    fn a_sheet_with_no_header_is_emitted_as_a_document() {
        // Every row leads with a number, so no row is a header candidate.
        let rows = vec![
            row(1, &[num(1, 1), txt(2, "alpha")]),
            row(2, &[num(1, 2), txt(2, "beta")]),
        ];
        assert!(find_header(&rows).is_none());
        let mut recs = Vec::new();
        let mut stats = ExtractStats::default();
        emit_document_sheet(
            "Notes",
            "book",
            &rows,
            &mut |r| {
                recs.push(r);
                true
            },
            &mut stats,
        );
        assert_eq!(stats.records, 1);
        let body = recs[0].fields["body"].as_str().unwrap();
        assert!(body.contains("1 | alpha") && body.contains("2 | beta"));
        assert_eq!(recs[0].fields["title"], "book — Notes");
        assert_eq!(recs[0].fields["sheet"], "Notes");
    }

    #[test]
    fn date_cell_formats_match_the_xlsx_extractor() {
        // calamine resolves an ExcelDateTime; check the bare-date vs datetime
        // split against a serial we can predict (1904-01-01 is serial 1462 in
        // the 1900 system; use a known midnight serial instead).
        use calamine::ExcelDateTime;
        // 2025-07-01 00:00:00 — serial 45839 in the 1900 date system.
        let d = Data::DateTime(ExcelDateTime::new(
            45839.0,
            calamine::ExcelDateTimeType::DateTime,
            false,
        ));
        let cell = date_cell(1, &d).unwrap();
        assert_eq!(cell.kind, Kind::Date);
        assert_eq!(cell.value, json!("2025-07-01"));
    }
}
