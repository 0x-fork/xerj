//! `xerj autoindex remove` — delete one autoindexed folder, after listing it.
//!
//! A prefix can hold more than one folder (an older run left in the catalog,
//! or `--replace-prefix` writing into names the previous folder used). This
//! command reads `autoindex-catalog` first and deletes only the rows and
//! indices that belong to the folder it was asked for. An index another
//! folder's catalog row still names is left in place. `autoindex-catalog`
//! itself is never deleted.
//!
//! Without `--yes` on a non-interactive stdin, nothing is deleted and the
//! process exits 4 with the plan on stdout. A terminal prompts. `--yes` is
//! the confirmation, not a root selector: two folders under one prefix still
//! require `--root`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Write};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::catalog::{self, CATALOG_INDEX};
use crate::esclient::Es;

/// Same exit as the indexing gate: a decision is required, and a retry
/// without new information will not help.
pub const EXIT_NEEDS_DECISION: i32 = 4;

const PAGE: usize = 500;
/// `from + size` must stay within the default `index.max_result_window`.
const WINDOW: usize = 10_000;

#[derive(Debug, Clone)]
pub struct CatalogDoc {
    pub id: String,
    pub source: Value,
}

#[derive(Debug, Clone)]
pub struct LiveIndex {
    pub name: String,
    pub docs: u64,
    pub store_bytes: u64,
}

#[derive(Debug, Clone)]
struct RootGroup {
    /// The path as the catalog recorded it. This is what `--root` prints.
    display: String,
    key: String,
    run_ids: BTreeSet<String>,
    last_started: String,
    datasets: Vec<Row>,
}

#[derive(Debug, Clone)]
struct Row {
    id: String,
    doc_kind: String,
    root: String,
    index_name: String,
    record_count: u64,
    bytes: u64,
    file_count: u64,
}

#[derive(Debug, Clone)]
pub struct RemovePlan {
    pub prefix: String,
    roots: Vec<RootGroup>,
    pub selected_key: Option<String>,
    pub needs_root: bool,
    pub root_not_found: bool,
    pub empty: bool,
    indices: Vec<PlannedIndex>,
    kept_indices: Vec<Kept>,
    rows: Vec<Row>,
    kept_rows: Vec<Row>,
}

#[derive(Debug, Clone)]
struct PlannedIndex {
    name: String,
    live_docs: Option<u64>,
    store_bytes: Option<u64>,
    record_count: u64,
    catalog_bytes: u64,
}

#[derive(Debug, Clone)]
struct Kept {
    name: String,
    reason: String,
}

pub struct RemoveRequest {
    pub url: String,
    pub api_key: Option<String>,
    pub prefix: String,
    pub root: Option<String>,
    pub yes: bool,
    /// Read a `y`/`yes` line from stdin. The CLI sets this only on a terminal.
    /// MCP leaves it false: an agent passes `confirm: true` instead.
    pub prompt: bool,
}

pub struct RemoveReport {
    pub exit_code: i32,
    pub body: Value,
    pub summary: String,
}

pub fn execute(req: RemoveRequest) -> Result<RemoveReport> {
    let es = Es::new(&req.url, req.api_key.clone())?;
    es.ping().with_context(|| {
        format!(
            "autoindex remove: no node at {} — start one, then rerun the same command",
            req.url
        )
    })?;
    let docs = fetch_catalog(&es, &req.prefix)?;
    let mentioned = index_names_of(&docs, &req.prefix);
    let live = fetch_live(&es, &mentioned)?;
    let plan = plan_remove(&req.prefix, req.root.as_deref(), &docs, &live);
    if plan.needs_root || plan.root_not_found || plan.empty {
        let code = if plan.empty || plan.root_not_found {
            1
        } else {
            EXIT_NEEDS_DECISION
        };
        return Ok(report(&plan, code, false, None));
    }
    let confirmed = if req.yes {
        true
    } else if req.prompt {
        eprint!("{}", prompt_line(&plan));
        let _ = std::io::stderr().flush();
        read_yes()
    } else {
        false
    };
    if !confirmed {
        return Ok(report(&plan, EXIT_NEEDS_DECISION, false, None));
    }
    match apply(&es, &plan) {
        Ok(()) => Ok(report(&plan, 0, true, None)),
        Err(err) => {
            let left = match refetch(&es, &req.prefix, req.root.as_deref()) {
                Ok(left) => Some(left),
                Err(read_err) => {
                    return Ok(report(
                        &plan,
                        1,
                        false,
                        Some(format!(
                            "{err:#}\nThe follow-up read of what is left also failed: {read_err:#}"
                        )),
                    ));
                }
            };
            let mut out = report(&plan, 1, false, Some(format!("{err:#}")));
            if let Some(left) = left {
                out.body["remaining"] = plan_body(&left, 1, false, None);
            }
            out.summary.push_str(
                "\nA delete failed partway. `remaining` in the JSON is what the catalog \
                 and the indices still say is there. Nothing further was attempted.",
            );
            Ok(out)
        }
    }
}

fn refetch(es: &Es, prefix: &str, root: Option<&str>) -> Result<RemovePlan> {
    let docs = fetch_catalog(es, prefix)?;
    let live = fetch_live(es, &index_names_of(&docs, prefix))?;
    Ok(plan_remove(prefix, root, &docs, &live))
}

fn prompt_line(plan: &RemovePlan) -> String {
    format!(
        "Remove {} index(es) and {} catalog row(s) for prefix {}? [y/N] ",
        plan.indices.len(),
        plan.rows.len(),
        plan.prefix
    )
}

fn read_yes() -> bool {
    let mut line = String::new();
    match std::io::stdin().lock().read_line(&mut line) {
        Ok(_) => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        Err(_) => false,
    }
}

/// What `remove` will delete for `prefix`, given the catalog and live stats.
///
/// Pure: no HTTP. `root_filter` of `None` selects the only root, and refuses
/// when there is more than one. Indices another remaining root still names,
/// and `autoindex-catalog`, are never in the delete set.
pub fn plan_remove(
    prefix: &str,
    root_filter: Option<&str>,
    docs: &[CatalogDoc],
    live: &[LiveIndex],
) -> RemovePlan {
    let mut by_id: BTreeMap<&str, &CatalogDoc> = BTreeMap::new();
    for doc in docs {
        by_id.entry(doc.id.as_str()).or_insert(doc);
    }
    let docs: Vec<&CatalogDoc> = by_id.into_values().collect();

    let mut groups: BTreeMap<String, RootGroup> = BTreeMap::new();
    for doc in &docs {
        if kind(doc) != "run" || !run_matches_prefix(doc, prefix) {
            continue;
        }
        let recorded = doc.source.get("root").and_then(Value::as_str).unwrap_or("");
        let display = if recorded.is_empty() {
            catalog::UNRECORDED_PREFIX_ROOT.to_string()
        } else {
            recorded.to_string()
        };
        let key = root_key(&display);
        let started = doc
            .source
            .get("started")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let group = groups.entry(key.clone()).or_insert_with(|| RootGroup {
            display: display.clone(),
            key,
            run_ids: BTreeSet::new(),
            last_started: String::new(),
            datasets: Vec::new(),
        });
        if started > group.last_started {
            group.last_started = started;
            group.display = display;
        }
        group.run_ids.insert(run_identity(doc));
        group.run_ids.insert(doc.id.clone());
    }

    let mut roots: Vec<RootGroup> = groups.into_values().collect();
    // Datasets with no run document still describe a folder. One implicit
    // root keeps `remove --prefix` able to drop them; it does not invent a
    // second folder.
    if roots.is_empty()
        && docs.iter().any(|doc| {
            catalog::dataset_matches_prefix(&doc.source, prefix)
                || catalog::file_matches_prefix(&doc.source, prefix)
                || catalog::correlation_matches_prefix(&doc.source, prefix)
        })
    {
        roots.push(RootGroup {
            display: catalog::UNRECORDED_PREFIX_ROOT.to_string(),
            key: root_key(catalog::UNRECORDED_PREFIX_ROOT),
            run_ids: BTreeSet::new(),
            last_started: String::new(),
            datasets: Vec::new(),
        });
    }
    roots.sort_by(|a, b| a.display.cmp(&b.display));
    attach_root_datasets(&mut roots, &docs, prefix);

    let (selected_key, needs_root, root_not_found) = if let Some(wanted) = root_filter {
        let key = root_key(wanted);
        if roots.iter().any(|g| g.key == key) {
            (Some(key), false, false)
        } else {
            (None, false, true)
        }
    } else if roots.len() > 1 {
        (None, true, false)
    } else {
        (roots.first().map(|g| g.key.clone()), false, false)
    };

    let selected_ids: BTreeSet<&str> = selected_key
        .as_deref()
        .and_then(|key| roots.iter().find(|g| g.key == key))
        .map(|g| g.run_ids.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let single = roots.len() <= 1;
    let deciding = selected_key.is_some() && !needs_root && !root_not_found;

    // Runs of another prefix, by run id, and their prefixes. A `{prefix}-`
    // match on a dataset name is not ownership: `ax-2-docs` starts with `ax-`.
    // A row whose run belongs to another prefix, or a slug-less row whose
    // index sits under a longer sibling prefix, is that prefix's, not ours.
    let mut foreign_runs: BTreeSet<String> = BTreeSet::new();
    let mut sibling_prefixes: BTreeSet<String> = BTreeSet::new();
    for doc in &docs {
        if kind(doc) != "run" || run_matches_prefix(doc, prefix) {
            continue;
        }
        foreign_runs.insert(run_identity(doc));
        foreign_runs.insert(doc.id.clone());
        if let Some(other) = doc
            .source
            .get("prefix")
            .or_else(|| doc.source.get(catalog::CORPUS_SCOPE_FIELD))
            .and_then(Value::as_str)
        {
            if other.len() > prefix.len() && other.starts_with(&format!("{prefix}-")) {
                sibling_prefixes.insert(other.to_string());
            }
        }
    }
    let foreign = |source: &Value| -> bool {
        let run_id = source.get("run_id").and_then(Value::as_str).unwrap_or("");
        if !run_id.is_empty() && foreign_runs.contains(run_id) {
            return true;
        }
        let slugless = source
            .get("slug")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty);
        let names: Vec<&str> = ["index_name", "a_index", "b_index"]
            .iter()
            .filter_map(|k| source.get(*k).and_then(Value::as_str))
            .collect();
        slugless
            && names.iter().any(|name| {
                sibling_prefixes
                    .iter()
                    .any(|other| name.starts_with(&format!("{other}-")))
            })
    };

    let mut rows = Vec::new();
    let mut kept_rows = Vec::new();
    let mut delete_index_names: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut kept_index_reasons: BTreeMap<String, String> = BTreeMap::new();

    for doc in &docs {
        let doc_kind = kind(doc);
        if doc_kind == "run" {
            if !run_matches_prefix(doc, prefix) {
                // Another prefix. Kept so an edges index it shares is not deleted.
                kept_rows.push(row_of(doc, ""));
                continue;
            }
            let key = root_key(
                doc.source
                    .get("root")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(catalog::UNRECORDED_PREFIX_ROOT),
            );
            if deciding && selected_key.as_deref() == Some(key.as_str()) {
                rows.push(row_of(doc, &display_of(&roots, &key)));
            } else if !deciding {
                // Shown under roots; not a delete until a root is chosen.
            } else {
                kept_rows.push(row_of(doc, &display_of(&roots, &key)));
            }
            continue;
        }

        let belongs = match doc_kind {
            "dataset" => catalog::dataset_matches_prefix(&doc.source, prefix),
            "file" => catalog::file_matches_prefix(&doc.source, prefix),
            "correlation" => catalog::correlation_matches_prefix(&doc.source, prefix),
            _ => false,
        };
        if !belongs || foreign(&doc.source) {
            continue;
        }
        let owned = owns_row(&doc.source, &selected_ids, single);
        if deciding && owned {
            let row = row_of(doc, "");
            if doc_kind == "dataset" && !row.index_name.is_empty() {
                let entry = delete_index_names
                    .entry(row.index_name.clone())
                    .or_insert((0, 0));
                entry.0 += row.record_count;
                entry.1 += row.bytes;
            }
            rows.push(row);
        } else if deciding {
            let row = row_of(doc, "");
            if doc_kind == "dataset" && !row.index_name.is_empty() {
                kept_index_reasons
                    .entry(row.index_name.clone())
                    .or_insert_with(|| format!("still named by catalog row {}", row.id));
            }
            kept_rows.push(row);
        }
    }

    // Edges indices are not `{prefix}-*`. Delete one only when every run that
    // names it is being deleted — a sibling prefix's brain is not this folder.
    if deciding {
        let mut edges_kept: BTreeSet<String> = BTreeSet::new();
        let mut edges_drop: BTreeSet<String> = BTreeSet::new();
        for doc in &docs {
            if kind(doc) != "run" {
                continue;
            }
            let Some(edges) = edges_index(&doc.source) else {
                continue;
            };
            let dropping = rows.iter().any(|row| row.id == doc.id);
            if dropping {
                edges_drop.insert(edges);
            } else {
                edges_kept.insert(edges);
            }
        }
        for name in edges_drop {
            if edges_kept.contains(&name) {
                kept_index_reasons
                    .entry(name)
                    .or_insert_with(|| "another catalog run still names this edges index".into());
            } else {
                delete_index_names.entry(name).or_insert((0, 0));
            }
        }
    }

    for name in kept_index_reasons.keys() {
        delete_index_names.remove(name);
    }

    let mut indices = Vec::new();
    let mut kept_indices = Vec::new();
    for (name, (record_count, catalog_bytes)) in &delete_index_names {
        if !deletable_index(name) {
            kept_indices.push(Kept {
                name: name.clone(),
                reason: "this command never deletes autoindex-catalog or a wildcard".into(),
            });
            continue;
        }
        let found = live.iter().find(|idx| idx.name == *name);
        indices.push(PlannedIndex {
            name: name.clone(),
            live_docs: found.map(|idx| idx.docs),
            store_bytes: found.map(|idx| idx.store_bytes),
            record_count: *record_count,
            catalog_bytes: *catalog_bytes,
        });
    }
    for (name, reason) in kept_index_reasons {
        if indices.iter().any(|idx| idx.name == name) {
            continue;
        }
        kept_indices.push(Kept { name, reason });
    }
    indices.sort_by(|a, b| a.name.cmp(&b.name));
    kept_indices.sort_by(|a, b| a.name.cmp(&b.name));
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    kept_rows.sort_by(|a, b| a.id.cmp(&b.id));

    let empty = if root_not_found || needs_root {
        false
    } else if deciding {
        rows.is_empty() && indices.is_empty()
    } else {
        roots.is_empty()
    };

    RemovePlan {
        prefix: prefix.to_string(),
        roots,
        selected_key,
        needs_root,
        root_not_found,
        empty,
        indices,
        kept_indices,
        rows,
        kept_rows,
    }
}

fn attach_root_datasets(roots: &mut [RootGroup], docs: &[&CatalogDoc], prefix: &str) {
    for doc in docs {
        if kind(doc) != "dataset" || !catalog::dataset_matches_prefix(&doc.source, prefix) {
            continue;
        }
        let run_id = doc
            .source
            .get("run_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let row = row_of(doc, "");
        if roots.len() == 1 {
            roots[0].datasets.push(row);
            continue;
        }
        if let Some(group) = roots
            .iter_mut()
            .find(|g| !run_id.is_empty() && g.run_ids.contains(run_id))
        {
            group.datasets.push(row);
        }
    }
    for group in roots {
        group.datasets.sort_by(|a, b| a.id.cmp(&b.id));
    }
}

fn display_of(roots: &[RootGroup], key: &str) -> String {
    roots
        .iter()
        .find(|g| g.key == key)
        .map(|g| g.display.clone())
        .unwrap_or_default()
}

fn owns_row(source: &Value, selected_ids: &BTreeSet<&str>, single_root: bool) -> bool {
    let run_id = source.get("run_id").and_then(Value::as_str).unwrap_or("");
    if !run_id.is_empty() && selected_ids.contains(run_id) {
        return true;
    }
    // One folder under the prefix: a legacy row with no run_id, and a row
    // whose run_id no longer matches, still belong to that folder. Two
    // folders: a row we cannot attribute is left alone.
    single_root
}

fn deletable_index(name: &str) -> bool {
    !name.is_empty()
        && name != CATALOG_INDEX
        && name != "_all"
        && !name.contains('*')
        && !name.contains('?')
}

fn kind(doc: &CatalogDoc) -> &str {
    doc.source
        .get("doc_kind")
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn run_matches_prefix(doc: &CatalogDoc, prefix: &str) -> bool {
    doc.source
        .get("prefix")
        .or_else(|| doc.source.get(catalog::CORPUS_SCOPE_FIELD))
        .and_then(Value::as_str)
        == Some(prefix)
}

fn run_identity(doc: &CatalogDoc) -> String {
    doc.source
        .get("run_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| doc.id.clone())
}

fn edges_index(source: &Value) -> Option<String> {
    source
        .pointer("/graph/edges_index")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn row_of(doc: &CatalogDoc, root: &str) -> Row {
    let index_name = doc
        .source
        .get("index_name")
        .or_else(|| doc.source.get("a_index"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let root = if root.is_empty() {
        doc.source
            .get("root")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    } else {
        root.to_string()
    };
    Row {
        id: doc.id.clone(),
        doc_kind: kind(doc).to_string(),
        root,
        index_name,
        record_count: u64_of(&doc.source, "record_count")
            .or_else(|| u64_of(&doc.source, "records"))
            .unwrap_or(0),
        bytes: u64_of(&doc.source, "bytes").unwrap_or(0),
        file_count: u64_of(&doc.source, "file_count").unwrap_or(0),
    }
}

fn u64_of(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

/// Compare a catalog root and a `--root` argument. A path that exists is
/// canonicalized; a folder that has already been deleted still matches the
/// string the catalog stored.
pub fn root_key(root: &str) -> String {
    let trimmed = root.trim().trim_end_matches(['/', '\\']);
    if trimmed.is_empty() || trimmed == catalog::UNRECORDED_PREFIX_ROOT {
        return catalog::UNRECORDED_PREFIX_ROOT.to_string();
    }
    let path = std::path::Path::new(trimmed);
    if path.exists() {
        if let Ok(canon) = path.canonicalize() {
            let text = canon.to_string_lossy();
            return text.trim_end_matches(['/', '\\']).to_string();
        }
    }
    trimmed.to_string()
}

fn index_names_of(docs: &[CatalogDoc], prefix: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for doc in docs {
        if catalog::dataset_matches_prefix(&doc.source, prefix) {
            if let Some(name) = doc.source.get("index_name").and_then(Value::as_str) {
                if deletable_index(name) {
                    names.insert(name.to_string());
                }
            }
        }
        if kind(doc) == "run" && run_matches_prefix(doc, prefix) {
            if let Some(name) = edges_index(&doc.source) {
                if deletable_index(&name) {
                    names.insert(name);
                }
            }
        }
    }
    names
}

fn fetch_catalog(es: &Es, prefix: &str) -> Result<Vec<CatalogDoc>> {
    let mut docs = Vec::new();
    docs.extend(search_kind(es, json!({"term": {"doc_kind": "run"}}))?);
    let scoped = format!("{prefix}-");
    docs.extend(search_kind(
        es,
        json!({"bool": {"filter": [
            {"term": {"doc_kind": "dataset"}},
            {"prefix": {"index_name": scoped}}
        ]}}),
    )?);
    docs.extend(search_kind(
        es,
        json!({"bool": {"filter": [
            {"term": {"doc_kind": "file"}},
            {"bool": {"should": [
                {"term": {"prefix": prefix}},
                {"term": {catalog::CORPUS_SCOPE_FIELD: prefix}}
            ], "minimum_should_match": 1}}
        ]}}),
    )?);
    docs.extend(search_kind(
        es,
        json!({"bool": {"filter": [
            {"term": {"doc_kind": "correlation"}},
            {"prefix": {"a_index": scoped}}
        ]}}),
    )?);
    Ok(docs)
}

fn search_kind(es: &Es, query: Value) -> Result<Vec<CatalogDoc>> {
    let mut from = 0usize;
    let mut out = Vec::new();
    loop {
        if from >= WINDOW {
            anyhow::bail!(
                "autoindex remove: the catalog page is larger than {WINDOW} documents, so this \
                 command cannot see every row. Nothing was deleted."
            );
        }
        let body = json!({
            "query": query,
            "size": PAGE,
            "from": from,
            "sort": [{"_id": "asc"}]
        });
        let Some(page) = es.search_page_present(CATALOG_INDEX, &body)? else {
            return Ok(Vec::new());
        };
        let hits = page
            .pointer("/hits/hits")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if hits.is_empty() {
            break;
        }
        for hit in &hits {
            let id = hit
                .get("_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if id.is_empty() {
                continue;
            }
            let source = hit.get("_source").cloned().unwrap_or(Value::Null);
            out.push(CatalogDoc { id, source });
        }
        let total = hit_total(&page);
        from += hits.len();
        if hits.len() < PAGE || total.is_some_and(|n| from as u64 >= n) {
            break;
        }
        if from >= WINDOW {
            anyhow::bail!(
                "autoindex remove: read {WINDOW} catalog documents and the page was still full. \
                 Nothing was deleted."
            );
        }
    }
    Ok(out)
}

fn hit_total(page: &Value) -> Option<u64> {
    let total = page.pointer("/hits/total")?;
    total
        .as_u64()
        .or_else(|| total.get("value").and_then(Value::as_u64))
}

fn fetch_live(es: &Es, names: &BTreeSet<String>) -> Result<Vec<LiveIndex>> {
    let mut out = Vec::new();
    for name in names {
        let path = format!("/{}/_stats", path_escape(name));
        let (status, body) = es.request_json("GET", &path, None)?;
        if status == 404 {
            continue;
        }
        if !(200..300).contains(&status) {
            // A stats miss is not a reason to guess a size. The plan still
            // lists the index from the catalog; live numbers stay absent.
            continue;
        }
        let stats = body
            .pointer(&format!("/indices/{name}/primaries"))
            .or_else(|| body.pointer(&format!("/indices/{name}/total")))
            .or_else(|| body.pointer("/_all/primaries"));
        let docs = stats
            .and_then(|s| s.pointer("/docs/count"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let store_bytes = stats
            .and_then(|s| s.pointer("/store/size_in_bytes"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        out.push(LiveIndex {
            name: name.clone(),
            docs,
            store_bytes,
        });
    }
    Ok(out)
}

fn apply(es: &Es, plan: &RemovePlan) -> Result<()> {
    for index in &plan.indices {
        if !deletable_index(&index.name) {
            anyhow::bail!("refusing to delete {}", index.name);
        }
        let path = format!("/{}", path_escape(&index.name));
        let (status, body) = es
            .request_json("DELETE", &path, None)
            .with_context(|| format!("DELETE {path}"))?;
        if status == 404 || (200..300).contains(&status) {
            continue;
        }
        anyhow::bail!("DELETE {path} returned HTTP {status}: {body}");
    }
    if plan.rows.is_empty() {
        return Ok(());
    }
    let mut body = Vec::new();
    for row in &plan.rows {
        if row.id == CATALOG_INDEX {
            anyhow::bail!("refusing to delete the {CATALOG_INDEX} index");
        }
        let action = json!({"delete": {"_index": CATALOG_INDEX, "_id": row.id}});
        body.extend_from_slice(action.to_string().as_bytes());
        body.push(b'\n');
    }
    let outcome = es.bulk(body).context("delete catalog rows")?;
    if outcome.item_errors > 0 || outcome.server_errors > 0 {
        // A missing row is already the end state we want. Retry each id and
        // treat 404 as gone. Anything else is the partway failure.
        for row in &plan.rows {
            let path = format!("/{CATALOG_INDEX}/_doc/{}", path_escape(&row.id));
            let (status, resp) = es.request_json("DELETE", &path, None)?;
            if status == 404 || (200..300).contains(&status) {
                continue;
            }
            anyhow::bail!(
                "catalog row {} was not deleted (HTTP {status}: {resp}). bulk said: {}",
                row.id,
                outcome
                    .first_error
                    .as_deref()
                    .or(outcome.first_server_error.as_deref())
                    .unwrap_or("unknown")
            );
        }
    }
    es.refresh(CATALOG_INDEX).ok();
    Ok(())
}

fn path_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn report(plan: &RemovePlan, exit_code: i32, deleted: bool, error: Option<String>) -> RemoveReport {
    let summary = summary_text(plan, exit_code, deleted, error.as_deref());
    RemoveReport {
        exit_code,
        body: plan_body(plan, exit_code, deleted, error),
        summary,
    }
}

fn plan_body(plan: &RemovePlan, exit_code: i32, deleted: bool, error: Option<String>) -> Value {
    let selected = plan.selected_key.as_ref().and_then(|key| {
        plan.roots
            .iter()
            .find(|g| g.key == *key)
            .map(|g| g.display.clone())
    });
    json!({
        "action": "autoindex-remove",
        "prefix": plan.prefix,
        "exit": exit_code,
        "deleted": deleted,
        "needs_root": plan.needs_root,
        "root_not_found": plan.root_not_found,
        "empty": plan.empty,
        "selected_root": selected,
        "roots": plan.roots.iter().map(|g| json!({
            "root": g.display,
            "last_started": if g.last_started.is_empty() { Value::Null } else { json!(g.last_started) },
            "datasets": g.datasets.iter().map(row_json).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "remove": {
            "indices": plan.indices.iter().map(|idx| json!({
                "name": idx.name,
                "docs": idx.live_docs,
                "store_bytes": idx.store_bytes,
                "record_count": idx.record_count,
                "bytes": idx.catalog_bytes,
            })).collect::<Vec<_>>(),
            "catalog_rows": plan.rows.iter().map(row_json).collect::<Vec<_>>(),
        },
        "keep": {
            "indices": plan.kept_indices.iter().map(|idx| json!({
                "name": idx.name,
                "reason": idx.reason,
            })).collect::<Vec<_>>(),
            "catalog_rows": plan.kept_rows.iter().filter(|row| {
                // Runs from other prefixes are evidence for edges sharing, not
                // rows of this prefix the caller asked to see as "kept".
                row.doc_kind != "run" || plan.roots.iter().any(|g| g.display == row.root || g.key == root_key(&row.root))
            }).map(row_json).collect::<Vec<_>>(),
        },
        "error": error,
    })
}

fn row_json(row: &Row) -> Value {
    json!({
        "id": row.id,
        "doc_kind": row.doc_kind,
        "root": if row.root.is_empty() { Value::Null } else { json!(row.root) },
        "index_name": if row.index_name.is_empty() { Value::Null } else { json!(row.index_name) },
        "record_count": row.record_count,
        "bytes": row.bytes,
        "file_count": row.file_count,
    })
}

fn summary_text(plan: &RemovePlan, exit_code: i32, deleted: bool, error: Option<&str>) -> String {
    let mut lines = Vec::new();
    lines.push(format!("autoindex remove: prefix {}", plan.prefix));
    if plan.roots.is_empty() {
        lines.push("no catalog run under this prefix.".into());
    } else {
        lines.push("roots:".into());
        for root in &plan.roots {
            let when = if root.last_started.is_empty() {
                String::new()
            } else {
                format!("  last {}", root.last_started)
            };
            lines.push(format!("  {}{when}", root.display));
        }
    }
    if plan.needs_root {
        lines.push(
            "this prefix holds more than one folder. Nothing was deleted. Pass --root with one \
             path listed above. --yes does not choose a folder."
                .into(),
        );
    } else if plan.root_not_found {
        lines.push(
            "error: --root does not match a folder under this prefix. Nothing was deleted.".into(),
        );
    } else if plan.empty {
        lines.push("error: nothing under this prefix to remove. Nothing was deleted.".into());
    } else {
        lines.push("will remove:".into());
        if plan.indices.is_empty() && plan.rows.is_empty() {
            lines.push("  (nothing)".into());
        }
        for idx in &plan.indices {
            let docs = idx
                .live_docs
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unknown".into());
            let size = idx
                .store_bytes
                .map(|n| format!("{n}b"))
                .unwrap_or_else(|| "unknown".into());
            lines.push(format!(
                "  index {}  live docs {docs}  store {size}  catalog records {}  catalog bytes {}",
                idx.name, idx.record_count, idx.catalog_bytes
            ));
        }
        for row in &plan.rows {
            lines.push(format!(
                "  catalog {}  {}  records {}  bytes {}",
                row.id, row.doc_kind, row.record_count, row.bytes
            ));
        }
        if !plan.kept_indices.is_empty() {
            lines.push(
                "will keep (another folder under this prefix, or a shared edges index):".into(),
            );
            for idx in &plan.kept_indices {
                lines.push(format!("  index {}  {}", idx.name, idx.reason));
            }
        }
        if deleted {
            lines.push(format!(
                "removed {} index(es) and {} catalog row(s).",
                plan.indices.len(),
                plan.rows.len()
            ));
        } else if exit_code == EXIT_NEEDS_DECISION {
            lines.push(
                "Nothing was deleted. On a terminal, answer the prompt. Otherwise rerun with --yes."
                    .into(),
            );
        }
    }
    if let Some(error) = error {
        lines.push(format!("error: {error}"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: &str, source: Value) -> CatalogDoc {
        CatalogDoc {
            id: id.to_string(),
            source,
        }
    }

    fn run(id: &str, prefix: &str, root: &str, started: &str) -> CatalogDoc {
        doc(
            id,
            json!({
                "doc_kind": "run",
                "prefix": prefix,
                "root": root,
                "run_id": id.trim_start_matches("run:"),
                "started": started
            }),
        )
    }

    fn dataset(
        id: &str,
        index: &str,
        slug: &str,
        run_id: &str,
        records: u64,
        bytes: u64,
    ) -> CatalogDoc {
        doc(
            id,
            json!({
                "doc_kind": "dataset",
                "index_name": index,
                "slug": slug,
                "run_id": run_id,
                "record_count": records,
                "bytes": bytes,
                "file_count": 1
            }),
        )
    }

    fn two_roots() -> Vec<CatalogDoc> {
        vec![
            run("run:a", "ax", "/data/a", "2026-01-01T00:00:00Z"),
            run("run:b", "ax", "/data/b", "2026-02-01T00:00:00Z"),
            dataset("ds:ax:only-a", "ax-only-a", "only-a", "a", 2, 10),
            dataset("ds:ax:docs", "ax-docs", "docs", "b", 5, 50),
            doc(
                "file:ax:a",
                json!({"doc_kind":"file","prefix":"ax","run_id":"a","records":2,"bytes":10}),
            ),
            doc(
                "file:ax:b",
                json!({"doc_kind":"file","prefix":"ax","run_id":"b","records":5,"bytes":50}),
            ),
            dataset("ds:ax-2:docs", "ax-2-docs", "docs", "other", 9, 90),
            doc(
                "run:other",
                json!({
                    "doc_kind":"run","prefix":"ax-2","root":"/data/other",
                    "run_id":"other","started":"2026-03-01T00:00:00Z",
                    "graph": {"edges_index": ".xerj-memory-shared-edges"}
                }),
            ),
        ]
    }

    fn names(plan: &RemovePlan) -> Vec<String> {
        plan.indices.iter().map(|i| i.name.clone()).collect()
    }

    fn row_ids(plan: &RemovePlan) -> Vec<String> {
        plan.rows.iter().map(|r| r.id.clone()).collect()
    }

    #[test]
    fn a_sibling_prefix_dataset_is_never_ours() {
        // A legacy row with no slug: `ax-2-docs` starts with `ax-`, but the
        // catalog names a run of prefix `ax-2` for it. Removing `ax` keeps it,
        // and keeps it even when the row carries no run id at all.
        let mut docs = vec![
            run("run:a", "ax", "/data/a", "2026-01-01T00:00:00Z"),
            dataset("ds:ax:docs", "ax-docs", "docs", "a", 5, 50),
            doc(
                "run:other",
                json!({"doc_kind":"run","prefix":"ax-2","root":"/data/other",
                       "run_id":"other","started":"2026-03-01T00:00:00Z"}),
            ),
            doc(
                "ds:ax-2:docs",
                json!({"doc_kind":"dataset","index_name":"ax-2-docs","run_id":"other",
                       "record_count":9,"bytes":90}),
            ),
            doc(
                "ds:ax-2:code",
                json!({"doc_kind":"dataset","index_name":"ax-2-code",
                       "record_count":3,"bytes":30}),
            ),
        ];
        docs.push(dataset("ds:ax:code", "ax-code", "code", "a", 1, 10));
        let plan = plan_remove("ax", None, &docs, &[]);
        assert!(!plan.needs_root, "{plan:?}");
        let n = names(&plan);
        assert_eq!(
            n,
            vec!["ax-code".to_string(), "ax-docs".to_string()],
            "{n:?}"
        );
        let ids = row_ids(&plan);
        assert!(!ids.iter().any(|id| id.starts_with("ds:ax-2")), "{ids:?}");
        assert!(!ids.contains(&"run:other".to_string()), "{ids:?}");
    }

    #[test]
    fn two_roots_without_a_root_delete_nothing() {
        let plan = plan_remove("ax", None, &two_roots(), &[]);
        assert!(plan.needs_root, "{plan:?}");
        assert!(plan.indices.is_empty());
        assert!(plan.rows.is_empty());
        assert_eq!(plan.roots.len(), 2);
        let body = plan_body(&plan, 4, false, None);
        let roots: Vec<&str> = body["roots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["root"].as_str().unwrap())
            .collect();
        assert_eq!(roots, vec!["/data/a", "/data/b"]);
        let a = body["roots"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["root"] == "/data/a")
            .unwrap();
        let indexes: Vec<&str> = a["datasets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["index_name"].as_str().unwrap())
            .collect();
        assert_eq!(indexes, vec!["ax-only-a"]);
    }

    #[test]
    fn removing_one_root_leaves_the_other_folders_indices_and_rows() {
        let live = vec![
            LiveIndex {
                name: "ax-only-a".into(),
                docs: 2,
                store_bytes: 100,
            },
            LiveIndex {
                name: "ax-docs".into(),
                docs: 5,
                store_bytes: 500,
            },
        ];
        let plan = plan_remove("ax", Some("/data/a"), &two_roots(), &live);
        assert!(!plan.needs_root && !plan.root_not_found, "{plan:?}");
        assert_eq!(names(&plan), vec!["ax-only-a".to_string()]);
        assert!(plan.indices[0].live_docs == Some(2));
        assert!(plan.indices[0].store_bytes == Some(100));
        assert_eq!(plan.indices[0].record_count, 2);
        let ids = row_ids(&plan);
        assert!(ids.contains(&"ds:ax:only-a".into()), "{ids:?}");
        assert!(ids.contains(&"run:a".into()), "{ids:?}");
        assert!(ids.contains(&"file:ax:a".into()), "{ids:?}");
        assert!(!ids.iter().any(|id| id.contains("ds:ax:docs")), "{ids:?}");
        assert!(
            !ids.iter().any(|id| id == "run:b" || id == "file:ax:b"),
            "{ids:?}"
        );
        assert!(!names(&plan)
            .iter()
            .any(|n| n == "ax-docs" || n == "ax-2-docs" || n == CATALOG_INDEX));
        assert!(
            plan.kept_indices.iter().any(|k| k.name == "ax-docs"),
            "{:?}",
            plan.kept_indices
        );
        let kept_ids: Vec<_> = plan.kept_rows.iter().map(|r| r.id.as_str()).collect();
        assert!(kept_ids.contains(&"ds:ax:docs"), "{kept_ids:?}");
        assert!(kept_ids.contains(&"run:b"), "{kept_ids:?}");
        assert!(kept_ids.contains(&"file:ax:b"), "{kept_ids:?}");
    }

    /// The index that made #2114 expensive: both folders' catalog rows name
    /// `ax-docs`. Removing one folder deletes its rows and leaves the index,
    /// because the other folder still names it.
    #[test]
    fn an_index_another_root_still_names_is_not_deleted() {
        let docs = vec![
            run("run:a", "ax", "/data/a", "2026-01-01T00:00:00Z"),
            run("run:b", "ax", "/data/b", "2026-02-01T00:00:00Z"),
            dataset("ds:ax:docs", "ax-docs", "docs", "a", 2, 10),
            dataset("ds:ax:docs-b", "ax-docs", "docs", "b", 5, 50),
        ];
        let plan = plan_remove("ax", Some("/data/a"), &docs, &[]);
        assert!(
            !names(&plan).iter().any(|n| n == "ax-docs"),
            "{:?}",
            names(&plan)
        );
        assert!(plan.kept_indices.iter().any(|k| k.name == "ax-docs"));
        let ids = row_ids(&plan);
        assert!(ids.contains(&"ds:ax:docs".into()), "{ids:?}");
        assert!(ids.contains(&"run:a".into()), "{ids:?}");
        assert!(!ids.contains(&"ds:ax:docs-b".into()), "{ids:?}");
        assert!(!ids.contains(&"run:b".into()), "{ids:?}");
    }

    #[test]
    fn a_shared_edges_index_is_not_deleted() {
        let mut docs = two_roots();
        docs[0].source["graph"] = json!({"edges_index": ".xerj-memory-shared-edges"});
        let plan = plan_remove("ax", Some("/data/a"), &docs, &[]);
        assert!(
            !names(&plan).iter().any(|n| n.contains("edges")),
            "{:?}",
            names(&plan)
        );
        assert!(plan
            .kept_indices
            .iter()
            .any(|k| k.name == ".xerj-memory-shared-edges"));
    }

    #[test]
    fn an_edges_index_only_this_folder_names_is_removed() {
        let mut docs = vec![
            run("run:a", "ax", "/data/a", "2026-01-01T00:00:00Z"),
            dataset("ds:ax:docs", "ax-docs", "docs", "a", 1, 1),
        ];
        docs[0].source["graph"] = json!({"edges_index": ".xerj-memory-a-edges"});
        let plan = plan_remove("ax", None, &docs, &[]);
        assert!(
            names(&plan).contains(&".xerj-memory-a-edges".into()),
            "{:?}",
            names(&plan)
        );
        assert!(names(&plan).contains(&"ax-docs".into()));
    }

    #[test]
    fn the_catalog_index_is_never_a_delete_target() {
        let docs = vec![
            run("run:a", "ax", "/data/a", "2026-01-01T00:00:00Z"),
            dataset("ds:ax:catalog", CATALOG_INDEX, "catalog", "a", 1, 1),
        ];
        let plan = plan_remove("ax", None, &docs, &[]);
        assert!(
            !names(&plan).iter().any(|n| n == CATALOG_INDEX),
            "{:?}",
            names(&plan)
        );
    }

    #[test]
    fn an_unknown_root_deletes_nothing() {
        let plan = plan_remove("ax", Some("/data/missing"), &two_roots(), &[]);
        assert!(plan.root_not_found);
        assert!(plan.indices.is_empty());
        assert!(plan.rows.is_empty());
    }

    #[test]
    fn one_root_is_selected_without_passing_root() {
        let docs = vec![
            run("run:a", "ax", "/data/a", "2026-01-01T00:00:00Z"),
            dataset("ds:ax:docs", "ax-docs", "docs", "a", 4, 40),
        ];
        let plan = plan_remove("ax", None, &docs, &[]);
        assert!(!plan.needs_root, "{plan:?}");
        assert_eq!(names(&plan), vec!["ax-docs".to_string()]);
        assert_eq!(plan.indices[0].record_count, 4);
        assert_eq!(plan.indices[0].catalog_bytes, 40);
    }

    #[test]
    fn trailing_slash_on_root_matches_the_catalog_path() {
        let docs = vec![run("run:a", "ax", "/data/a", "2026-01-01T00:00:00Z")];
        let plan = plan_remove("ax", Some("/data/a/"), &docs, &[]);
        assert!(!plan.root_not_found, "{plan:?}");
        assert!(row_ids(&plan).contains(&"run:a".into()));
    }

    /// The acceptance check that is not a planner property: without `--yes`
    /// and without a terminal, the HTTP client must not send DELETE.
    #[test]
    fn without_yes_the_node_receives_no_delete() {
        let (url, log) = spawn_catalog_server(&two_roots());
        let report = execute(RemoveRequest {
            url,
            api_key: None,
            prefix: "ax".into(),
            root: None,
            yes: false,
            prompt: false,
        })
        .expect("plan");
        assert_eq!(report.exit_code, EXIT_NEEDS_DECISION, "{}", report.summary);
        assert_eq!(report.body["deleted"], false);
        assert_eq!(report.body["needs_root"], true);
        let roots = report.body["roots"].as_array().unwrap();
        assert_eq!(roots.len(), 2);
        let calls = log.lock().unwrap().clone();
        assert!(
            calls.iter().all(|c| !c.starts_with("DELETE ")),
            "a plan must not delete: {calls:?}"
        );
        assert!(report.summary.contains("/data/a"), "{}", report.summary);
        assert!(
            report.summary.contains("Nothing was deleted"),
            "{}",
            report.summary
        );
    }

    /// `--yes` confirms a plan. It does not pick a folder when the prefix
    /// holds more than one.
    #[test]
    fn yes_without_a_root_still_deletes_nothing_when_two_folders_share_the_prefix() {
        let (url, log) = spawn_catalog_server(&two_roots());
        let report = execute(RemoveRequest {
            url,
            api_key: None,
            prefix: "ax".into(),
            root: None,
            yes: true,
            prompt: false,
        })
        .expect("plan");
        assert_eq!(report.exit_code, EXIT_NEEDS_DECISION, "{}", report.summary);
        assert_eq!(report.body["deleted"], false);
        assert_eq!(report.body["needs_root"], true);
        assert!(
            report.summary.contains("--yes does not choose a folder"),
            "{}",
            report.summary
        );
        let calls = log.lock().unwrap().clone();
        assert!(
            calls.iter().all(|c| !c.starts_with("DELETE ")),
            "choosing a folder is not optional: {calls:?}"
        );
    }

    #[test]
    fn yes_with_a_root_deletes_only_that_folders_index_and_rows() {
        let (url, log) = spawn_catalog_server(&two_roots());
        let report = execute(RemoveRequest {
            url,
            api_key: None,
            prefix: "ax".into(),
            root: Some("/data/a".into()),
            yes: true,
            prompt: false,
        })
        .expect("remove");
        assert_eq!(report.exit_code, 0, "{}\n{}", report.summary, report.body);
        assert_eq!(report.body["deleted"], true);
        let calls = log.lock().unwrap().clone();
        let deletes: Vec<_> = calls
            .iter()
            .filter(|c| c.starts_with("DELETE "))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(deletes, vec!["DELETE /ax-only-a".to_string()], "{calls:?}");
        let bulk = calls
            .iter()
            .find(|c| c.starts_with("BULK "))
            .cloned()
            .unwrap_or_default();
        assert!(bulk.contains("ds:ax:only-a"), "{bulk}");
        assert!(bulk.contains("run:a"), "{bulk}");
        assert!(!bulk.contains("ds:ax:docs"), "{bulk}");
        assert!(!bulk.contains("run:b"), "{bulk}");
        assert!(
            report.body["remove"]["indices"][0]["docs"] == 2,
            "{}",
            report.body
        );
        assert!(
            report.body["remove"]["indices"][0]["store_bytes"] == 100,
            "{}",
            report.body
        );
    }

    fn spawn_catalog_server(
        docs: &[CatalogDoc],
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        let hits: Vec<Value> = docs
            .iter()
            .map(|d| json!({"_id": d.id, "_source": d.source}))
            .collect();
        let catalog = json!({"hits": {"total": {"value": hits.len()}, "hits": hits}}).to_string();
        let log = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let log_thread = Arc::clone(&log);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = ready_tx.send(());
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .ok();
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                loop {
                    match stream.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            while let Some((head, body)) = take_http(&mut buf) {
                                let resp = answer(&head, &body, &catalog, &log_thread);
                                if stream.write_all(resp.as_bytes()).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
            }
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("catalog server thread");
        (format!("http://127.0.0.1:{port}"), log)
    }

    fn take_http(buf: &mut Vec<u8>) -> Option<(String, Vec<u8>)> {
        let split = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
        let head = String::from_utf8_lossy(&buf[..split]).to_string();
        let rest = buf.split_off(split + 4);
        *buf = rest;
        let len = head
            .lines()
            .find_map(|l| {
                l.split_once(':')
                    .filter(|(n, _)| n.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, v)| v.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        if buf.len() < len {
            // Put the bytes back and wait for the rest of the body.
            let mut combined = head.into_bytes();
            combined.extend_from_slice(b"\r\n\r\n");
            combined.extend_from_slice(buf);
            *buf = combined;
            return None;
        }
        let body = buf.drain(..len).collect();
        Some((head, body))
    }

    fn answer(
        head: &str,
        body: &[u8],
        catalog: &str,
        log: &std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> String {
        let req = head.lines().next().unwrap_or("");
        let mut parts = req.split_whitespace();
        let method = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("/").split('?').next().unwrap_or("/");
        let mut line = format!("{method} {path}");
        if method == "POST" && path == "/_bulk" {
            line = format!("BULK {}", String::from_utf8_lossy(body));
        }
        log.lock().unwrap().push(line);
        let json_body = if method == "GET" && path == "/" {
            "{}".into()
        } else if path.contains("_search") {
            catalog.to_string()
        } else if path.ends_with("/_stats") {
            let name = path.trim_start_matches('/').trim_end_matches("/_stats");
            let (docs, bytes) = if name == "ax-only-a" {
                (2u64, 100u64)
            } else {
                (5, 500)
            };
            json!({
                "indices": {
                    name: {
                        "primaries": {
                            "docs": {"count": docs},
                            "store": {"size_in_bytes": bytes}
                        }
                    }
                }
            })
            .to_string()
        } else if method == "POST" && path == "/_bulk" {
            "{\"errors\":false,\"items\":[]}".into()
        } else {
            "{\"acknowledged\":true}".into()
        };
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{json_body}",
            json_body.len()
        )
    }
}
