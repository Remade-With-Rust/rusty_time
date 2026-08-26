#!/bin/bash
# Show what chrony computed for each interleaved sample.
WORK=${WORK:-$HOME/m4-interop}
echo "=== offset / delay lines ==="
grep -i -e offset -e delay "$WORK/chronyd-xleave.log" | head -25
echo
echo "=== interleaved mentions ==="
grep -i interleav "$WORK/chronyd-xleave.log" | head -15
echo
echo "=== timestamp lines ==="
grep -i -e "rx=" -e "tx=" -e "org=" "$WORK/chronyd-xleave.log" | head -15
