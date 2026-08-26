//! Source selection: falseticker rejection by interval intersection (RFC 5905 §11.2.1)
//! and weighted combining.

/// One source's current estimate, as fed to selection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SourceEstimate {
    /// Caller's identifier for the source (index, socket id, …).
    pub id: usize,
    /// Seconds to add to the local clock.
    pub offset: f64,
    /// Root distance: delay/2 + dispersion, seconds. Bounds the interval.
    pub root_distance: f64,
    pub stratum: u8,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Selection {
    /// ids of sources judged truechimers, best first.
    pub truechimers: Vec<usize>,
    /// Combined offset (weighted by 1/root_distance), if any source survived.
    pub offset: Option<f64>,
}

/// Find the largest clique of sources whose correctness intervals
/// `[offset - root_distance, offset + root_distance]` share a common point, then
/// combine the survivors.
pub fn select(sources: &[SourceEstimate]) -> Selection {
    let n = sources.len();
    if n == 0 {
        return Selection::default();
    }

    // Endpoint sweep, per RFC 5905: find [low, high] contained in at least n - f
    // intervals, for the smallest achievable number of falsetickers f.
    #[derive(Clone, Copy)]
    struct Edge {
        value: f64,
        kind: i32, // +1 = lower endpoint, -1 = upper endpoint
    }
    let mut edges: Vec<Edge> = Vec::with_capacity(2 * n);
    for s in sources {
        let rd = s.root_distance.max(1e-9);
        edges.push(Edge {
            value: s.offset - rd,
            kind: 1,
        });
        edges.push(Edge {
            value: s.offset + rd,
            kind: -1,
        });
    }
    edges.sort_by(|a, b| a.value.total_cmp(&b.value));

    let mut chosen: Option<(f64, f64)> = None;
    for f in 0..=(n.saturating_sub(1)) / 2 {
        let need = (n - f) as i32;
        // Scan up for the low endpoint.
        let mut count = 0;
        let mut low = None;
        for e in &edges {
            count += e.kind;
            if count >= need {
                low = Some(e.value);
                break;
            }
        }
        // Scan down for the high endpoint.
        let mut count = 0;
        let mut high = None;
        for e in edges.iter().rev() {
            count -= e.kind;
            if count >= need {
                high = Some(e.value);
                break;
            }
        }
        if let (Some(lo), Some(hi)) = (low, high)
            && lo <= hi
        {
            chosen = Some((lo, hi));
            break;
        }
    }

    let Some((lo, hi)) = chosen else {
        return Selection::default();
    };

    let mut survivors: Vec<&SourceEstimate> = sources
        .iter()
        .filter(|s| {
            let rd = s.root_distance.max(1e-9);
            s.offset + rd >= lo && s.offset - rd <= hi
        })
        .collect();
    if survivors.is_empty() {
        return Selection::default();
    }

    // Best first: lowest stratum, then tightest interval.
    survivors.sort_by(|a, b| {
        (a.stratum, a.root_distance)
            .partial_cmp(&(b.stratum, b.root_distance))
            .unwrap_or(core::cmp::Ordering::Equal)
    });

    let mut wsum = 0.0;
    let mut osum = 0.0;
    for s in &survivors {
        let w = 1.0 / s.root_distance.max(1e-9);
        wsum += w;
        osum += w * s.offset;
    }

    Selection {
        truechimers: survivors.iter().map(|s| s.id).collect(),
        offset: (wsum > 0.0).then(|| osum / wsum),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(id: usize, offset: f64, rd: f64) -> SourceEstimate {
        SourceEstimate {
            id,
            offset,
            root_distance: rd,
            stratum: 2,
        }
    }

    #[test]
    fn falseticker_is_excluded() {
        let sources = [
            src(0, 0.001, 0.005),
            src(1, 0.002, 0.005),
            src(2, 0.0015, 0.005),
            src(3, 0.500, 0.005), // liar
        ];
        let sel = select(&sources);
        assert_eq!(sel.truechimers.len(), 3);
        assert!(!sel.truechimers.contains(&3));
        let o = sel.offset.expect("offset");
        assert!(o > 0.0005 && o < 0.0035, "combined {o}");
    }

    #[test]
    fn all_disjoint_yields_majority_failure() {
        let sources = [
            src(0, 0.0, 0.001),
            src(1, 1.0, 0.001),
            src(2, 2.0, 0.001),
            src(3, 3.0, 0.001),
        ];
        let sel = select(&sources);
        // No majority clique exists; selection must not invent one.
        assert!(sel.offset.is_none() || sel.truechimers.len() <= 2);
    }

    #[test]
    fn single_source_is_used() {
        let sel = select(&[src(0, 0.010, 0.002)]);
        assert_eq!(sel.truechimers, vec![0]);
        assert!((sel.offset.expect("offset") - 0.010).abs() < 1e-12);
    }

    #[test]
    fn empty_input_is_empty_output() {
        assert_eq!(select(&[]), Selection::default());
    }
}
