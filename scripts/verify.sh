#!/bin/bash
# Verifies an Artifacts directory (default build/Artifacts) without building:
# checksums (and that nothing unlisted exists), the two arm64 slices of each
# XCFramework, minos and platform of every object, every exported entry
# point and the C signatures the Swift layer relies on, headers and module
# maps against this checkout, notices, and PROVENANCE.md against
# sources.lock, the patches, and the committed licence texts.
#
#   scripts/verify.sh [artifacts-dir]
set -euo pipefail

# shellcheck source=lib/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib/common.sh"

ARTIFACTS="$(cd "${1:-${BUILD_DIR}/Artifacts}" && pwd -P)"
EXPECTED_MAJOR="${DEPLOYMENT_TARGET%%.*}"

for manifest in Package.swift Tests/LinkProbe/Package.swift; do
    grep -Fq ".iOS(.v${EXPECTED_MAJOR})" "${ROOT_DIR}/${manifest}" \
        || die "${manifest} does not target iOS ${EXPECTED_MAJOR}"
done

cd "${ARTIFACTS}"
[[ -f SHA256SUMS ]] || die "${ARTIFACTS}/SHA256SUMS is missing"
checksum_output="$(shasum -a 256 -c SHA256SUMS 2>&1)" || {
    echo "${checksum_output}" >&2
    die "checksum mismatch in ${ARTIFACTS}"
}
listed="$(awk '{ print $2 }' SHA256SUMS | LC_ALL=C sort)"
present="$(find ./* -type f ! -name SHA256SUMS | sed 's|^\./||' | LC_ALL=C sort)"
[[ "${listed}" == "${present}" ]] || {
    comm -13 <(echo "${listed}") <(echo "${present}") >&2
    die "${ARTIFACTS} contains files not listed in SHA256SUMS"
}

for framework in "${FRAMEWORKS[@]}"; do
    xcframework="${ARTIFACTS}/${framework}.xcframework"
    [[ -f "${xcframework}/Info.plist" ]] || die "missing ${framework}.xcframework/Info.plist"
    info_json="$(plutil -convert json -o - "${xcframework}/Info.plist")"
    grep -q '"SupportedArchitectures":\["arm64"\]' <<<"${info_json}" || die "${framework} has no arm64 slice"
    grep -q '"SupportedPlatformVariant":"simulator"' <<<"${info_json}" || die "${framework} has no Simulator slice"
    slices="$(find "${xcframework}" -mindepth 1 -maxdepth 1 -type d ! -name _CodeSignature \
        -exec basename {} \; | LC_ALL=C sort | paste -sd ' ' -)"
    [[ "${slices}" == "ios-arm64 ios-arm64-simulator" ]] \
        || die "${framework} slices are '${slices}', expected ios-arm64 and ios-arm64-simulator"

    for slice in ios-arm64 ios-arm64-simulator; do
        bundle="${xcframework}/${slice}/${framework}.framework"
        for required in "${bundle}/${framework}" "${bundle}/Info.plist" \
            "${bundle}/Headers/${framework}.h" "${bundle}/Modules/module.modulemap"; do
            [[ -f "${required}" ]] || die "missing ${required#"${ARTIFACTS}/"}"
        done
        cmp -s "${ROOT_DIR}/include/${framework}/${framework}.h" "${bundle}/Headers/${framework}.h" \
            || die "${framework} ${slice} does not carry include/${framework}/${framework}.h"
        cmp -s "${ROOT_DIR}/include/${framework}/module.modulemap" "${bundle}/Modules/module.modulemap" \
            || die "${framework} ${slice} does not carry include/${framework}/module.modulemap"
        grep -Fq "framework module ${framework} " "${bundle}/Modules/module.modulemap" \
            || die "${bundle} module map does not declare module ${framework}"
        minimum_version="$(plutil -extract MinimumOSVersion raw -o - "${bundle}/Info.plist")"
        [[ "${minimum_version}" == "${DEPLOYMENT_TARGET}" ]] \
            || die "${framework} ${slice} Info.plist targets iOS ${minimum_version}, expected ${DEPLOYMENT_TARGET}"

        if [[ "${slice}" == "ios-arm64" ]]; then
            expected_platform=2   # PLATFORM_IOS
        else
            expected_platform=7   # PLATFORM_IOSSIMULATOR
        fi
        load_commands="$(xcrun otool -l "${bundle}/${framework}")"
        binary_targets="$(awk '$1 == "minos" { print $2 }' <<<"${load_commands}" | LC_ALL=C sort -u)"
        [[ "${binary_targets}" == "${DEPLOYMENT_TARGET}" ]] \
            || die "${framework} ${slice} contains deployment targets '${binary_targets}', expected ${DEPLOYMENT_TARGET}"
        legacy="$(awk '/cmd LC_VERSION_MIN/ { getline; getline; print $2 }' <<<"${load_commands}" | LC_ALL=C sort -u)"
        [[ -z "${legacy}" || "${legacy}" == "${DEPLOYMENT_TARGET}" ]] \
            || die "${framework} ${slice} contains LC_VERSION_MIN versions '${legacy}'"
        platforms="$(awk '$1 == "platform" { print $2 }' <<<"${load_commands}" | LC_ALL=C sort -u)"
        [[ "${platforms}" == "${expected_platform}" ]] \
            || die "${framework} ${slice} contains platforms '${platforms}', expected ${expected_platform}"
        architectures="$(xcrun lipo -archs "${bundle}/${framework}")"
        [[ "${architectures}" == "arm64" ]] || die "${framework} ${slice} has architectures '${architectures}'"
    done
done

# The entry points the Swift layer (Heeler's HeelerOverlay) relies on.
TAILSCALE_SYMBOLS="tailscale_new tailscale_start tailscale_close tailscale_dial
    tailscale_status_json tailscale_errmsg tailscale_set_dir tailscale_set_logfd
    heeler_tailscale_disable_log_upload heeler_tailscale_log_upload_state
    heeler_tailscale_logout"
ZEROTIER_SYMBOLS="zts_init_from_memory zts_node_start zts_node_get_id_pair zts_net_join
    zts_net_leave zts_bsd_socket zts_bsd_connect zts_bsd_poll zts_bsd_close
    zts_id_new zts_id_pair_is_valid zts_node_get_id zts_moon_orbit zts_moon_deorbit
    zts_bsd_setsockopt heeler_zt_peers heeler_zt_planet_inspect heeler_zt_add_moon
    heeler_zt_bind_network heeler_zt_network_reaches"
EASYTIER_SYMBOLS="heeler_et_start heeler_et_stop heeler_et_stop_all heeler_et_tcp_connect_fd
    heeler_et_status_json heeler_et_web_start heeler_et_web_stop"
check_exports() {
    local framework="$1" slice="$2" symbols="$3" exported symbol
    exported="$(xcrun nm -gU "${ARTIFACTS}/${framework}.xcframework/${slice}/${framework}.framework/${framework}" 2>/dev/null || true)"
    for symbol in ${symbols}; do
        grep -Eq "[[:space:]]_${symbol}$" <<<"${exported}" || die "${framework} ${slice} does not export ${symbol}"
    done
}

SIGNATURES="$(mktemp -d "${TMPDIR:-/tmp}/heeler-natives-verify.XXXXXX")"
trap 'rm -rf "${SIGNATURES}"' EXIT
cat > "${SIGNATURES}/signatures.c" <<'EOF'
#include <stddef.h>
#include <stdint.h>
#include <CTailscale/CTailscale.h>
#include <CZeroTier/CZeroTier.h>
#include <CEasyTier/CEasyTier.h>

/* Each assignment fails to compile if a declaration changes its C type. */
int (*const et_start)(const char *, const char *, uint32_t, char *, size_t) = heeler_et_start;
void (*const et_stop)(const char *) = heeler_et_stop;
void (*const et_stop_all)(void) = heeler_et_stop_all;
int (*const et_web_start)(const char *, const char *, const char *, const char *, int, char *, size_t) = heeler_et_web_start;
void (*const et_web_stop)(const char *) = heeler_et_web_stop;
int (*const et_connect)(const char *, const char *, const char *, uint16_t, uint32_t, char *, size_t) = heeler_et_tcp_connect_fd;
int (*const et_status)(const char *, char *, size_t) = heeler_et_status_json;
_Static_assert(HEELER_ET_ABI_VERSION == 2, "CEasyTier is not the multi-instance ABI");
int (*const zt_peers)(heeler_zt_peer *, unsigned int) = heeler_zt_peers;
int (*const zt_inspect)(const void *, unsigned int, heeler_zt_planet_info *) = heeler_zt_planet_inspect;
int (*const zt_add_moon)(const void *, unsigned int, uint64_t) = heeler_zt_add_moon;
int (*const zt_bind_network)(int, uint64_t, int) = heeler_zt_bind_network;
int (*const zt_network_reaches)(uint64_t, int, const void *) = heeler_zt_network_reaches;
void (*const ts_disable_log_upload)(void) = heeler_tailscale_disable_log_upload;
int (*const ts_log_upload_state)(void) = heeler_tailscale_log_upload_state;
int (*const ts_logout)(int, int) = heeler_tailscale_logout;
int (*const ts_status)(tailscale, char **) = tailscale_status_json;
int (*const ts_dial)(tailscale, const char *, const char *, tailscale_conn *) = tailscale_dial;
EOF

for slice in ios-arm64 ios-arm64-simulator; do
    check_exports CTailscale "${slice}" "${TAILSCALE_SYMBOLS}"
    check_exports CZeroTier "${slice}" "${ZEROTIER_SYMBOLS}"
    check_exports CEasyTier "${slice}" "${EASYTIER_SYMBOLS}"
    cmp -s "${ROOT_DIR}/native/zerotier/heeler_zerotier.h" \
        "${ARTIFACTS}/CZeroTier.xcframework/${slice}/CZeroTier.framework/Headers/heeler_zerotier.h" \
        || die "CZeroTier ${slice} does not carry native/zerotier/heeler_zerotier.h"
    cmp -s "${ROOT_DIR}/native/easytier/include/heeler_easytier.h" \
        "${ARTIFACTS}/CEasyTier.xcframework/${slice}/CEasyTier.framework/Headers/heeler_easytier.h" \
        || die "CEasyTier ${slice} does not carry native/easytier/include/heeler_easytier.h"
    if [[ "${slice}" == "ios-arm64" ]]; then
        sdk=iphoneos target="arm64-apple-ios${DEPLOYMENT_TARGET}"
    else
        sdk=iphonesimulator target="arm64-apple-ios${DEPLOYMENT_TARGET}-simulator"
    fi
    framework_flags=()
    for framework in "${FRAMEWORKS[@]}"; do
        framework_flags+=(-F "${ARTIFACTS}/${framework}.xcframework/${slice}")
    done
    xcrun --sdk "${sdk}" clang -target "${target}" -fsyntax-only -Werror \
        -Werror=incompatible-function-pointer-types "${framework_flags[@]}" \
        "${SIGNATURES}/signatures.c" || die "the C signatures in the ${slice} headers changed"
done

for notice in \
    libtailscale-BSD-3-Clause.txt Tailscale-BSD-3-Clause.txt Go-BSD-3-Clause.txt \
    Tailscale-Go-modules.txt libzt-BUSL-1.1-Apache-2.0.txt \
    ZeroTierOne-BUSL-1.1-Apache-2.0.txt ZeroTierOne-LZ4-BSD-2-Clause.txt \
    lwIP-BSD-3-Clause.txt lwIP-contrib-BSD-3-Clause.txt miniupnpc-BSD-3-Clause.txt \
    libnatpmp-BSD-3-Clause.txt nlohmann-json-MIT.txt ZeroTier-Heeler-modifications.txt \
    EasyTier-LGPL-3.0.txt EasyTier-Rust-crates.txt heeler-easytier-LGPL-3.0-or-later.txt; do
    [[ -s "Notices/${notice}" ]] || die "missing notice Notices/${notice}"
done
notice_count="$(find Notices -type f | wc -l | tr -d ' ')"
[[ "${notice_count}" == "16" ]] || die "Notices holds ${notice_count} files, expected 16"
grep -q "Change License:       Apache License version 2.0" Notices/libzt-BUSL-1.1-Apache-2.0.txt
grep -q "Change License:       Apache License version 2.0" Notices/ZeroTierOne-BUSL-1.1-Apache-2.0.txt

# Committed build inputs must match sources.lock.
verify_licence_texts
verify_file go.mod "${ROOT_DIR}/native/tailscale/go.mod" "${LIBTAILSCALE_GO_MOD_SHA256}"
verify_file go.sum "${ROOT_DIR}/native/tailscale/go.sum" "${LIBTAILSCALE_GO_SUM_SHA256}"

provenance="PROVENANCE.md"
has() {
    grep -Fq -- "$1" "${provenance}" || die "PROVENANCE.md does not record: $1"
}
has "libtailscale: commit ${LIBTAILSCALE_COMMIT}"
for glue in native/tailscale/heeler_tailscale.go native/zerotier/heeler_zerotier.cpp native/zerotier/heeler_zerotier.h; do
    has "${glue} (sha256 $(sha256_of "${ROOT_DIR}/${glue}"))"
done
has "tailscale.com: ${TAILSCALE_MODULE_VERSION}"
has "native/tailscale/go.mod sha256 ${LIBTAILSCALE_GO_MOD_SHA256}"
has "native/tailscale/go.sum sha256 ${LIBTAILSCALE_GO_SUM_SHA256}"
has "Go toolchain: ${GO_VERSION}, darwin-arm64 archive sha256 ${GO_SHA256}"
has "Go experiment: none (GOEXPERIMENT unset)"
has "libzt: commit ${LIBZT_COMMIT}"
has "commit ${ZEROTIERONE_COMMIT}"
has "lwIP: commit ${LWIP_COMMIT}"
has "lwIP contrib: commit ${LWIP_CONTRIB_COMMIT}"
grep -Fxq -- "- libzt patches: ${ZEROTIER_PATCHES}" "${provenance}" || die "PROVENANCE.md does not record the libzt patches"
for entry in ${ZEROTIER_PATCHES}; do
    [[ "$(sha256_of "${ROOT_DIR}/patches/libzt/${entry%%:*}")" == "${entry##*:}" ]] \
        || die "patches/libzt/${entry%%:*} does not match sources.lock"
    grep -Fq "${entry%%:*} (SHA-256 ${entry##*:})" Notices/ZeroTier-Heeler-modifications.txt \
        || die "ZeroTier-Heeler-modifications.txt does not list ${entry%%:*}"
done
has "EasyTier: ${EASYTIER_VERSION}, commit ${EASYTIER_COMMIT}"
has "Rust toolchain: rustc ${EASYTIER_RUST_TOOLCHAIN} "
has "protoc: ${PROTOC_VERSION}, osx-aarch_64 archive sha256 ${PROTOC_SHA256}"
has "Cargo.lock sha256 $(sha256_of "${ROOT_DIR}/native/easytier/Cargo.lock")"
grep -Fxq -- "- EasyTier patches: ${EASYTIER_PATCHES}" "${provenance}" || die "PROVENANCE.md does not record the EasyTier patches"
for entry in ${EASYTIER_PATCHES}; do
    [[ "$(sha256_of "${ROOT_DIR}/patches/easytier/${entry%%:*}")" == "${entry##*:}" ]] \
        || die "patches/easytier/${entry%%:*} does not match sources.lock"
    grep -Fq "${entry##*:}" Notices/EasyTier-LGPL-3.0.txt || die "EasyTier-LGPL-3.0.txt does not list ${entry%%:*}"
done
grep -q "Installation information" Notices/EasyTier-LGPL-3.0.txt
grep -Fq "commit ${EASYTIER_COMMIT}" Notices/EasyTier-LGPL-3.0.txt
grep -q "GNU LESSER GENERAL PUBLIC LICENSE" Notices/EasyTier-LGPL-3.0.txt
grep -q "GNU GENERAL PUBLIC LICENSE" Notices/EasyTier-LGPL-3.0.txt
grep -q "^compiler_builtins | " Notices/EasyTier-Rust-crates.txt
grep -q "^smoltcp | " Notices/EasyTier-Rust-crates.txt
cmp -s "${ROOT_DIR}/native/easytier/NOTICE.txt" Notices/heeler-easytier-LGPL-3.0-or-later.txt \
    || die "heeler-easytier-LGPL-3.0-or-later.txt is not native/easytier/NOTICE.txt"
has "Deployment target: iOS ${DEPLOYMENT_TARGET}"
has "ZeroTier licence: BUSL-1.1, converted to Apache-2.0"

signature_line="$(grep '^- Signature: ' "${provenance}")" || die "PROVENANCE.md does not record the signature status"
for framework in "${FRAMEWORKS[@]}"; do
    if [[ "${signature_line}" == "- Signature: signed"* ]]; then
        codesign --verify --strict "${ARTIFACTS}/${framework}.xcframework" \
            || die "${framework} XCFramework signature is invalid"
    else
        [[ ! -e "${ARTIFACTS}/${framework}.xcframework/_CodeSignature" ]] \
            || die "${framework} is signed but PROVENANCE.md records it as unsigned"
    fi
done

echo "Verified ${ARTIFACTS}: checksums, slices, deployment targets, exports, C signatures, headers, notices, and provenance."
