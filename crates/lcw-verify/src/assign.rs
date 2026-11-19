//! Minimum-cost assignment (Hungarian algorithm), so that identical parts are paired with the
//! placements they are nearest to as a whole, not one greedy pair at a time.

/// Cost of a pair that must not be made.
pub const FORBIDDEN: f64 = 1e12;
/// Cost of leaving a row unmatched: above any real cost, below FORBIDDEN.
const UNMATCHED: f64 = 1e9;

/// `cost[r][c]`: pair row r with column c. Returns, for each row, its column, or None when it
/// is left unmatched (it had only FORBIDDEN pairs, or every column was taken by a cheaper
/// pairing). Each column is used at most once.
pub fn assign(cost: &[Vec<f64>]) -> Vec<Option<usize>> {
    let n = cost.len();
    if n == 0 {
        return Vec::new();
    }
    let m = cost.iter().map(Vec::len).max().unwrap_or(0);
    // One spare column per row, so that every row can be left unmatched.
    let cols = m + n;
    let c = |r: usize, k: usize| -> f64 {
        if k < m {
            cost[r].get(k).copied().unwrap_or(FORBIDDEN).min(FORBIDDEN)
        } else {
            UNMATCHED
        }
    };
    // e-maxx formulation, 1-based, rows <= columns.
    let inf = f64::INFINITY;
    let mut u = vec![0.0; n + 1];
    let mut v = vec![0.0; cols + 1];
    let mut p = vec![0usize; cols + 1];
    let mut way = vec![0usize; cols + 1];
    for i in 1..=n {
        p[0] = i;
        let mut j0 = 0usize;
        let mut minv = vec![inf; cols + 1];
        let mut used = vec![false; cols + 1];
        loop {
            used[j0] = true;
            let i0 = p[j0];
            let mut delta = inf;
            let mut j1 = 0usize;
            for j in 1..=cols {
                if !used[j] {
                    let cur = c(i0 - 1, j - 1) - u[i0] - v[j];
                    if cur < minv[j] {
                        minv[j] = cur;
                        way[j] = j0;
                    }
                    if minv[j] < delta {
                        delta = minv[j];
                        j1 = j;
                    }
                }
            }
            for j in 0..=cols {
                if used[j] {
                    u[p[j]] += delta;
                    v[j] -= delta;
                } else {
                    minv[j] -= delta;
                }
            }
            j0 = j1;
            if p[j0] == 0 {
                break;
            }
        }
        loop {
            let j1 = way[j0];
            p[j0] = p[j1];
            j0 = j1;
            if j0 == 0 {
                break;
            }
        }
    }
    let mut out = vec![None; n];
    for j in 1..=cols {
        if p[j] != 0 && j <= m {
            let r = p[j] - 1;
            if c(r, j - 1) < FORBIDDEN {
                out[r] = Some(j - 1);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_by_total_cost_not_greedily() {
        // Greedy would give row 0 column 0 (1) and row 1 column 1 (100); the best is 2 + 3.
        let cost = vec![vec![1.0, 2.0], vec![3.0, 100.0]];
        assert_eq!(assign(&cost), vec![Some(1), Some(0)]);
    }

    #[test]
    fn forbidden_pairs_and_surplus_rows_stay_unmatched() {
        let cost = vec![vec![5.0, FORBIDDEN], vec![FORBIDDEN, FORBIDDEN], vec![4.0, FORBIDDEN]];
        let r = assign(&cost);
        assert_eq!(r[1], None);
        assert_eq!(r.iter().filter(|x| x.is_some()).count(), 1);
        assert_eq!(r[2], Some(0), "the cheaper of the two rows competing for column 0 gets it");
    }

    #[test]
    fn empty_and_without_columns() {
        assert!(assign(&[]).is_empty());
        assert_eq!(assign(&[vec![], vec![]]), vec![None, None]);
    }
}
