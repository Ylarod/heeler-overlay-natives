#!/bin/bash
# Compares XCFrameworks byte for byte between two directories that hold
# <Framework>.xcframework (build/Artifacts, a build/stage/<library>, or
# another checkout's artifacts). Exits non-zero on any difference.
#
#   scripts/compare-artifacts.sh <dir-a> <dir-b> [CTailscale CZeroTier CEasyTier]
set -euo pipefail

A="${1:?usage: compare-artifacts.sh <dir-a> <dir-b> [framework...]}"
B="${2:?usage: compare-artifacts.sh <dir-a> <dir-b> [framework...]}"
shift 2
frameworks=("$@")
[[ ${#frameworks[@]} -gt 0 ]] || frameworks=(CTailscale CZeroTier CEasyTier)

status=0
for framework in "${frameworks[@]}"; do
    for side in "${A}" "${B}"; do
        [[ -d "${side}/${framework}.xcframework" ]] || {
            echo "error: ${side}/${framework}.xcframework is missing" >&2
            exit 1
        }
    done
    list_a="$(cd "${A}" && find "${framework}.xcframework" -type f | LC_ALL=C sort)"
    list_b="$(cd "${B}" && find "${framework}.xcframework" -type f | LC_ALL=C sort)"
    if [[ "${list_a}" != "${list_b}" ]]; then
        echo "DIFFERENT FILE SETS in ${framework}.xcframework:"
        diff <(echo "${list_a}") <(echo "${list_b}") || true
        status=1
        continue
    fi
    differing=0
    while read -r file; do
        if ! cmp -s "${A}/${file}" "${B}/${file}"; then
            echo "differs: ${file}"
            differing=$((differing + 1))
        fi
    done <<<"${list_a}"
    if [[ "${differing}" == 0 ]]; then
        echo "identical: ${framework}.xcframework ($(wc -l <<<"${list_a}" | tr -d ' ') files)"
    else
        status=1
    fi
done
exit "${status}"
