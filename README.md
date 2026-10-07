# heeler-overlay-natives

The native libraries behind [Heeler](https://github.com/ZingerLittleBee/Heeler)'s
in-process overlay networks, built from pinned upstream sources into static
XCFrameworks and published as a Swift package:

| Product | Built from | What Heeler uses it for |
| --- | --- | --- |
| `CTailscale` | libtailscale (tsnet, Go `c-archive`) + `native/tailscale` | One tsnet node per tailnet; dials TCP for SSH. |
| `CZeroTier` | libzt (ZeroTierOne + lwIP, C++) + `native/zerotier` | One ZeroTier node per process; networks, moons, self-hosted planets; each connection bound to its network's interface, so networks that assign the same address stay apart. |
| `CEasyTier` | EasyTier (Rust, smoltcp, no TUN) through the `heeler-easytier` crate in `native/easytier` | Any number of networks side by side, one EasyTier instance per instance key (C ABI v2), each manual or from a config server; a config server may assign up to eight; a dial picks the one the destination fits. |

Each XCFramework has an arm64 iPhoneOS slice and an arm64 iPhone Simulator
slice; every object in them targets iOS 18.0 (`minos 18.0`). The Swift layer
that drives them (`HeelerOverlay`) lives in Heeler; this repository only owns
the native code, its patches, and the build.

## Versions

| Component | Pin | Where |
| --- | --- | --- |
| libtailscale | commit `59d4bb82744915815178e0f0776d60026a397ee7` (no tags upstream) | submodule `upstream/libtailscale` |
| tailscale.com | `v1.102.5` (libtailscale's own go.mod says v1.94.1) | `native/tailscale/go.mod`, `go.sum` |
| Go | `go1.27.1` darwin-arm64, SHA-256 verified download | `sources.lock` |
| libzt | tag `1.16.2`, commit `7e5d0f99a2de81c45faa28e44289bcd19d933bf2` (Ylarod/libzt: zerotier/libzt main `a707ea6` plus ZeroTierOne 1.16.2, blocking `zts_node_stop`, opt-in metrics) | submodule `upstream/libzt` |
| ZeroTierOne | 1.16.2, commit `fc5c3ec22090b5b2a0f274e863651fe9ca489bf4` | libzt's submodule `ext/ZeroTierOne` |
| lwIP | commit `32708c0a8b140efb545cc35101ee5fdeca6d6489` (joseph-henry/lwip, STABLE-2_1_x) | libzt's submodule `ext/lwip` |
| lwIP contrib | commit `4fd612c9c72dfcd1db6618bd59c1a17d9f5b55f8` (joseph-henry/lwip-contrib) | libzt's submodule `ext/lwip-contrib` |
| EasyTier | 2.7.0 development commit `728ba94b5029240add6204eed591f22e4e039e6b` | submodule `upstream/EasyTier` |
| Rust | 1.95.0 (rustup) | `native/easytier/rust-toolchain.toml`, `sources.lock` |
| protoc | 36.2 osx-aarch_64 release, SHA-256 verified download (EasyTier's protobuf code generation) | `sources.lock` |
| Xcode | 26.3 (17C529), iOS SDK 26.2, CMake 4.4.3 | recorded in `PROVENANCE.md` |

EasyTier is pinned to a development commit ahead of 2.7.0. Once v2.7.0 is
tagged upstream, move the submodule to the tag (see "Upgrading").

## Layout

```
sources.lock            every pin and hash (shell syntax, sourced by the scripts)
upstream/               git submodules: libtailscale, libzt (+ ext/*), EasyTier
patches/libzt/          patches to libzt and its ZeroTierOne submodule
patches/easytier/       patches to EasyTier
native/tailscale/       heeler_tailscale.go, and the go.mod/go.sum that replace libtailscale's
native/zerotier/        heeler_zerotier.cpp/.h, compiled into libzt's src/
native/easytier/        the heeler-easytier crate: Cargo.lock, rust-toolchain.toml,
                        src/, include/heeler_easytier.h, examples/, notice tooling
include/<Framework>/    umbrella header and module map of each framework
licenses/               GPL-3.0 and Rust 1.95.0 licence texts (committed, hash-checked)
scripts/                build.sh, verify.sh, package.sh, release.sh, audit-upstream.sh,
                        compare-artifacts.sh, link-probe.sh, easytier-dev.sh
Tests/LinkProbe/        Swift package that links the three frameworks on a Simulator
Package.swift           development manifest (build/Artifacts); release tags carry
                        the generated url + checksum manifest
```

The glue sources started as byte-identical copies of the ones Heeler
audited at `db87976b`; their comments still name Heeler's former paths
(`NativeSupport/...`). `native/tailscale/heeler_tailscale.go` is unchanged
since; `native/zerotier/heeler_zerotier.*` and the `heeler-easytier` crate
have changed here (see "Native interfaces").

## Native interfaces

- **CZeroTier** adds to libzt: `heeler_zt_peers`, `heeler_zt_planet_inspect`,
  `heeler_zt_add_moon` (see `heeler_zerotier.h`), and for joined networks
  that assign the node the same address:
  - `int heeler_zt_bind_network(int fd, uint64_t net_id, int family)` binds
    a socket to the network's lwIP interface (`SO_BINDTODEVICE`): it sends
    and receives on that network alone, and lwIP routes it by the interface
    without consulting patch 0002's source hook. Returns `ZTS_ERR_OK`,
    `ZTS_ERR_ARG`, `ZTS_ERR_NO_RESULT` (not joined, or no interface for the
    family yet), `ZTS_ERR_SERVICE`, or `ZTS_ERR_SOCKET`.
  - `int heeler_zt_network_reaches(uint64_t net_id, int family, const void *address)`
    tells before connecting whether the network reaches an IPv4 address by
    itself (its subnet, or one of its managed routes through a gateway on
    that subnet), so a bound connection to an unreachable address can fail
    at once instead of timing out: `ZTS_ERR_OK` or `HEELER_ZT_ERR_NO_ROUTE`
    (-110), else `ZTS_ERR_ARG` / `ZTS_ERR_NO_RESULT` / `ZTS_ERR_SERVICE`.
    IPv6 is not checked.
- **CEasyTier** is C ABI version 2 (`HEELER_ET_ABI_VERSION`): every function
  takes an instance key (1 to 128 bytes of printable UTF-8; at most
  `HEELER_ET_MAX_INSTANCES`, 32, at once). A key holds one manual network
  (`heeler_et_start(key, toml, ...)`) or one config-server session
  (`heeler_et_web_start(key, url, machine_id, hostname, secure_mode, ...)`)
  running up to `HEELER_ET_MAX_WEB_NETWORKS` (8) networks; starting either
  replaces only that key. Every network is its own EasyTier instance (own
  peers, routes, smoltcp stack) on one shared runtime.
  `heeler_et_tcp_connect_fd(key, network, host, port, ...)` dials through
  exactly one network: the named one, or (`network` NULL) the only one, the
  one with a peer at that address or of that name, or the one whose subnet
  holds the address; several fits return `HEELER_ET_ERR_AMBIGUOUS` (-5).
  `heeler_et_stop(key)`, `heeler_et_web_stop(key)`, `heeler_et_stop_all()`,
  and `heeler_et_status_json(key, buf, len)` complete it. Version 1 (one
  network per process, no keys) is gone; a Heeler build that still uses it
  does not link against these frameworks.

## Building

Requirements: an Apple silicon Mac with Xcode 26.3, CMake, rustup, Python 3,
and `git submodule update --init --recursive`. Then:

```sh
scripts/build.sh                     # build what changed, skip the rest, verify
scripts/verify.sh                    # verify build/Artifacts without building
scripts/link-probe.sh                # link and call the frameworks on a Simulator
```

`scripts/build.sh` writes:

```
build/stage/<library>/   one library's XCFramework, notices, provenance, FINGERPRINT
build/Artifacts/         CTailscale/CZeroTier/CEasyTier.xcframework, Notices/,
                         PROVENANCE.md, SHA256SUMS (what Package.swift points at)
```

How each library is built (the same commands Heeler used, so the output is
the same):

- **CTailscale**: the pinned Go archive is extracted into the work directory;
  `git archive` of the libtailscale commit gets `native/tailscale/go.mod` and
  `go.sum` (after checking the commit still requires tailscale.com v1.94.1)
  and `heeler_tailscale.go`; `go build -buildmode=c-archive -trimpath
  -buildvcs=false -tags ios -ldflags "-s -w -buildid="` with `GOTOOLCHAIN=local`,
  `-mod=readonly`, `GOSUMDB=sum.golang.org`, and a clang wrapper per platform.
  `go mod verify` re-checks the shared module cache against `go.sum`.
- **CZeroTier**: `git archive` of libzt and of each of its submodule commits,
  laid out like GitHub's commit archives, then `patches/libzt`, then
  `heeler_zerotier.cpp/.h` in `src/`; CMake (`zt-static`,
  `BUILD_IOS_FRAMEWORK`), `strip -S -x`.
- **CEasyTier**: `native/easytier` is copied to the work directory with
  `git archive` of EasyTier plus `patches/easytier` in `vendor/easytier`;
  the pinned protoc is extracted beside it and named by `PROTOC` and
  `PROTOC_INCLUDE` (no protoc on `PATH` is needed or used);
  `cargo build --locked --release` for `aarch64-apple-ios` and
  `aarch64-apple-ios-sim`; LLVM bitcode sections removed, Rust standard
  library objects restamped to minos 18.0 (`restamp-macho.py`), debug sections
  stripped, and each slice link-checked by a dead-stripped probe executable.

Patches are applied to copies under `build/`, never to the submodules' work
trees; the source is read from the object database with `git archive`, which
applies `export-ignore` exactly as GitHub's archives do.

### Caches and skipping

- **Downloads** (the Go toolchain, protoc; the archives `audit-upstream.sh` checks)
  are cached in `~/Library/Caches/heeler-overlay-natives/downloads`
  (`HEELER_NATIVES_CACHE_DIR` moves the whole cache). A cached file is
  hashed on every use and replaced if it does not match.
- **Go**: the module cache (`<cache>/go-mod`) is shared between builds and
  verified with `go mod verify`; the Go build cache (`<cache>/go-build`) is
  reused unless `FORCE=1`.
- **Cargo**: the registry is Cargo's own (`~/.cargo`); the target directory
  `build/cargo-target` is reused, and the crate copy is synced by content so
  unchanged crates are not recompiled.
- **Skipping**: each library has an input fingerprint (upstream commits,
  patch list and hashes, glue sources, build scripts, header and module map,
  licence texts, Go/Rust/Xcode/clang/SDK/CMake/Python versions, deployment
  target). When it matches `build/stage/<library>/FINGERPRINT`, the library is
  skipped; when it does not, the changed lines are printed and the library is
  rebuilt. `build/Artifacts` is reassembled only when a stage changed. A
  second `scripts/build.sh` with nothing changed takes a few seconds (the
  fingerprints and `verify.sh`).
- **`FORCE=1 scripts/build.sh`** deletes the selected stages and work
  directories and the cargo target, uses a fresh Go build cache, and rebuilds
  everything (downloads and the verified module cache are still reused). Use
  it for releases and reproducibility checks.
- `--only tailscale,zerotier` builds a subset (Artifacts are reassembled only
  when all three stages exist); `HEELER_NATIVES_BUILD_DIR` builds elsewhere
  (used by CI's reproducibility check); `HEELER_NATIVES_JOBS` sets CMake's
  parallelism; `HEELER_NATIVES_SIGNING_IDENTITY` codesigns the XCFrameworks
  (the published ones are unsigned; `PROVENANCE.md` records which).

Every curl call uses `--connect-timeout 20 --speed-time 30 --speed-limit 1024
--retry 3 --retry-all-errors`; `collect-notices.py` takes standard-library
crates from Cargo's local cache when their checksum matches and otherwise
downloads them with retries. Proxies come from the usual `https_proxy` /
`all_proxy` / `CARGO_HTTP_PROXY` variables.

### Reproducibility

Unsigned builds are byte-for-byte reproducible for a given Xcode, SDK, CMake,
Go, and Rust: `ZERO_AR_DATE=1`; every work directory is remapped to
`/heeler-overlay` (`-ffile-prefix-map`), the crate to `/heeler-easytier`, the
target directory to `/target`, Cargo's home to `/cargo`, and `$HOME` to
`/home` (`--remap-path-prefix`); Go uses `-trimpath` and an empty build ID;
each XCFramework's `Info.plist` is rewritten with its slices sorted. Static
archives record their members' owner and group, which new files inherit from
the build directory: build where the group matches (a directory under
`/tmp` belongs to `wheel`; `chgrp staff` it first) when comparing builds.
The binaries built at this repository's commit `25a0093` are byte-identical
to the artifacts Heeler accepted at commit `db87976b`
(`Packages/HeelerOverlay/Artifacts`); CTailscale still is, while CZeroTier
and CEasyTier have changed since. Check with:

```sh
scripts/compare-artifacts.sh build/Artifacts <heeler>/Packages/HeelerOverlay/Artifacts
```

The notices differ from Heeler's on purpose: they point at this repository
for the corresponding source.

## Auditing

- `scripts/build.sh --sources-only` checks the submodule pins against
  `sources.lock` (including libzt's nested gitlinks and the repository URLs in
  libzt's `.gitmodules`), the Go download, the go.mod/go.sum and licence
  hashes, and that every patch is listed, matches its hash, and applies with
  `--fuzz=0`.
- `scripts/audit-upstream.sh` downloads GitHub's archive of every pinned
  commit, checks it against the SHA-256 Heeler originally audited
  (`*_ARCHIVE_SHA256`), and checks that `git archive` of the submodule commit
  holds exactly the same files.
- `scripts/verify.sh [dir]` checks an Artifacts directory: `SHA256SUMS` (and
  that no unlisted file exists), the two arm64 slices, `minos`, platform, and
  architecture of every object, every exported entry point Heeler uses
  (including `heeler_tailscale_logout`, `heeler_tailscale_disable_log_upload`,
  `heeler_zt_peers`, `heeler_zt_planet_inspect`, `heeler_zt_add_moon`,
  `heeler_zt_bind_network`, `heeler_zt_network_reaches`, `heeler_et_stop_all`,
  and `heeler_et_web_start`), the C types of those declarations (compiled
  against the shipped headers, so the keyed EasyTier ABI v2 signatures, such
  as `heeler_et_web_start`'s seven parameters, are enforced), headers and
  module maps against this checkout, the 17 notices, and `PROVENANCE.md`
  against `sources.lock`, the patches, and the glue hashes.

## Patches

| Patch | Purpose |
| --- | --- |
| `libzt/0001-independent-root-sets.patch` | ZeroTierOne looks peers up in one root of each root set and relays through the root that relayed the peer; adds `Node::addLocalMoon` (self-hosted planets as local moons). |
| `libzt/0002-managed-gateway-routes.patch` | Installs a network's IPv4 managed gateway routes in lwIP, with a source-routing hook that keeps each network's traffic on it (networks sharing an address rely on `heeler_zt_bind_network` instead). |
| `libzt/0003-metrics-saver-on-demand.patch` | ZeroTierOne 1.16's metrics saver (prometheus-cpp-lite `SaveToFile`) starts its thread only while metrics are written, instead of from static initialization, polling every 10 ms, in every process that links libzt. CZeroTier never enables metrics. |
| `easytier/0001-outbound-only-packet-proxy.patch` | EasyTier's packet proxy ignores a peer-set exit-node bit and never forwards a peer to loopback. |
| `easytier/0002-web-client-backend.patch` | Makes the config-server client's backend pluggable (`run_web_client_with_backend`). |
| `easytier/0003-websocket-verify-server-certificates.patch` | The config-server wss:// connection verifies the server certificate with the system trust store. |

Each patch is listed in order with its SHA-256 in `sources.lock`
(`ZEROTIER_PATCHES`, `EASYTIER_PATCHES`); every file a patch changes carries
a "Modified by Heeler contributors" notice. To change one, edit the patch,
update its hash in `sources.lock`, and run `scripts/build.sh --sources-only`.

For local Rust work on the crate:

```sh
scripts/easytier-dev.sh                  # patched EasyTier and protoc into native/easytier/vendor
cd native/easytier
export PROTOC="$PWD/vendor/protoc/bin/protoc"
cargo test --locked
cargo run --locked --release --example e2e        # two nodes, banner + echo
cargo run --locked --release --example security   # outbound-only regression
cargo run --locked --release --example multi      # networks side by side, same subnets
(cd vendor/easytier && cargo build --release -p easytier-web)
EASYTIER_WEB=vendor/easytier/target/release/easytier-web \
    cargo run --locked --release --example webconfig   # config-server mode
EASYTIER_WEB=vendor/easytier/target/release/easytier-web \
    cargo run --locked --release --example multi       # plus a multi-network session
# Peers for Heeler's EasyTierMultiLiveTests (127.0.0.1:21310-21312, web 22550/11750):
cargo run --locked --release --example multi-peer -- --web vendor/easytier/target/release/easytier-web
```

## Upgrading

1. **Move a submodule**: `git -C upstream/<name> fetch && git -C upstream/<name>
   checkout <commit-or-tag>` (for libzt also `git -C upstream/libzt submodule
   update --init --recursive`), then update the commit(s) in `sources.lock`
   and, for audit, the matching `*_ARCHIVE_SHA256`
   (`curl -L <repo>/archive/<commit>.tar.gz | shasum -a 256`).
2. **Refresh patches** against the new tree; `scripts/build.sh --sources-only`
   must pass with `--fuzz=0`. Update the hashes in `sources.lock`.
3. **Tailscale**: in a `git archive` copy of libtailscale with
   `heeler_tailscale.go` added, using the pinned Go:
   `GOTOOLCHAIN=local GOFLAGS=-modcacherw GOPROXY=https://proxy.golang.org GOSUMDB=sum.golang.org go get tailscale.com@<version> && go mod tidy`;
   copy `go.mod`/`go.sum` to `native/tailscale/`, update
   `TAILSCALE_MODULE_VERSION` and both hashes, and review the module diff
   and the `ipnstate.Status` fields Heeler reads.
4. **EasyTier 2.7.0 release**: point the submodule at `v2.7.0`, refresh the
   patches, regenerate `native/easytier/Cargo.lock`
   (`scripts/easytier-dev.sh && cd native/easytier && cargo update -p easytier`),
   and drop the "development commit" wording from `sources.lock`.
5. **Toolchains**: Go (`GO_VERSION`, `GO_URL`, `GO_SHA256`), protoc
   (`PROTOC_VERSION`, `PROTOC_URL`, `PROTOC_SHA256`), Rust
   (`rust-toolchain.toml`, `EASYTIER_RUST_TOOLCHAIN`, and new
   `licenses/rust-<version>/` texts with their hashes), or Xcode (CI's
   `DEVELOPER_DIR`). Any of them changes the bytes.
6. `scripts/build.sh && scripts/link-probe.sh`, then release.

## Releases

A release is a tag `v<version>` whose commit carries a generated
`Package.swift`: each binary target downloads
`https://github.com/Ylarod/heeler-overlay-natives/releases/download/v<version>/<Framework>.xcframework.zip`
and SwiftPM checks it against the recorded checksum. The main branch keeps
the development manifest (paths into `build/Artifacts`), so a release commit
sits on top of the commit it was built from and is reachable through its tag.

Release assets: `CTailscale.xcframework.zip`, `CZeroTier.xcframework.zip`,
`CEasyTier.xcframework.zip`, `Notices.zip` (the notices and `PROVENANCE.md`),
`Package.swift`, `checksums.txt` (`swift package compute-checksum` of each
zip), and `SHA256SUMS`. Zips are deterministic (sorted entries, 1980-01-01
timestamps, fixed permissions, deflate level 9 with the zlib of Xcode's
python3).

Two ways to cut one:

- **Locally (recommended for releases that must equal an accepted build):**
  `scripts/release.sh <version>` runs `FORCE=1 scripts/build.sh`,
  `scripts/package.sh v<version>`, writes the release commit through a
  temporary index (the work tree and branch do not move), and tags it. It
  pushes nothing and prints the two publishing commands
  (`git push origin v<version>` and `gh release create ... build/dist/v<version>/*`).
  Pushing the tag also runs `release.yml`, which rebuilds on CI and uploads
  its own assets only if they reproduce the tag's checksums.
- **On CI:** run the Release workflow with a version. It audits the
  submodules, builds with `FORCE=1`, packages, pushes the release tag, and
  creates the GitHub release with the assets.

Heeler then depends on the package with
`.package(url: "https://github.com/Ylarod/heeler-overlay-natives", exact: "<version>")`
and the products `CTailscale`, `CZeroTier`, and `CEasyTier`, and copies
`Notices.zip`'s notices into its bundled notices (its `LicenseNoticeTests`
compare them).

### Using a local build in Heeler

SwiftPM lets a local package override a remote one with the same identity:
build here, then add this checkout (its directory must be named
`heeler-overlay-natives`) to Heeler's Xcode project as a local package
(File > Add Package Dependencies > Add Local, or drag the folder into the
project navigator). Its development `Package.swift` serves `build/Artifacts`.

## CI

- `.github/workflows/ci.yml` (pull requests, main): audit the submodules,
  build with the stage, download, Go, and Cargo caches (unchanged libraries
  are skipped), verify, rebuild CZeroTier and CTailscale with `FORCE=1` in a
  separate directory and compare them byte for byte (all three with the
  `reproduce_all` dispatch input), and run the Simulator link probe.
- `.github/workflows/release.yml`: described under "Releases".

Both select Xcode 26.3 through `DEVELOPER_DIR`.

## Licences

- This repository's own build scripts and glue (`scripts/`,
  `native/tailscale/`, `native/zerotier/`, `include/`, `Tests/`) are licensed
  under the Apache License 2.0 (`LICENSE`).
- `native/easytier` (the heeler-easytier crate, which links EasyTier
  statically) is licensed under the GNU Lesser General Public License v3.0 or
  later (`native/easytier/LICENSE` with the LGPL-3.0 text,
  `licenses/gpl-3.0.txt` for the GPL it supplements, and
  `native/easytier/NOTICE.txt`).
- Upstream components keep their own licences: libtailscale, Tailscale, and
  Go are BSD-3-Clause (other Go modules as listed in
  `Tailscale-Go-modules.txt`); libzt is BUSL-1.1 converted to Apache-2.0 on
  its Change Date (2026-01-01); ZeroTierOne 1.16's core (`node/`, `osdep/`)
  is MPL-2.0 (its source-available `nonfree/` controller is never compiled,
  and the build checks that); with lwIP, lwIP contrib, MiniUPnPc, libnatpmp
  (BSD-3-Clause), LZ4 and moodycamel::ConcurrentQueue (BSD-2-Clause), and
  prometheus-cpp-lite (MIT); EasyTier is LGPL-3.0; the Rust crates and standard
  library linked into CEasyTier are listed with their licence files in
  `EasyTier-Rust-crates.txt`.
- The build generates all 17 notices into `build/Artifacts/Notices` (and
  `Notices.zip` in each release). `ZeroTier-Heeler-modifications.txt` lists
  every libzt/ZeroTierOne patch and the files it changes (Apache-2.0 section
  4(b)) and names the release tag as the Source Code Form of the modified
  MPL-2.0 files (MPL-2.0 section 3.2).

**LGPL corresponding source.** CEasyTier statically links EasyTier
(LGPL-3.0). `EasyTier-LGPL-3.0.txt` carries the LGPL-3.0 and GPL-3.0 texts and
names the corresponding source: the release tag of this repository the binary
was published under, which pins EasyTier by submodule commit and holds every
patch (with its hash), the crate with its `Cargo.lock`, and the build scripts.
Its installation information (LGPL-3.0 section 4(e)) explains how to rebuild
CEasyTier with a modified EasyTier and run it in Heeler with a free Apple
developer account, using the local-package override above.
