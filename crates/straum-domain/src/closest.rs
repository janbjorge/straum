//! The name someone most likely meant, for errors about a name nothing
//! has. [`closest`] measures each candidate's edit distance from the name
//! written and offers the nearest, if it is near enough and no other
//! candidate is as near. Errors about unknown views, keys and measures use
//! it, and so does the command line for commands and flags.

/// At most this many edits apart, a name is a likely slip.
const MAX_EDITS: usize = 2;

/// The candidate `written` most likely meant: the one fewest edits away,
/// if there is just one, and it is at most two edits and a third of the
/// longer name away. An edit inserts, removes or replaces a character, or
/// swaps two neighbours.
pub fn closest<'a>(written: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let mut best: Option<&'a str> = None;
    let mut best_edits = usize::MAX;
    let mut tied = false;
    for candidate in candidates {
        let edits = edits_between(written, candidate);
        if edits < best_edits {
            best = Some(candidate);
            best_edits = edits;
            tied = false;
        } else if edits == best_edits {
            tied = true;
        }
    }
    let best = best?;
    let longer = written.chars().count().max(best.chars().count());
    if tied || best_edits == 0 || best_edits > MAX_EDITS || best_edits * 3 > longer {
        return None;
    }
    Some(best)
}

/// Edits from `from` to `to`, a swap of neighbours counted as one: the
/// optimal string alignment distance.
fn edits_between(from: &str, to: &str) -> usize {
    let from: Vec<char> = from.chars().collect();
    let to: Vec<char> = to.chars().collect();
    let width = to.len() + 1;
    // rows[i * width + j]: edits from the first i of `from` to the first
    // j of `to`.
    let mut rows = vec![0; (from.len() + 1) * width];
    for (j, cell) in rows.iter_mut().take(width).enumerate() {
        *cell = j;
    }
    for i in 1..=from.len() {
        rows[i * width] = i;
        for j in 1..=to.len() {
            let replace = usize::from(from[i - 1] != to[j - 1]);
            let mut edits = (rows[(i - 1) * width + j] + 1)
                .min(rows[i * width + j - 1] + 1)
                .min(rows[(i - 1) * width + j - 1] + replace);
            let swapped = i > 1 && j > 1 && from[i - 1] == to[j - 2] && from[i - 2] == to[j - 1];
            if swapped {
                edits = edits.min(rows[(i - 2) * width + j - 2] + 1);
            }
            rows[i * width + j] = edits;
        }
    }
    rows[from.len() * width + to.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_each_kind_of_edit_once() {
        assert_eq!(edits_between("trips", "trips"), 0);
        assert_eq!(edits_between("trip", "trips"), 1);
        assert_eq!(edits_between("tripss", "trips"), 1);
        assert_eq!(edits_between("trups", "trips"), 1);
        assert_eq!(edits_between("trpis", "trips"), 1);
        assert_eq!(edits_between("", "abc"), 3);
        assert_eq!(edits_between("abc", ""), 3);
        assert_eq!(edits_between("kitten", "sitting"), 3);
        assert_eq!(edits_between("zoné", "zone"), 1);
    }

    #[test]
    fn offers_the_one_near_name() {
        let names = ["trips", "fare", "avg_fare", "borough"];
        assert_eq!(closest("trpis", &names), Some("trips"));
        assert_eq!(closest("trip", &names), Some("trips"));
        assert_eq!(closest("burough", &names), Some("borough"));
        assert_eq!(closest("avgfare", &names), Some("avg_fare"));
        assert_eq!(closest("--ulr", &["--url", "--json"]), Some("--url"));
    }

    #[test]
    fn offers_nothing_far_tied_or_exact() {
        let names = ["trips", "fare", "avg_fare"];
        // Far: more than two edits, or too many for a short name.
        assert_eq!(closest("passengers", &names), None);
        assert_eq!(closest("x", &["n"]), None);
        // Tied: no way to tell which was meant.
        assert_eq!(closest("cat", &["bat", "hat"]), None);
        assert_eq!(closest("far", &["fa", "fare"]), None);
        // Exact: nothing was misspelled.
        assert_eq!(closest("fare", &names), None);
        assert_eq!(closest("fare", &[]), None);
    }
}
