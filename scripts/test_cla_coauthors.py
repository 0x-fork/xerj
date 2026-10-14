#!/usr/bin/env python3
"""Regression tests for the Co-authored-by CLA check (issue #269).

Before this check existed, cla-bot matched only commit *authors* against
.contributors, so a maintainer re-land of a fork PR — commits authored
xerj-org, contributor credited via a Co-authored-by trailer — went green
without the contributor ever signing (live instance: PR #248 re-landing
buger's #166). These tests pin the trailer-aware behaviour.

Run: python3 scripts/test_cla_coauthors.py
"""

import pathlib
import re
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from check_cla_coauthors import (
    FIELD_REPORT_DIR,
    check_commits,
    is_field_report_only,
    is_self_cla_signature,
    noreply_login,
    parse_coauthors,
)

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent

# The shape of PR #248: maintainer-authored re-land, real attribution
# carried only by the trailer.
RELAND_COMMITS = [
    {
        "sha": "c337eb8f",
        "message": "perf(autoindex): reuse the phase-A PDF extraction\n\n"
        "Body text.\n\nCo-authored-by: Leonid Bugaev <leonsbox@gmail.com>",
    },
    {
        "sha": "81c64883",
        "message": "docs(changelog): record the change\n\n"
        "Co-authored-by: Leonid Bugaev <leonsbox@gmail.com>",
    },
]


def no_network(email):
    raise AssertionError(f"unexpected network resolution for {email!r}")


class ParseTests(unittest.TestCase):
    def test_extracts_name_and_email(self):
        got = parse_coauthors(RELAND_COMMITS[0]["message"])
        self.assertEqual(got, [("Leonid Bugaev", "leonsbox@gmail.com")])

    def test_trailer_key_is_case_insensitive(self):
        # git trailers are case-insensitive; GitHub emits "Co-authored-by".
        msg = "subject\n\nCO-AUTHORED-BY: A B <a@b.c>\nco-authored-by: C D <c@d.e>"
        self.assertEqual(
            parse_coauthors(msg), [("A B", "a@b.c"), ("C D", "c@d.e")]
        )

    def test_no_trailers(self):
        self.assertEqual(parse_coauthors("fix: something\n\nplain body"), [])


class NoreplyTests(unittest.TestCase):
    def test_modern_noreply(self):
        self.assertEqual(
            noreply_login("1234567+buger@users.noreply.github.com"), "buger"
        )

    def test_legacy_noreply(self):
        self.assertEqual(noreply_login("buger@users.noreply.github.com"), "buger")

    def test_bot_noreply(self):
        self.assertEqual(
            noreply_login("49699333+dependabot[bot]@users.noreply.github.com"),
            "dependabot[bot]",
        )

    def test_ordinary_email_is_not_a_login(self):
        self.assertIsNone(noreply_login("leonsbox@gmail.com"))


class GateTests(unittest.TestCase):
    def test_unsigned_coauthor_fails_the_gate(self):
        # THE regression for issue #269: every commit author is signed, the
        # co-author is not — the old gate said green here.
        problems = check_commits(
            RELAND_COMMITS,
            contributors=["xerj-org", "dependabot[bot]"],
            resolve=lambda email: "buger",
        )
        self.assertTrue(problems, "gate passed a re-land with an unsigned co-author")
        self.assertTrue(any("buger" in p for p in problems), problems)

    def test_unresolvable_coauthor_fails_the_gate(self):
        # An identity the gate cannot attribute is exactly what it exists to
        # flag — unresolvable must fail, not silently pass.
        problems = check_commits(
            RELAND_COMMITS,
            contributors=["xerj-org", "dependabot[bot]"],
            resolve=lambda email: None,
        )
        self.assertTrue(problems, "gate passed an unresolvable co-author")

    def test_signed_coauthor_passes(self):
        problems = check_commits(
            RELAND_COMMITS,
            contributors=["xerj-org", "dependabot[bot]", "buger"],
            resolve=lambda email: "buger",
        )
        self.assertEqual(problems, [])

    def test_login_match_is_case_insensitive(self):
        # GitHub logins are case-insensitive; a case mismatch between the
        # resolved login and the .contributors entry must not flag a signer.
        problems = check_commits(
            RELAND_COMMITS,
            contributors=["xerj-org", "Buger"],
            resolve=lambda email: "buger",
        )
        self.assertEqual(problems, [])

    def test_noreply_coauthor_needs_no_network(self):
        commits = [
            {
                "sha": "abc12345",
                "message": "subject\n\n"
                "Co-authored-by: B <99+buger@users.noreply.github.com>",
            }
        ]
        problems = check_commits(
            commits, contributors=["buger"], resolve=no_network
        )
        self.assertEqual(problems, [])

    def test_no_trailers_needs_no_network(self):
        commits = [{"sha": "abc12345", "message": "fix: plain commit"}]
        self.assertEqual(
            check_commits(commits, contributors=["xerj-org"], resolve=no_network), []
        )

    def test_duplicate_emails_resolved_once(self):
        calls = []

        def counting(email):
            calls.append(email)
            return "buger"

        check_commits(RELAND_COMMITS, contributors=["buger"], resolve=counting)
        self.assertEqual(calls, ["leonsbox@gmail.com"])


class FieldReportExemptionTests(unittest.TestCase):
    """The narrow carve-out: a field-report-only diff is exempt; anything else
    is not. Bundling a code change with a field report must NOT bypass the gate
    — that would be a security hole, so the mixed case is asserted explicitly."""

    def _report(self, name):
        return f"{FIELD_REPORT_DIR}{name}"

    def test_single_field_report_is_exempt(self):
        self.assertTrue(
            is_field_report_only([self._report("2026-08-18-reference-coding.md")])
        )

    def test_several_field_reports_are_exempt(self):
        self.assertTrue(
            is_field_report_only(
                [
                    self._report("2026-08-18-a.md"),
                    self._report("2026-08-18-b.md"),
                ]
            )
        )

    def test_field_report_plus_code_is_NOT_exempt(self):
        # THE security case: a code change riding along with a field report must
        # get the full gate, never the exemption.
        self.assertFalse(
            is_field_report_only(
                [
                    self._report("2026-08-18-a.md"),
                    "engine/crates/xerj-server/src/main.rs",
                ]
            )
        )

    def test_field_report_plus_other_docs_is_NOT_exempt(self):
        self.assertFalse(
            is_field_report_only([self._report("2026-08-18-a.md"), "README.md"])
        )

    def test_the_folder_readme_is_NOT_a_field_report(self):
        self.assertFalse(is_field_report_only([self._report("README.md")]))

    def test_a_nested_path_is_NOT_a_field_report(self):
        self.assertFalse(is_field_report_only([self._report("sub/2026-08-18-a.md")]))

    def test_a_non_markdown_file_in_the_folder_is_NOT_exempt(self):
        self.assertFalse(is_field_report_only([self._report("2026-08-18-a.txt")]))

    def test_an_empty_diff_is_NOT_exempt(self):
        # "nothing changed" must never read as "exempt".
        self.assertFalse(is_field_report_only([]))
        self.assertFalse(is_field_report_only([""]))

    def test_a_lookalike_prefix_outside_the_folder_is_NOT_exempt(self):
        # A path that merely starts with the folder name but is not inside it.
        self.assertFalse(
            is_field_report_only(["user-feedback/16-agent-field-reports-notes.md"])
        )


class SelfClaSignatureTests(unittest.TestCase):
    """Self-signing .contributors PRs (e.g. #1375, #1366, #1355, #1342) deadlock
    on cla-bot because cla-bot reads .contributors from `main`. The narrow
    self-signature predicate satisfies `verification/cla-signed` only when a PR
    exclusively adds its own author to .contributors."""

    BASE = ["xerj-org", "dependabot[bot]", "buger"]
    COMMITS = [
        {
            "sha": "9bd098c0",
            "message": "chore: add todie to .contributors (CLA signature)",
            "author_login": "todie",
        }
    ]

    def test_valid_self_signature_passes(self):
        self.assertTrue(
            is_self_cla_signature(
                paths=[".contributors"],
                base_contributors=self.BASE,
                head_contributors=self.BASE + ["todie"],
                pr_author="todie",
                commits=self.COMMITS,
                resolve=no_network,
            )
        )

    def test_case_insensitive_author_match_passes(self):
        self.assertTrue(
            is_self_cla_signature(
                paths=[".contributors"],
                base_contributors=self.BASE,
                head_contributors=self.BASE + ["todie"],
                pr_author="Todie",
                commits=self.COMMITS,
                resolve=no_network,
            )
        )

    def test_touching_any_other_file_is_rejected(self):
        self.assertFalse(
            is_self_cla_signature(
                paths=[".contributors", "engine/src/lib.rs"],
                base_contributors=self.BASE,
                head_contributors=self.BASE + ["todie"],
                pr_author="todie",
                commits=self.COMMITS,
                resolve=no_network,
            )
        )

    def test_adding_another_user_is_rejected(self):
        self.assertFalse(
            is_self_cla_signature(
                paths=[".contributors"],
                base_contributors=self.BASE,
                head_contributors=self.BASE + ["someone-else"],
                pr_author="todie",
                commits=self.COMMITS,
                resolve=no_network,
            )
        )

    def test_adding_self_plus_another_user_is_rejected(self):
        self.assertFalse(
            is_self_cla_signature(
                paths=[".contributors"],
                base_contributors=self.BASE,
                head_contributors=self.BASE + ["todie", "someone-else"],
                pr_author="todie",
                commits=self.COMMITS,
                resolve=no_network,
            )
        )

    def test_removing_existing_contributor_is_rejected(self):
        self.assertFalse(
            is_self_cla_signature(
                paths=[".contributors"],
                base_contributors=self.BASE,
                head_contributors=["xerj-org", "dependabot[bot]", "todie"],
                pr_author="todie",
                commits=self.COMMITS,
                resolve=no_network,
            )
        )

    def test_commit_by_different_author_is_rejected(self):
        commits = [
            {
                "sha": "9bd098c0",
                "message": "chore: add todie to .contributors",
                "author_login": "other-user",
            }
        ]
        self.assertFalse(
            is_self_cla_signature(
                paths=[".contributors"],
                base_contributors=self.BASE,
                head_contributors=self.BASE + ["todie"],
                pr_author="todie",
                commits=commits,
                resolve=no_network,
            )
        )

    def test_unsigned_coauthor_trailer_is_rejected(self):
        commits = [
            {
                "sha": "9bd098c0",
                "message": "chore: add todie to .contributors\n\n"
                "Co-authored-by: Unsigned <99+unsigned@users.noreply.github.com>",
                "author_login": "todie",
            }
        ]
        self.assertFalse(
            is_self_cla_signature(
                paths=[".contributors"],
                base_contributors=self.BASE,
                head_contributors=self.BASE + ["todie"],
                pr_author="todie",
                commits=commits,
                resolve=no_network,
            )
        )


class WiringTests(unittest.TestCase):
    """A checker CI never runs is not a gate (cf. the #207 lesson)."""

    def _ci(self):
        return (REPO_ROOT / ".github" / "workflows" / "ci.yml").read_text()

    def _cla_job(self):
        ci = self._ci()
        m = re.search(r"\n  cla-config:\n(.*?)(?=\n  \S)", ci, re.S)
        self.assertIsNotNone(m, "cla-config job missing from ci.yml")
        return m.group(1)

    def test_ci_runs_the_coauthor_check(self):
        self.assertIn("check_cla_coauthors.py", self._cla_job())

    def test_ci_runs_these_tests(self):
        self.assertIn("test_cla_coauthors.py", self._cla_job())

    def test_cla_config_computes_changed_files_for_the_carveout(self):
        # The carve-out only works if the job actually passes the PR's changed
        # files to the checker — otherwise every PR runs the full gate.
        self.assertIn("--changed-files", self._cla_job())

    def test_a_separate_job_provides_the_field_report_status(self):
        # cla-bot cannot path-scope itself, so the required `verification/cla-signed`
        # status for a strictly field-report-only PR (or a self-CLA signature PR)
        # is provided by a dedicated workflow, guarded by the same predicates.
        wf = REPO_ROOT / ".github" / "workflows" / "cla-field-report-exempt.yml"
        self.assertTrue(wf.exists(), "field-report exemption workflow missing")
        text = wf.read_text()
        self.assertIn("verification/cla-signed", text)
        self.assertIn("--is-field-report-only", text)
        self.assertIn("--is-self-cla-signature", text)
        # It must never run the PR's own code under the elevated token.
        self.assertIn("pull_request_target", text)
        self.assertIn("base.sha", text)


if __name__ == "__main__":
    unittest.main()
