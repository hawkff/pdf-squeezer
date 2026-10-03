"""Run PDF/A conversion over a directory of PDFs and tabulate the outcomes.

    python3 tools/corpus.py --pdfa 4 --binary ./pdf-squeezer corpus/ results/

Every file is converted on its own so one failure cannot hide another. Outcomes:
converted (veraPDF confirmed), refused (the converter asked for user action),
rejected (veraPDF failed the result), crashed, and timeout. The summary groups
refusals by the option they ask for and rejections by rule.
"""

import argparse
import collections
import re
import shlex
import subprocess
import sys
import time
from pathlib import Path


def classify(code, stderr):
    if code == 0:
        return "converted", ""
    if "timed out" in stderr:
        return "timeout", ""
    if "did not confirm" in stderr:
        rules = sorted(set(re.findall(r"\b(6(?:\.\d+)+-\d+)", stderr)))
        return "rejected", ", ".join(rules) or "unknown rule"
    refusal = re.search(
        r"^PDF/A: (?:cannot produce [^:\n]+:\s*)?(.+)", stderr, re.DOTALL | re.MULTILINE
    )
    if refusal:
        options = sorted(
            set(re.findall(r"--[a-z-]+(?: [a-z0-9,-]+)?", refusal.group(1)))
        )
        return "refused", "; ".join(options) or refusal.group(1).strip()[:80]
    crash = re.search(r"^PDF tools: (.+)$", stderr, re.MULTILINE)
    if crash:
        return "crashed", crash.group(1)[:120]
    return "crashed", stderr.strip().splitlines()[-1][:120] if stderr.strip() else ""


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("corpus", type=Path)
    parser.add_argument("results", type=Path)
    parser.add_argument("--pdfa", default="4", choices=["2b", "3b", "4"])
    parser.add_argument("--binary", default="pdf-squeezer")
    parser.add_argument("--timeout", type=int, default=900)
    parser.add_argument(
        "--extra", default="", help="extra CLI flags, e.g. '--strip actions'"
    )
    args = parser.parse_args()
    args.results.mkdir(parents=True, exist_ok=True)
    files = sorted(p for p in args.corpus.rglob("*.pdf") if p.is_file())
    outcomes, details = (
        collections.Counter(),
        collections.defaultdict(collections.Counter),
    )
    for path in files:
        relative = path.relative_to(args.corpus)
        output = args.results / relative.parent / f"{relative.stem}.pdfa{args.pdfa}.pdf"
        output.parent.mkdir(parents=True, exist_ok=True)
        if output.exists():
            output.unlink()
        command = [
            args.binary,
            "--pdfa",
            args.pdfa,
            *shlex.split(args.extra),
            "-o",
            str(output),
            str(path),
        ]
        started = time.monotonic()
        try:
            run = subprocess.run(
                command,
                capture_output=True,
                text=True,
                timeout=args.timeout,
                check=False,
            )
            code, stderr = run.returncode, run.stderr
        except subprocess.TimeoutExpired:
            code, stderr = -1, "timed out"
        outcome, detail = classify(code, stderr)
        outcomes[outcome] += 1
        details[outcome][detail] += 1
        print(
            f"{outcome:9} {time.monotonic() - started:6.1f}s {path.name}"
            + (f"  [{detail}]" if detail else ""),
            flush=True,
        )
    print()
    for outcome in ("converted", "refused", "rejected", "crashed", "timeout"):
        if outcomes[outcome]:
            print(f"{outcome:9} {outcomes[outcome]:4}")
            for detail, count in details[outcome].most_common():
                if detail:
                    print(f"            {count:4}  {detail}")
    return 0 if files else 1


if __name__ == "__main__":
    sys.exit(main())
