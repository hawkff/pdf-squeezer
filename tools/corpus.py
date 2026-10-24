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

GRACE_SECONDS = 60  # after SIGTERM, before SIGKILL


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
    parser.add_argument(
        "--overwrite", action="store_true", help="replace existing results"
    )
    args = parser.parse_args()
    args.results.mkdir(parents=True, exist_ok=True)
    # Scanners often write .PDF; glob patterns are case-sensitive on Linux.
    files = sorted(
        p for p in args.corpus.rglob("*") if p.suffix.lower() == ".pdf" and p.is_file()
    )
    outcomes, details = (
        collections.Counter(),
        collections.defaultdict(collections.Counter),
    )
    for path in files:
        relative = path.relative_to(args.corpus)
        # The input's own suffix keeps report.PDF and report.pdf apart.
        output = (
            args.results
            / relative.parent
            / f"{relative.stem}.pdfa{args.pdfa}{relative.suffix}"
        )
        output.parent.mkdir(parents=True, exist_ok=True)
        if output.exists() and not args.overwrite:
            outcomes["skipped"] += 1
            print(
                f"skipped          {path.name}  [output exists; pass --overwrite]",
                flush=True,
            )
            continue
        partial = output.with_name(output.name + ".part")
        partial.unlink(missing_ok=True)
        command = [
            args.binary,
            "--pdfa",
            args.pdfa,
            *shlex.split(args.extra),
            "-o",
            str(partial),
            str(path),
        ]
        started = time.monotonic()
        with subprocess.Popen(
            command, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True
        ) as process:
            try:
                stderr = process.communicate(timeout=args.timeout)[1]
                code = process.returncode
            except subprocess.TimeoutExpired:
                # SIGTERM lets the CLI stop its tool process groups and remove its
                # staging files; SIGKILL would leave both behind.
                process.terminate()
                try:
                    process.communicate(timeout=GRACE_SECONDS)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.communicate()
                code, stderr = -1, "timed out"
        outcome, detail = classify(code, stderr)
        if outcome == "converted":
            partial.replace(output)
        else:
            partial.unlink(missing_ok=True)
        outcomes[outcome] += 1
        details[outcome][detail] += 1
        print(
            f"{outcome:9} {time.monotonic() - started:6.1f}s {path.name}"
            + (f"  [{detail}]" if detail else ""),
            flush=True,
        )
    print()
    for outcome in (
        "converted",
        "refused",
        "rejected",
        "crashed",
        "timeout",
        "skipped",
    ):
        if outcomes[outcome]:
            print(f"{outcome:9} {outcomes[outcome]:4}")
            for detail, count in details[outcome].most_common():
                if detail:
                    print(f"            {count:4}  {detail}")
    return 0 if files else 1


if __name__ == "__main__":
    sys.exit(main())
