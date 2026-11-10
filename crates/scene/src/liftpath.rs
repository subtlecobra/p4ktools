//! Which doors lie on the path of a lift.
//!
//! CONFIRMED (EMPIRICAL, Mole): the lift and the hull hatch under it are separate loadout items with no record field naming each other.
//! INFERRED (the rule): a door is on the lift's path when its closed box overlaps the volume the lift's moving parts sweep through by at
//! least `MIN_OVERLAP` on every axis and does not overlap the lift's parked box.  UNVERIFIED: that the engine opens such a door.

use serde_json::{json, Value};

use crate::interact::{box_xf, Mat, V3};

/// a linked door counts as a lift (platform) when its parts travel this far up / down (m)
pub const LIFT_TRAVEL: f64 = 1.0;
/// the car box is shrunk this much before sweeping (m)
pub const CAR_INSET: V3 = [0.3, 0.3, 0.1];
/// a door box must overlap the swept volume by this much on every axis (m)
pub const MIN_OVERLAP: f64 = 0.05;

pub type Bx = [V3; 2];

pub fn overlap(a: &Bx, b: &Bx) -> [f64; 3] {
    let mut o = [0.0; 3];
    for k in 0..3 {
        o[k] = f64::min(a[1][k], b[1][k]) - f64::max(a[0][k], b[0][k]);
    }
    o
}

pub fn intersects(a: &Bx, b: &Bx) -> bool {
    overlap(a, b).iter().all(|o| *o >= MIN_OVERLAP)
}

/// Ids of the doors of `others` (id, closed box) that the part moving from `parked` through `swept` crosses (sorted).
pub fn path_doors(parked: &Bx, swept: &Bx, others: &[(i64, Bx)]) -> Vec<i64> {
    let mut v: Vec<i64> = others.iter().filter(|(_, b)| intersects(b, swept) && !intersects(b, parked)).map(|(i, _)| *i).collect();
    v.sort();
    v
}

fn bx_of(v: &Value) -> Option<Bx> {
    let a = v.as_array()?;
    let p = |i: usize| -> Option<V3> {
        let r = a.get(i)?.as_array()?;
        Some([r.first()?.as_f64()?, r.get(1)?.as_f64()?, r.get(2)?.as_f64()?])
    };
    Some([p(0)?, p(1)?])
}

/// Give every lift door of `items` (feed dicts with the transient `bx` closed box and `sw` swept box, zone coordinates; `lift_ids` = ids of
/// the lift doors) a `pd` list: the ids of the doors on its path.  Returns the number of (lift, path door) pairs.
pub fn link_lift_paths(items: &mut [Value], lift_ids: &[i64]) -> usize {
    let doors: Vec<(i64, Bx, Option<Bx>)> = items
        .iter()
        .filter(|it| it["k"] == "door")
        .filter_map(|it| Some((it["id"].as_i64()?, bx_of(&it["bx"])?, bx_of(&it["sw"]))))
        .collect();
    let lifts: Vec<i64> = {
        let mut l: Vec<i64> = lift_ids
            .iter()
            .copied()
            .filter(|i| doors.iter().any(|(j, b, s)| j == i && s.as_ref().is_some_and(|s| (s[1][2] - s[0][2]) - (b[1][2] - b[0][2]) >= LIFT_TRAVEL)))
            .collect();
        l.sort();
        l.dedup();
        l
    };
    let mut pairs = 0;
    for i in &lifts {
        let (_, bx, sw) = doors.iter().find(|d| d.0 == *i).unwrap();
        let Some(sw) = sw else { continue };
        let others: Vec<(i64, Bx)> = doors.iter().filter(|d| d.0 != *i && !lifts.contains(&d.0)).map(|d| (d.0, d.1)).collect();
        let pd = path_doors(bx, sw, &others);
        if !pd.is_empty() {
            pairs += pd.len();
            if let Some(it) = items.iter_mut().find(|it| it["k"] == "door" && it["id"].as_i64() == Some(*i)) {
                it["pd"] = json!(pd);
            }
        }
    }
    pairs
}

fn inset(b: &Bx) -> Bx {
    let mut r = *b;
    for k in 0..3 {
        r[0][k] += CAR_INSET[k];
        r[1][k] -= CAR_INSET[k];
    }
    r
}

/// World box swept by the car's rider box (car-local) between the stop matrices `a` and `b`: the union of the box at both stops.
pub fn car_sweep(b: &Bx, a: &Mat, c: &Mat) -> Bx {
    let (ba, bb) = (box_xf(a, &inset(b)), box_xf(c, &inset(b)));
    let mut r = ba;
    for k in 0..3 {
        r[0][k] = f64::min(ba[0][k], bb[0][k]);
        r[1][k] = f64::max(ba[1][k], bb[1][k]);
    }
    r
}

/// Ids of the `doors` the car crosses on the trip between two stop matrices: inside the swept box, outside the car box at either end, not in `skip`.
pub fn car_path_doors(b: &Bx, a: &Mat, c: &Mat, doors: &[(i64, Bx)], skip: &[i64]) -> Vec<i64> {
    let sw = car_sweep(b, a, c);
    let ends = [box_xf(a, &inset(b)), box_xf(c, &inset(b))];
    let mut v: Vec<i64> = doors
        .iter()
        .filter(|(i, bx)| !skip.contains(i) && intersects(bx, &sw) && !ends.iter().any(|e| intersects(bx, e)))
        .map(|(i, _)| *i)
        .collect();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hatch_under_a_platform_is_on_its_path() {
        let parked: Bx = [[0.0, 0.0, 0.0], [2.0, 2.0, 0.2]];
        let swept: Bx = [[0.0, 0.0, -3.0], [2.0, 2.0, 0.2]];
        let hatch: Bx = [[0.0, 0.0, -2.0], [2.0, 2.0, -1.8]];
        let wall: Bx = [[5.0, 0.0, -2.0], [6.0, 2.0, -1.0]];
        assert_eq!(path_doors(&parked, &swept, &[(7, hatch), (8, wall)]), vec![7]);
    }

    #[test]
    fn link_lift_paths_marks_only_travelling_doors() {
        let mut items = vec![
            json!({"id": 1, "k": "door", "bx": [[0, 0, 0], [2, 2, 0.2]], "sw": [[0, 0, -3], [2, 2, 0.2]]}),
            json!({"id": 2, "k": "door", "bx": [[0, 0, -2], [2, 2, -1.8]]}),
        ];
        assert_eq!(link_lift_paths(&mut items, &[1]), 1);
        assert_eq!(items[0]["pd"], json!([2]));
        assert!(items[1].get("pd").is_none());
    }
}
