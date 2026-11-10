//! How many items a loot crate gets: the fill-volume model.
//!
//! The engine has no "number of results" roll (CONFIRMED against the engine): loot generation sets
//! `target = floor(capacity x (min + (max - min) x u))` micro-SCU from `fullnessFactorRange`; the container's capacity is its inventory
//! container capacity; it then rolls one
//! table entry (and one item of its pool, uniform) at a time and adds a stack of it until the used volume reaches the target, the count of
//! results hits `totalResultsLimit` (<= 0 means unlimited) or the tree is empty.  The stack is `min(rolled stack size, floor(space / item volume))`,
//! the stack size defaulting to 1.  `ELootFullnessMode` 0 `stopAfterExceed` lets the last stack overshoot the target
//! (bounded by the capacity), 1 `preventExceed` does not.
//!
//! This module turns that into expected numbers.  The roll sequence is a renewal process whose step is the volume of one rolled stack, so with
//! the step mean `mu` and second moment `ev2` the expected number of stacks until the used volume reaches `T` is `T / mu + ev2 / (2 mu^2)` for
//! `stopAfterExceed` (the standard renewal result for the first passage, INFERRED to be a good approximation here: it ignores the capacity
//! clamp of the last stack) and `T / mu` for `preventExceed`.  EMPIRICAL checks are in the tests (Monte Carlo against the formula).

/// One possible outcome of a single roll: its probability, and the volume of the stack it adds in micro-SCU.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Outcome {
    pub p: f64,
    pub volume: f64,
}

/// First and second moment of the stack volume of one roll.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Moments {
    pub mu: f64,
    pub ev2: f64,
}

pub fn moments(outcomes: &[Outcome]) -> Moments {
    let total: f64 = outcomes.iter().map(|o| o.p).sum();
    if total <= 0.0 {
        return Moments::default();
    }
    Moments {
        mu: outcomes.iter().map(|o| o.p * o.volume).sum::<f64>() / total,
        ev2: outcomes.iter().map(|o| o.p * o.volume * o.volume).sum::<f64>() / total,
    }
}

/// Expected number of stacks rolled until the used volume reaches `target` (micro-SCU).  `prevent_exceed` = `ELootFullnessMode::preventExceed`.
/// At least one stack for a positive target, at most `limit` when `limit > 0`.
pub fn expected_stacks(m: Moments, target: f64, prevent_exceed: bool, limit: f64) -> f64 {
    if target <= 0.0 || m.mu <= 0.0 {
        return 0.0;
    }
    let mut n = target / m.mu;
    if !prevent_exceed {
        n += m.ev2 / (2.0 * m.mu * m.mu);
    }
    n = n.max(1.0);
    if limit > 0.0 {
        n = n.min(limit);
    }
    n
}

/// Expected stacks for a crate at the low, mean and high end of the fullness range (`target = floor(capacity x fullness)`).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Count {
    pub capacity: f64,
    pub target: [f64; 3],
    pub stacks: [f64; 3],
}

pub fn crate_count(m: Moments, capacity: f64, fullness: [f64; 2], prevent_exceed: bool, limit: f64) -> Count {
    let (lo, hi) = (fullness[0].min(fullness[1]), fullness[0].max(fullness[1]));
    let target = [(capacity * lo).floor(), (capacity * (lo + hi) / 2.0).floor(), (capacity * hi).floor()];
    Count { capacity, target, stacks: target.map(|t| expected_stacks(m, t, prevent_exceed, limit)) }
}

/// One selectable entry of a loot table (a V3 table entry / V1 weighted archetype) as the selection model sees it.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct EntryIn {
    /// effective weight (weight x ChanceToExist, already renormalised or not: only ratios matter)
    pub weight: f64,
    /// priorityCount: the entry is taken first, with no weight roll, until it has been chosen this many times
    pub prio: f64,
    /// choiceLimit (0 = none): the entry is removed once chosen this often
    pub limit: f64,
    /// mean and second moment of the volume one pick of the entry adds (stack + spawn-with extras), micro-SCU
    pub mu: f64,
    pub ev2: f64,
}

/// Expected picks of each entry for one crate: the priority picks, the weighted picks and their sum.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Plan {
    pub forced: Vec<f64>,
    pub regular: Vec<f64>,
    pub picks: Vec<f64>,
    /// expected number of picks (stacks) of the whole crate
    pub total: f64,
}

/// Expected picks per entry when `n` weighted picks are made and an entry is removed after `cap[i]` picks (`f64::INFINITY` = no cap).  Entries over
/// their cap get exactly the cap and the surplus is re-spread over the others by weight (water filling).  This is the expectation of the engine's
/// "remove the entry once chosen >= choiceLimit" rule (CONFIRMED rule; the expectation is our approximation, checked against a simulation in
/// the tests).  Picks that no entry can take any more are lost.
pub fn allocate_picks(weights: &[f64], cap: &[f64], n: f64) -> Vec<f64> {
    let mut out = vec![0.0; weights.len()];
    let mut active: Vec<usize> = (0..weights.len()).filter(|&i| weights[i] > 0.0 && cap[i] > 0.0).collect();
    let mut left = n.max(0.0);
    while !active.is_empty() && left > 1e-12 {
        let w: f64 = active.iter().map(|&i| weights[i]).sum();
        let over: Vec<usize> = active.iter().copied().filter(|&i| left * weights[i] / w > cap[i] - out[i]).collect();
        if over.is_empty() {
            for &i in &active {
                out[i] += left * weights[i] / w;
            }
            break;
        }
        for &i in &over {
            left -= cap[i] - out[i];
            out[i] = cap[i];
        }
        active.retain(|i| !over.contains(i));
    }
    out
}

/// The selection model of one crate: priority entries first, then weighted picks until the fill target.
///
/// CONFIRMED rules: while any entry has `prio > chosen` the first such entry in table order is taken with no weight roll, and those
/// subtrees ignore the fullness target (only the capacity limits them); an entry is removed once `prio <= chosen` and `limit != 0` and
/// `limit <= chosen`; `chosenCount` counts picks (not units or spawn-withs) and `totalResultsLimit` (`result_limit`, 0 = none) caps the picks.
/// Our expectation (INFERRED): the forced picks use up `forced volume` of the target, the remaining target is filled by the renewal model
/// (`expected_stacks`) over the weight mix of all entries, and the weighted picks are spread by `allocate_picks`.
pub fn plan(entries: &[EntryIn], target: f64, capacity: f64, prevent_exceed: bool, result_limit: f64) -> Plan {
    let n = entries.len();
    let mut forced = vec![0.0; n];
    let (mut used, mut count) = (0.0, 0.0);
    for (i, e) in entries.iter().enumerate() {
        let mut want = e.prio.max(0.0);
        if result_limit > 0.0 {
            want = want.min((result_limit - count).max(0.0));
        }
        if e.mu > 0.0 && capacity > 0.0 {
            want = want.min(((capacity - used) / e.mu).max(0.0));
        }
        forced[i] = want;
        used += want * e.mu;
        count += want;
    }
    let (wsum, mu, ev2) = entries.iter().fold((0.0, 0.0, 0.0), |a, e| (a.0 + e.weight.max(0.0), a.1 + e.weight.max(0.0) * e.mu, a.2 + e.weight.max(0.0) * e.ev2));
    let remaining_target = target - used;
    let mut regular = vec![0.0; n];
    if wsum > 0.0 && remaining_target > 0.0 && (result_limit <= 0.0 || result_limit > count) {
        let m = Moments { mu: mu / wsum, ev2: ev2 / wsum };
        let lim = if result_limit > 0.0 { result_limit - count } else { 0.0 };
        let rolls = expected_stacks(m, remaining_target, prevent_exceed, lim);
        let cap: Vec<f64> = entries.iter().zip(&forced).map(|(e, f)| if e.limit > 0.0 { (e.limit - f).max(0.0) } else { f64::INFINITY }).collect();
        let w: Vec<f64> = entries.iter().map(|e| e.weight.max(0.0)).collect();
        regular = allocate_picks(&w, &cap, rolls);
    }
    let picks: Vec<f64> = forced.iter().zip(&regular).map(|(a, b)| a + b).collect();
    Plan { total: picks.iter().sum(), forced, regular, picks }
}

/// Probability that an item with per-roll probability `p` shows up at least once in `n` rolls.
pub fn p_any(p: f64, n: f64) -> f64 {
    if p <= 0.0 || n <= 0.0 {
        0.0
    } else {
        1.0 - (1.0 - p.min(1.0)).powf(n)
    }
}

/// A config is valid only if `min <= max` and not both 0 (CONFIRMED); an invalid one generates nothing.
pub fn fullness_valid(fullness: [f64; 2]) -> bool {
    fullness[0] <= fullness[1] && !(fullness[0] == 0.0 && fullness[1] == 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moments_of_a_mix() {
        let m = moments(&[Outcome { p: 0.5, volume: 100.0 }, Outcome { p: 0.5, volume: 300.0 }]);
        assert_eq!(m.mu, 200.0);
        assert_eq!(m.ev2, 50_000.0);
        assert_eq!(moments(&[]), Moments::default());
    }

    #[test]
    fn stacks_follow_the_target() {
        let m = moments(&[Outcome { p: 1.0, volume: 100.0 }]);
        // constant 100: target 1000 -> exactly 10 steps reach it; the renewal first-passage formula gives 10 + 0.5 (an overestimate for a lattice)
        assert_eq!(expected_stacks(m, 1000.0, true, 0.0), 10.0);
        assert_eq!(expected_stacks(m, 1000.0, false, 0.0), 10.5);
        assert_eq!(expected_stacks(m, 1000.0, true, 4.0), 4.0, "totalResultsLimit caps the count");
        assert_eq!(expected_stacks(m, 10.0, true, 0.0), 1.0, "a positive target needs at least one stack");
        assert_eq!(expected_stacks(m, 0.0, true, 0.0), 0.0);
    }

    #[test]
    fn first_passage_matches_a_simulation() {
        // volumes 50 / 250 with p 0.8 / 0.2: mu 90, run a deterministic LCG to the target 3000
        let outs = [Outcome { p: 0.8, volume: 50.0 }, Outcome { p: 0.2, volume: 250.0 }];
        let m = moments(&outs);
        let (mut s, mut tot, runs) = (12345u64, 0u64, 20000u64);
        for _ in 0..runs {
            let mut used = 0.0;
            while used < 3000.0 {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                used += if (s >> 33) as f64 / (1u64 << 31) as f64 > 0.8 { 250.0 } else { 50.0 };
                tot += 1;
            }
        }
        let sim = tot as f64 / runs as f64;
        let model = expected_stacks(m, 3000.0, false, 0.0);
        assert!((sim - model).abs() / sim < 0.03, "sim {sim} model {model}");
    }

    /// Deterministic LCG in [0, 1).
    fn rnd(s: &mut u64) -> f64 {
        *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*s >> 11) as f64 / (1u64 << 53) as f64
    }

    #[test]
    fn choice_limits_match_a_simulation() {
        // weights 5/3/2, entry 0 limited to 2 picks: 10 weighted picks, an entry is removed once chosen >= its limit
        let (w, cap) = ([5.0, 3.0, 2.0], [2.0, f64::INFINITY, f64::INFINITY]);
        let model = allocate_picks(&w, &cap, 10.0);
        assert!((model[0] - 2.0).abs() < 1e-9 && (model.iter().sum::<f64>() - 10.0).abs() < 1e-9);
        let (mut s, runs) = (99u64, 40000);
        let mut sim = [0.0; 3];
        for _ in 0..runs {
            let mut chosen = [0u32; 3];
            for _ in 0..10 {
                let live: Vec<usize> = (0..3).filter(|&i| i != 0 || chosen[0] < 2).collect();
                let tot: f64 = live.iter().map(|&i| w[i]).sum();
                let mut r = rnd(&mut s) * tot;
                for &i in &live {
                    if r < w[i] {
                        chosen[i] += 1;
                        break;
                    }
                    r -= w[i];
                }
            }
            for i in 0..3 {
                sim[i] += chosen[i] as f64 / runs as f64;
            }
        }
        // the expectation of the capped process is not exactly the water-filling split (the cap is reached at a random time), within 6 %
        for i in 0..3 {
            assert!((sim[i] - model[i]).abs() / sim[i] < 0.06, "entry {i}: sim {} model {}", sim[i], model[i]);
        }
        // everything capped: picks are lost
        assert_eq!(allocate_picks(&[1.0], &[3.0], 10.0), vec![3.0]);
        assert_eq!(allocate_picks(&[0.0, 1.0], &[1.0, 0.0], 5.0), vec![0.0, 0.0]);
    }

    #[test]
    fn priority_entries_come_first_and_ignore_the_target() {
        let e = |weight, prio, limit| EntryIn { weight, prio, limit, mu: 100.0, ev2: 10_000.0 };
        // two priority picks of entry 0 (volume 200) already pass a target of 150: nothing is rolled by weight
        let p = plan(&[e(1.0, 2.0, 0.0), e(1.0, 0.0, 0.0)], 150.0, 1000.0, true, 0.0);
        assert_eq!((p.forced.clone(), p.regular.clone(), p.total), (vec![2.0, 0.0], vec![0.0, 0.0], 2.0));
        // the capacity bounds the forced picks (a 3rd would not fit) and totalResultsLimit bounds everything
        assert_eq!(plan(&[e(1.0, 5.0, 0.0)], 0.0, 300.0, true, 0.0).forced, vec![3.0]);
        assert_eq!(plan(&[e(1.0, 5.0, 0.0)], 0.0, 1000.0, true, 4.0).total, 4.0);
        // forced picks use up target volume: target 1000, 2 forced -> 8 more weighted picks of 100, split by weight and the limit of entry 1 (3 total)
        let p = plan(&[e(1.0, 2.0, 0.0), e(1.0, 0.0, 3.0)], 1000.0, 5000.0, true, 0.0);
        assert!((p.regular[1] - 3.0).abs() < 1e-9 && (p.total - 10.0).abs() < 1e-9, "{p:?}");
        assert!((p.picks[0] - (2.0 + (8.0 - 3.0))).abs() < 1e-9, "{p:?}");
        // without priority or limits the plan is the plain renewal count
        let m = Moments { mu: 100.0, ev2: 10_000.0 };
        let plain = plan(&[e(3.0, 0.0, 0.0), e(1.0, 0.0, 0.0)], 1000.0, 5000.0, false, 0.0);
        assert!((plain.total - expected_stacks(m, 1000.0, false, 0.0)).abs() < 1e-9);
    }

    #[test]
    fn crate_range_and_probability() {
        let m = moments(&[Outcome { p: 1.0, volume: 1000.0 }]);
        let c = crate_count(m, 100_000.0, [0.1, 0.5], true, 0.0);
        assert_eq!(c.target, [10_000.0, 30_000.0, 50_000.0]);
        assert_eq!(c.stacks, [10.0, 30.0, 50.0]);
        assert!((p_any(0.1, 2.0) - 0.19).abs() < 1e-12);
        assert_eq!(p_any(0.0, 5.0), 0.0);
        assert!(fullness_valid([0.1, 0.2]) && !fullness_valid([0.0, 0.0]) && !fullness_valid([0.5, 0.2]));
    }
}
