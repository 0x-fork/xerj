//! `xerj def "<symbol>"` — go-to-definition over a XERJ index.
//!
//! `xerj search` answers "find me something relevant"; this answers the
//! sharper question an agent doing reference coding actually has most of the
//! time: "where is this symbol DEFINED, and what is its signature?". It leans
//! on the per-symbol documents a code autoindex writes (issue #500 —
//! `name`/`kind`/`line`/`code`, with `name` mapped as a keyword), so an exact
//! identifier is a `term` hit at the top, and falls back to the file-level
//! `defs` text for indexes built before symbol docs existed. Output is one
//! `path:line` plus the declaration per hit — the measured shape that lets an
//! agent act in one call instead of re-searching (~570 output tokens vs a
//! whole-file dump).

use std::collections::BTreeSet;

use crate::esclient::Es;
use crate::search::{
    field_mapped, hit_location, is_identifier, mapped_property_names, passage_text,
    weighted_query_fields,
};
use serde_json::{json, Value};

const USAGE: &str = "\
xerj def — find where a symbol is defined (go-to-definition, no scripts)

USAGE:
    xerj def [OPTIONS] \"<symbol or capability>\"

OPTIONS:
    --url <U>       node endpoint (default $XERJ_URL or http://localhost:9200)
    --prefix <P>    index prefix to search (default \"ax\" — what `xerj autoindex` writes)
    --api-key <K>   Authorization (or $XERJ_API_KEY; --insecure nodes need none)
    -k <N>          number of definitions to return (default 5)
    --kind <KIND>   filter by symbol kind (function, class, method, ...)
    --lang <LANG>   filter by language (python, rust, typescript, ...)
    --json          print the raw response instead of formatted definitions

EXAMPLE:
    xerj def IOSIntfLine                       # exact identifier -> its declaration
    xerj def \"euler angles to rotation matrix\"  # plain words also resolve the definer

SEE ALSO:
    xerj search \"<plain words>\"    ranked passages when you don't know the name
";

/// Historical `xerj def` multi_match weights. Order is part of the
/// code-corpus query.
const DEF_MM_FIELDS: &[(&str, &str)] = &[
    ("name", "name^4"),
    ("defs", "defs^3"),
    ("code", "code^2"),
    ("title", "title"),
];

/// Joined into the recall floor only when the target maps no `name` /
/// `code` / `defs` (#1601). `body` is not in the code-shaped list above.
const DEF_PROSE_EXTRA_FIELDS: &[&str] = &["body", "text", "impl", "message"];

/// The `xerj def` query body: definition-first ranking over the per-symbol
/// docs, narrowed by the optional `--kind` / `--lang` filters.
///
/// `mapped` is the union of property names under the prefix. `None` keeps
/// the historical code-shaped clauses. A read mapping drops every lookup
/// clause whose field no index maps, including `--kind` / `--lang` — those
/// `term` filters 400 a strict index the same way `name` does (#1601).
fn def_query(
    query: &str,
    kind: Option<&str>,
    lang: Option<&str>,
    k: usize,
    mapped: Option<&BTreeSet<String>>,
) -> Value {
    let built = build_def_query(query, kind, lang, k, mapped);
    let empty = built
        .pointer("/query/bool/should")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty);
    if empty {
        return build_def_query(query, kind, lang, k, None);
    }
    built
}

fn build_def_query(
    query: &str,
    kind: Option<&str>,
    lang: Option<&str>,
    k: usize,
    mapped: Option<&BTreeSet<String>>,
) -> Value {
    // Definition-first ranking, measured at 94% correct-file-at-rank-1 on an
    // 80-task cross-file benchmark (vs 38% for body-text search):
    //   - `term name`: the whole query IS the identifier — keyword-exact on
    //     the per-symbol docs, the strongest signal, boost 12.
    //   - `match_phrase defs`: the file-level definition list; also carries
    //     pre-#500 indexes that have no symbol docs, boost 8.
    //   - `multi_match`: recall floor so plain-words queries ("euler angles to
    //     rotation matrix") still surface the definer via `defs`/`code` text.
    let mut should = Vec::new();
    if field_mapped(mapped, "name") {
        should.push(json!({ "term": { "name": { "value": query, "boost": 12 } } }));
    }
    if field_mapped(mapped, "defs") {
        should.push(json!({ "match_phrase": { "defs": { "query": query, "boost": 8 } } }));
    }
    let fields = weighted_query_fields(DEF_MM_FIELDS, DEF_PROSE_EXTRA_FIELDS, mapped);
    if !fields.is_empty() {
        should.push(json!({ "multi_match": {
            "query": query,
            "fields": fields,
            "type": "most_fields"
        }}));
    }
    if is_identifier(query) && field_mapped(mapped, "code") {
        // `code` holds the declaration text; a phrase hit there ("def <name>(",
        // "class <name>:") is a definition even when `name` casing differs.
        should.push(json!({ "match_phrase": { "code": { "query": query, "boost": 4 } } }));
    }
    let mut filter = Vec::new();
    if let Some(kd) = kind {
        if field_mapped(mapped, "kind") {
            filter.push(json!({ "term": { "kind": kd } }));
        }
    }
    if let Some(lg) = lang {
        if field_mapped(mapped, "language") {
            filter.push(json!({ "term": { "language": lg } }));
        }
    }
    json!({
        "size": k,
        // A filter clause makes every `should` optional, so without this a
        // `--kind`/`--lang` filter alone matches: a name defined nowhere came
        // back as every definition of that kind instead of "no definition".
        "query": { "bool": {
            "should": should,
            "filter": filter,
            "minimum_should_match": 1
        } },
        "_source": ["ax_path", "path", "line", "start_line", "kind", "language", "name", "code"],
        "fields": ["_passage"]
    })
}

/// Entry point for the `def` subcommand. Returns a process exit code.
pub fn run_def_cli() -> i32 {
    let mut url = std::env::var("XERJ_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let mut api_key = std::env::var("XERJ_API_KEY").ok().filter(|s| !s.is_empty());
    let mut prefix = "ax".to_string();
    let mut k: usize = 5;
    let mut kind: Option<String> = None;
    let mut lang: Option<String> = None;
    let mut as_json = false;
    let mut query: Option<String> = None;

    // Skip argv[0] (binary) + argv[1] ("def").
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
            "--kind" => kind = it.next(),
            "--lang" => lang = it.next(),
            "--json" => as_json = true,
            other if !other.starts_with('-') && query.is_none() => query = Some(other.to_string()),
            other => {
                eprintln!("xerj def: unexpected argument '{other}'\n\n{USAGE}");
                return 2;
            }
        }
    }

    let query = match query {
        Some(q) if !q.trim().is_empty() => q,
        _ => {
            eprintln!("xerj def: a symbol or phrase is required.\n\n{USAGE}");
            return 2;
        }
    };
    if url.is_empty() {
        url = "http://localhost:9200".to_string();
    }

    let es = match Es::new(&url, api_key) {
        Ok(es) => es,
        Err(e) => {
            eprintln!("xerj def: could not build client for {url}: {e}");
            return 2;
        }
    };
    if let Err(e) = es.ping() {
        eprintln!(
            "xerj def: no XERJ node reachable at {url} ({e}).\n\n\
             Start one, then index a repo:\n  \
             xerj --insecure -d ./.xerj-data &\n  \
             xerj autoindex <folder>\n\n\
             (or point at an existing node with --url / $XERJ_URL)"
        );
        return 2;
    }

    let pattern = format!("{prefix}-*");
    let mapped = es
        .get_json(&format!("/{pattern}/_mapping"))
        .ok()
        .and_then(|body| mapped_property_names(&body));
    if let Some(set) = mapped.as_ref().filter(|s| !s.is_empty()) {
        if kind.is_some() && !set.contains("kind") {
            eprintln!("xerj def: --kind ignored; no index under this prefix maps `kind`");
        }
        if lang.is_some() && !set.contains("language") {
            eprintln!("xerj def: --lang ignored; no index under this prefix maps `language`");
        }
    }
    let body = def_query(&query, kind.as_deref(), lang.as_deref(), k, mapped.as_ref());
    let resp = match es.search(&pattern, &body) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "xerj def: query failed against {url}/{pattern}: {e}\n\
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
        println!("no definition in {pattern} for: {query}");
        println!("(broader ranked retrieval: xerj search \"{query}\")");
        return 0;
    }

    for h in &hits {
        let src = h.get("_source").cloned().unwrap_or(Value::Null);
        let loc = hit_location(h);
        let kind = src.get("kind").and_then(Value::as_str).unwrap_or("");
        let tag = if kind.is_empty() {
            String::new()
        } else {
            format!("  [{kind}]")
        };
        println!("\n─── {loc}{tag}");
        // The declaration, not the file: `code` on symbol docs is the
        // signature block; `_passage` covers file-level hits. Two lines is
        // enough to act on — `xerj search --full` exists for reading more.
        let passage_owned = passage_text(h);
        let decl = src
            .get("code")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or(passage_owned);
        if let Some(text) = decl {
            for l in text.lines().take(2) {
                println!("    {l}");
            }
        }
    }
    println!(
        "\n{} definition(s) from '{pattern}'. Cite path:line for anything you rely on.",
        hits.len()
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ES `bool` semantics: once a `filter` clause is present, `should`
    /// clauses are optional unless `minimum_should_match` says otherwise. A
    /// `--kind`/`--lang` filter must narrow the definitions of the name, not
    /// replace the name with "anything of this kind".
    #[test]
    fn kind_and_lang_filters_still_require_the_name_to_match() {
        for (kind, lang) in [
            (Some("function"), None),
            (None, Some("python")),
            (Some("class"), Some("rust")),
            (None, None),
        ] {
            let body = def_query("zzqq_nonexistent", kind, lang, 5, None);
            let b = &body["query"]["bool"];
            assert_eq!(b["minimum_should_match"], 1, "{body}");
            assert!(!b["should"].as_array().unwrap().is_empty());
            let filters = b["filter"].as_array().unwrap().len();
            assert_eq!(filters, kind.is_some() as usize + lang.is_some() as usize);
        }
    }

    fn names_of(fields: &[&str]) -> BTreeSet<String> {
        let mut props = serde_json::Map::new();
        for f in fields {
            props.insert((*f).to_string(), json!({ "type": "text" }));
        }
        mapped_property_names(&json!({
            "c-docs": { "mappings": { "properties": props } }
        }))
        .expect("mapping shape")
    }

    /// #1601: `xerj def` on a corpus with no symbol fields must not send
    /// `term name`, `match_phrase defs`, or `match_phrase code`. An identifier
    /// query is the case that used to add the `code` leg.
    #[test]
    fn prose_mapping_drops_unmapped_definition_legs() {
        let prose = names_of(&["body", "title", "text", "impl"]);
        let body = def_query(
            "compressPayload",
            Some("function"),
            Some("markdown"),
            5,
            Some(&prose),
        );
        let should = body["query"]["bool"]["should"].as_array().unwrap();
        assert_eq!(should.len(), 1, "{body}");
        assert_eq!(
            should[0]["multi_match"]["fields"],
            json!(["title", "body", "text", "impl"])
        );
        assert!(
            body["query"]["bool"]["filter"]
                .as_array()
                .unwrap()
                .is_empty(),
            "kind and language are unmapped, so their term filters must not 400 the query: {body}"
        );
        let rendered = body["query"].to_string();
        assert!(!rendered.contains("\"defs\""), "{rendered}");
        assert!(!rendered.contains("\"name\""), "{rendered}");
        assert!(!rendered.contains("\"code\""), "{rendered}");
        assert!(!rendered.contains("\"kind\""), "{rendered}");
    }

    /// The measured code-corpus query is unchanged when those fields are mapped.
    #[test]
    fn code_mapping_keeps_the_historical_def_query() {
        let code = names_of(&[
            "name", "defs", "code", "title", "body", "kind", "language", "text",
        ]);
        let query = "compressPayload";
        assert_eq!(
            def_query(query, Some("function"), Some("rust"), 5, Some(&code)),
            def_query(query, Some("function"), Some("rust"), 5, None)
        );
        assert_eq!(
            def_query(query, None, None, 5, Some(&BTreeSet::new())),
            def_query(query, None, None, 5, None)
        );
    }
}
