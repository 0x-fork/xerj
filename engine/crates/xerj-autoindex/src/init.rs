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
    .cursor/rules/xerj.mdc       same teaching file for Cursor (if .cursor/)
    AGENTS.md                    a short XERJ section (appended if the file
                                 exists, created otherwise)

OPTIONS:
    --url <U>       node endpoint baked into the MCP config
                    (default $XERJ_URL or http://localhost:9200)
    --dry-run       print what would be written, write nothing
    -h, --help      this help

After init: restart the agent session so it picks up the new tools, then
`xerj autoindex <folder>` anything you want searchable.

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

/// Back up `path` to `path.bak` before the first edit. Returns true if a
/// backup was made (false when the file did not exist). A backup that cannot
/// be written is an error, so the caller never edits a file it could not save
/// a copy of first.
fn backup(path: &Path, dry: bool) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    if !dry {
        let bak = path.with_extension(match path.extension() {
            Some(e) => format!("{}.bak", e.to_string_lossy()),
            None => "bak".to_string(),
        });
        fs::copy(path, &bak).map_err(|e| {
            format!(
                "could not back up {} to {} ({e}); not touching it",
                path.display(),
                bak.display()
            )
        })?;
    }
    Ok(true)
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
            println!(
                "\ndone. Restart the agent session to pick up the tools, then\n\
                 `xerj autoindex <folder>` anything you want searchable."
            );
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
        let had = backup(&mcp_path, dry)?;
        write_file(&mcp_path, &merged, dry).map_err(|e| e.to_string())?;
        out.push(report(if had { "merged" } else { "wrote" }, &mcp_path));
    }

    // 2) Claude Code skill — written whenever the project (or user) uses
    //    Claude Code; the directory existing is the signal.
    let claude_dir_present = cwd.join(".claude").is_dir()
        || dirs_home()
            .map(|h| h.join(".claude").is_dir())
            .unwrap_or(false);
    if claude_dir_present {
        let skill = cwd.join(".claude/skills/xerj/SKILL.md");
        if skill.exists() {
            out.push(report("kept", &skill));
        } else {
            write_file(&skill, &skill_md(), dry).map_err(|e| e.to_string())?;
            out.push(report("wrote", &skill));
        }
    }

    // 3) Cursor rule — only when the project already has a .cursor directory.
    if cwd.join(".cursor").is_dir() {
        let rule = cwd.join(".cursor/rules/xerj.mdc");
        if rule.exists() {
            out.push(report("kept", &rule));
        } else {
            write_file(&rule, &skill_md(), dry).map_err(|e| e.to_string())?;
            out.push(report("wrote", &rule));
        }
    }

    // 4) AGENTS.md — the cross-agent convention. Append a short section if the
    //    file exists without one; create the file if absent.
    let agents = cwd.join("AGENTS.md");
    match fs::read_to_string(&agents) {
        Ok(text) if text.contains("xerj search") || text.contains("xerj def") => {
            out.push(report("kept", &agents));
        }
        Ok(text) => {
            backup(&agents, dry)?;
            write_file(&agents, &format!("{text}{}", agents_md_section()), dry)
                .map_err(|e| e.to_string())?;
            out.push(report("appended", &agents));
        }
        // Unreadable but present (not UTF-8, no permission): never replace it.
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(format!(
                "could not read {} ({e}); not touching it",
                agents.display()
            ));
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

    /// When the `.bak` cannot be written, the original is not edited: the
    /// promise is "never touch a byte we did not write without a backup".
    #[test]
    fn failed_backup_leaves_mcp_json_untouched() {
        let d = tmp();
        let original = r#"{"mcpServers":{"other":{"command":"other-bin"}}}"#;
        fs::write(d.path().join(".mcp.json"), original).unwrap();
        // A directory where the backup file must go makes the copy fail.
        fs::create_dir(d.path().join(".mcp.json.bak")).unwrap();
        let err = run_init_in(d.path(), "http://x:1", false).unwrap_err();
        assert!(err.contains("could not back up"), "{err}");
        assert_eq!(
            fs::read_to_string(d.path().join(".mcp.json")).unwrap(),
            original
        );
    }

    /// An AGENTS.md that exists but is not valid UTF-8 is never replaced by a
    /// fresh file (it used to be: any read error took the "absent" branch).
    #[test]
    fn unreadable_agents_md_is_not_replaced() {
        let d = tmp();
        let bytes = b"# notes\n\xff\xfe not utf-8\n";
        fs::write(d.path().join("AGENTS.md"), bytes).unwrap();
        let err = run_init_in(d.path(), "http://x:1", false).unwrap_err();
        assert!(err.contains("AGENTS.md"), "{err}");
        assert_eq!(fs::read(d.path().join("AGENTS.md")).unwrap(), bytes);
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
