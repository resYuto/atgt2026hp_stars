#!/usr/bin/env python3
import re
import statistics
import sys
from pathlib import Path


ARENA_PATTERNS = [
    re.compile(r"\barena=(\d+)\b"),
    re.compile(r"\bArena nodes:\s*(\d+)\b"),
    re.compile(r"\bArena:\s*(\d+)\b"),
]
FREE_PATTERNS = [
    re.compile(r"\bfree_list=(\d+)\b"),
    re.compile(r"\bfree=(\d+)\b"),
]


def parse_log(path: Path):
    max_arena = 0
    min_free = None
    gc_count = 0
    elapsed = None
    depth_files = []

    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        for p in ARENA_PATTERNS:
            m = p.search(line)
            if m:
                max_arena = max(max_arena, int(m.group(1)))

        for p in FREE_PATTERNS:
            m = p.search(line)
            if m:
                v = int(m.group(1))
                min_free = v if min_free is None else min(min_free, v)

        if "GC:" in line:
            gc_count += 1

        m = re.search(r"ELAPSED_SECONDS=([0-9]+(?:\.[0-9]+)?)", line)
        if m:
            elapsed = float(m.group(1))

        m = re.search(r"Saved:\s+(.+_depth\d+_[^ ]+\.pgm)\s*$", line)
        if m:
            depth_files.append(m.group(1))

    return {
        "run": path.name,
        "elapsed_seconds": elapsed,
        "max_arena_nodes": max_arena,
        "gc_count": gc_count,
        "min_free_list": min_free,
        "depth_files": "|".join(depth_files),
    }


def main():
    if len(sys.argv) != 2:
        print("usage: summarize_baseline_io.py <baseline_log_dir>", file=sys.stderr)
        sys.exit(1)

    log_dir = Path(sys.argv[1])
    logs = sorted(log_dir.glob("run*.log"))
    if not logs:
        print("no run*.log found", file=sys.stderr)
        sys.exit(2)

    rows = [parse_log(p) for p in logs]
    print("run\telapsed_seconds\tmax_arena_nodes\tgc_count\tmin_free_list\tdepth_files")
    for r in rows:
        print(
            f"{r['run']}\t{r['elapsed_seconds']}\t{r['max_arena_nodes']}\t"
            f"{r['gc_count']}\t{r['min_free_list']}\t{r['depth_files']}"
        )

    elapsed = [r["elapsed_seconds"] for r in rows if r["elapsed_seconds"] is not None]
    arenas = [r["max_arena_nodes"] for r in rows]
    if elapsed:
        print(
            "stats\t"
            f"mean_elapsed={statistics.mean(elapsed):.3f};"
            f"stdev_elapsed={statistics.pstdev(elapsed):.3f};"
            f"min_elapsed={min(elapsed):.3f};"
            f"max_elapsed={max(elapsed):.3f}\t"
            f"mean_max_arena={statistics.mean(arenas):.1f};"
            f"min_max_arena={min(arenas)};"
            f"max_max_arena={max(arenas)}\t\t"
        )


if __name__ == "__main__":
    main()
