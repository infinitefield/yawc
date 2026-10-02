#!/usr/bin/env python3
"""Summarize saved samples without copying process logs or machine identifiers."""

import argparse
import json
from pathlib import Path
import statistics


def key(sample):
    return sample["connections"], sample["bytes"], sample["window"], sample["kind"]


def summary(report):
    if report.get("complete") is False:
        raise ValueError("run did not finish")
    groups = {}
    for sample in report["measurements"]:
        groups.setdefault((sample["library"], key(sample)), []).append(sample)
    for samples in groups.values():
        if len(samples) != report["repeats"]:
            raise ValueError("incomplete repetitions")
    return groups


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("paired", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--capacity", type=Path)
    parser.add_argument("--micro", type=Path)
    parser.add_argument("--validation", type=Path)
    args = parser.parse_args()
    baseline = json.loads(args.baseline.read_text())
    paired = json.loads(args.paired.read_text())
    if baseline["transport"] != paired["transport"]:
        raise ValueError("cannot compare different transports")
    if baseline["load_sha256"] != paired["load_sha256"]:
        raise ValueError("load generator changed between runs")
    base = summary(baseline)
    changes = summary(paired)
    cases = sorted({case for _, case in base})
    names = ["yawc-before", "tokio-tungstenite", "fastwebsockets", "uWebSockets", "Boost.Beast"]
    lines = ["# Measured baseline", "",
             f"Transport: {baseline['transport']}. One server thread, two client processes",
             "(one for the single-connection case), separate physical cores, no TLS or compression.",
             f"Baseline: {baseline['repeats']} repetitions, {baseline['warmup_seconds']:g} s warmup,",
             f"{baseline['measurement_seconds']:g} s measurement per sample. Values are median messages/s.",
             "These results describe local echo on a shared machine.", "",
             "Raw data: [baseline](baseline.json), [paired run](paired.json),",
             "[microbenchmarks](micro.json). Compiler versions and C++ revisions are in the baseline;",
             "Rust versions are pinned in [Cargo.lock](../comparison/Cargo.lock).", "",
             "| Connections / bytes / window / type | " + " | ".join(names) + " |",
             "|---|" + "---:|" * len(names)]
    for case in cases:
        row = [f"{case[0]} / {case[1]} / {case[2]} / {case[3]}"]
        for name in names:
            row.append(f"{statistics.median(s['messages_per_second'] for s in base[name, case]):,.0f}")
        lines.append("| " + " | ".join(row) + " |")
    lines += ["", "## Paired yawc comparison", "",
              f"{paired['repeats']} repetitions of each binary in randomized order using the same client.",
              "Ranges show the minimum and maximum sample throughput, not confidence intervals.", "",
              "| Connections / bytes / window / type | Before median (range) | After median (range) | Change |",
              "|---|---:|---:|---:|"]
    for case in cases:
        values = [[s["messages_per_second"] for s in changes[name, case]] for name in ["yawc-before", "yawc"]]
        medians = [statistics.median(v) for v in values]
        cells = [f"{case[0]} / {case[1]} / {case[2]} / {case[3]}"]
        cells += [f"{statistics.median(v):,.0f} ({min(v):,.0f} to {max(v):,.0f})" for v in values]
        cells.append(f"{100 * (medians[1] / medians[0] - 1):+.1f}%")
        lines.append("| " + " | ".join(cells) + " |")
    peak_client = max(w["client_cpu_fraction"] for report in [baseline, paired]
                      for s in report["measurements"] for w in s["workers"])
    lines += ["", f"Highest client CPU fraction in any sample: {peak_client:.2f} of one core.",
              "Raw samples include CPU usage, per-worker batch RTT percentiles and binary hashes.",
              "RTT includes client work and queueing; window 16 measures batch completion.", ""]
    if args.validation:
        validation = json.loads(args.validation.read_text())
        groups = summary(validation)
        case = (64, 20, 1, "binary")
        if {k for _, k in groups} != {case}:
            raise ValueError("validation must contain only the small-message case")
        before = statistics.median(s["messages_per_second"] for s in changes["yawc-before", case])
        after = statistics.median(s["messages_per_second"] for s in changes["yawc", case])
        lines += ["## Small-message repeat check", "",
                  f"The first paired run changed throughput by {100 * (after / before - 1):+.1f}% for 64 connections and 20-byte messages.",
                  f"A repeat with {validation['repeats']} samples per binary, {validation['warmup_seconds']:g} s warmup",
                  f"and {validation['measurement_seconds']:g} s measurement gave:", "",
                  "| Library | Median messages/s | Range |", "|---|---:|---:|"]
        for name in ["yawc-before", "yawc"]:
            values = [s["messages_per_second"] for (library, _), samples in groups.items()
                      if library == name for s in samples]
            lines.append(f"| {name} | {statistics.median(values):,.0f} | {min(values):,.0f} to {max(values):,.0f} |")
        lines += ["", "Both runs are retained to show measurement variability.", ""]
    if args.capacity:
        capacity = json.loads(args.capacity.read_text())
        groups = summary(capacity)
        lines += ["## Client capacity check", "",
                  f"The cases below use {len(capacity['client_cpus'])} client workers instead of two.",
                  "This checks whether client CPU capacity capped the results with two workers.",
                  "Medians in messages/s; these results are a separate workload.", "",
                  "| Connections / bytes / window | Before | After | uWebSockets |",
                  "|---|---:|---:|---:|"]
        for case in sorted({case for _, case in groups}):
            cells = [f"{case[0]} / {case[1]} / {case[2]}"]
            for name in ["yawc-before", "yawc", "uWebSockets"]:
                cells.append(f"{statistics.median(s['messages_per_second'] for s in groups[name, case]):,.0f}")
            lines.append("| " + " | ".join(cells) + " |")
        lines.append("")
    if args.micro:
        micro = json.loads(args.micro.read_text())
        lines += ["## In-memory echo", "",
                  "Criterion mean time per complete client/server echo, with no sockets.",
                  "40 samples per case; full estimates and confidence intervals are in micro.json.", "",
                  "| Payload bytes | Before ns | After ns | Time change |",
                  "|---|---:|---:|---:|"]
        for sample in sorted(micro["benchmarks"], key=lambda s: int(s["name"].split("/")[-1])):
            if sample["name"].startswith("echo_duplex/"):
                a, b = sample["before_ns"], sample["after_ns"]
                lines.append(f"| {sample['name'].split('/')[-1]} | {a:,.1f} | {b:,.1f} | {100 * (b/a-1):+.1f}% |")
        codec = next(s for s in micro["benchmarks"] if s["name"] == "codec_roundtrip/client/20")
        delta = 100 * (codec["after_ns"] / codec["before_ns"] - 1)
        lines += ["", f"The isolated 20-byte client codec changed by {delta:+.1f}% in time.",
                  "The full JSON retains all codec and masking results, including regressions.", ""]
    args.output.write_text("\n".join(lines))


if __name__ == "__main__":
    main()
