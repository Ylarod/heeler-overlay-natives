#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-3.0-or-later
"""Writes the notice for every third-party Rust crate linked into CEasyTier.

Two groups are listed, each crate with its licence files verbatim (identical
texts are printed once and referenced afterwards):

- The crates rustc actually linked into each target's archive (the sets must
  agree): every archive member `<crate>-<hash>.*` is matched to a package of
  heeler-easytier's normal (non-build, non-dev, non-proc-macro) dependency
  graph by its library name, and where several versions share that name, by
  the source files its dep-info (`<deps-dir>/<crate>-<hash>.d`) lists. Graph
  crates rustc did not link (unused, or behind a feature or cfg the build
  does not take) are left out; an archive crate that matches no package fails.
  The EasyTier workspace crates are covered by the EasyTier LGPL notice. Each
  is the exact package recorded in Cargo.lock.
- The Rust standard-library crates rustc linked into the archive (archive
  members whose crate hash matches an rlib in the target's sysroot). Versions
  come from the toolchain's library/Cargo.lock; crates.io ones are downloaded
  and checked against its checksums for their licence files, and the
  in-tree ones carry the Rust project's COPYRIGHT and licences (--rust-licences,
  verified by the caller against sources.lock).

A linked crate without a licence file fails the build.

    collect-notices.py --crate-dir DIR --output FILE --rust-licences DIR \
        --target-dir CARGO_TARGET_DIR \
        --archive TRIPLE=LIB [--archive TRIPLE=LIB ...]

The archives are the ones built in --target-dir (dep-info is read from
<target-dir>/<triple>/release/deps).
"""

import argparse
import hashlib
import io
import json
import re
import subprocess
import sys
import tarfile
import time
import tomllib
import urllib.request
from pathlib import Path

# SPDX standard MIT text, used only for crates whose package declares MIT (or
# offers it under OR) but ships no licence file.
MIT_TEXT = """Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
"""

LICENCE_PREFIXES = ("license", "licence", "copying", "notice", "unlicense", "copyright")
EASYTIER_WORKSPACE_CRATES = {"easytier", "easytier-core", "easytier-proto"}


def metadata(crate_dir: Path, target: str) -> dict:
    output = subprocess.run(
        ["cargo", "metadata", "--locked", "--offline", "--format-version", "1",
         "--filter-platform", target],
        cwd=crate_dir, check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(output)


def graph_packages(data: dict) -> list:
    """The root and its normal (non-build, non-dev) dependencies, without
    proc-macro crates (they run in the compiler and are never linked)."""
    packages = {package["id"]: package for package in data["packages"]}
    nodes = {node["id"]: node for node in data["resolve"]["nodes"]}

    def is_proc_macro(package: dict) -> bool:
        return any("proc-macro" in target["kind"] for target in package["targets"])

    root = data["resolve"]["root"]
    seen = {root}
    pending = [root]
    while pending:
        node = nodes[pending.pop()]
        for dep in node["deps"]:
            normal = any(kind["kind"] is None for kind in dep["dep_kinds"])
            if not normal or dep["pkg"] in seen or is_proc_macro(packages[dep["pkg"]]):
                continue
            seen.add(dep["pkg"])
            pending.append(dep["pkg"])
    return [packages[package_id] for package_id in seen]


def library_name(package: dict):
    for target in package["targets"]:
        if any(kind in ("lib", "rlib", "staticlib") for kind in target["kind"]):
            return target["name"].replace("-", "_")
    return None


def dep_info_sources(path: Path, crate_dir: Path) -> list:
    """Source files a rustc dep-info file lists (escaped spaces kept)."""
    sources = []
    for line in path.read_text().splitlines():
        if not line or line.startswith("#") or ":" not in line:
            continue
        _, _, rest = line.partition(": ")
        for item in re.split(r"(?<!\\) ", rest):
            item = item.replace("\\ ", " ").strip()
            if item:
                candidate = Path(item)
                sources.append(candidate if candidate.is_absolute() else crate_dir / candidate)
    return sources


def linked_packages(data: dict, crate_dir: Path, archive: Path, deps_dir: Path, std_members: set) -> list:
    """Packages of the dependency graph whose objects are in `archive`."""
    by_library = {}
    for package in graph_packages(data):
        name = library_name(package)
        if name:
            by_library.setdefault(name, []).append(package)
    linked = {}
    for member in sorted(archive_crates(archive) - std_members):
        name, digest = member.rsplit("-", 1)
        candidates = by_library.get(name, [])
        dep_info = deps_dir / f"{member}.d"
        if dep_info.is_file():
            sources = [source.resolve() for source in dep_info_sources(dep_info, crate_dir)]
            candidates = [
                package for package in candidates
                if any(Path(package["manifest_path"]).parent.resolve() in source.parents for source in sources)
            ]
        elif len(candidates) > 1:
            raise SystemExit(f"error: {member} matches {len(candidates)} packages and {dep_info} is missing")
        if len(candidates) != 1:
            raise SystemExit(f"error: archive crate {member} matches {len(candidates)} packages of the graph")
        linked[candidates[0]["id"]] = candidates[0]
    root = data["resolve"]["root"]
    if root not in linked:
        raise SystemExit(f"error: {archive} does not contain heeler-easytier itself")
    linked.pop(root)
    return sorted(
        (p for p in linked.values() if p["name"] not in EASYTIER_WORKSPACE_CRATES),
        key=lambda p: (p["name"], p["version"]),
    )


def source_description(package: dict) -> str:
    source = package.get("source") or ""
    if source.startswith("registry+"):
        return f"https://crates.io/crates/{package['name']}/{package['version']}"
    if source.startswith("git+"):
        return source[len("git+"):].replace("#", " commit ")
    return source or "local path"


def licence_files(package: dict) -> list:
    directory = Path(package["manifest_path"]).parent
    files = [
        path for path in directory.iterdir()
        if path.is_file() and path.name.lower().startswith(LICENCE_PREFIXES)
    ]
    declared = package.get("license_file")
    if declared:
        declared_path = (directory / declared).resolve()
        if declared_path.is_file() and declared_path not in files:
            files.append(declared_path)
    return sorted(files, key=lambda path: path.name)


def rust_toolchain(crate_dir: Path) -> str:
    return subprocess.run(
        ["rustc", "--print", "sysroot"], cwd=crate_dir, check=True, capture_output=True, text=True
    ).stdout.strip()


RUST_MEMBER = re.compile(r"^([A-Za-z0-9_]+-[0-9a-f]{16})\.")


def archive_crates(archive: Path) -> set:
    """`name-hash` of every Rust crate with an object in the archive (rustc
    names them `<crate>-<16 hex digits>.<crate>.<cgu>.rcgu.o`; objects that
    build scripts compile from C or assembly do not match)."""
    members = subprocess.run(["ar", "t", str(archive)], check=True, capture_output=True, text=True).stdout
    return {match.group(1) for member in members.splitlines() if (match := RUST_MEMBER.match(member))}


def std_members(sysroot: Path, target: str, archive: Path) -> set:
    """`name-hash` of the standard-library crates linked into the archive
    (archive members whose crate hash matches an rlib in the sysroot)."""
    rlibs = {
        path.stem[len("lib"):]
        for path in (sysroot / "lib" / "rustlib" / target / "lib").glob("lib*.rlib")
    }
    return archive_crates(archive) & rlibs


def crate_file(name: str, version: str, checksum: str) -> bytes:
    """The .crate archive, from Cargo's local cache when it holds a copy with
    the expected checksum, else downloaded with a few retries. The checksum
    is verified by the caller either way."""
    file_name = f"{name}-{version}.crate"
    cache = Path.home() / ".cargo" / "registry" / "cache"
    for cached in sorted(cache.glob(f"*/{file_name}")):
        data = cached.read_bytes()
        if hashlib.sha256(data).hexdigest() == checksum:
            return data
    url = f"https://static.crates.io/crates/{name}/{file_name}"
    last_error = None
    for attempt in range(4):
        if attempt:
            time.sleep(2 * attempt)
        try:
            with urllib.request.urlopen(url, timeout=60) as response:
                return response.read()
        except OSError as error:
            last_error = error
    raise SystemExit(f"error: could not download {url}: {last_error}")


def licence_from_crate_file(name: str, version: str, checksum: str) -> tuple:
    """(licence expression, [(file name, text)]) from the verified .crate."""
    data = crate_file(name, version, checksum)
    actual = hashlib.sha256(data).hexdigest()
    if actual != checksum:
        raise SystemExit(f"error: {name} {version} checksum mismatch ({actual} != {checksum})")
    texts = []
    licence = "see licence files"
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        for member in sorted(archive.getmembers(), key=lambda m: m.name):
            parts = member.name.split("/")
            if not member.isfile() or len(parts) != 2:
                continue
            handle = archive.extractfile(member)
            if handle is None:
                continue
            if parts[1] == "Cargo.toml":
                licence = tomllib.loads(handle.read().decode()).get("package", {}).get("license", licence)
            elif parts[1].lower().startswith(LICENCE_PREFIXES):
                texts.append((parts[1], handle.read().decode("utf-8", errors="replace")))
    return licence, texts


def std_entries(sysroot: Path, crate_names: list, rust_licences: Path) -> list:
    library = sysroot / "lib" / "rustlib" / "src" / "rust" / "library"
    lock = tomllib.loads((library / "Cargo.lock").read_text())
    by_name = {}
    for package in lock["package"]:
        by_name.setdefault(package["name"].replace("-", "_"), []).append(package)
    manifests = {}
    for manifest in library.glob("**/Cargo.toml"):
        try:
            data = tomllib.loads(manifest.read_text())
        except tomllib.TOMLDecodeError:
            continue
        name = data.get("package", {}).get("name")
        if name:
            manifests.setdefault(name.replace("-", "_"), (manifest.parent, data["package"]))
    rust_project = [
        (name, (rust_licences / name).read_text()) for name in ("COPYRIGHT", "LICENSE-APACHE", "LICENSE-MIT")
    ]

    entries = []
    for crate in crate_names:
        candidates = by_name.get(crate, [])
        if len(candidates) != 1:
            raise SystemExit(f"error: the standard library's Cargo.lock has {len(candidates)} entries for {crate}")
        package = candidates[0]
        name, version = package["name"], package["version"]
        if package.get("source", "").startswith("registry+"):
            licence, texts = licence_from_crate_file(name, version, package["checksum"])
            source = f"https://crates.io/crates/{name}/{version} (Rust standard-library dependency)"
        else:
            directory, manifest = manifests[crate]
            texts = [
                (path.name, path.read_text())
                for path in sorted(directory.iterdir())
                if path.is_file() and path.name.lower().startswith(LICENCE_PREFIXES)
            ]
            parent = directory.parent
            if not texts and parent != library:
                texts = [
                    (path.name, path.read_text())
                    for path in sorted(parent.iterdir())
                    if path.is_file() and path.name.lower().startswith(LICENCE_PREFIXES)
                ]
            texts += rust_project
            licence = manifest.get("license", "MIT OR Apache-2.0")
            source = f"rust-lang/rust 1.95.0, library/{directory.relative_to(library)}"
        if not texts:
            raise SystemExit(f"error: standard-library crate {name} {version} has no licence file")
        entries.append({"name": name, "version": version, "licence": licence, "source": source, "texts": texts})
    return entries


def crate_entries(packages: list) -> list:
    entries = []
    for package in packages:
        files = licence_files(package)
        licence = package.get("license") or "see licence file"
        entry = {
            "name": package["name"],
            "version": package["version"],
            "licence": licence,
            "source": source_description(package),
            "texts": [(path.name, path.read_text(encoding="utf-8", errors="replace")) for path in files],
        }
        if not files:
            declared = package.get("license") or ""
            if "MIT" not in declared.replace("/", " OR ").split(" OR ") and declared != "MIT":
                raise SystemExit(f"error: {package['name']} {package['version']} ships no licence file")
            owners = ", ".join(package.get("authors") or []) or f"the {package['name']} authors"
            entry["texts"] = [(
                "MIT (declared in Cargo.toml)",
                f"The package ships no licence file; its manifest declares \"{declared}\"\n"
                f"({package.get('repository') or source_description(package)}). Used under MIT:\n\n"
                f"Copyright (c) {owners}\n\n{MIT_TEXT}",
            )]
        entries.append(entry)
    return entries


def mpl_note(entries: list) -> list:
    if not any("MPL" in entry["licence"] for entry in entries):
        return []
    return [
        "Source Code Form of the MPL-2.0 crates: the crates.io package at the URL",
        "listed for it (download https://static.crates.io/crates/<name>/<name>-<version>.crate);",
        "these crates are unmodified.",
        "",
    ]


def table(entries: list) -> list:
    rows = ["Crate | Version | Licence | Source", "------+---------+---------+-------"]
    rows += [f"{e['name']} | {e['version']} | {e['licence']} | {e['source']}" for e in entries]
    return rows


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--crate-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rust-licences", type=Path, required=True)
    parser.add_argument("--target-dir", type=Path, required=True, help="the CARGO_TARGET_DIR the archives were built in")
    parser.add_argument("--archive", action="append", required=True, help="TRIPLE=static library")
    args = parser.parse_args()
    archives = dict(item.split("=", 1) for item in args.archive)
    targets = list(archives)

    sysroot = Path(rust_toolchain(args.crate_dir))
    std_linked = {t: std_members(sysroot, t, Path(path)) for t, path in archives.items()}
    sets = {
        target: linked_packages(
            metadata(args.crate_dir, target), args.crate_dir, Path(archives[target]),
            args.target_dir / target / "release" / "deps", std_linked[target])
        for target in targets
    }
    packages = sets[targets[0]]
    for target, linked in sets.items():
        if [p["id"] for p in linked] != [p["id"] for p in packages]:
            print(f"error: {target} links a different crate set than {targets[0]}", file=sys.stderr)
            return 1

    std_sets = {t: sorted(member.rsplit("-", 1)[0] for member in members) for t, members in std_linked.items()}
    std_names = std_sets[targets[0]]
    if not std_names or any(names != std_names for names in std_sets.values()):
        print(f"error: standard-library crates differ between targets: {std_sets}", file=sys.stderr)
        return 1
    rustc = subprocess.run(["rustc", "--version"], cwd=args.crate_dir, check=True, capture_output=True, text=True).stdout.strip()

    crates = crate_entries(packages)
    std = std_entries(sysroot, std_names, args.rust_licences)

    lines = [
        "Third-party Rust crates linked into CEasyTier (heeler-easytier with EasyTier,",
        f"targets {', '.join(targets)}; EasyTier itself: EasyTier-LGPL-3.0.txt;",
        "heeler-easytier: heeler-easytier-LGPL-3.0-or-later.txt).",
        "",
        *mpl_note(crates),
        "Dependencies whose code rustc linked into the archives, each the package",
        "recorded in heeler-easytier's Cargo.lock:",
        "",
        *table(crates),
        "",
        f"Rust standard library, statically linked by {rustc}:",
        "",
        *table(std),
    ]

    texts = {}
    sections = []
    for entry in crates + std:
        for file_name, text in entry["texts"]:
            text = text.rstrip() + "\n"
            digest = hashlib.sha256(text.encode()).hexdigest()
            header = f"{entry['name']} {entry['version']} - {file_name}"
            sections += ["", "=" * 80, header, "=" * 80]
            if digest in texts:
                sections.append(f"(Identical to the text under \"{texts[digest]}\" above.)")
            else:
                texts[digest] = header
                sections += ["", text]

    args.output.write_text("\n".join(lines + sections) + "\n", encoding="utf-8")
    print(f"{len(crates)} crates and {len(std)} standard-library crates, {len(texts)} distinct licence texts -> {args.output}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
