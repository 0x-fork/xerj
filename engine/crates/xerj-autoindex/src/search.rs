//! `xerj search "<query>"` — the binary as a query client.
//!
//! Reference coding is "index a repo, then retrieve the definition before you
//! write". Indexing is already one command (`xerj autoindex`); this makes the
//! retrieval side one command too, so the whole loop is the binary plus a
//! running node — no wrapper scripts. It detects a node the same way the
//! flagship path does (health-ping the ES-compat endpoint) and runs a
//! `multi_match` biased toward symbol definitions, printing a passage to read
//! rather than a JSON blob to parse.

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

fn is_two_letter_tech_term(w: &str) -> bool {
    matches!(
        w,
        "db" | "ui"
            | "ux"
            | "ip"
            | "gc"
            | "vm"
            | "id"
            | "kv"
            | "ir"
            | "ci"
            | "pr"
            | "ai"
            | "ml"
            | "tx"
            | "rx"
            | "fs"
            | "fd"
            | "pk"
            | "fk"
    )
}

fn is_three_letter_glue_word(w: &str) -> bool {
    matches!(
        w,
        "del"
            | "por"
            | "los"
            | "las"
            | "una"
            | "uno"
            | "con"
            | "sin"
            | "sus"
            | "mis"
            | "tus"
            | "nos"
            | "les"
            | "mas"
            | "muy"
            | "tan"
            | "que"
            | "son"
            | "fue"
            | "era"
            | "ser"
            | "hay"
            | "the"
            | "and"
            | "for"
            | "not"
            | "are"
            | "but"
            | "can"
            | "all"
            | "has"
            | "had"
            | "was"
            | "one"
            | "out"
            | "who"
            | "how"
            | "why"
            | "his"
            | "her"
            | "its"
            | "our"
            | "you"
            | "any"
            | "may"
            | "via"
            | "per"
            | "des"
            | "aux"
            | "par"
            | "sur"
            | "une"
            | "est"
            | "ses"
            | "vos"
            | "ils"
            | "pas"
            | "qui"
            | "der"
            | "die"
            | "das"
            | "den"
            | "dem"
            | "ein"
            | "und"
            | "mit"
            | "von"
            | "aus"
            | "bei"
            | "vor"
            | "zur"
            | "zum"
            | "wie"
            | "wir"
            | "sie"
            | "ich"
            | "uma"
            | "dos"
            | "nas"
            | "com"
            | "sem"
            | "seu"
            | "sua"
            | "nao"
            | "gli"
            | "nel"
            | "sul"
            | "col"
            | "tra"
            | "fra"
            | "che"
            | "non"
            | "piu"
    )
}

fn is_natural_language_glue_token(clean: &str) -> bool {
    if clean.is_empty() || clean.contains('_') {
        return false;
    }
    if !clean.chars().all(|c| c.is_alphabetic() && c.is_lowercase()) {
        return false;
    }
    let char_len = clean.chars().count();
    (char_len <= 2 && !is_two_letter_tech_term(clean)) || is_three_letter_glue_word(clean)
}

/// Normalize a plain-words `xerj search` query before expanding it into
/// `multi_match` / `match_phrase` / `term` clauses (#1364):
///
/// 1. Deduplicate tokens case-insensitively while preserving first-seen order
///    so a repeated word (`"de"` ×3) never gets 3× BM25 multiplication.
/// 2. When a query has 3+ distinct tokens, drop lowercase natural-language
///    glue tokens (`"de"`, `"el"`, `"la"`, `"en"`, `"the"`, `"of"`, `"in"`, …)
///    so short function/variable names like `de` or `in` on `name^4` / `title`
///    cannot outscore files matching the query's content words, and unrelated
///    questions that only share `"de"` do not collapse onto the same top hits.
///    Single-token queries (`xerj search de`) and 2-token queries keep all
///    tokens untouched, as do UPPERCASE/camelCase acronyms (`IO`, `DB`, `ID`)
///    and alphanumeric tokens (`v2`, `u8`, `o68`).
pub(crate) fn normalize_code_search_query(query: &str) -> String {
    let mut seen = Vec::<String>::new();
    let mut dedup = Vec::<&str>::new();
    for raw in query.split_whitespace() {
        let clean = raw.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
        if clean.is_empty() {
            continue;
        }
        let lower = clean.to_lowercase();
        if !seen.contains(&lower) {
            seen.push(lower);
            dedup.push(clean);
        }
    }
    // Under three distinct tokens the query is a lookup, not a question:
    // return it verbatim. Re-joining the trimmed tokens here would strip
    // identifier punctuation — R's `.mensaje_motor` (the #1364 reporter's own
    // control query) would lose its leading dot and miss the exact-`name`
    // `term` clause.
    if dedup.len() < 3 {
        return query.trim().to_string();
    }
    let content: Vec<&str> = dedup
        .iter()
        .copied()
        .filter(|tok| !is_natural_language_glue_token(tok))
        .collect();
    if content.is_empty() {
        dedup.join(" ")
    } else {
        content.join(" ")
    }
}

/// Build the ES search request body used by `xerj search`.
pub fn search_query_body(query: &str, k: usize) -> Value {
    let shaped = normalize_code_search_query(query);
    json!({
        "size": k,
        "query": { "bool": { "should": [
            { "multi_match": {
                "query": shaped,
                "fields": ["name^4", "code^2", "body", "defs^3", "title"],
                "type": "most_fields"
            }},
            { "match_phrase": { "defs": { "query": shaped, "boost": 8 } } },
            { "term": { "name": { "value": query.trim(), "boost": 10 } } }
        ]}},
        // No `body` here: whole-file bodies are the token flood (measured ~9x
        // the size of the matching passage). `_passage` carries the snippet.
        "_source": ["ax_path", "line", "start_line", "language", "kind", "code", "title"],
        "fields": ["_passage"]
    })
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
    let body = search_query_body(&query, k);

    let pattern = format!("{prefix}-*");
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
        let path = src.get("ax_path").and_then(Value::as_str).unwrap_or("?");
        let score = h.get("_score").and_then(Value::as_f64).unwrap_or(0.0);
        let line = src
            .get("line")
            .or_else(|| src.get("start_line"))
            .and_then(Value::as_u64);
        let loc = match line {
            Some(n) => format!("{path}:{n}"),
            None => path.to_string(),
        };
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
}
