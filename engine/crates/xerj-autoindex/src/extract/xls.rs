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
use std::path::Path;

/// Non-empty rows searched for a header before the sheet is treated as a
/// document (mirrors the XLSX extractor).
const HEADER_SCAN_ROWS: usize = 20;

/// Defensive cap on rows inspected per sheet, in case a crafted BIFF dimension
/// record claims a range far larger than the format's own 65,536-row ceiling.
const MAX_ROWS_PER_SHEET: usize = 1 << 20;

/// Text kept from one sheet indexed as a document.
const MAX_DOC_SHEET_BYTES: usize = 16 << 20;

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
    let f = std::fs::File::open(path)?;
    // calamine reports a malformed/truncated workbook as an error; never a
    // panic. Treat any such failure as junk rather than propagating — a single
    // unreadable legacy file must not abort a folder scan.
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
