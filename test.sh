#!/usr/bin/env bash

set -euxo pipefail

cargo build --release
ENSYNC="$(cargo metadata --format-version 1 | jq -r '.target_directory')/release/ensync"

TEST_DIR="$(mktemp --directory --tmpdir ensync-test.XXXXXXXXX)"
cd "${TEST_DIR}"
trap "rm -rf '${TEST_DIR}'" exit

LOCAL="${PWD}/local"
CONFIG="${PWD}/config"
REMOTE="${PWD}/remote"

mkdir "${LOCAL}" "${CONFIG}"

cat > "${CONFIG}/config.toml" <<EOF
[general]
path = "${LOCAL}"
server = "path:${REMOTE}"
server_root = "root"
passphrase = "string:test"
compression = "best"

[[rules.root.files]]
mode = "conservative-sync"
trust_client_unix_mode = true
EOF

touch "${PWD}/testfile"

${ENSYNC} key init "${CONFIG}"
${ENSYNC} mkdir "${CONFIG}" /root
${ENSYNC} mkdir "${CONFIG}" remote-new-dir
${ENSYNC} put "${CONFIG}" "${PWD}/testfile" remote-new-file
${ENSYNC} put "${CONFIG}" "${PWD}/testfile" edit-edit-file
mkdir "${LOCAL}/local-new-dir"
touch "${LOCAL}/local-new-file"
touch "${LOCAL}/edit-edit-file"

JSON_STATUS_OUT="${PWD}/json-status-out"

exec 3>"${JSON_STATUS_OUT}"
${ENSYNC} sync --json-status-fd=3 "${CONFIG}"

touch "${LOCAL}/local-new-file"
${ENSYNC} sync --json-status-fd=3 "${CONFIG}"

cat "${JSON_STATUS_OUT}"
