#!/usr/bin/env python3
"""Run validated echo workloads in randomized order and save numeric measurements."""

import argparse
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import random
import select
import statistics
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
BUILD = ROOT / "target" / "comparison"
RUST = HERE / "target" / "release"
CASES = [
    (1, 20, 1, "binary"),
    (64, 20, 1, "binary"),
    (16, 1024, 1, "binary"),
    (16, 16384, 1, "binary"),
    (16, 65536, 1, "binary"),
    (16, 1024, 16, "binary"),
    (16, 1024, 1, "text"),
    (128, 16384, 1, "binary"),
]


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def cpu_time(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


def last_level_cache(cpu_path):
    caches = list((cpu_path / "cache").glob("index*"))
    last = max(caches, key=lambda p: int((p / "level").read_text()))
    return (last / "shared_cpu_list").read_text().strip()


def measurement(args, name, case, commands):
    connections, size, window, kind = case
    command = ["taskset", "-c", str(args.server_cpu), *commands[name]]
    socket_dir = tempfile.TemporaryDirectory(prefix="s-", dir=BUILD)
    unix_address = f"unix:{socket_dir.name}/ws.sock"
    if args.unix:
        command[-1] = unix_address
    server = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    workers = []
    try:
        if not select.select([server.stdout], [], [], 10)[0]:
            raise RuntimeError(f"{name}: startup timed out")
        ready = server.stdout.readline().strip().split()
        if len(ready) != 2 or ready[0] != "READY":
            stop(server)
            raise RuntimeError(f"{name}: startup failed: {server.stderr.read()}")
        address = unix_address if args.unix else f"[{args.bind_ip}]:{int(ready[1])}"
        worker_count = min(connections, len(args.client_cpus))
        start = time.monotonic()
        cpu_start = cpu_time(server.pid)
        for index in range(worker_count):
            count = connections // worker_count + (index < connections % worker_count)
            workers.append(subprocess.Popen([
                "taskset", "-c", str(args.client_cpus[index]), str(args.load_generator), address,
                str(count), str(size), str(window), str(args.warmup), str(args.seconds), kind,
            ], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True))
        results = []
        for worker in workers:
            output, error = worker.communicate(timeout=args.warmup + args.seconds + 30)
            if worker.returncode:
                raise RuntimeError(f"{name}: load validation failed: {error}")
            results.append(json.loads(output))
        cpu = cpu_time(server.pid) - cpu_start
        wall = time.monotonic() - start
        if server.poll() is not None:
            raise RuntimeError(f"{name}: server exited during measurement")
        return {
            "library": name, "connections": connections, "bytes": size,
            "window": window, "kind": kind,
            "messages_per_second": sum(r["messages_per_second"] for r in results),
            "server_cpu_fraction_including_warmup": cpu / wall,
            "workers": results,
        }
    finally:
        for worker in workers:
            stop(worker)
        stop(server)
        socket_dir.cleanup()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    transport = parser.add_mutually_exclusive_group(required=True)
    transport.add_argument("--bind-ip", type=ipaddress.IPv6Address)
    transport.add_argument("--unix", action="store_true")
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--libraries", nargs="+", default=["yawc", "tokio-tungstenite", "fastwebsockets", "uWebSockets", "Boost.Beast"])
    parser.add_argument("--baseline-server", type=Path)
    parser.add_argument("--load-generator", type=Path, default=RUST / "load")
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("--warmup", type=float, default=1)
    parser.add_argument("--seconds", type=float, default=3)
    parser.add_argument("--server-cpu", type=int, default=2)
    parser.add_argument("--client-cpus", type=int, nargs="+", default=[4, 6])
    parser.add_argument("--smoke", action="store_true")
    parser.add_argument("--case-index", type=int, nargs="+", choices=range(len(CASES)))
    args = parser.parse_args()
    if args.bind_ip and (args.bind_ip.is_unspecified or args.bind_ip.is_multicast):
        parser.error("an explicit unicast address is required")
    if args.repeats < 1 or args.seconds <= 0 or args.warmup < 0:
        parser.error("invalid timing parameters")
    if args.server_cpu in args.client_cpus or len(set(args.client_cpus)) != len(args.client_cpus):
        parser.error("server and client CPUs must be distinct")
    allowed = os.sched_getaffinity(0)
    if not set([args.server_cpu, *args.client_cpus]).issubset(allowed):
        parser.error("selected CPUs are outside this process's affinity")
    cpus = [args.server_cpu, *args.client_cpus]
    topology = [Path(f"/sys/devices/system/cpu/cpu{cpu}") for cpu in cpus]
    physical_cores = [tuple((path / "topology" / field).read_text().strip()
                           for field in ["physical_package_id", "core_id"]) for path in topology]
    if len(set(physical_cores)) != len(cpus):
        parser.error("selected CPUs include SMT siblings")
    caches = [last_level_cache(path) for path in topology]
    address = "unix:" if args.unix else f"[{args.bind_ip}]:0"
    commands = {name: [str(RUST / "server"), name, address]
                for name in ["yawc", "yawc-batched", "yawc-buffered", "yawc-buffered-128k", "yawc-buffered-512k", "tokio-tungstenite", "tokio-tungstenite-batched", "fastwebsockets"]}
    cpp_address = "unix:" if args.unix else str(args.bind_ip)
    commands.update({"uWebSockets": [str(BUILD / "uws"), cpp_address],
                     "Boost.Beast": [str(BUILD / "beast"), cpp_address]})
    if args.baseline_server:
        commands["yawc-before"] = [str(args.baseline_server.resolve()), "yawc", address]
        commands["yawc-batched-before"] = [str(args.baseline_server.resolve()), "yawc-batched", address]
        commands["yawc-buffered-before"] = [str(args.baseline_server.resolve()), "yawc-buffered", address]
    if any(name not in commands for name in args.libraries):
        parser.error("unknown library or missing --baseline-server")
    if args.output.exists():
        parser.error("output already exists; choose a new baseline name")
    cases = CASES[:1] if args.smoke else CASES
    if args.case_index is not None:
        cases = [CASES[index] for index in args.case_index]
    schedule = [(repeat, name, case) for repeat in range(args.repeats)
                for case in cases for name in args.libraries]
    random.Random(6455).shuffle(schedule)
    report = {
        "schema": 1, "complete": False,
        "warmup_seconds": args.warmup, "measurement_seconds": args.seconds,
        "repeats": args.repeats, "server_threads": 1, "server_cpu": args.server_cpu,
        "cases": cases, "libraries": args.libraries,
        "client_cpus": args.client_cpus,
        "shared_last_level_cache": len(set(caches)) == 1,
        "transport": "Unix sockets" if args.unix else "IPv6 TCP",
        "tls": False, "compression": False,
        "build": json.loads((BUILD / "build.json").read_text()),
        "binary_sha256": {name: digest(Path(commands[name][0])) for name in args.libraries},
        "load_sha256": digest(args.load_generator),
        "source_revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "source_sha256": hashlib.sha256(b"".join(p.read_bytes() for p in sorted((ROOT / "src").rglob("*.rs")))).hexdigest(),
        "measurements": [],
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    if args.baseline_server and args.baseline_server.with_suffix(".json").exists():
        report["baseline_build"] = json.loads(args.baseline_server.with_suffix(".json").read_text())
    for repeat, name, case in schedule:
        result = measurement(args, name, case, commands)
        result["repeat"] = repeat
        report["measurements"].append(result)
        # Checkpoint complete samples without recording addresses, paths or logs.
        args.output.write_text(json.dumps(report, indent=2) + "\n")
        print(f"{len(report['measurements'])}/{len(schedule)} {name} {case}: {result['messages_per_second']:.0f} msg/s", flush=True)
    report["complete"] = True
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    for case in cases:
        for name in args.libraries:
            values = [r["messages_per_second"] for r in report["measurements"]
                      if r["library"] == name and (r["connections"], r["bytes"], r["window"], r["kind"]) == case]
            print(f"{name} {case}: median {statistics.median(values):.0f} msg/s, range {min(values):.0f}..{max(values):.0f}")


if __name__ == "__main__":
    main()
