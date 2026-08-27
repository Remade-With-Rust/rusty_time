#!/usr/bin/env python3
"""Paired verdict over accuracy_cell.sh output.

Reads lines of `<scenario> <arm> <seed> <signed-us> <mean-abs-us> <n>` and, for
each scenario, compares every arm against a baseline arm SEED BY SEED.

Why paired, and why a sign test:

  Steady-state error varies more between simulated worlds than between the
  implementations being compared -- on S1 the same code ranged 0.36 to 2.03 us
  across five seeds. Comparing medians of unpaired runs therefore measures the
  draw, not the code. Pairing removes the world; the sign test then asks the
  only question that survives: in how many worlds did this arm win?

  The rig is seeded, so identical code in the same world produces bit-identical
  output. The null-arm floor is exactly zero by construction rather than by
  measurement, which is why a 5% median difference can be believed here when
  the same difference on a wall-clock rig could not.

|z| > 2 is a verdict. Anything less is reported as NOT RESOLVED, with the
number of seeds it would take to resolve an effect of the observed size.
"""

import sys
import math
from collections import defaultdict


def median(xs):
    s = sorted(xs)
    n = len(s)
    if not n:
        return float("nan")
    return s[n // 2] if n % 2 else (s[n // 2 - 1] + s[n // 2]) / 2


def main():
    baseline = sys.argv[1] if len(sys.argv) > 1 else "chrony"
    # (scenario, arm) -> {seed: (signed, absmean)}
    cells = defaultdict(dict)
    for line in sys.stdin:
        f = line.split()
        if len(f) < 6:
            continue
        scen, arm, seed = f[0], f[1], int(f[2])
        poll = float(f[6]) if len(f) > 6 else 0.0
        cells[(scen, arm)][seed] = (float(f[3]), float(f[4]), poll)

    scenarios = sorted({s for s, _ in cells})
    for scen in scenarios:
        arms = sorted(a for s, a in cells if s == scen and a != baseline)
        base = cells.get((scen, baseline), {})
        if not base:
            print(f"{scen}: no {baseline} arm")
            continue
        print(f"\n=== {scen} — paired against {baseline} "
              f"({len(base)} seeds) ===")
        bpoll = median([v[2] for v in base.values()])
        print(f"  {baseline:<18} median |e| {median([v[1] for v in base.values()]):6.2f} us"
              f"   worst {max(v[1] for v in base.values()):6.2f} us"
              f"   poll {bpoll:5.1f} s")
        for arm in arms:
            got = cells[(scen, arm)]
            seeds = sorted(set(base) & set(got))
            if not seeds:
                continue
            wins = sum(1 for s in seeds if got[s][1] < base[s][1])
            n = len(seeds)
            z = (wins - n / 2) / (0.5 * math.sqrt(n)) if n else 0.0
            med = median([got[s][1] for s in seeds])
            bmed = median([base[s][1] for s in seeds])
            worst = max(got[s][1] for s in seeds)
            # Median of the per-seed ratio: the paired effect size, immune to
            # one bad world dominating a mean.
            ratio = median([got[s][1] / base[s][1] for s in seeds
                            if base[s][1] > 0])
            verdict = ("RESOLVED, "
                       + (arm if wins > n / 2 else baseline)
                       + " ahead") if abs(z) > 2 else "NOT RESOLVED"
            apoll = median([got[s][2] for s in seeds])
            print(f"  {arm:<18} median |e| {med:6.2f} us   worst {worst:6.2f} us"
                  f"   poll {apoll:5.1f} s")
            print(f"  {'':<18} {wins:2d}/{n} wins  z={z:+5.2f}  x{ratio:.2f}  {verdict}")

            # Accuracy PER PACKET SPENT. Offset error falls as 1/sqrt(N), so an
            # arm that polls more often is more accurate before its estimator
            # does anything. Dividing that out asks the question the raw win
            # rate cannot: which estimator is better at equal cost?
            eff = [(got[s][1] / math.sqrt(got[s][2]))
                   / (base[s][1] / math.sqrt(base[s][2]))
                   for s in seeds if got[s][2] > 0 and base[s][2] > 0
                   and base[s][1] > 0]
            if eff:
                ewins = sum(1 for r in eff if r < 1.0)
                ez = (ewins - len(eff) / 2) / (0.5 * math.sqrt(len(eff)))
                everdict = ("RESOLVED, "
                            + (arm if ewins > len(eff) / 2 else baseline)
                            + " ahead") if abs(ez) > 2 else "NOT RESOLVED"
                print(f"  {'':<18} per-packet: x{median(eff):.2f}  "
                      f"{ewins:2d}/{len(eff)} wins  z={ez:+5.2f}  {everdict}")
            if abs(z) <= 2 and wins != n / 2:
                # Seeds needed for this win RATE to clear |z|=2.
                p = wins / n
                if p not in (0.0, 1.0) and abs(p - 0.5) > 1e-9:
                    need = math.ceil((1.0 / (2 * abs(p - 0.5))) ** 2)
                    print(f"  {'':<18} at this win rate, ~{need} seeds "
                          f"would resolve it")
            print(f"  {'':<18} signed bias: median "
                  f"{median([got[s][0] for s in seeds]):+6.2f} us vs "
                  f"{median([base[s][0] for s in seeds]):+6.2f} us "
                  f"({baseline})   [baseline median |e| {bmed:.2f}]")


if __name__ == "__main__":
    main()
