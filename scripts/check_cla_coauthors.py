#!/usr/bin/env python3
"""CLA check for Co-authored-by trailers (issue #269).

cla-bot resolves signatures by matching commit *authors* against
.contributors. When a maintainer re-lands a fork PR — squashing the
contributor's commits onto an xerj-org branch and crediting them with a
Co-authored-by trailer — every commit author is xerj-org, so the gate goes
green even though the person whose work it is has never signed (live
instance: PR #248, the re-land of buger's #166). This check closes that
hole: every Co-authored-by trailer on a PR must resolve to a login listed
in .contributors.

An email resolves to a login by, in order:

 1. GitHub noreply address — ``[id+]login@users.noreply.github.com``.
 2. This repo's own commit history — ``GET /repos/{repo}/commits?author=<email>``
    returns the login GitHub itself attributes that email to. The CLA
    signature flow ("open a PR adding your username to .contributors from
    your own account") lands a commit authored with the signer's email, so
    signing makes the signer resolvable as a side effect.

Anything unresolvable FAILS the gate, deliberately: an identity the gate
cannot attribute is exactly what it exists to flag.

Field-report carve-out (issue: the agent field-report loop was dead)
-------------------------------------------------------------------

A pull request whose ENTIRE diff is agent field reports — new markdown files
under ``user-feedback/16-agent-field-reports/`` and nothing else — is exempt
from this trailer gate. That folder is documentation of the *experience* of
using XERJ (see its README and https://xerj.org/llms.txt); requiring a CLA to
add one file there is exactly the friction that kept the folder empty. The
exemption is deliberately narrow: it needs every changed path to be a
field-report ``*.md``, so bundling any code, CI, or other-docs change with a
field report re-arms the full gate — a mixed PR must never slip a code change in
under a field report. ``README.md`` in that folder is NOT a field report and
does not qualify. See :func:`is_field_report_only`.

Usage (CI):
  check_cla_coauthors.py --pr <number> [--changed-files "<paths>"]
    Runs the trailer gate. If --changed-files is a field-report-only set, the
    gate is skipped (exempt) and this exits 0 without a network lookup.
   check_cla_coauthors.py --is-field-report-only --changed-files "<paths>"
     Prints nothing; exits 0 iff the paths are a field-report-only set, else 1.
     Used by the separate CI job that provides `verification/cla-signed` for
     strictly field-report-only pull requests.
   check_cla_coauthors.py --is-self-cla-signature --pr <number> --changed-files "<paths>"
     Prints nothing; exits 0 iff the PR only modifies `.contributors` to add the
     PR author's own GitHub login (preserving all existing entries, with all
     commits authored by the PR author and no unsigned Co-authored-by trailers).
     Used by the separate CI job so `.contributors` signature PRs do not deadlock
     on cla-bot reading `.contributors` from `main`.
   Needs `gh` and GH_TOKEN for --pr; repo comes from GITHUB_REPOSITORY or --repo.

Tests: scripts/test_cla_coauthors.py
"""

import argparse
import base64
import json
import os
import pathlib
import re
import subprocess
import sys
import urllib.parse

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent

# git trailer keys are case-insensitive; GitHub emits "Co-authored-by".
_COAUTHOR_RE = re.compile(
    r"^\s*co-authored-by:\s*(?P<name>[^<]*?)\s*<(?P<email>[^>]+)>\s*$",
    re.IGNORECASE | re.MULTILINE,
)

_NOREPLY_RE = re.compile(
    r"^(?:\d+\+)?(?P<login>[^@+]+)@users\.noreply\.github\.com$", re.IGNORECASE
)


def parse_coauthors(message):
    """All Co-authored-by trailers in a commit message as (name, email)."""
    return [
        (m.group("name"), m.group("email").strip())
        for m in _COAUTHOR_RE.finditer(message)
    ]


def noreply_login(email):
    """Login embedded in a GitHub noreply address, or None."""
    m = _NOREPLY_RE.match(email.strip())
    return m.group("login") if m else None


# The one directory agent field reports live in. Kept in lock-step with
# xerj-autoindex's `feedback::FIELD_REPORT_DIR`; the `xerj feedback` command
# writes reports here and the carve-out below recognises them here.
FIELD_REPORT_DIR = "user-feedback/16-agent-field-reports/"


def is_field_report_only(paths):
    """True iff `paths` is a non-empty set of ONLY field-report markdown files.

    A field report is a ``*.md`` file directly inside FIELD_REPORT_DIR — not a
    nested subdirectory, and not the folder's own ``README.md`` (which is the
    template, not a report). An empty diff is NOT field-report-only: "nothing
    changed" must never read as "exempt". Any single non-qualifying path makes
    the whole set non-exempt, so a code change bundled with a field report gets
    the full gate.
    """
    paths = [p for p in paths if p.strip()]
    if not paths:
        return False
    for p in paths:
        p = p.strip()
        if not p.startswith(FIELD_REPORT_DIR):
            return False
        rest = p[len(FIELD_REPORT_DIR):]
        if "/" in rest:  # a nested subdirectory, not a direct field report
            return False
        if not rest.endswith(".md"):
            return False
        if rest == "README.md":  # the folder template is not a field report
            return False
    return True


def is_self_cla_signature(
    paths,
    base_contributors,
    head_contributors,
    pr_author,
    commits,
    resolve=lambda _email: None,
):
    """True iff the PR exclusively adds `pr_author` to `.contributors` and every
    commit is authored by `pr_author` with no unsigned Co-authored-by trailers.

    cla-bot always reads `https://raw.githubusercontent.com/<repo>/main/.contributors`,
    so a PR whose sole purpose is signing the CLA (adding the PR author's own
    GitHub login to `.contributors`) fails cla-bot's check until it is merged.
    This predicate lets the trusted `pull_request_target` workflow satisfy
    `verification/cla-signed` for strictly self-signing `.contributors` PRs
    while rejecting any PR that:
      - touches any file other than `.contributors`,
      - removes or modifies any existing `.contributors` entry (including
        `dependabot[bot]`),
      - adds any login other than `pr_author`,
      - contains any commit not authored by `pr_author`, or
      - carries an unresolvable or unsigned `Co-authored-by` trailer.
    """
    clean_paths = [p.strip() for p in paths if p.strip()]
    if clean_paths != [".contributors"]:
        return False
    if not isinstance(pr_author, str) or not pr_author.strip():
        return False
    author_lower = pr_author.strip().lower()
    if not isinstance(base_contributors, list) or not isinstance(head_contributors, list):
        return False
    if not all(isinstance(c, str) and c.strip() for c in base_contributors):
        return False
    if not all(isinstance(c, str) and c.strip() for c in head_contributors):
        return False
    if "dependabot[bot]" not in head_contributors:
        return False
    base_counts = {}
    for c in base_contributors:
        base_counts[c] = base_counts.get(c, 0) + 1
    head_counts = {}
    for c in head_contributors:
        head_counts[c] = head_counts.get(c, 0) + 1
    for c, count in base_counts.items():
        if head_counts.get(c, 0) < count:
            return False
    if len(head_contributors) > len(base_contributors) + 1:
        return False
    for c, count in head_counts.items():
        added = count - base_counts.get(c, 0)
        if added > 0 and c.strip().lower() != author_lower:
            return False
    if not any(c.strip().lower() == author_lower for c in head_contributors):
        return False
    if not commits:
        return False
    for commit in commits:
        commit_author = commit.get("author_login")
        if not isinstance(commit_author, str) or commit_author.strip().lower() != author_lower:
            return False
    if check_commits(commits, head_contributors, resolve):
        return False
    return True


def check_commits(commits, contributors, resolve):
    """Return a list of problem strings; empty means the gate passes.

    commits: [{"sha": ..., "message": ...}]
    contributors: logins from .contributors
    resolve: email -> login-or-None (network lookup; injectable for tests)
    """
    signed = {c.lower() for c in contributors}
    resolved = {}  # email -> login or None, so each email is looked up once
    problems = []
    for commit in commits:
        sha = commit["sha"][:8]
        for name, email in parse_coauthors(commit["message"]):
            if email not in resolved:
                resolved[email] = noreply_login(email) or resolve(email)
            login = resolved[email]
            if login is None:
                problems.append(
                    f"{sha}: cannot attribute Co-authored-by '{name} <{email}>' "
                    "to any GitHub login (not a noreply address, and no commit "
                    "in this repo is authored with it). Ask them to sign per "
                    "CLA.md — the signature PR itself makes their email "
                    "resolvable — or use their @users.noreply.github.com "
                    "address in the trailer."
                )
            elif login.lower() not in signed:
                problems.append(
                    f"{sha}: Co-authored-by '{name} <{email}>' is GitHub user "
                    f"'{login}', who has not signed the CLA (not in "
                    ".contributors). See CLA.md for the one-time signature "
                    "flow."
                )
    return problems


def _gh_api(path):
    out = subprocess.run(
        ["gh", "api", "--paginate", path],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    # --paginate concatenates JSON arrays; parse them all.
    items = []
    dec = json.JSONDecoder()
    i = 0
    while i < len(out):
        chunk, end = dec.raw_decode(out, i)
        items.extend(chunk if isinstance(chunk, list) else [chunk])
        i = end
        while i < len(out) and out[i].isspace():
            i += 1
    return items


def make_repo_resolver(repo):
    """email -> login via the repo's own commit history."""

    def resolve(email):
        q = urllib.parse.quote(email)
        commits = _gh_api(f"repos/{repo}/commits?author={q}&per_page=1")
        for c in commits:
            author = c.get("author") or {}
            if author.get("login"):
                return author["login"]
        return None

    return resolve


def _split_paths(raw):
    """Changed-file paths from a --changed-files value (whitespace/newline sep)."""
    return [] if raw is None else raw.split()


def _fetch_head_contributors(repo, head_sha):
    """Fetch and parse `.contributors` at `head_sha` as pure JSON data via the
    GitHub contents API (never checking out or executing the PR head)."""
    try:
        items = _gh_api(
            f"repos/{repo}/contents/.contributors?ref={urllib.parse.quote(head_sha)}"
        )
        if not items or not isinstance(items[0], dict):
            return None
        raw = base64.b64decode(items[0]["content"]).decode("utf-8")
        return json.loads(raw)
    except ( subprocess.CalledProcessError, KeyError, ValueError, TypeError):
        return None


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--pr", type=int, help="pull request number")
    ap.add_argument(
        "--repo",
        default=os.environ.get("GITHUB_REPOSITORY", "xerj-org/xerj"),
        help="owner/name (default: $GITHUB_REPOSITORY or xerj-org/xerj)",
    )
    ap.add_argument(
        "--changed-files",
        help="whitespace/newline-separated changed paths in the PR. When this is "
        "a field-report-only set, the trailer gate is skipped (exempt).",
    )
    ap.add_argument(
        "--is-field-report-only",
        action="store_true",
        help="predicate mode: exit 0 iff --changed-files is a field-report-only "
        "set, else 1. Prints nothing; used by the exemption CI job.",
    )
    ap.add_argument(
        "--is-self-cla-signature",
        action="store_true",
        help="predicate mode: exit 0 iff the PR exclusively adds the PR author's "
        "own GitHub login to .contributors, else 1. Used by the exemption CI job.",
    )
    args = ap.parse_args()

    paths = _split_paths(args.changed_files)

    # Predicate mode: no PR, no network — just answer the path question.
    if args.is_field_report_only:
        return 0 if is_field_report_only(paths) else 1

    if args.is_self_cla_signature:
        if args.pr is None:
            ap.error("--pr is required with --is-self-cla-signature")
        if [p.strip() for p in paths if p.strip()] != [".contributors"]:
            return 1
        base_contributors = json.loads((REPO_ROOT / ".contributors").read_text())
        pr_info = _gh_api(f"repos/{args.repo}/pulls/{args.pr}")[0]
        pr_author = (pr_info.get("user") or {}).get("login") or ""
        head_sha = (pr_info.get("head") or {}).get("sha") or ""
        head_contributors = _fetch_head_contributors(args.repo, head_sha)
        if head_contributors is None:
            return 1
        raw = _gh_api(f"repos/{args.repo}/pulls/{args.pr}/commits?per_page=100")
        commits = [
            {
                "sha": c["sha"],
                "message": c["commit"]["message"],
                "author_login": (c.get("author") or {}).get("login"),
            }
            for c in raw
        ]
        ok = is_self_cla_signature(
            paths,
            base_contributors,
            head_contributors,
            pr_author,
            commits,
            resolve=make_repo_resolver(args.repo),
        )
        return 0 if ok else 1

    if args.pr is None:
        ap.error("--pr is required unless --is-field-report-only is given")

    # Carve-out: a field-report-only PR skips the trailer gate entirely. The
    # predicate is a guard here, so check_commits() stays untouched and every
    # mixed or non-field-report PR runs the full check below.
    if args.changed_files is not None and is_field_report_only(paths):
        print(
            f"CLA co-author check EXEMPT for PR #{args.pr}: the diff is "
            f"{len(paths)} field-report file(s) under {FIELD_REPORT_DIR} and "
            "nothing else."
        )
        return 0

    contributors = json.loads((REPO_ROOT / ".contributors").read_text())
    raw = _gh_api(f"repos/{args.repo}/pulls/{args.pr}/commits?per_page=100")
    commits = [{"sha": c["sha"], "message": c["commit"]["message"]} for c in raw]

    problems = check_commits(commits, contributors, make_repo_resolver(args.repo))
    if problems:
        print(f"CLA co-author check FAILED for PR #{args.pr}:", file=sys.stderr)
        for p in problems:
            print(f"  - {p}", file=sys.stderr)
        return 1
    trailers = sum(len(parse_coauthors(c["message"])) for c in commits)
    print(
        f"CLA co-author check passed for PR #{args.pr}: "
        f"{len(commits)} commit(s), {trailers} Co-authored-by trailer(s), "
        "all attributable to signed contributors."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
