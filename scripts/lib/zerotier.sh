# shellcheck shell=bash
# CZeroTier: libzt (ZeroTierOne + lwIP) as a static library. Sourced by
# scripts/build.sh.

ZEROTIER_CPP="native/zerotier/heeler_zerotier.cpp"
ZEROTIER_H="native/zerotier/heeler_zerotier.h"

zerotier_fingerprint() {
    echo "library: zerotier"
    echo "deployment-target: ${DEPLOYMENT_TARGET}"
    echo "libzt: ${LIBZT_COMMIT}"
    echo "ZeroTierOne: ${ZEROTIERONE_COMMIT}"
    echo "lwip: ${LWIP_COMMIT}"
    echo "lwip-contrib: ${LWIP_CONTRIB_COMMIT}"
    echo "patches: ${ZEROTIER_PATCHES}"
    echo "cmake: $(cmake --version | sed -n '1p')"
    apple_toolchain_fingerprint
    hash_files scripts/build.sh scripts/lib/common.sh scripts/lib/zerotier.sh \
        "${ZEROTIER_CPP}" "${ZEROTIER_H}" \
        include/CZeroTier/CZeroTier.h include/CZeroTier/module.modulemap
    hash_tree patches/libzt
}

# The libzt tree as GitHub's commit archives lay it out (libzt with each
# submodule's archive in ext/), patched.
prepare_libzt_source() {
    local source="$1"
    local libzt="${ROOT_DIR}/upstream/libzt"
    export_tree "${libzt}" "${LIBZT_COMMIT}" "${source}"
    export_tree "${libzt}/ext/ZeroTierOne" "${ZEROTIERONE_COMMIT}" "${source}/ext/ZeroTierOne"
    export_tree "${libzt}/ext/lwip" "${LWIP_COMMIT}" "${source}/ext/lwip"
    export_tree "${libzt}/ext/lwip-contrib" "${LWIP_CONTRIB_COMMIT}" "${source}/ext/lwip-contrib"
    apply_patches "${ROOT_DIR}/patches/libzt" "${ZEROTIER_PATCHES}" "${source}" \
        "libzt ${LIBZT_COMMIT} (ZeroTierOne ${ZEROTIERONE_COMMIT})"
}

build_zerotier() {
    local stage="$1"
    local work="${BUILD_DIR}/work/zerotier"
    rm -rf "${work}"
    mkdir -p "${work}/src"
    work="$(cd "${work}" && pwd -P)"
    local source="${work}/src/libzt"
    prepare_libzt_source "${source}"

    # heeler_zerotier.cpp joins libzt's src/, which its CMake globs into the
    # zt-static library: heeler_zt_peers, heeler_zt_planet_inspect, and
    # heeler_zt_add_moon (through Node::addLocalMoon from the patches).
    local file
    for file in heeler_zerotier.cpp heeler_zerotier.h; do
        [[ ! -e "${source}/src/${file}" ]] || die "libzt already contains src/${file}"
    done
    cp "${ROOT_DIR}/${ZEROTIER_CPP}" "${ROOT_DIR}/${ZEROTIER_H}" "${source}/src/"

    (
        export ZERO_AR_DATE=1
        local prefix_map="-ffile-prefix-map=${work}=${REMAPPED_WORK_DIR}"
        local name sdk build_dir library
        for name in device simulator; do
            [[ "${name}" == device ]] && sdk=iphoneos || sdk=iphonesimulator
            build_dir="${work}/libzt-${name}"
            log "cmake CZeroTier (${name})"
            env -u CPPFLAGS -u CFLAGS -u CXXFLAGS -u LDFLAGS \
                cmake -S "${source}" -B "${build_dir}" -G "Unix Makefiles" \
                    -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
                    -DCMAKE_SYSTEM_NAME=iOS \
                    -DCMAKE_OSX_SYSROOT="${sdk}" \
                    -DCMAKE_OSX_ARCHITECTURES=arm64 \
                    -DCMAKE_OSX_DEPLOYMENT_TARGET="${DEPLOYMENT_TARGET}" \
                    -DCMAKE_BUILD_TYPE=Release \
                    -DCMAKE_C_FLAGS="${prefix_map}" \
                    -DCMAKE_CXX_FLAGS="${prefix_map}" \
                    -DBUILD_IOS_FRAMEWORK=True \
                    -DIOS_ARM64=True >/dev/null
            cmake --build "${build_dir}" --target zt-static --parallel "${JOBS}" >/dev/null
            library="$(find "${build_dir}" -name 'libzt.a' -type f | head -n 1)"
            [[ -n "${library}" ]] || die "libzt.a was not produced for ${name}"
            # Drop local symbols and debug info; the zts_* API stays exported.
            xcrun strip -S -x "${library}" -o "${build_dir}/libzt-stripped.a"
        done

        local frameworks="${work}/Frameworks"
        for name in device simulator; do
            local platform=iPhoneOS
            [[ "${name}" == simulator ]] && platform=iPhoneSimulator
            create_static_framework CZeroTier "${work}/libzt-${name}/libzt-stripped.a" "${platform}" \
                "${frameworks}/${name}/CZeroTier.framework" "${source}/include/ZeroTierSockets.h" \
                "${ROOT_DIR}/${ZEROTIER_H}"
        done
        create_xcframework CZeroTier "${frameworks}/device/CZeroTier.framework" \
            "${frameworks}/simulator/CZeroTier.framework" "${stage}/CZeroTier.xcframework"
    )

    local zerotierone="${source}/ext/ZeroTierOne"
    local notices="${stage}/Notices"
    mkdir -p "${notices}"
    cp "${source}/LICENSE.txt" "${notices}/libzt-BUSL-1.1-Apache-2.0.txt"
    cp "${zerotierone}/LICENSE.txt" "${notices}/ZeroTierOne-BUSL-1.1-Apache-2.0.txt"
    # Apache-2.0 section 4(b): each changed file carries its own notice; this
    # lists the changes.
    {
        cat <<EOF
Heeler modifies libzt (commit ${LIBZT_COMMIT}) and its ZeroTierOne
submodule (commit ${ZEROTIERONE_COMMIT}) before building CZeroTier, with the
patches below, applied in order. They are kept in patches/libzt of
https://github.com/Ylarod/heeler-overlay-natives; the release tag the
CZeroTier binary was published under holds the exact patches, the pinned
upstream submodules, and the build scripts. Each file a patch changes carries
a "Modified by Heeler contributors" notice. libzt and ZeroTierOne are used
under the Apache License 2.0, to which their Business Source License
converted on its Change Date. Heeler also adds src/heeler_zerotier.cpp and
src/heeler_zerotier.h (native/zerotier in that repository) to libzt.

EOF
        local entry patch_file
        for entry in ${ZEROTIER_PATCHES}; do
            patch_file="${ROOT_DIR}/patches/libzt/${entry%%:*}"
            echo "${entry%%:*} (SHA-256 ${entry##*:})"
            sed -n '1s/^/  /p' "${patch_file}"
            echo "  Files changed:"
            sed -n 's|^+++ b/|    |p' "${patch_file}"
            echo
        done
    } > "${notices}/ZeroTier-Heeler-modifications.txt"
    cp "${zerotierone}/ext/miniupnpc/LICENSE" "${notices}/miniupnpc-BSD-3-Clause.txt"
    cp "${zerotierone}/ext/libnatpmp/LICENSE" "${notices}/libnatpmp-BSD-3-Clause.txt"
    cp "${zerotierone}/ext/nlohmann/LICENSE.MIT" "${notices}/nlohmann-json-MIT.txt"
    cp "${source}/ext/lwip/COPYING" "${notices}/lwIP-BSD-3-Clause.txt"
    extract_comment_block "${source}/ext/lwip-contrib/ports/unix/port/sys_arch.c" \
        "Redistribution and use" > "${notices}/lwIP-contrib-BSD-3-Clause.txt"
    extract_comment_block "${zerotierone}/node/Packet.cpp" \
        "BSD 2-Clause License" > "${notices}/ZeroTierOne-LZ4-BSD-2-Clause.txt"
    chmod 644 "${notices}"/*
    grep -q "Change Date:          2026-01-01" "${notices}/libzt-BUSL-1.1-Apache-2.0.txt"
    grep -q "Change Date:          2025-01-01" "${notices}/ZeroTierOne-BUSL-1.1-Apache-2.0.txt"
    grep -q "Yann Collet" "${notices}/ZeroTierOne-LZ4-BSD-2-Clause.txt"
    grep -q "Swedish Institute" "${notices}/lwIP-contrib-BSD-3-Clause.txt"

    local version
    version="$(awk '
        /define ZEROTIER_ONE_VERSION_MAJOR/ { major = $3 }
        /define ZEROTIER_ONE_VERSION_MINOR/ { minor = $3 }
        /define ZEROTIER_ONE_VERSION_REVISION/ { revision = $3 }
        END { print major "." minor "." revision }
    ' "${zerotierone}/version.h")"
    cat > "${stage}/provenance.md" <<EOF
## CZeroTier

- libzt: commit ${LIBZT_COMMIT} (submodule upstream/libzt, ${LIBZT_REPO})
- libzt patches: ${ZEROTIER_PATCHES}
- libzt addition: ${ZEROTIER_CPP} (sha256 $(sha256_of "${ROOT_DIR}/${ZEROTIER_CPP}")) and ${ZEROTIER_H} (sha256 $(sha256_of "${ROOT_DIR}/${ZEROTIER_H}")) in src/; exports heeler_zt_peers (the node's peer list), heeler_zt_planet_inspect, and heeler_zt_add_moon (a self-hosted planet as a local moon)
- ZeroTierOne: ${version}, commit ${ZEROTIERONE_COMMIT} (${ZEROTIERONE_REPO})
- lwIP: commit ${LWIP_COMMIT} (${LWIP_REPO}, STABLE-2_1_x fork)
- lwIP contrib: commit ${LWIP_CONTRIB_COMMIT} (${LWIP_CONTRIB_REPO})
- libzt configuration: BUILD_IOS_FRAMEWORK, zt-static, central API disabled, local symbols stripped
- ZeroTier licence: BUSL-1.1, converted to Apache-2.0 on the Change Date (libzt 2026-01-01, ZeroTierOne 2025-01-01)
- CMake: $(cmake --version | sed -n '1p')
EOF
}
