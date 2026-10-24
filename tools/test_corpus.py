"""Checks for the corpus runner against a stub CLI; no PDF tools are needed."""

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

# Writes its -o output, except for slow.pdf, which waits and records SIGTERM.
STUB = """#!/bin/sh
trap 'kill "$pid"; echo terminated > "$MARKER"; exit 1' TERM
for arg do [ "$previous" = -o ] && output=$arg; previous=$arg; done
case "$output" in
  */slow.*) sleep 30 & pid=$!; wait "$pid" ;;
  *) : > "$output" ;;
esac
"""


@unittest.skipIf(sys.platform == "win32", "POSIX shell stub")
class CorpusTests(unittest.TestCase):
    def test_uppercase_extensions_and_timeouts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            corpus, results, stub = root / "corpus", root / "results", root / "stub"
            corpus.mkdir()
            for name in ("SCAN.PDF", "slow.pdf"):
                (corpus / name).write_bytes(b"%PDF-1.7\n")
            stub.write_text(STUB)
            stub.chmod(0o700)
            run = subprocess.run(
                [sys.executable, str(Path(__file__).with_name("corpus.py"))]
                + ["--binary", str(stub), "--timeout", "2", str(corpus), str(results)],
                env={**os.environ, "MARKER": str(root / "marker")},
                capture_output=True,
                text=True,
                timeout=120,
                check=False,
            )
            self.assertEqual(run.returncode, 0, run.stderr)
            self.assertRegex(run.stdout, r"converted +\S+ SCAN\.PDF")
            self.assertRegex(run.stdout, r"timeout +\S+ slow\.pdf")
            self.assertTrue((results / "SCAN.pdfa4.pdf").exists())
            self.assertEqual((root / "marker").read_text().strip(), "terminated")


if __name__ == "__main__":
    unittest.main()
