#!/bin/bash
# clknetsim rig patch for Rust clients (mission plan §7.1 / LEDGER interception note).
#
# Rust's std creates sockets as `socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, ...)`;
# clknetsim's socket() hook exact-matches `type` and returns EINVAL on the flag
# bits, so every Rust std client fails before its first packet. Mask the flag
# bits, as the kernel itself does. Rig-only patch: clknetsim is GPL test
# tooling and is never shipped or linked into rusty_time.
set -e
CLKNETSIM_PATH=${CLKNETSIM_PATH:-$HOME/clknetsim}
cd "$CLKNETSIM_PATH"

if grep -q 'SOCK_CLOEXEC | SOCK_NONBLOCK); /\* rusty_time rig patch \*/' client.c; then
    echo "already patched"
else
    sed -i 's/^int socket(int domain, int type, int protocol) {$/int socket(int domain, int type, int protocol) {\n\ttype \&= ~(SOCK_CLOEXEC | SOCK_NONBLOCK); \/* rusty_time rig patch *\//' client.c
    grep -n "rusty_time rig patch" client.c
fi
make
echo "clknetsim rebuilt with Rust-socket patch"
