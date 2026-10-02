#!/usr/bin/env python3
"""Build pinned comparison servers. Requires Linux, C++20, Boost and zlib headers."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile
import urllib.request

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
BUILD = ROOT / "target" / "comparison"
UWS = "2cb3a77d89045b9e39ca8c85e37f614d2b3afa2b"
USOCKETS = "182b7e4fe7211f98682772be3df89c71dc4884fa"


def run(args, **kwargs):
    subprocess.run(args, check=True, **kwargs)


def dependency(repo, revision):
    destination = BUILD / f"{repo}-{revision}"
    if not destination.exists():
        archive = BUILD / f"{repo}-{revision}.tar.gz"
        urllib.request.urlretrieve(
            f"https://codeload.github.com/uNetworking/{repo}/tar.gz/{revision}", archive
        )
        with tarfile.open(archive) as source:
            source.extractall(BUILD, filter="data")
    return destination


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rust-only", action="store_true")
    args = parser.parse_args()
    BUILD.mkdir(parents=True, exist_ok=True)
    run(["cargo", "build", "--release", "--locked", "--manifest-path", str(HERE / "Cargo.toml")])
    if not args.rust_only:
        uws = dependency("uWebSockets", UWS)
        sockets = dependency("uSockets", USOCKETS)
        run(["make", "WITH_LTO=0"], cwd=sockets)
        compiler = os.environ.get("CXX", "g++")
        flags = [compiler, "-O3", "-DNDEBUG", "-std=c++20", "-pthread"]
        run([*flags, str(HERE / "beast.cpp"), "-o", str(BUILD / "beast")])
        run([*flags, "-DLIBUS_NO_SSL", f"-I{uws / 'src'}", f"-I{sockets / 'src'}",
             str(HERE / "uws.cpp"), str(sockets / "uSockets.a"), "-lz", "-o", str(BUILD / "uws")])
    metadata = {
        "uWebSockets": UWS,
        "uSockets": USOCKETS,
        "rust_lock_sha256": hashlib.sha256((HERE / "Cargo.lock").read_bytes()).hexdigest(),
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "custom_rust_flags": bool(os.environ.get("RUSTFLAGS")),
        "cpp_flags": "-O3 -DNDEBUG -std=c++20 -pthread; uSockets WITH_LTO=0",
    }
    # Keep only version numbers, never compiler configuration paths or host details.
    metadata["g++"] = subprocess.check_output([os.environ.get("CXX", "g++"), "-dumpfullversion"], text=True).strip()
    boost_version = Path("/usr/include/boost/version.hpp")
    if boost_version.exists():
        import re
        metadata["boost"] = re.search(r'#define BOOST_LIB_VERSION "([^"]+)"', boost_version.read_text())[1]
    (BUILD / "build.json").write_text(json.dumps(metadata, indent=2) + "\n")


if __name__ == "__main__":
    main()
