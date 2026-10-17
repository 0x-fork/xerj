//! `xerj search "<query>"` — the binary as a query client.
//!
//! Reference coding is "index a repo, then retrieve the definition before you
//! write". Indexing is already one command (`xerj autoindex`); this makes the
//! retrieval side one command too, so the whole loop is the binary plus a
//! running node — no wrapper scripts. It detects a node the same way the
//! flagship path does (health-ping the ES-compat endpoint) and runs a
//! `multi_match` biased toward symbol definitions, printing a passage to read
//! rather than a JSON blob to parse.

use std::collections::BTreeSet;

use crate::esclient::Es;
use serde_json::{json, Value};

const USAGE: &str = "\
xerj search — retrieve code from a running XERJ node (reference coding, no scripts)

USAGE:
    xerj search [OPTIONS] \"<what you need, in plain words>\"

OPTIONS:
    --url <U>       node endpoint (default $XERJ_URL or http://localhost:9200)
    --prefix <P>    index prefix to search (default \"ax\" — what `xerj autoindex` writes)
    --api-key <K>   Authorization (or $XERJ_API_KEY; --insecure nodes need none)
    -k <N>          number of passages to return (default 5)
    --full <CHARS>  max characters of each passage to print (default 600)
    --json          print the raw response instead of formatted passages

EXAMPLE:
    xerj --insecure -d ./.xerj-data &                 # a node, running
    xerj autoindex ref/sled                           # index once
    xerj search \"fsync the WAL segment on rotation\"    # one line in, a passage out

SEE ALSO:
    xerj def <symbol>    exact go-to-definition when you already know the name
";

/// Extract the `_passage` snippet from a hit. The engine returns it under
/// `fields._passage` as an array whose first element is either a plain string
/// or an object carrying the text — tolerate both shapes rather than betting
/// on one (the shape has already bitten one external client).
pub(crate) fn passage_text(hit: &Value) -> Option<String> {
    let first = hit.pointer("/fields/_passage/0")?;
    match first {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => {
            if matches!(
                o.get("field").and_then(Value::as_str),
                Some("name" | "title" | "defs")
            ) && hit
                .pointer("/_source/code")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            {
                return None;
            }
            o.get("text")
                .or_else(|| o.get("passage"))
                .or_else(|| o.get("snippet"))
                .and_then(Value::as_str)
                .map(str::to_string)
        }
        _ => None,
    }
}

/// True when the query looks like a single code identifier (`IOSIntfLine`,
/// `euler_to_rotationmatrix`) rather than a plain-words phrase — the case
/// where `xerj def` is the better tool.
pub(crate) fn is_identifier(q: &str) -> bool {
    !q.is_empty()
        && !q.contains(char::is_whitespace)
        && q.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// `name` / `code` / `defs` are the code-shaped legs (#916). A corpus that
/// maps none of them — markdown, logs, anything autoindex accepted that is
/// not source — must not be queried with those legs: autoindex indices are
/// created `strict_unknown_fields`, and a `term` or `match_phrase` on a
/// field no resolved index maps is HTTP 400 (#1601).
const CODE_SYMBOL_FIELDS: &[&str] = &["name", "code", "defs"];

/// Historical `xerj search` multi_match weights. Order is part of the
/// code-corpus query: do not reorder.
const SEARCH_MM_FIELDS: &[(&str, &str)] = &[
    ("name", "name^4"),
    ("code", "code^2"),
    ("body", "body"),
    ("defs", "defs^3"),
    ("title", "title"),
];

/// Content fields a non-code autoindex maps (`text` on prose, `impl` on
/// markdown sections, `message` on the log family). Joined only when the
/// target maps none of [`CODE_SYMBOL_FIELDS`], so a code corpus keeps the
/// #916 shape exactly.
const PROSE_EXTRA_FIELDS: &[&str] = &["text", "impl", "message"];

/// Union of property names in a `GET /{pattern}/_mapping` body.
///
/// `None` when `mapping` is not that shape, so a transport failure keeps the
/// historical code-shaped query. An empty object is a real answer (no
/// indices) and comes back empty — callers treat an empty set like an
/// unreadable mapping and do not drop clauses they cannot prove unmapped.
pub(crate) fn mapped_property_names(mapping: &Value) -> Option<BTreeSet<String>> {
    let obj = mapping.as_object()?;
    if obj.is_empty() {
        return Some(BTreeSet::new());
    }
    let mut names = BTreeSet::new();
    let mut saw_mapping = false;
    for body in obj.values() {
        let Some(props) = body
            .pointer("/mappings/properties")
            .and_then(Value::as_object)
        else {
            continue;
        };
        saw_mapping = true;
        names.extend(props.keys().cloned());
    }
    if !saw_mapping {
        return None;
    }
    Some(names)
}

/// Whether `field` may be named in a lookup clause.
///
/// An unreadable mapping (`None`) or an empty union keeps every historical
/// clause: dropping them with no evidence would change the #916 ranking on
/// a code corpus whose mapping we failed to read.
pub(crate) fn field_mapped(mapped: Option<&BTreeSet<String>>, field: &str) -> bool {
    match mapped {
        None => true,
        Some(set) if set.is_empty() => true,
        Some(set) => set.contains(field),
    }
}

/// True when the mapping was read and no index maps `name`, `code`, or `defs`.
pub(crate) fn code_symbol_fields_absent(mapped: Option<&BTreeSet<String>>) -> bool {
    match mapped {
        Some(set) if !set.is_empty() => CODE_SYMBOL_FIELDS.iter().all(|f| !set.contains(*f)),
        _ => false,
    }
}

/// `preferred` entries whose base field is mapped, then `extras` (bare names)
/// when the corpus maps no code-symbol field.
pub(crate) fn weighted_query_fields(
    preferred: &[(&str, &str)],
    extras: &[&str],
    mapped: Option<&BTreeSet<String>>,
) -> Vec<String> {
    let mut out: Vec<String> = preferred
        .iter()
        .filter(|(name, _)| field_mapped(mapped, name))
        .map(|(_, weighted)| (*weighted).to_string())
        .collect();
    if code_symbol_fields_absent(mapped) {
        if let Some(set) = mapped {
            for name in extras {
                if set.contains(*name) && !out.iter().any(|f| f.split('^').next() == Some(*name)) {
                    out.push((*name).to_string());
                }
            }
        }
    }
    out
}

/// Build the ES search request body used by `xerj search`.
///
/// `mapped` is the union of property names under the prefix. `None` (and an
/// empty union) is the historical code-shaped query.
pub fn search_query_body(query: &str, k: usize) -> Value {
    search_query_body_for_mapping(query, k, None)
}

/// [`search_query_body`] with the target's mapped field names (#1601).
pub fn search_query_body_for_mapping(
    query: &str,
    k: usize,
    mapped: Option<&BTreeSet<String>>,
) -> Value {
    let built = build_search_query(query, k, mapped);
    if should_clauses(&built).is_empty() {
        // Nothing we can name is mapped. The historical shape is what the
        // server's 400 then explains, listing the searchable fields.
        return build_search_query(query, k, None);
    }
    built
}

fn should_clauses(body: &Value) -> &[Value] {
    body.pointer("/query/bool/should")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn build_search_query(query: &str, k: usize, mapped: Option<&BTreeSet<String>>) -> Value {
    let shaped = xerj_common::code_query::normalize_code_search_query(query);
    let mut should = Vec::new();
    let fields = weighted_query_fields(SEARCH_MM_FIELDS, PROSE_EXTRA_FIELDS, mapped);
    if !fields.is_empty() {
        should.push(json!({ "multi_match": {
            "query": shaped,
            "fields": fields,
            "type": "most_fields"
        }}));
    }
    if field_mapped(mapped, "defs") {
        should.push(json!({ "match_phrase": { "defs": { "query": shaped, "boost": 8 } } }));
    }
    if field_mapped(mapped, "name") {
        should.push(json!({ "term": { "name": { "value": query.trim(), "boost": 10 } } }));
    }
    json!({
        "size": k,
        "query": { "bool": { "should": should }},
        // No `body` here: whole-file bodies are the token flood (measured ~9x
        // the size of the matching passage). `_passage` carries the snippet.
        "_source": ["ax_path", "line", "start_line", "language", "kind", "code", "title"],
        "fields": ["_passage"]
    })
}

/// Extract the 1-based starting line number from `fields._passage[0].ordinal`
/// when `_passage` comes from a document body field (ignoring synthetic
/// single-line or symbol-list metadata fields such as `defs` or `title`).
///
/// Only valid for LEXICAL passages, where the engine defines `ordinal` as
/// the zero-based line index (`PassageMatch` in xerj-query). On a semantic
/// hit it is a chunk ordinal instead; `xerj search` and `xerj def` send
/// lexical queries only, so every passage they render is lexical.
pub(crate) fn passage_line(hit: &Value) -> Option<u64> {
    let first = hit.pointer("/fields/_passage/0")?.as_object()?;
    if matches!(
        first.get("field").and_then(Value::as_str),
        Some("defs" | "title" | "name")
    ) {
        return None;
    }
    first.get("ordinal").and_then(Value::as_u64).map(|o| o + 1)
}

/// Format the `path[:line]` header token for a search or definition hit.
pub(crate) fn hit_location(hit: &Value) -> String {
    let src = hit.get("_source").unwrap_or(&Value::Null);
    let path = src
        .get("ax_path")
        .or_else(|| src.get("path"))
        .and_then(Value::as_str)
        .unwrap_or("?");
    let line = src
        .get("line")
        .or_else(|| src.get("start_line"))
        .and_then(Value::as_u64)
        .or_else(|| passage_line(hit));
    match line {
        Some(n) => format!("{path}:{n}"),
        None => path.to_string(),
    }
}

/// Entry point for the `search` subcommand. Returns a process exit code.
pub fn run_search_cli() -> i32 {
    let mut url = std::env::var("XERJ_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let mut api_key = std::env::var("XERJ_API_KEY").ok().filter(|s| !s.is_empty());
    let mut prefix = "ax".to_string();
    let mut k: usize = 5;
    let mut full: usize = 600;
    let mut as_json = false;
    let mut query: Option<String> = None;

    // Skip argv[0] (binary) + argv[1] ("search").
    let mut it = std::env::args().skip(2);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return 0;
            }
            "--url" => url = it.next().unwrap_or_default(),
            "--prefix" => prefix = it.next().unwrap_or(prefix),
            "--api-key" => api_key = it.next(),
            "-k" => k = it.next().and_then(|s| s.parse().ok()).unwrap_or(k),
            "--full" => full = it.next().and_then(|s| s.parse().ok()).unwrap_or(full),
            "--json" => as_json = true,
            other if !other.starts_with('-') && query.is_none() => query = Some(other.to_string()),
            other => {
                eprintln!("xerj search: unexpected argument '{other}'\n\n{USAGE}");
                return 2;
            }
        }
    }

    let query = match query {
        Some(q) if !q.trim().is_empty() => q,
        _ => {
            eprintln!("xerj search: a query is required.\n\n{USAGE}");
            return 2;
        }
    };
    if url.is_empty() {
        url = "http://localhost:9200".to_string();
    }

    let es = match Es::new(&url, api_key) {
        Ok(es) => es,
        Err(e) => {
            eprintln!("xerj search: could not build client for {url}: {e}");
            return 2;
        }
    };

    // Detect the node. If nothing answers, say exactly how to start one rather
    // than dumping a transport error — this is the most common first-run miss.
    if let Err(e) = es.ping() {
        eprintln!(
            "xerj search: no XERJ node reachable at {url} ({e}).\n\n\
             Start one, then index a repo:\n  \
             xerj --insecure -d ./.xerj-data &\n  \
             xerj autoindex <folder>\n\n\
             (or point at an existing node with --url / $XERJ_URL)"
        );
        return 2;
    }

    // Defs-first shape (same rationale as the catalog's suggested query, and
    // measured on an 80-task cross-file benchmark: the phrase clause on `defs`
    // is what ranks the file that DEFINES a symbol above files that merely
    // mention it — 38%→94% correct-file-at-rank-1 vs the plain multi_match).
    // The multi_match stays as the recall floor; the phrase clauses are
    // self-gating (a multi-word conceptual query that matches no `defs` line
    // simply contributes no score). `name` is a keyword field on symbol docs
    // (issue #500), so the `term` clause fires only when the whole query IS
    // the identifier — the strongest possible signal.
    //
    // Those legs name fields a non-code autoindex does not map. The indices
    // are strict, so the clause is HTTP 400 rather than a silent miss
    // (#1601). Read the mapping and drop a leg no resolved index maps; an
    // unreadable mapping keeps the code-shaped query.
    let pattern = format!("{prefix}-*");
    let mapped = es
        .get_json(&format!("/{pattern}/_mapping"))
        .ok()
        .and_then(|body| mapped_property_names(&body));
    let body = search_query_body_for_mapping(&query, k, mapped.as_ref());
    let resp = match es.search(&pattern, &body) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "xerj search: query failed against {url}/{pattern}: {e}\n\
                 (is the prefix right? `xerj autoindex` writes 'ax' by default — \
                 pass --prefix if you indexed under another.)"
            );
            return 1;
        }
    };

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&resp).unwrap_or_default()
        );
        return 0;
    }

    let hits = resp
        .pointer("/hits/hits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if hits.is_empty() {
        println!("no match in {pattern} for: {query}");
        if is_identifier(&query) {
            println!("(looks like a symbol name — try: xerj def {query})");
        }
        println!("(index a relevant repo first: xerj autoindex <folder>)");
        return 0;
    }

    for h in &hits {
        let src = h.get("_source").cloned().unwrap_or(Value::Null);
        let score = h.get("_score").and_then(Value::as_f64).unwrap_or(0.0);
        let loc = hit_location(h);
        let kind = src.get("kind").and_then(Value::as_str).unwrap_or("");
        let tag = if kind.is_empty() {
            String::new()
        } else {
            format!("  [{kind}]")
        };
        println!("\n─── {loc}{tag}  (score {score:.2})");
        // Passage-first: `_passage` is the engine's matching snippet (an
        // array-wrapped object with a text payload); `code` is the symbol
        // declaration. Never the whole file body — that is the 9x token flood
        // this command exists to avoid.
        let passage_owned = passage_text(h);
        let passage = passage_owned
            .as_deref()
            .or_else(|| src.get("code").and_then(Value::as_str));
        if let Some(text) = passage {
            let snippet: String = text.chars().take(full).collect();
            for l in snippet.lines() {
                println!("    {l}");
            }
            if text.chars().count() > full {
                println!("    … (--full {} to see more)", full * 2);
            }
        }
    }
    println!(
        "\n{} passage(s) from '{pattern}'. Cite ax_path:line for anything you rely on.",
        hits.len()
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use xerj_common::code_query::normalize_code_search_query;

    /// #1364: multi-word natural-language queries strip repeated/glue particles
    /// (`de`, `la`, `por`, `en`, `the`, `of`) so a 2-letter function named `de`
    /// cannot hijack `name^4`/`title` and unrelated questions do not return
    /// identical top hits, while single-identifier lookups (`de`), acronyms
    /// (`IO`, `WAL`), and 2-letter tech terms (`db`, `gc`, `tx`) are preserved.
    #[test]
    fn normalize_code_search_query_strips_multiword_glue_and_deduplicates() {
        assert_eq!(
            normalize_code_search_query(
                "factor de calibracion de la encuesta multiplicador de ponderacion"
            ),
            "factor calibracion encuesta multiplicador ponderacion"
        );
        assert_eq!(
            normalize_code_search_query("ponderacion de hogares por departamento en la encuesta"),
            "ponderacion hogares departamento encuesta"
        );
        assert_eq!(
            normalize_code_search_query("ingreso total del hogar correccion por inflacion"),
            "ingreso total hogar correccion inflacion"
        );
        // Single-token lookup for a function literally named `de` is untouched.
        assert_eq!(normalize_code_search_query("de"), "de");
        // Identifier punctuation survives a short lookup verbatim: the
        // reporter's control query `.mensaje_motor` (an R name with a leading
        // dot) must still reach the exact-`name` term clause unchanged.
        assert_eq!(
            normalize_code_search_query(".mensaje_motor"),
            ".mensaje_motor"
        );
        let lookup = search_query_body(".mensaje_motor", 5);
        assert_eq!(
            lookup["query"]["bool"]["should"][2]["term"]["name"]["value"],
            ".mensaje_motor"
        );
        assert_eq!(
            lookup["query"]["bool"]["should"][0]["multi_match"]["query"],
            ".mensaje_motor"
        );
        // Two-letter technical terms, uppercase acronyms, and alphanumeric tokens stay.
        assert_eq!(
            normalize_code_search_query("fsync the WAL segment on db tx rotation v2"),
            "fsync WAL segment db tx rotation v2"
        );
        assert_eq!(
            normalize_code_search_query("indicador de agua potable o68"),
            "indicador agua potable o68"
        );

        let body = search_query_body(
            "factor de calibracion de la encuesta multiplicador de ponderacion",
            5,
        );
        assert_eq!(
            body["query"]["bool"]["should"][0]["multi_match"]["query"],
            "factor calibracion encuesta multiplicador ponderacion"
        );
    }

    /// #1364: if `_passage` came back from a bare identifier metadata field
    /// (`name` or `title`) on a symbol doc that carries `_source.code`,
    /// `passage_text` yields `None` so the caller falls back to `_source.code`
    /// (`de <- function(col)`) instead of printing bare `de`.
    #[test]
    fn passage_text_skips_bare_name_field_when_source_code_is_present() {
        let hit = json!({
            "_source": {
                "ax_path": "hogares_ech.R",
                "line": 1221,
                "kind": "function",
                "name": "de",
                "title": "de",
                "code": "de <- function(col)"
            },
            "fields": {
                "_passage": [{
                    "field": "name",
                    "ordinal": 0,
                    "text": "de"
                }]
            }
        });
        assert_eq!(passage_text(&hit), None);
    }

    /// #1365: whole-file hits (`ax-code-*`, `ax-docs-md-*`, `ax-text-*`) carry
    /// no `_source.line` or `_source.start_line`, so `hit_location` must
    /// convert `fields._passage[0].ordinal` (0-based line index of the snippet)
    /// to a 1-based `:line` suffix when `_passage` comes from the file body
    /// (and ignore synthetic metadata fields like `defs` or `title`).
    #[test]
    fn whole_file_hit_uses_one_based_passage_ordinal_when_source_has_no_line() {
        let whole_file_hit = json!({
            "_source": {
                "ax_path": "crates/fjall/src/batch.rs",
                "language": "rust"
            },
            "fields": {
                "_passage": [{
                    "field": "body",
                    "ordinal": 41,
                    "start_offset": 1200,
                    "end_offset": 1450,
                    "text": "pub fn commit(&mut self) -> crate::Result<()> {\n    Ok(())\n}"
                }]
            }
        });
        assert_eq!(
            hit_location(&whole_file_hit),
            "crates/fjall/src/batch.rs:42"
        );

        let symbol_hit = json!({
            "_source": {
                "ax_path": "crates/fjall/src/batch.rs",
                "line": 108,
                "kind": "function"
            },
            "fields": {
                "_passage": [{
                    "field": "code",
                    "ordinal": 0,
                    "text": "pub fn commit(&mut self) -> crate::Result<()>"
                }]
            }
        });
        assert_eq!(hit_location(&symbol_hit), "crates/fjall/src/batch.rs:108");

        let synthetic_defs_hit = json!({
            "_source": {
                "ax_path": "crates/fjall/src/batch.rs"
            },
            "fields": {
                "_passage": [{
                    "field": "defs",
                    "ordinal": 0,
                    "text": "fn commit"
                }]
            }
        });
        assert_eq!(
            hit_location(&synthetic_defs_hit),
            "crates/fjall/src/batch.rs"
        );
    }

    fn index_mapping(fields: &[&str]) -> Value {
        let mut props = serde_json::Map::new();
        for f in fields {
            props.insert((*f).to_string(), json!({ "type": "text" }));
        }
        json!({ "c-docs": { "mappings": { "properties": props } } })
    }

    fn names_of(fields: &[&str]) -> BTreeSet<String> {
        mapped_property_names(&index_mapping(fields)).expect("mapping shape")
    }

    /// #1601: a markdown/prose corpus maps `body`/`title`/`text`/`impl` and
    /// none of `defs`/`name`/`code`. The code-shaped `term` and `match_phrase`
    /// legs are what strict autoindex indices refuse with HTTP 400.
    #[test]
    fn prose_mapping_drops_unmapped_code_legs() {
        let mapped = names_of(&["body", "title", "text", "impl"]);
        let body = search_query_body_for_mapping("retention ceiling", 5, Some(&mapped));
        let should = body["query"]["bool"]["should"].as_array().unwrap();
        assert_eq!(should.len(), 1, "{body}");
        assert!(should[0].get("match_phrase").is_none(), "{body}");
        assert!(should[0].get("term").is_none(), "{body}");
        assert_eq!(
            should[0]["multi_match"]["fields"],
            json!(["body", "title", "text", "impl"])
        );
        let rendered = body["query"].to_string();
        assert!(!rendered.contains("\"defs\""), "{rendered}");
        assert!(!rendered.contains("\"name\""), "{rendered}");
        assert!(!rendered.contains("\"code\""), "{rendered}");
    }

    /// A log index whose text lives in `message` still gets a query the
    /// strict mapping will accept.
    #[test]
    fn log_mapping_searches_message_and_names_no_code_field() {
        let mapped = names_of(&["message", "ts", "host"]);
        let body = search_query_body_for_mapping("SIP 403 Forbidden", 3, Some(&mapped));
        let should = body["query"]["bool"]["should"].as_array().unwrap();
        assert_eq!(should.len(), 1, "{body}");
        assert_eq!(should[0]["multi_match"]["fields"], json!(["message"]));
    }

    /// A code corpus — and an unreadable mapping — keep the #916 shape,
    /// including when the union also carries prose fields.
    #[test]
    fn code_mapping_and_unreadable_mapping_keep_the_historical_query() {
        let query = "fsync the WAL segment";
        let historical = search_query_body(query, 5);
        assert_eq!(search_query_body_for_mapping(query, 5, None), historical);
        let empty = BTreeSet::new();
        assert_eq!(
            search_query_body_for_mapping(query, 5, Some(&empty)),
            historical
        );
        assert!(mapped_property_names(&json!({"error": "no such index"})).is_none());
        let code = names_of(&["name", "code", "body", "defs", "title", "text", "impl"]);
        assert_eq!(
            search_query_body_for_mapping(query, 5, Some(&code)),
            historical
        );
        // One index in the union mapping `defs` is enough to keep the leg:
        // strict mode refuses a field no resolved index maps, not a field
        // some index does not map.
        let partial = names_of(&["body", "defs", "title"]);
        let body = search_query_body_for_mapping(query, 5, Some(&partial));
        let should = body["query"]["bool"]["should"].as_array().unwrap();
        assert!(
            should.iter().any(|c| c.get("match_phrase").is_some()),
            "{body}"
        );
        assert!(should.iter().all(|c| c.get("term").is_none()), "{body}");
        assert_eq!(
            should[0]["multi_match"]["fields"],
            json!(["body", "defs^3", "title"])
        );
    }
}
