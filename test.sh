#!/usr/bin/env bash

set -euxo pipefail

cargo build --release
ENSYNC="$(cargo metadata --format-version 1 | jq -r '.target_directory')/release/ensync"

TEST_DIR="$(mktemp --directory --tmpdir ensync-test.XXXXXXXXX)"
cd "${TEST_DIR}"
trap "rm -rf '${TEST_DIR}'" exit

CLIENT="${PWD}/client"
CONFIG="${PWD}/config"
SERVER="${PWD}/server"

mkdir "${CLIENT}" "${CONFIG}"

cat > "${CONFIG}/config.toml" <<EOF
[general]
path = "${CLIENT}"
server = "path:${SERVER}"
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
${ENSYNC} mkdir "${CONFIG}" server-new-dir
${ENSYNC} put "${CONFIG}" "${PWD}/testfile" server-new-file
mkdir "${CLIENT}/client-new-dir"
touch "${CLIENT}/client-new-file"
${ENSYNC} sync --json-status-fd=2 "${CONFIG}"
