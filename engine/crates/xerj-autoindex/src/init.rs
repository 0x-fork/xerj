//! `xerj init` — wire XERJ into the coding agents already installed here.
//!
//! The adoption lesson from the agent-tool field is one-command setup: the
//! tools that got used are the ones a user (or an agent) could wire in with a
//! single command that writes the right config files and gets out of the way.
//! This does exactly that, and nothing else: detect which agent surfaces
//! exist, write the smallest useful teaching file for each, register the MCP
//! server, and never touch a byte we did not write without a `.bak` backup.
//!
//! Deliberately NOT here: prompt rewriting, command interception, or anything
//! that changes what the agent was going to do. XERJ is a tool the agent
//! calls; init only makes the tool visible.

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

const USAGE: &str = "\
xerj init — wire XERJ into the coding agents installed in this project

USAGE:
    xerj init [OPTIONS]

Detects agent surfaces in the current directory and writes the minimal
config for each — with a .bak backup for any file it edits:

    .mcp.json                    register the `xerj mcp` server (Claude Code
                                 project scope; merged, other servers kept)
    .claude/skills/xerj/SKILL.md teach `xerj search` / `xerj def` (10 lines)
    .claude/skills/xerj-hub/SKILL.md
                                 teach `xerj \"<question>\"`: when to ask the
                                 hub, `--def`, the MCP registration per client
    .cursor/rules/xerj.mdc       same teaching file for Cursor (if .cursor/)
    .cursor/rules/xerj-hub.mdc   the hub teaching file for Cursor (if .cursor/)
    AGENTS.md                    a short XERJ section (appended if the file
                                 exists, created otherwise)

OPTIONS:
    --url <U>       node endpoint baked into the MCP config
                    (default $XERJ_URL or http://localhost:9200)
    --dry-run       print what would be written, write nothing
    -h, --help      this help

After init: restart the agent session so it picks up the new tools, then
`xerj autoindex <folder>` anything you want searchable. Public libraries,
standards and advisories need no indexing: `xerj \"<question>\"` asks the hub.
init also prints the MCP registration line for each client.

Auth: nothing secret is written (`.mcp.json` is meant to be committed). For a
loopback node the MCP server reads <data-dir>/admin.key by itself and says so
on stderr; for any other node add \"XERJ_AUTH\": \"ApiKey <key>\" to the env
block — init prints which case you are in.
";

/// The ten-line teaching file. One idea per line, no filler: what the tools
/// are, when to reach for them, what comes back.
fn skill_md() -> String {
    "---\nname: xerj\ndescription: Definition-first code search over locally indexed repos (xerj search / xerj def)\n---\n\n\
     Use XERJ instead of grep when you need code you cannot see in the open files:\n\n\
     - `xerj def \"<symbol>\"` — where is this defined? Returns file:line + signature in one call.\n\
     - `xerj search \"<plain words>\"` — ranked passages, definitions first; cite the printed file:line.\n\
     - `xerj autoindex <folder>` — index a repo first (once; re-index is incremental).\n\
     - MCP: the `xerj_search` tool accepts a plain string query and returns the same passages.\n\n\
     Trust the passage over memory: it is the actual code at that path.\n"
        .to_string()
}

fn agents_md_section() -> String {
    "\n## XERJ (code search)\n\n\
     This project uses XERJ for reference coding. Before writing code that touches\n\
     unfamiliar territory: `xerj def \"<symbol>\"` for go-to-definition, or\n\
     `xerj search \"<plain words>\"` for ranked, definition-first passages.\n\
     Index a repo with `xerj autoindex <folder>`. Cite file:line for anything you rely on.\n"
        .to_string()
}

/// The second teaching file (Corpus Hub v2, plan section 9): when to ask the
/// hub instead of searching local files, the one line that does it, the exact
/// form, how to stop a node safely, and the MCP registration per client.
///
/// Kept as a raw string rather than a `.md` asset: the repository ignores
/// `*.md` by default, and a `include_str!` of an ignored file compiles on a
/// developer machine and fails in CI.
///
/// Rules this text follows (docs/research/llms-txt-2026-09/README.md section
/// 6): no obligation language; every step ends with what success looks like;
/// the registration is printed per client with the root key that client reads
/// (`mcpServers` vs VS Code's `servers`); no line over 300 characters.
const HUB_SKILL_MD: &str = r##"---
name: xerj-hub
description: Answer a question about public source code, a standard, a regulation or an advisory feed from a pre-indexed hub corpus with one command, xerj "your question". Use it for exact public material you cannot quote from memory. Not for this project's own files.
---

# XERJ hub: ask, do not clone

Pick the source before you search:

- **HUB**: public material you cannot quote exactly. A library's source at a pinned commit, a
  standard, a regulation, an advisory feed.
- **LOCAL**: files only this machine has. This project, private documents, exports.
  `xerj autoindex <folder>` once, then `xerj search "<words>"` or `xerj def "<symbol>"`.
- **NEITHER**: a fact you can already state exactly. Retrieval on memorised content only adds cost.

## Ask the hub

```sh
xerj "<question>"
```

One line. No corpus name, no clone, no index. XERJ picks the corpus (your project's dependencies
first), fetches its signed, pre-indexed pack on first use, and answers. Retrieval is lexical, not
semantic: use the words and identifiers the source uses.

A good answer is three lines, then any `warning:` lines:

```text
<corpus>  pin <date> (<N> days old)  <licence>  <file>:<line>
<the passage, or the declaration>
answered from: local pack
```

`pin` is the source revision the corpus was built from: check its age when you ask about the current
version. `answered from:` can also say `hosted hub`. Tell the user which corpus, pin date and licence
your answer rests on. `warning: approach-only licence` means read the source, do not copy it.

## Exact questions

```sh
xerj ask --def <SYMBOL>
```

Use `--def` for a value, a signature or a default. It prints the definition (`file:line`, the
declaration, and a `value:` line for a constant or macro) and no passages. Add `--corpus <slug>` when
more than one corpus defines the name.

## When it does not answer

- Exit 4: more than one corpus fits. The candidates print, one per line. Run again with
  `--corpus <slug>`.
- Exit 1, `no hub corpus matches`: the hub does not hold it. Say so. Do not answer from memory.
- Exit 3: a downloaded pack failed its checksum or signature check and was discarded. Do not use it.
  Tell the user.
- `run: xerj hub pull <slug> (<MB> MB)`: the pack is above the automatic download limit. Run that
  line, then ask again. Success: `pulled <slug> <pin8> <bytes> → <n> indices, <defs> definitions`.

## The node, and where the question goes

If a node answers on `localhost:9200`, XERJ uses it as it is. If none does, XERJ starts its own and
records its PID. Stop only the node you started: `xerj hub stop` for XERJ's own,
`kill $(cat ~/xerj-node.pid)` for one you started yourself and saved the PID of.
Never `pkill xerj` or `killall xerj`: other XERJ nodes may be running on this machine.

By default, inside a git repository the question is answered on this machine. Outside one it may be
sent to the hosted hub. `xerj hub config hosted off` forbids that.

## MCP

- `xerj_hub_ask` with `{"question": "<question>"}`: the one line. Add `"corpus": "<slug>"` to name
  a corpus.
- `xerj_hub_def` with `{"slug": "<slug>", "symbol": "<SYMBOL>"}`: the `--def` form.
- `xerj_hub_find` with `{"question": "<question>"}`: which corpora hold this.

Register `xerj mcp` once, then restart the session. Success: `xerj_hub_ask` is in the tool list.
Use the absolute path to the binary (`command -v xerj`): an editor started from a desktop icon does
not inherit your shell PATH.

- Claude Code: `claude mcp add --scope user xerj -- "$(command -v xerj)" mcp`
- `.mcp.json` (Claude Code, project scope; `xerj init` writes it), Cursor, Windsurf, Claude Desktop,
  Cline: root key `mcpServers`.
  `{"mcpServers": {"xerj": {"command": "/abs/path/to/xerj", "args": ["mcp"]}}}`
- VS Code (`.vscode/mcp.json`): root key `servers`, not `mcpServers`. A block under the wrong key is
  silently ignored.
  `{"servers": {"xerj": {"type": "stdio", "command": "/abs/path/to/xerj", "args": ["mcp"]}}}`
"##;

/// What `xerj init` prints for the clients `.mcp.json` does not cover. The same
/// three registrations the hub skill carries; a unit test pins the two together
/// so they cannot drift apart.
const MCP_REGISTRATION: &str = r##"MCP registration for other clients (restart the session after; success is `xerj_hub_ask` in the tool list):
  Claude Code, every project:  claude mcp add --scope user xerj -- "$(command -v xerj)" mcp
  Cursor, Windsurf, Claude Desktop, Cline:  copy the "xerj" entry from .mcp.json; the root key is "mcpServers"
  VS Code (.vscode/mcp.json):  root key "servers", not "mcpServers"; the entry needs "type": "stdio""##;

/// What init says last: where to go next, the hub's one line, and the
/// per-client MCP registration.
fn closing_message() -> String {
    format!(
        "\ndone. Restart the agent session to pick up the tools, then\n\
         `xerj autoindex <folder>` anything you want searchable.\n\n\
         Public libraries, standards and advisories need no indexing: ask the hub with\n\
         `xerj \"<question>\"` (the xerj-hub skill says when).\n\n\
         {MCP_REGISTRATION}"
    )
}

/// Write `content` to `path` unless a file is already there, so a skill the
/// user has edited is never overwritten. Returns the report line for it.
fn write_if_absent(path: &Path, content: &str, dry: bool) -> Result<String, String> {
    if path.exists() {
        return Ok(report("kept", path));
    }
    write_file(path, content, dry).map_err(|e| e.to_string())?;
    Ok(report("wrote", path))
}

/// Back up `path` to `path.bak` before the first edit. Returns true if a
/// backup was made (false when the file did not exist).
fn backup(path: &Path, dry: bool) -> bool {
    if !path.exists() {
        return false;
    }
    if !dry {
        let bak = path.with_extension(match path.extension() {
            Some(e) => format!("{}.bak", e.to_string_lossy()),
            None => "bak".to_string(),
        });
        let _ = fs::copy(path, bak);
    }
    true
}

fn write_file(path: &Path, content: &str, dry: bool) -> std::io::Result<()> {
    if dry {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)
}

/// Merge the xerj server into an `.mcp.json`, preserving everything else.
/// Absolute binary path — an MCP host launched from a desktop icon does not
/// inherit the shell PATH (same rule the `xerj mcp` help documents).
fn merged_mcp_json(existing: Option<&str>, url: &str) -> Result<String, String> {
    let bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "xerj".to_string());
    let entry = json!({
        "command": bin,
        "args": ["mcp"],
        "env": { "XERJ_URL": url }
    });
    let mut root: Value = match existing {
        Some(text) => serde_json::from_str(text)
            .map_err(|e| format!("existing .mcp.json is not valid JSON ({e}); not touching it"))?,
        None => json!({}),
    };
    if !root.is_object() {
        return Err("existing .mcp.json is not a JSON object; not touching it".into());
    }
    let servers = root
        .as_object_mut()
        .expect("checked object above")
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    if !servers.is_object() {
        return Err("existing .mcp.json mcpServers is not an object; not touching it".into());
    }
    servers
        .as_object_mut()
        .expect("checked object above")
        .insert("xerj".into(), entry);
    serde_json::to_string_pretty(&root).map_err(|e| e.to_string())
}

/// One detected surface, written. Returns the human line to print.
fn report(action: &str, path: &Path) -> String {
    format!("  {action:<8} {}", path.display())
}

/// Entry point for the `init` subcommand. Returns a process exit code.
pub fn run_init_cli() -> i32 {
    let mut url = std::env::var("XERJ_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://localhost:9200".to_string());
    let mut dry = false;
    let mut it = std::env::args().skip(2);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return 0;
            }
            "--url" => url = it.next().unwrap_or(url),
            "--dry-run" => dry = true,
            other => {
                eprintln!("xerj init: unexpected argument '{other}'\n\n{USAGE}");
                return 2;
            }
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match run_init_in(&cwd, &url, dry) {
        Ok(lines) => {
            println!(
                "xerj init{} — wiring XERJ into this project:\n",
                if dry { " (dry run)" } else { "" }
            );
            for l in &lines {
                println!("{l}");
            }
            println!("{}", closing_message());
            0
        }
        Err(e) => {
            eprintln!("xerj init: {e}");
            1
        }
    }
}

/// The writable core, separated from argv/cwd so tests drive it directly.
pub(crate) fn run_init_in(cwd: &Path, url: &str, dry: bool) -> Result<Vec<String>, String> {
    let mut out = Vec::new();

    // 1) MCP registration (.mcp.json, Claude Code project scope; other hosts
    //    read the same shape). Merge, never clobber.
    let mcp_path = cwd.join(".mcp.json");
    let existing = fs::read_to_string(&mcp_path).ok();
    let already = existing
        .as_deref()
        .map(|t| t.contains("\"xerj\""))
        .unwrap_or(false);
    if already {
        out.push(report("kept", &mcp_path));
    } else {
        let merged = merged_mcp_json(existing.as_deref(), url)?;
        let had = backup(&mcp_path, dry);
        write_file(&mcp_path, &merged, dry).map_err(|e| e.to_string())?;
        out.push(report(if had { "merged" } else { "wrote" }, &mcp_path));
    }

    // 2) Claude Code skills — written whenever the project (or user) uses
    //    Claude Code; the directory existing is the signal. Two files: the
    //    local-code skill, and the hub skill (when to ask the hub instead).
    let claude_dir_present = cwd.join(".claude").is_dir()
        || dirs_home()
            .map(|h| h.join(".claude").is_dir())
            .unwrap_or(false);
    if claude_dir_present {
        out.push(write_if_absent(
            &cwd.join(".claude/skills/xerj/SKILL.md"),
            &skill_md(),
            dry,
        )?);
        out.push(write_if_absent(
            &cwd.join(".claude/skills/xerj-hub/SKILL.md"),
            HUB_SKILL_MD,
            dry,
        )?);
    }

    // 3) Cursor rules — only when the project already has a .cursor directory.
    //    The same two teaching files, one rule each.
    if cwd.join(".cursor").is_dir() {
        out.push(write_if_absent(
            &cwd.join(".cursor/rules/xerj.mdc"),
            &skill_md(),
            dry,
        )?);
        out.push(write_if_absent(
            &cwd.join(".cursor/rules/xerj-hub.mdc"),
            HUB_SKILL_MD,
            dry,
        )?);
    }

    // 4) AGENTS.md — the cross-agent convention. Append a short section if the
    //    file exists without one; create the file if absent.
    let agents = cwd.join("AGENTS.md");
    match fs::read_to_string(&agents) {
        Ok(text) if text.contains("xerj search") || text.contains("xerj def") => {
            out.push(report("kept", &agents));
        }
        Ok(text) => {
            backup(&agents, dry);
            write_file(&agents, &format!("{text}{}", agents_md_section()), dry)
                .map_err(|e| e.to_string())?;
            out.push(report("appended", &agents));
        }
        Err(_) => {
            write_file(
                &agents,
                &format!("# Agent notes\n{}", agents_md_section()),
                dry,
            )
            .map_err(|e| e.to_string())?;
            out.push(report("wrote", &agents));
        }
    }

    // 5) Auth note — say plainly what will happen at tool-call time (#961).
    //
    // `.mcp.json` is Claude Code *project* scope, i.e. a file meant to be
    // committed, so init deliberately writes no credential into it; auth is
    // ON by default, and the gap between "ran xerj init" and "tools work"
    // used to be a silent 401 on the first tool call. The MCP server now
    // falls back to the local admin key for loopback nodes by itself; here
    // we only tell the user which of the two worlds they are in — the key
    // itself is never printed or written.
    if xerj_common::localauth::url_is_loopback(url) {
        match xerj_common::localauth::discover_local_admin_key() {
            Some((_, path)) => out.push(format!(
                "  auth     MCP authenticates with the admin key at {} (local node)",
                path.display()
            )),
            None => out.push(
                "  auth     no --auth given and no local admin key found; if the node at \
                 {url} runs with auth on, add \"XERJ_AUTH\": \"ApiKey <key>\" to the \
                 .mcp.json env — the key is the node's <data-dir>/admin.key"
                    .to_string(),
            ),
        }
    } else {
        out.push(format!(
            "  auth     {url} is not local, so no admin key is auto-discovered for it; \
                 if the node runs with auth on, add \"XERJ_AUTH\": \"ApiKey <key>\" to \
                 the .mcp.json env — the key is the node's <data-dir>/admin.key"
        ));
    }

    Ok(out)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn fresh_project_gets_mcp_and_agents_md() {
        let d = tmp();
        let lines = run_init_in(d.path(), "http://localhost:9200", false).unwrap();
        assert!(lines.iter().any(|l| l.contains(".mcp.json")));
        let mcp: Value =
            serde_json::from_str(&fs::read_to_string(d.path().join(".mcp.json")).unwrap()).unwrap();
        assert!(mcp["mcpServers"]["xerj"]["args"][0] == "mcp");
        assert_eq!(
            mcp["mcpServers"]["xerj"]["env"]["XERJ_URL"],
            "http://localhost:9200"
        );
        let agents = fs::read_to_string(d.path().join("AGENTS.md")).unwrap();
        assert!(agents.contains("xerj def"));
    }

    #[test]
    fn existing_mcp_json_is_merged_not_clobbered_and_backed_up() {
        let d = tmp();
        fs::write(
            d.path().join(".mcp.json"),
            r#"{"mcpServers":{"other":{"command":"other-bin"}}}"#,
        )
        .unwrap();
        run_init_in(d.path(), "http://x:1", false).unwrap();
        let mcp: Value =
            serde_json::from_str(&fs::read_to_string(d.path().join(".mcp.json")).unwrap()).unwrap();
        assert_eq!(mcp["mcpServers"]["other"]["command"], "other-bin");
        assert!(mcp["mcpServers"]["xerj"].is_object());
        assert!(d.path().join(".mcp.json.bak").exists());
    }

    #[test]
    fn second_run_is_idempotent() {
        // The pair must see ONE `HOME`: `run_init_in` reads it through
        // `dirs_home()` to decide whether the Claude skill is in scope, and
        // the auth-note `Sandbox` tests below swap `HOME` under the same
        // `ENV_LOCK`. Without the lock, a sandboxed test landing between the
        // two runs makes the first see no `~/.claude` (skill out of scope)
        // and the second see one (skill "wrote") — a scheduling-dependent
        // failure, not an idempotency failure.
        let _env = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let d = tmp();
        run_init_in(d.path(), "http://x:1", false).unwrap();
        let first = fs::read_to_string(d.path().join("AGENTS.md")).unwrap();
        let lines = run_init_in(d.path(), "http://x:1", false).unwrap();
        let second = fs::read_to_string(d.path().join("AGENTS.md")).unwrap();
        assert_eq!(first, second, "AGENTS.md must not grow on re-run");
        // "kept" for every surface; the only other line init emits is the
        // auth note (section 5), which rewrites nothing.
        assert!(
            lines
                .iter()
                .all(|l| l.contains("kept") || l.contains("auth")),
            "{lines:?}"
        );
    }

    #[test]
    fn invalid_mcp_json_is_left_alone() {
        let d = tmp();
        fs::write(d.path().join(".mcp.json"), "{not json").unwrap();
        let err = run_init_in(d.path(), "http://x:1", false).unwrap_err();
        assert!(err.contains("not valid JSON"));
        assert_eq!(
            fs::read_to_string(d.path().join(".mcp.json")).unwrap(),
            "{not json"
        );
    }

    #[test]
    fn dry_run_writes_nothing() {
        let d = tmp();
        run_init_in(d.path(), "http://x:1", true).unwrap();
        assert!(!d.path().join(".mcp.json").exists());
        assert!(!d.path().join("AGENTS.md").exists());
    }

    #[test]
    fn cursor_rule_written_only_when_cursor_dir_exists() {
        let d = tmp();
        run_init_in(d.path(), "http://x:1", false).unwrap();
        assert!(!d.path().join(".cursor/rules/xerj.mdc").exists());
        fs::create_dir_all(d.path().join(".cursor")).unwrap();
        run_init_in(d.path(), "http://x:1", false).unwrap();
        assert!(d.path().join(".cursor/rules/xerj.mdc").exists());
    }

    // ── the hub skill (Corpus Hub v2, plan section 9) ───────────────────

    /// True when `lines` holds a report line for `action` whose path ends in
    /// `tail`. Paths are compared with `/` so the test reads the same on
    /// Windows, where `Path::display` prints backslashes.
    fn has_report(lines: &[String], action: &str, tail: &str) -> bool {
        lines.iter().any(|l| {
            let l = l.replace('\\', "/");
            l.contains(action) && l.ends_with(tail)
        })
    }

    /// `.claude/` is the signal for both skills: the hub skill lands beside the
    /// local-code skill and is a different file with different content.
    #[test]
    fn claude_projects_get_the_hub_skill_beside_the_xerj_skill() {
        let d = tmp();
        fs::create_dir_all(d.path().join(".claude")).unwrap();
        let lines = run_init_in(d.path(), "http://x:1", false).unwrap();
        let local = fs::read_to_string(d.path().join(".claude/skills/xerj/SKILL.md")).unwrap();
        let hub = fs::read_to_string(d.path().join(".claude/skills/xerj-hub/SKILL.md")).unwrap();
        assert_eq!(local, skill_md());
        assert_eq!(hub, HUB_SKILL_MD);
        assert_ne!(local, hub);
        assert!(
            has_report(&lines, "wrote", "skills/xerj-hub/SKILL.md"),
            "{lines:?}"
        );
    }

    /// Cursor gets the same hub text as a rule, and only when `.cursor/` exists.
    #[test]
    fn cursor_projects_get_the_hub_rule_with_the_same_text() {
        let d = tmp();
        run_init_in(d.path(), "http://x:1", false).unwrap();
        assert!(!d.path().join(".cursor/rules/xerj-hub.mdc").exists());
        fs::create_dir_all(d.path().join(".cursor")).unwrap();
        let lines = run_init_in(d.path(), "http://x:1", false).unwrap();
        assert_eq!(
            fs::read_to_string(d.path().join(".cursor/rules/xerj-hub.mdc")).unwrap(),
            HUB_SKILL_MD
        );
        assert!(
            has_report(&lines, "wrote", "rules/xerj-hub.mdc"),
            "{lines:?}"
        );
    }

    /// A hub skill the user edited is never overwritten.
    #[test]
    fn an_edited_hub_skill_is_kept_not_overwritten() {
        let d = tmp();
        let hub = d.path().join(".claude/skills/xerj-hub/SKILL.md");
        fs::create_dir_all(hub.parent().unwrap()).unwrap();
        fs::write(&hub, "my own rules\n").unwrap();
        let lines = run_init_in(d.path(), "http://x:1", false).unwrap();
        assert_eq!(fs::read_to_string(&hub).unwrap(), "my own rules\n");
        assert!(
            has_report(&lines, "kept", "skills/xerj-hub/SKILL.md"),
            "{lines:?}"
        );
    }

    /// `--dry-run` reports both hub files and writes neither.
    #[test]
    fn dry_run_reports_the_hub_files_and_writes_none() {
        let d = tmp();
        fs::create_dir_all(d.path().join(".claude")).unwrap();
        fs::create_dir_all(d.path().join(".cursor")).unwrap();
        let lines = run_init_in(d.path(), "http://x:1", true).unwrap();
        assert!(!d.path().join(".claude/skills").exists());
        assert!(!d.path().join(".cursor/rules").exists());
        assert!(has_report(&lines, "wrote", "skills/xerj-hub/SKILL.md"));
        assert!(has_report(&lines, "wrote", "rules/xerj-hub.mdc"));
    }

    /// The second run rewrites nothing: all four teaching files are `kept`.
    #[test]
    fn second_run_keeps_all_four_teaching_files() {
        // `ENV_LOCK`: see `second_run_is_idempotent` — the auth note reads the
        // process cwd and HOME, which the sandboxed tests swap.
        let _env = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let d = tmp();
        fs::create_dir_all(d.path().join(".claude")).unwrap();
        fs::create_dir_all(d.path().join(".cursor")).unwrap();
        run_init_in(d.path(), "http://x:1", false).unwrap();
        let lines = run_init_in(d.path(), "http://x:1", false).unwrap();
        for tail in [
            "skills/xerj/SKILL.md",
            "skills/xerj-hub/SKILL.md",
            "rules/xerj.mdc",
            "rules/xerj-hub.mdc",
        ] {
            assert!(has_report(&lines, "kept", tail), "{tail}: {lines:?}");
        }
    }

    /// The reason the file exists: the decision rule, the one line, `--def`,
    /// the safe-stop rule, and the registration per client. Deleting any of
    /// them has to fail here.
    #[test]
    fn hub_skill_carries_the_rule_the_line_and_the_registration() {
        for needed in [
            "**HUB**",
            "**LOCAL**",
            "**NEITHER**",
            r#"xerj "<question>""#,
            "xerj ask --def <SYMBOL>",
            "Stop only the node you started",
            "Never `pkill xerj` or `killall xerj`",
            "xerj hub pull",
            "xerj_hub_ask",
            "claude mcp add",
            "root key `mcpServers`",
            "root key `servers`, not `mcpServers`",
        ] {
            assert!(
                HUB_SKILL_MD.contains(needed),
                "the hub skill lost {needed:?}"
            );
        }
    }

    /// The frontmatter a skill loader reads: a `name` that matches the
    /// directory, and a `description` that is a valid plain YAML scalar with no
    /// angle brackets (the Agent Skills spec forbids XML-like tags there).
    #[test]
    fn hub_skill_frontmatter_is_loadable() {
        let rest = HUB_SKILL_MD
            .strip_prefix("---\n")
            .expect("the skill opens with frontmatter");
        let (front, _body) = rest.split_once("\n---\n").expect("the frontmatter closes");
        fn field<'a>(front: &'a str, key: &str) -> &'a str {
            front
                .lines()
                .find_map(|l| l.strip_prefix(key))
                .unwrap_or_else(|| panic!("frontmatter has no {key}"))
        }
        assert_eq!(field(front, "name: "), "xerj-hub");
        let description = field(front, "description: ");
        assert!(!description.is_empty() && description.len() <= 1024);
        assert!(
            !description.contains('<') && !description.contains('>'),
            "no angle brackets in the description: {description}"
        );
        assert!(
            !description.contains(": ") && !description.contains(" #"),
            "a plain YAML scalar may not contain `: ` or ` #`: {description}"
        );
    }

    /// llms.txt study rule 7 (`docs/research/llms-txt-2026-09/README.md`): no
    /// line over about 300 characters in text an agent reads.
    #[test]
    fn hub_texts_have_no_line_over_300_characters() {
        for (name, text) in [
            ("hub skill", HUB_SKILL_MD),
            ("mcp registration", MCP_REGISTRATION),
        ] {
            for (i, line) in text.lines().enumerate() {
                let n = line.chars().count();
                assert!(n <= 300, "{name} line {} is {n} characters", i + 1);
            }
        }
    }

    /// What init prints and what the skill teaches are one registration, not
    /// two that drift: the same Claude Code command and the same root keys.
    #[test]
    fn printed_registration_and_hub_skill_agree() {
        let claude = r#"claude mcp add --scope user xerj -- "$(command -v xerj)" mcp"#;
        assert!(HUB_SKILL_MD.contains(claude));
        assert!(MCP_REGISTRATION.contains(claude));
        for key in ["mcpServers", "servers", "stdio"] {
            assert!(HUB_SKILL_MD.contains(key), "skill: {key}");
            assert!(MCP_REGISTRATION.contains(key), "printed: {key}");
        }
        let closing = closing_message();
        assert!(closing.contains(MCP_REGISTRATION));
        assert!(closing.contains(r#"xerj "<question>""#));
        assert!(closing.contains("xerj autoindex <folder>"));
    }

    /// `--help` lists every file init can write, the hub pair included.
    #[test]
    fn help_lists_every_file_init_writes() {
        for file in [
            ".mcp.json",
            ".claude/skills/xerj/SKILL.md",
            ".claude/skills/xerj-hub/SKILL.md",
            ".cursor/rules/xerj.mdc",
            ".cursor/rules/xerj-hub.mdc",
            "AGENTS.md",
        ] {
            assert!(USAGE.contains(file), "--help does not list {file}");
        }
    }

    // ── auth note (#961) ────────────────────────────────────────────────

    /// The auth note reads the working directory and `HOME` (that is what
    /// admin-key discovery does), so its tests sandbox both — serialised,
    /// because they are per-process state.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct Sandbox {
        _lock: std::sync::MutexGuard<'static, ()>,
        dir: tempfile::TempDir,
        previous_dir: PathBuf,
        previous_home: Option<std::ffi::OsString>,
    }

    impl Sandbox {
        fn new() -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
            let dir = tempfile::tempdir().unwrap();
            let previous_dir = std::env::current_dir().unwrap();
            let previous_home = std::env::var_os("HOME");
            std::env::set_current_dir(dir.path()).unwrap();
            std::env::set_var("HOME", dir.path().join("home"));
            Self {
                _lock: lock,
                dir,
                previous_dir,
                previous_home,
            }
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.dir.path().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.previous_dir).unwrap();
            match &self.previous_home {
                Some(home) => std::env::set_var("HOME", home),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    /// The happy path of #961: a local node's key exists, init says the MCP
    /// server will use it — and the secret still goes nowhere near the
    /// committable `.mcp.json`.
    #[test]
    fn auth_note_names_the_discovered_key_and_writes_no_secret() {
        let sandbox = Sandbox::new();
        sandbox.write("data/admin.key", "local-admin-key\n");
        let lines = run_init_in(sandbox.dir.path(), "http://localhost:9200", false).unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l.contains("admin key at") && l.ends_with("data/admin.key (local node)")),
            "the note must name the file the MCP server will use: {lines:?}"
        );
        let mcp = fs::read_to_string(sandbox.dir.path().join(".mcp.json")).unwrap();
        assert!(
            !mcp.contains("local-admin-key") && !mcp.contains("XERJ_AUTH"),
            "a credential must never be written into a committable file: {mcp}"
        );
    }

    /// No key on disk: the note must say the manual step plainly instead of
    /// letting the first tool call be the user's first 401.
    #[test]
    fn auth_note_gives_the_manual_step_when_no_key_exists() {
        let sandbox = Sandbox::new();
        let lines = run_init_in(sandbox.dir.path(), "http://localhost:9200", false).unwrap();
        let note = lines
            .iter()
            .find(|l| l.contains("auth"))
            .expect("the auth note is always present");
        assert!(
            note.contains("XERJ_AUTH") && note.contains("admin.key"),
            "the fallback note must name the env var and the key file: {note}"
        );
    }

    /// A non-loopback URL never triggers discovery, even with a local key
    /// sitting right there — reading it would only encourage sending it
    /// off-box.
    #[test]
    fn remote_url_gets_no_discovery_even_with_a_local_key_present() {
        let sandbox = Sandbox::new();
        sandbox.write("data/admin.key", "local-admin-key\n");
        let lines =
            run_init_in(sandbox.dir.path(), "http://search.example.com:9200", false).unwrap();
        let note = lines
            .iter()
            .find(|l| l.contains("auth"))
            .expect("the auth note is always present");
        assert!(
            note.contains("XERJ_AUTH") && !note.contains("admin key at"),
            "a remote node gets the manual step, never a local key: {note}"
        );
    }
}
