"""Checks for the corpus runner against a stub CLI; no PDF tools are needed."""

import contextlib
import importlib.util
import io
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

spec = importlib.util.spec_from_file_location(
    "corpus", Path(__file__).with_name("corpus.py")
)
corpus = importlib.util.module_from_spec(spec)
spec.loader.exec_module(corpus)

# Writes its -o output. slow.pdf waits and records SIGTERM; stubborn.pdf ignores it.
STUB = """#!/bin/sh
for arg do [ "$previous" = -o ] && output=$arg; previous=$arg; done
case "$output" in
  */slow.*)
    trap 'kill "$pid"; echo terminated > "$MARKER"; exit 1' TERM
    sleep 30 & pid=$!
    wait "$pid" ;;
  */stubborn.*) trap '' TERM; exec sleep 30 ;;
  *) : > "$output" ;;
esac
"""


class CorpusTests(unittest.TestCase):
    def run_corpus(self, *names):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        documents, results, stub = root / "corpus", root / "results", root / "stub"
        documents.mkdir()
        for name in names:
            (documents / name).write_bytes(b"%PDF-1.7\n")
        stub.write_text(STUB)
        stub.chmod(0o700)
        argv = ["corpus.py", "--binary", str(stub), "--timeout", "1"]
        output = io.StringIO()
        with (
            mock.patch.dict(os.environ, MARKER=str(root / "marker")),
            mock.patch.object(sys, "argv", argv + [str(documents), str(results)]),
            contextlib.redirect_stdout(output),
        ):
            self.assertEqual(corpus.main(), 0)
        return root, results, output.getvalue()

    def test_uppercase_extensions_and_timeouts(self):
        root, results, output = self.run_corpus("SCAN.PDF", "slow.pdf")
        self.assertRegex(output, r"converted +\S+ SCAN\.PDF")
        self.assertRegex(output, r"timeout +\S+ slow\.pdf")
        self.assertTrue((results / "SCAN.pdfa4.PDF").exists())
        self.assertEqual((root / "marker").read_text().strip(), "terminated")

    def test_kills_a_run_that_ignores_sigterm(self):
        with mock.patch.object(corpus, "GRACE_SECONDS", 1):
            _, _, output = self.run_corpus("stubborn.pdf", "z.pdf")
        self.assertRegex(output, r"timeout +\S+ stubborn\.pdf")
        self.assertRegex(output, r"converted +\S+ z\.pdf")


if __name__ == "__main__":
    unittest.main()
