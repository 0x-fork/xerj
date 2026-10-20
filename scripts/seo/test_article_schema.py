#!/usr/bin/env python3
"""Regression tests for the required `updated:` frontmatter field (#974).

Until #974 an article could omit `updated:`, and build_articles fell back to
the content file's git committer date (`article.updated or
context.dates.last_modified(...)`).  A squash merge rewrites that date to the
merge day, so any PR whose pages were regenerated on day N went red on
`build_articles --check` the moment it merged on day N+1 (#942 was the live
instance; #972 band-aided it).  These tests pin the fix: the field is
materialized in every content source and `article_data.load` rejects a source
without it, so article dates are author-controlled data and cannot drift with
git history.

Run: python3 scripts/seo/test_article_schema.py
"""

import pathlib
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import article_data
from article_data import FrontmatterError, load

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent.parent

# Minimal frontmatter that passes _validate as written; each test mutates
# only the `updated:` line it exercises.
FRONTMATTER = """---
title: "Schema test article"
h1: "H1 of the schema test article?"
description: "A minimal article that satisfies the frontmatter schema."
slug: "schema-test-article"
cluster: "Test cluster"
question: "What does the schema test article cover?"
intent: "how-to"
published: "2026-08-21"
updated: "2026-09-01"
author: "XERJ documentation team"
reviewer: "XERJ engineering team"
schema_type: "TechArticle"
links_out:
  - /answers/what-is-xerj/
faq:
  - q: "Question one?"
    a: "Answer one."
  - q: "Question two?"
    a: "Answer two."
  - q: "Question three?"
    a: "Answer three."
  - q: "Question four?"
    a: "Answer four."
---
Body of the schema test article.
"""


def _write(tmp: pathlib.Path, text: str) -> pathlib.Path:
    # The slug must match the filename and the parent directory selects the
    # category, so the fixture layout mirrors content/answers/.
    directory = tmp / "answers"
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / "schema-test-article.md"
    path.write_text(text, encoding="utf-8")
    return path


class UpdatedFieldTests(unittest.TestCase):
    def test_missing_updated_is_rejected(self):
        # The #974 regression: before the field became required this loaded
        # cleanly and the generator silently dated the page from git.
        text = FRONTMATTER.replace('updated: "2026-09-01"\n', "")
        with tempfile.TemporaryDirectory() as tmp:
            path = _write(pathlib.Path(tmp), text)
            with self.assertRaises(FrontmatterError) as ctx:
                load(path)
            self.assertIn("missing required frontmatter field(s): updated",
                          str(ctx.exception))

    def test_valid_updated_is_parsed(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = _write(pathlib.Path(tmp), FRONTMATTER)
            article = load(path)
        self.assertEqual(article.updated, "2026-09-01")

    def test_malformed_updated_is_rejected(self):
        text = FRONTMATTER.replace('updated: "2026-09-01"',
                                   'updated: "Sep 1st"')
        with tempfile.TemporaryDirectory() as tmp:
            path = _write(pathlib.Path(tmp), text)
            with self.assertRaises(FrontmatterError) as ctx:
                load(path)
            self.assertIn("updated must be an ISO date YYYY-MM-DD",
                          str(ctx.exception))

    def test_impossible_calendar_date_is_rejected(self):
        # Matches the YYYY-MM-DD shape but is not a real date, so only the
        # date.fromisoformat fallback catches it.
        text = FRONTMATTER.replace('updated: "2026-09-01"',
                                   'updated: "2026-09-32"')
        with tempfile.TemporaryDirectory() as tmp:
            path = _write(pathlib.Path(tmp), text)
            with self.assertRaises(FrontmatterError) as ctx:
                load(path)
            self.assertIn("updated is not a valid ISO date", str(ctx.exception))


class CommittedContentTests(unittest.TestCase):
    def test_every_committed_article_carries_updated(self):
        """No source may regress to the git-date fallback (#974's root cause).

        load() already rejects a missing field, so this doubles as a check
        that the whole committed corpus still parses under the stricter
        schema.
        """
        sources = sorted(
            (REPO_ROOT / "content").glob("answers/*.md")
        ) + sorted((REPO_ROOT / "content").glob("compare/*.md"))
        self.assertGreater(len(sources), 0, "no content sources found")
        bad: list[str] = []
        for path in sources:
            try:
                article = article_data.load(path)
            except FrontmatterError as exc:
                bad.append(str(exc))
                continue
            if not article.updated:
                bad.append(f"{path}: updated is missing")
        self.assertEqual(bad, [])


class StructuredDataOptInTests(unittest.TestCase):
    """The opt-in FAQPage/HowTo/SoftwareApplication JSON-LD (README trap 6)."""

    OPT_IN = ('structured_data: [faq, howto, software]\n'
              'og_image: "og/answers-schema-test-article.png"\n'
              'og_image_alt: "Cover for the schema test article."\n'
              'commands:\n'
              '  - cmd: "xerj autoindex ./data"\n'
              '    note: "Index the folder"\n'
              '  - cmd: "xerj search \'query\'"\n'
              '    note: "Search it"\n')

    def _load(self, extra: str):
        text = FRONTMATTER.replace("faq:\n", extra + "faq:\n", 1)
        with tempfile.TemporaryDirectory() as tmp:
            return load(_write(pathlib.Path(tmp), text))

    def test_default_is_off(self):
        with tempfile.TemporaryDirectory() as tmp:
            article = load(_write(pathlib.Path(tmp), FRONTMATTER))
        self.assertEqual(article.structured_data, ())
        self.assertNotIn("structured_data", article.head_meta())

    def test_opt_in_emits_three_blocks(self):
        import seo_head
        article = self._load(self.OPT_IN)
        meta = article.head_meta()
        self.assertEqual(meta["og_image"], "og/answers-schema-test-article.png")
        blocks = seo_head.extra_structured_data("https://xerj.org/x", "d", meta)
        self.assertEqual([b["@type"] for b in blocks],
                         ["FAQPage", "HowTo", "SoftwareApplication"])
        self.assertEqual(len(blocks[0]["mainEntity"]), 4)
        self.assertEqual(blocks[1]["step"][1]["text"], "xerj search 'query'")
        self.assertNotIn("aggregateRating", blocks[2])

    def test_unknown_kind_is_rejected(self):
        with self.assertRaises(FrontmatterError) as ctx:
            self._load("structured_data: [faq, review]\n")
        self.assertIn("must be one of", str(ctx.exception))

    def test_howto_needs_commands(self):
        with self.assertRaises(FrontmatterError) as ctx:
            self._load("structured_data: [howto]\n")
        self.assertIn("howto needs at least two commands", str(ctx.exception))

    def test_og_image_must_match_slug(self):
        with self.assertRaises(FrontmatterError):
            self._load('og_image: "og/answers-other.png"\nog_image_alt: "x"\n')
        with self.assertRaises(FrontmatterError):
            self._load('og_image: "og/answers-schema-test-article.png"\n')


if __name__ == "__main__":
    unittest.main()
