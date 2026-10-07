#!/bin/bash
# Prepares native/easytier for local cargo work: writes EasyTier (submodule
# commit from sources.lock, with patches/easytier applied) into
# native/easytier/vendor/easytier, where the crate's path dependencies expect
# it, and the pinned protoc into native/easytier/vendor/protoc. Re-run after
# changing the submodule or a patch.
#
#   scripts/easytier-dev.sh
#   cd native/easytier && export PROTOC="$PWD/vendor/protoc/bin/protoc" && cargo test --locked
set -euo pipefail

# shellcheck source=lib/common.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib/common.sh"
# shellcheck source=lib/easytier.sh
source "${ROOT_DIR}/scripts/lib/easytier.sh"

require_commands git tar patch rsync shasum curl unzip
check_submodules
sync_easytier_source "${ROOT_DIR}/native/easytier/vendor/easytier"
install_protoc "${ROOT_DIR}/native/easytier/vendor/protoc"
echo "native/easytier/vendor/easytier holds EasyTier ${EASYTIER_COMMIT} with patches/easytier applied."
echo "Pinned protoc ${PROTOC_VERSION}: export PROTOC=${ROOT_DIR}/native/easytier/vendor/protoc/bin/protoc"
