# shellcheck shell=bash
# CEasyTier: EasyTier through the heeler-easytier crate (native/easytier) as
# a Rust static library. Sourced by scripts/build.sh and
# scripts/easytier-dev.sh.

EASYTIER_CRATE_SOURCE="native/easytier"
EASYTIER_LIBRARY="libheeler_easytier.a"

easytier_fingerprint() {
    echo "library: easytier"
    echo "deployment-target: ${DEPLOYMENT_TARGET}"
    echo "easytier: ${EASYTIER_VERSION} ${EASYTIER_COMMIT}"
    echo "patches: ${EASYTIER_PATCHES}"
    echo "rust-toolchain: ${EASYTIER_RUST_TOOLCHAIN}"
    if rustc +"${EASYTIER_RUST_TOOLCHAIN}" -vV >/dev/null 2>&1; then
        rustc +"${EASYTIER_RUST_TOOLCHAIN}" -vV | sed 's/^/rustc: /'
    else
        echo "rustc: not installed"
    fi
    apple_toolchain_fingerprint
    hash_files scripts/build.sh scripts/lib/common.sh scripts/lib/easytier.sh \
        include/CEasyTier/CEasyTier.h include/CEasyTier/module.modulemap \
        "${GPL3_TEXT_FILE}" "${RUST_LICENSE_DIR}/COPYRIGHT" \
        "${RUST_LICENSE_DIR}/LICENSE-APACHE" "${RUST_LICENSE_DIR}/LICENSE-MIT"
    hash_tree "${EASYTIER_CRATE_SOURCE}"
    hash_tree patches/easytier
}

# Writes the verified, patched EasyTier tree to destination, changing only
# files whose content differs (so Cargo's mtime-based fingerprints keep
# unchanged crates of a reused target directory).
sync_easytier_source() {
    local destination="$1"
    local staging
    staging="$(mktemp -d "${TMPDIR:-/tmp}/heeler-easytier-src.XXXXXX")"
    export_tree "${ROOT_DIR}/upstream/EasyTier" "${EASYTIER_COMMIT}" "${staging}/easytier"
    apply_patches "${ROOT_DIR}/patches/easytier" "${EASYTIER_PATCHES}" "${staging}/easytier" \
        "EasyTier ${EASYTIER_COMMIT}"
    mkdir -p "${destination}"
    rsync -rlc --delete "${staging}/easytier/" "${destination}/"
    rm -rf "${staging}"
}

# The crate as Cargo builds it: native/easytier with the EasyTier tree in
# vendor/easytier, where its path dependencies expect it.
sync_easytier_crate() {
    local crate="$1"
    mkdir -p "${crate}"
    rsync -rlc --delete --exclude /vendor --exclude /target --exclude /out --exclude __pycache__ \
        "${ROOT_DIR}/${EASYTIER_CRATE_SOURCE}/" "${crate}/"
    sync_easytier_source "${crate}/vendor/easytier"
}

install_rust_toolchain() {
    local crate="$1" toolchain
    toolchain="$(sed -n 's/^channel = "\(.*\)"/\1/p' "${crate}/rust-toolchain.toml")"
    [[ "${toolchain}" == "${EASYTIER_RUST_TOOLCHAIN}" ]] \
        || die "rust-toolchain.toml pins ${toolchain}, sources.lock pins ${EASYTIER_RUST_TOOLCHAIN}"
    rustup toolchain install "${toolchain}" --profile minimal --no-self-update >/dev/null
    rustup target add --toolchain "${toolchain}" aarch64-apple-ios aarch64-apple-ios-sim >/dev/null
    # collect-notices.py reads the standard library's Cargo.lock and licences.
    rustup component add --toolchain "${toolchain}" rust-src >/dev/null
    rustc +"${toolchain}" --version | grep -Fq "rustc ${toolchain} " \
        || die "rustc +${toolchain} reports $(rustc +"${toolchain}" --version)"
}

build_easytier() {
    local stage="$1"
    local work="${BUILD_DIR}/work/easytier"
    local target_dir="${BUILD_DIR}/cargo-target"
    if [[ "${FORCE:-0}" == "1" ]]; then
        rm -rf "${work}" "${target_dir}"
    fi
    mkdir -p "${work}" "${target_dir}"
    work="$(cd "${work}" && pwd -P)"
    target_dir="$(cd "${target_dir}" && pwd -P)"
    local crate="${work}/heeler-easytier"
    sync_easytier_crate "${crate}"
    install_rust_toolchain "${crate}"

    local toolchain="${EASYTIER_RUST_TOOLCHAIN}"
    local objcopy
    objcopy="$(rustc +"${toolchain}" --print sysroot)/lib/rustlib/$(rustc +"${toolchain}" -vV | sed -n 's/^host: //p')/bin/rust-objcopy"
    [[ -x "${objcopy}" ]] || die "rust-objcopy not found at ${objcopy}"
    local cargo_home
    cargo_home="$(cd "${CARGO_HOME:-${HOME}/.cargo}" && pwd -P)"
    local scratch="${work}/scratch"
    rm -rf "${scratch}"
    mkdir -p "${scratch}"

    (
        cd "${crate}"
        # Rust's iOS targets reference ___chkstk_darwin from compiler-rt;
        # point any target link step (build-script probes) at Xcode's archive.
        local clang_bin toolchain_usr clang_rt_dir sim_sdk
        clang_bin="$(xcrun --find clang)"
        toolchain_usr="${clang_bin%/bin/clang}"
        clang_rt_dir="$(cd "${toolchain_usr}/lib/clang" && cd "$(ls | sort -V | tail -1)/lib/darwin" && pwd)"
        sim_sdk="$(xcrun --sdk iphonesimulator --show-sdk-path)"

        export CARGO_TARGET_DIR="${target_dir}"
        export IPHONEOS_DEPLOYMENT_TARGET="${DEPLOYMENT_TARGET}"
        export CARGO_INCREMENTAL=0
        # EasyTier embeds `git describe` through git-version and falls back to
        # its package version without a repository; vendor/ has no .git, so
        # stop git from finding this repository and baking its commit in.
        export GIT_CEILING_DIRECTORIES="${crate}/vendor"
        export ZERO_AR_DATE=1
        # No build-machine path reaches the objects: the crate (with
        # vendor/), Cargo's registry and git checkouts, and generated sources
        # under the target directory. The last matching prefix wins.
        local remap="--remap-path-prefix=${HOME}=/home"
        remap+=" --remap-path-prefix=${cargo_home}=/cargo"
        remap+=" --remap-path-prefix=${crate}=/heeler-easytier"
        remap+=" --remap-path-prefix=${target_dir}=/target"

        log "cargo build CEasyTier (aarch64-apple-ios)"
        RUSTFLAGS="${remap} -C link-arg=-L${clang_rt_dir} -C link-arg=-lclang_rt.ios" \
            cargo +"${toolchain}" build --locked --release --lib --target aarch64-apple-ios
        log "cargo build CEasyTier (aarch64-apple-ios-sim)"
        BINDGEN_EXTRA_CLANG_ARGS="--target=arm64-apple-ios${DEPLOYMENT_TARGET}-simulator -isysroot ${sim_sdk}" \
            RUSTFLAGS="${remap} -C link-arg=-L${clang_rt_dir} -C link-arg=-lclang_rt.iossim" \
            cargo +"${toolchain}" build --locked --release --lib --target aarch64-apple-ios-sim
    )

    # LC_BUILD_VERSION platform: 2 = iOS, 7 = iOS Simulator.
    package_easytier_library "${crate}" "${objcopy}" "${target_dir}" "${scratch}" \
        aarch64-apple-ios 2 "${scratch}/device"
    package_easytier_library "${crate}" "${objcopy}" "${target_dir}" "${scratch}" \
        aarch64-apple-ios-sim 7 "${scratch}/simulator"
    easytier_link_probe "${crate}" "${scratch}" "${scratch}/device/${EASYTIER_LIBRARY}" iphoneos \
        "arm64-apple-ios${DEPLOYMENT_TARGET}"
    easytier_link_probe "${crate}" "${scratch}" "${scratch}/simulator/${EASYTIER_LIBRARY}" iphonesimulator \
        "arm64-apple-ios${DEPLOYMENT_TARGET}-simulator"

    local frameworks="${scratch}/Frameworks" name platform
    for name in device simulator; do
        platform=iPhoneOS
        [[ "${name}" == simulator ]] && platform=iPhoneSimulator
        create_static_framework CEasyTier "${scratch}/${name}/${EASYTIER_LIBRARY}" "${platform}" \
            "${frameworks}/${name}/CEasyTier.framework" "${crate}/include/heeler_easytier.h"
    done
    create_xcframework CEasyTier "${frameworks}/device/CEasyTier.framework" \
        "${frameworks}/simulator/CEasyTier.framework" "${stage}/CEasyTier.xcframework"

    easytier_notices "${stage}" "${crate}" "${target_dir}" "${scratch}"
}

# Turns rustc's archive into the shipped one:
#  1. rustc embeds each crate's LLVM bitcode in __LLVM,__bitcode (two thirds
#     of the archive). Xcode's linker ignores it and its LLVM cannot read
#     rustc's newer bitcode, so it goes.
#  2. Rust's prebuilt standard-library objects carry older deployment targets
#     (LC_VERSION_MIN_IPHONEOS 10.0 on device, LC_BUILD_VERSION minos 14.0 on
#     the Simulator); restamp-macho.py sets them to the deployment target.
#  3. Debug sections are stripped; symbols stay for the app's link.
package_easytier_library() {
    local crate="$1" objcopy="$2" target_dir="$3" scratch="$4" triple="$5" platform="$6" destination="$7"
    local stage="${scratch}/${triple}"
    local members="${stage}/members"
    rm -rf "${stage}"
    mkdir -p "${members}" "${destination}"
    (
        export ZERO_AR_DATE=1
        "${objcopy}" --remove-section=__LLVM,__bitcode --remove-section=__LLVM,__cmdline \
            "${target_dir}/${triple}/release/${EASYTIER_LIBRARY}" "${stage}/bitcode-free.a"
        (cd "${members}" && ar t "${stage}/bitcode-free.a" | grep -v '^__\.SYMDEF' > "${stage}/order.txt")
        [[ -z "$(sort "${stage}/order.txt" | uniq -d)" ]] || die "${triple} archive has duplicate member names"
        (cd "${members}" && ar x "${stage}/bitcode-free.a")
        (cd "${members}" && xargs "${crate}/restamp-macho.py" "${platform}" "${DEPLOYMENT_TARGET}" < "${stage}/order.txt")
        sed "s|^|${members}/|" "${stage}/order.txt" > "${stage}/filelist.txt"
        xcrun libtool -static -no_warning_for_no_symbols \
            -filelist "${stage}/filelist.txt" -o "${stage}/restamped.a"
        xcrun strip -S -o "${destination}/${EASYTIER_LIBRARY}" "${stage}/restamped.a"
    )

    local load_commands targets platforms legacy
    load_commands="$(xcrun otool -l "${destination}/${EASYTIER_LIBRARY}")"
    targets="$(awk '$1 == "minos" { print $2 }' <<<"${load_commands}" | sort -u | paste -sd ' ' -)"
    platforms="$(awk '$1 == "platform" { print $2 }' <<<"${load_commands}" | sort -u | paste -sd ' ' -)"
    [[ "${targets}" == "${DEPLOYMENT_TARGET}" && "${platforms}" == "${platform}" ]] \
        || die "${triple} has minos '${targets}' and platforms '${platforms}'"
    legacy="$(awk '/cmd LC_VERSION_MIN/ { getline; getline; print $2 }' <<<"${load_commands}" | sort -u)"
    [[ -z "${legacy}" || "${legacy}" == "${DEPLOYMENT_TARGET}" ]] \
        || die "${triple} has LC_VERSION_MIN versions '${legacy}'"
    if strings -a "${destination}/${EASYTIER_LIBRARY}" | grep -Fq -e "${HOME}" -e "${BUILD_DIR}"; then
        die "${triple} contains a build-machine path"
    fi
}

# Links a dead-stripped executable against a library: proves it resolves
# with only the system frameworks an app links anyway (CoreFoundation for the
# config-server heartbeat's local time, Security for wss:// certificate
# verification), and reports its app cost.
easytier_link_probe() {
    local crate="$1" scratch="$2" library="$3" sdk="$4" target="$5"
    local probe="${scratch}/probe-${sdk}"
    cat > "${scratch}/probe.c" <<'EOF'
#include "heeler_easytier.h"
int main(int argc, char **argv) {
    char err[64];
    if (argc > 5) {
        heeler_et_stop();
        heeler_et_status_json(err, sizeof err);
        return heeler_et_tcp_connect_fd(argv[1], 22, 1, err, sizeof err);
    }
    if (argc > 3) {
        heeler_et_web_stop();
        return heeler_et_web_start(argv[1], argv[2], argv[3], 1, err, sizeof err);
    }
    return heeler_et_start(argv[0], 1000, err, sizeof err);
}
EOF
    xcrun --sdk "${sdk}" clang -target "${target}" -O2 -I "${crate}/include" \
        "${scratch}/probe.c" "${library}" -framework CoreFoundation -framework Security \
        -Wl,-dead_strip -o "${probe}"
    xcrun strip -o "${probe}.stripped" "${probe}"
    echo "    ${sdk}: a linked probe is $(stat -f %z "${probe}.stripped") bytes stripped"
}

easytier_notices() {
    local stage="$1" crate="$2" target_dir="$3" scratch="$4"
    local source="${crate}/vendor/easytier"
    local notices="${stage}/Notices"
    mkdir -p "${notices}"

    # EasyTier is LGPL-3.0, which supplements the GPL-3.0: ship both texts
    # with the corresponding-source and installation information LGPL-3.0
    # section 4 asks for.
    cmp -s "${source}/LICENSE" "${source}/easytier/LICENSE" \
        || die "EasyTier's workspace and crate LICENSE files differ"
    local patch_lines="" entry
    for entry in ${EASYTIER_PATCHES}; do
        patch_lines+="  - patches/easytier/${entry%%:*}"$'\n'
        patch_lines+="    (SHA-256 ${entry##*:})"$'\n'
    done
    local notice="${notices}/EasyTier-LGPL-3.0.txt"
    {
        cat <<EOF
EasyTier ${EASYTIER_VERSION} (commit ${EASYTIER_COMMIT}) is linked into CEasyTier,
statically, together with heeler-easytier, Heeler's LGPL-3.0-or-later wrapper
that exposes it to the app. EasyTier is licensed under the GNU Lesser General
Public License version 3; the full text follows, and then the GNU General
Public License version 3 it supplements.

Corresponding source. CEasyTier is built and published by
https://github.com/Ylarod/heeler-overlay-natives; every published build is a
release tag there (v<version>, the version Heeler's Package.resolved records
for the heeler-overlay-natives package), and the CEasyTier.xcframework.zip
attached to that release is the build you received. The tree at that tag,
with its git submodules, holds everything needed to rebuild CEasyTier from
source:
- EasyTier: ${EASYTIER_REPO} at commit ${EASYTIER_COMMIT}
  (git submodule upstream/EasyTier, pinned in sources.lock), with Heeler's
  modifications applied as patches:
${patch_lines}- heeler-easytier, its Cargo.lock, and the build scripts:
  native/easytier, scripts/build.sh, and scripts/lib/easytier.sh.

Installation information (LGPL-3.0 section 4(e)). Heeler
(https://github.com/ZingerLittleBee/Heeler) is open source under the Apache
License 2.0, and nothing more than public source is needed to run a modified
EasyTier in it: change the EasyTier source (move the upstream/EasyTier
submodule, or add a patch under patches/easytier and list it in
sources.lock), run scripts/build.sh, open Heeler in Xcode with your checkout
of heeler-overlay-natives added as a local package (it overrides the
released package of the same name; README.md explains), build the app, and
install it on your own iPhone or iPad with any Apple ID (a free Apple
Developer account, Xcode's "Personal Team", is enough). No key, signature,
or permission from the Heeler maintainers is required.

================================================================================
GNU LESSER GENERAL PUBLIC LICENSE, Version 3 (EasyTier LICENSE)
================================================================================

EOF
        cat "${source}/LICENSE"
        cat <<'EOF'

================================================================================
GNU GENERAL PUBLIC LICENSE, Version 3
================================================================================

EOF
        cat "${ROOT_DIR}/${GPL3_TEXT_FILE}"
    } > "${notice}"

    cp "${crate}/NOTICE.txt" "${notices}/heeler-easytier-LGPL-3.0-or-later.txt"

    # Every third-party crate linked into CEasyTier, including the Rust
    # standard library, with its licence files.
    (
        cd "${crate}"
        "${crate}/collect-notices.py" --crate-dir "${crate}" \
            --output "${notices}/EasyTier-Rust-crates.txt" \
            --rust-licences "${ROOT_DIR}/${RUST_LICENSE_DIR}" \
            --target-dir "${target_dir}" \
            --archive "aarch64-apple-ios=${scratch}/device/${EASYTIER_LIBRARY}" \
            --archive "aarch64-apple-ios-sim=${scratch}/simulator/${EASYTIER_LIBRARY}"
    )
    chmod 644 "${notices}"/*
    grep -q "GNU LESSER GENERAL PUBLIC LICENSE" "${notice}"
    grep -q "TERMS AND CONDITIONS" "${notice}"
    grep -q "Installation information" "${notice}"
    # An MPL-2.0 crate, if one is linked, ships with its licence text.
    if grep -q '^[^ ]* | [^ ]* | [^|]*MPL' "${notices}/EasyTier-Rust-crates.txt"; then
        grep -q "Mozilla Public License Version 2.0" "${notices}/EasyTier-Rust-crates.txt"
    fi
    grep -q "^compiler_builtins | " "${notices}/EasyTier-Rust-crates.txt"

    local crate_count
    crate_count="$(awk -F' [|] ' 'NF == 4 && $1 != "Crate"' "${notices}/EasyTier-Rust-crates.txt" | wc -l | tr -d ' ')"
    cat > "${stage}/provenance.md" <<EOF
## CEasyTier

- EasyTier: ${EASYTIER_VERSION}, commit ${EASYTIER_COMMIT} (submodule upstream/EasyTier, ${EASYTIER_REPO})
- EasyTier pin: a development commit ahead of the ${EASYTIER_VERSION} release; to be replaced by the v${EASYTIER_VERSION} tag once it is released
- EasyTier patches: ${EASYTIER_PATCHES}
- EasyTier licence: LGPL-3.0, linked statically through heeler-easytier (LGPL-3.0-or-later); LGPL-3.0 and GPL-3.0 texts in Notices
- Rust toolchain: $(rustc +"${EASYTIER_RUST_TOOLCHAIN}" --version)
- heeler-easytier: native/easytier, Cargo.lock sha256 $(sha256_of "${ROOT_DIR}/${EASYTIER_CRATE_SOURCE}/Cargo.lock") enforced (--locked); features easytier smoltcp, aes-gcm, dhcp-ipv4, web-client, websocket; release opt-level z, thin LTO, codegen-units 1, panic unwind
- heeler-easytier post-processing: __LLVM,__bitcode removed, Rust standard-library objects restamped to minos ${DEPLOYMENT_TARGET}, debug sections stripped
- Rust crates linked: ${crate_count}
EOF
}
