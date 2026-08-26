#!/bin/bash
# What can this machine actually verify for M7, and what needs the lab box?
#
# Run before claiming anything about hardware timestamping: the answer decides
# which parts of the milestone get evidence and which get an honest PENDING.

echo "=== PTP hardware clocks (/dev/ptp*) ==="
ls -la /dev/ptp* 2>/dev/null || echo "  none — PHC refclock cannot be exercised here"

echo
echo "=== PPS devices (/dev/pps*) ==="
ls -la /dev/pps* 2>/dev/null || echo "  none — PPS refclock cannot be exercised here"

echo
echo "=== NIC timestamping capabilities ==="
if command -v ethtool >/dev/null 2>&1; then
    for nic in $(ls /sys/class/net 2>/dev/null | grep -v lo); do
        echo "  -- $nic --"
        ethtool -T "$nic" 2>&1 | sed 's/^/    /' | head -14
    done
else
    echo "  ethtool not installed; checking /sys instead"
    ls /sys/class/net 2>/dev/null | sed 's/^/    iface: /'
fi

echo
echo "=== SO_TIMESTAMPING support in headers ==="
grep -rs "define SO_TIMESTAMPING" /usr/include 2>/dev/null | head -3 ||
    echo "  not found in headers (kernel may still support it)"

echo
echo "=== kernel ==="
uname -r

echo
echo "=== toolchains ==="
rustup toolchain list 2>/dev/null | sed 's/^/  /'
command -v cargo-fuzz >/dev/null && echo "  cargo-fuzz: present" || echo "  cargo-fuzz: absent"
