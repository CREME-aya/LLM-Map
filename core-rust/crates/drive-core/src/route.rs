//! The committed route, loaded from a `RouteBundle` (docs 04 §3.1) and indexed for
//! on-route alignment. Integer-only (see `geo`).
//!
//! - Segments are numbered in route order across legs and chunks; each segment belongs to
//!   one chunk and is evaluated in that chunk's plane.
//! - Each chunk is valid for a fix inside the bounding box of its points plus
//!   `CHUNK_MARGIN_CM` (1 km).
//! - Spatial index: per chunk, a uniform grid of `GRID_CELL_CM` cells listing the segments
//!   whose bounding box touches the cell (`BTreeMap`, so iteration is deterministic).
#![deny(clippy::float_arithmetic, clippy::cast_precision_loss)]

use crate::geo::{lerp_u64, LocalFrame};
use contracts::horizon::Maneuver;
use contracts::route::{RouteBundle, K_Q16_MAX};
use contracts::tick::BEARING_CDEG_FULL;
use contracts::RouteKey;
use std::collections::BTreeMap;

/// Margin added around a chunk's bounding box (04 §3.1: 1 km).
pub const CHUNK_MARGIN_CM: i64 = 100_000;
/// Grid cell of the per-chunk spatial index (provisional: 500 m).
pub const GRID_CELL_CM: i64 = 50_000;

#[derive(Debug, Clone)]
pub struct ChunkGeo {
    pub frame: LocalFrame,
    /// Valid range (bounding box + margin), chunk-local cm.
    pub min: (i64, i64),
    pub max: (i64, i64),
    grid: BTreeMap<(i64, i64), Vec<u32>>,
}

impl ChunkGeo {
    pub fn contains(&self, p: (i64, i64)) -> bool {
        p.0 >= self.min.0 && p.0 <= self.max.0 && p.1 >= self.min.1 && p.1 <= self.max.1
    }

    /// Segments whose bounding box touches the square `p ± r` (sorted, unique).
    pub fn query(&self, p: (i64, i64), r: i64) -> Vec<u32> {
        let (cx0, cy0) = cell((p.0 - r, p.1 - r));
        let (cx1, cy1) = cell((p.0 + r, p.1 + r));
        let mut out = Vec::new();
        for cx in cx0..=cx1 {
            for cy in cy0..=cy1 {
                if let Some(v) = self.grid.get(&(cx, cy)) {
                    out.extend_from_slice(v);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

fn cell(p: (i64, i64)) -> (i64, i64) {
    (p.0.div_euclid(GRID_CELL_CM), p.1.div_euclid(GRID_CELL_CM))
}

/// One shape segment in its chunk's plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seg {
    pub chunk: u32,
    pub a: (i64, i64),
    pub b: (i64, i64),
    /// `along_route_cm` at `a` / `b` (route-wide; from the bundle's `seg_along_cm`).
    pub along_a: u32,
    pub along_b: u32,
    pub bearing_cdeg: u16,
    pub leg_index: u32,
    /// `leg_shape_index` of `a`.
    pub shape_index: u32,
}

/// Edge timing on the route-wide axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EdgeSpan {
    /// Directed edge id (`legs[].edges[].edge_id`).
    edge_id: u64,
    along_start: u32,
    along_end: u32,
    flags: u32,
    time_start: u32,
    time_end: u32,
}

#[derive(Debug, Clone)]
pub struct LoadedRoute {
    pub route_key: RouteKey,
    pub eta_ms: u32,
    pub length_cm: u32,
    pub chunks: Vec<ChunkGeo>,
    pub segs: Vec<Seg>,
    /// All maneuvers in route order (validated non-decreasing `anchor.along_route_cm`).
    pub maneuvers: Vec<Maneuver>,
    edges: Vec<EdgeSpan>,
    group_heads: Vec<Option<usize>>,
    /// (anchor distance, earliest P1 time offset among this anchor and all following it).
    p1_deadlines: Vec<(u32, i64)>,
}

impl LoadedRoute {
    /// Validates and indexes a bundle. Errors are human-readable (→ `E_SCHEMA`).
    pub fn load(b: &RouteBundle) -> Result<Self, String> {
        let mut chunks = Vec::new();
        let mut segs: Vec<Seg> = Vec::new();
        let mut edges = Vec::new();
        let mut maneuvers: Vec<Maneuver> = Vec::new();
        let mut route_along: u64 = 0;
        let mut prev_time: u32 = 0;
        if b.legs.is_empty() {
            return Err("legs: empty".into());
        }
        for (li, leg) in b.legs.iter().enumerate() {
            let at = |m: &str| format!("legs[{li}]: {m}");
            if leg.leg_index as usize != li {
                return Err(at("leg_index != position"));
            }
            if u64::from(leg.along_route_start_cm) != route_along {
                return Err(at("along_route_start_cm != sum of previous leg lengths"));
            }
            if leg.chunks.is_empty() {
                return Err(at("chunks: empty"));
            }
            // Leg-internal along of every shape index.
            let mut shape_along: Vec<u32> = Vec::new();
            for (ci, c) in leg.chunks.iter().enumerate() {
                let at = |m: &str| format!("legs[{li}].chunks[{ci}]: {m}");
                let n = c.points_cm.len();
                if n < 2 {
                    return Err(at("points_cm: fewer than 2 points"));
                }
                if c.seg_along_cm.len() != n {
                    return Err(at("seg_along_cm length != points_cm length"));
                }
                if c.bearing_cdeg.len() != n - 1 {
                    return Err(at("bearing_cdeg length != points - 1"));
                }
                if c.bearing_cdeg.iter().any(|&d| d >= BEARING_CDEG_FULL) {
                    return Err(at("bearing_cdeg out of range (0..=35999)"));
                }
                if c.k_lat_q16 == 0
                    || c.k_lon_q16 == 0
                    || c.k_lat_q16 > K_Q16_MAX
                    || c.k_lon_q16 > K_Q16_MAX
                {
                    return Err(at("k_*_q16 out of range (1..=2^20)"));
                }
                if c.seg_along_cm.windows(2).any(|w| w[1] < w[0]) {
                    return Err(at("seg_along_cm decreases"));
                }
                // Chunks share their boundary point: same shape index and along.
                let expect_first = if ci == 0 {
                    0
                } else {
                    shape_along.len() as u32 - 1
                };
                if c.first_shape_index != expect_first {
                    return Err(at("first_shape_index does not continue the previous chunk"));
                }
                match shape_along.last() {
                    None if c.seg_along_cm[0] != 0 => {
                        return Err(at("seg_along_cm[0] of the first chunk != 0"))
                    }
                    Some(&last) if last != c.seg_along_cm[0] => {
                        return Err(at("seg_along_cm not continuous at the chunk boundary"))
                    }
                    _ => {}
                }
                if ci > 0 {
                    shape_along.pop();
                }
                shape_along.extend_from_slice(&c.seg_along_cm);

                let chunk_no = chunks.len() as u32;
                let pts: Vec<(i64, i64)> = c
                    .points_cm
                    .iter()
                    .map(|p| (i64::from(p[0]), i64::from(p[1])))
                    .collect();
                let (mut min, mut max) = (pts[0], pts[0]);
                for p in &pts {
                    min = (min.0.min(p.0), min.1.min(p.1));
                    max = (max.0.max(p.0), max.1.max(p.1));
                }
                let mut grid: BTreeMap<(i64, i64), Vec<u32>> = BTreeMap::new();
                for i in 0..n - 1 {
                    let seg_no = segs.len() as u32;
                    let (a, bb) = (pts[i], pts[i + 1]);
                    let (c0, c1) = (
                        cell((a.0.min(bb.0), a.1.min(bb.1))),
                        cell((a.0.max(bb.0), a.1.max(bb.1))),
                    );
                    for cx in c0.0..=c1.0 {
                        for cy in c0.1..=c1.1 {
                            grid.entry((cx, cy)).or_default().push(seg_no);
                        }
                    }
                    let along = |v: u32| -> Result<u32, String> {
                        u32::try_from(route_along + u64::from(v))
                            .map_err(|_| at("along_route_cm overflows u32"))
                    };
                    segs.push(Seg {
                        chunk: chunk_no,
                        a,
                        b: bb,
                        along_a: along(c.seg_along_cm[i])?,
                        along_b: along(c.seg_along_cm[i + 1])?,
                        bearing_cdeg: c.bearing_cdeg[i],
                        leg_index: leg.leg_index,
                        shape_index: c.first_shape_index + i as u32,
                    });
                }
                chunks.push(ChunkGeo {
                    frame: LocalFrame {
                        origin_lat_e7: c.origin_e7.lat,
                        origin_lon_e7: c.origin_e7.lon,
                        k_lat_q16: c.k_lat_q16,
                        k_lon_q16: c.k_lon_q16,
                    },
                    min: (min.0 - CHUNK_MARGIN_CM, min.1 - CHUNK_MARGIN_CM),
                    max: (max.0 + CHUNK_MARGIN_CM, max.1 + CHUNK_MARGIN_CM),
                    grid,
                });
            }
            let last_shape = shape_along.len() as u32 - 1;
            if *shape_along.last().expect("non-empty") != leg.length_cm {
                return Err(at("length_cm != seg_along_cm of the last point"));
            }
            // Edges cover the leg shape without gaps.
            if leg.edges.is_empty() {
                return Err(at("edges: empty"));
            }
            let mut next_from = 0u32;
            for (ei, e) in leg.edges.iter().enumerate() {
                let at = |m: &str| format!("legs[{li}].edges[{ei}]: {m}");
                if e.from_pt != next_from || e.to_pt <= e.from_pt || e.to_pt > last_shape {
                    return Err(at("edges must cover the shape in order without gaps"));
                }
                let (a0, a1) = (
                    shape_along[e.from_pt as usize],
                    shape_along[e.to_pt as usize],
                );
                if a1 - a0 != e.length_cm {
                    return Err(at(
                        "length_cm != seg_along_cm[to_pt] - seg_along_cm[from_pt]",
                    ));
                }
                if e.cum_time_ms < prev_time {
                    return Err(at("cum_time_ms decreases"));
                }
                edges.push(EdgeSpan {
                    edge_id: e.edge_id.0,
                    along_start: (route_along + u64::from(a0)) as u32,
                    along_end: (route_along + u64::from(a1)) as u32,
                    flags: e.flags,
                    time_start: prev_time,
                    time_end: e.cum_time_ms,
                });
                prev_time = e.cum_time_ms;
                next_from = e.to_pt;
            }
            if next_from != last_shape {
                return Err(at("edges do not reach the last shape point"));
            }
            for (mi, m) in leg.maneuvers.iter().enumerate() {
                let at = |s: &str| format!("legs[{li}].maneuvers[{mi}]: {s}");
                if m.id.route_key != b.route_key {
                    return Err(at("id.route_key != bundle route_key"));
                }
                if m.id.leg_index != leg.leg_index || m.anchor.leg_index != leg.leg_index {
                    return Err(at("leg_index mismatch"));
                }
                let si = m.anchor.leg_shape_index;
                if si > last_shape {
                    return Err(at("anchor.leg_shape_index beyond the leg shape"));
                }
                // The anchor lies on the segment starting at its shape index.
                let lo = route_along + u64::from(shape_along[si as usize]);
                let hi = route_along
                    + u64::from(shape_along[(si as usize + 1).min(last_shape as usize)]);
                let along = u64::from(m.anchor.along_route_cm);
                if along < lo || along > hi {
                    return Err(at("anchor.along_route_cm not on its shape segment"));
                }
                if let Some(exit) = m.roundabout_leave_anchor {
                    let idx = exit.leg_shape_index as usize;
                    if !matches!(m.arrow, contracts::horizon::Arrow::Roundabout { .. })
                        || exit.leg_index != leg.leg_index
                        || idx > last_shape as usize
                        || exit.along_route_cm < m.anchor.along_route_cm
                        || !(route_along + u64::from(shape_along[idx])
                            ..=route_along
                                + u64::from(shape_along[(idx + 1).min(last_shape as usize)]))
                            .contains(&u64::from(exit.along_route_cm))
                    {
                        return Err(at("invalid roundabout leave anchor"));
                    }
                }
                if let Some(prev) = maneuvers.last() {
                    if m.anchor.along_route_cm < prev.anchor.along_route_cm
                        || (prev.id.leg_index == m.id.leg_index && m.id.ordinal <= prev.id.ordinal)
                    {
                        return Err(at("maneuvers out of route order"));
                    }
                }
                maneuvers.push(m.clone());
            }
            route_along += u64::from(leg.length_cm);
        }
        if route_along != u64::from(b.length_cm) {
            return Err("length_cm != sum of leg lengths".into());
        }
        if prev_time != b.eta_ms {
            return Err("eta_ms != cum_time_ms of the last edge".into());
        }
        let mut route = Self {
            group_heads: vec![None; maneuvers.len()],
            p1_deadlines: Vec::new(),
            route_key: b.route_key.clone(),
            eta_ms: b.eta_ms,
            length_cm: b.length_cm,
            chunks,
            segs,
            maneuvers,
            edges,
        };
        route.index_groups();
        route.index_p1_deadlines();
        Ok(route)
    }

    /// Replaces the route key of the route and of every `ManeuverId` (the revision Rust
    /// assigns in `dc_route_commit`). Group heads are indices, so `group_id`s follow.
    pub fn assign_route_key(&mut self, key: RouteKey) {
        for m in &mut self.maneuvers {
            m.id.route_key = key.clone();
        }
        self.route_key = key;
    }

    /// `(edge_id, along_start_cm, along_end_cm)` of every edge in route order.
    pub fn edge_spans(&self) -> Vec<(u64, u32, u32)> {
        self.edges
            .iter()
            .map(|e| (e.edge_id, e.along_start, e.along_end))
            .collect()
    }

    pub(crate) fn motorway_approach(&self, along: u32) -> bool {
        let edge = self.edges.partition_point(|e| e.along_end < along);
        self.edges
            .get(edge)
            .is_some_and(|e| e.flags & contracts::route::EDGE_FLAG_MOTORWAY != 0)
    }

    fn index_groups(&mut self) {
        use contracts::horizon::Arrow;
        let mut i = 0;
        while i + 1 < self.maneuvers.len() {
            let a = &self.maneuvers[i];
            let b = &self.maneuvers[i + 1];
            let start = a.anchor.along_route_cm;
            let end = b.anchor.along_route_cm;
            let limit_cm = if self.motorway_approach(end) {
                20_000
            } else {
                12_000
            };
            let branch = |m: &Maneuver| {
                !matches!(
                    m.arrow,
                    Arrow::Arrive | Arrow::FerryBoard | Arrow::FerryLeave | Arrow::Generic
                )
            };
            // Legs bound stops in the available route body. Do not group across that boundary.
            if a.id.leg_index == b.id.leg_index
                && branch(a)
                && branch(b)
                && (end - start <= limit_cm || self.time_at(end) - self.time_at(start) <= 12_000)
            {
                self.group_heads[i..=i + 1].fill(Some(i));
                i += 2;
            } else {
                i += 1;
            }
        }
    }

    pub(crate) fn group_head(&self, index: usize) -> Option<&Maneuver> {
        self.group_heads[index].map(|head| &self.maneuvers[head])
    }

    /// `index` belongs to a `group_id` of two or more maneuvers.
    pub(crate) fn grouped(&self, index: usize) -> bool {
        self.group_heads[index].is_some()
    }

    /// Index of the last maneuver of the group of `index` (`index` itself when ungrouped).
    pub(crate) fn group_last(&self, index: usize) -> usize {
        let Some(head) = self.group_heads[index] else {
            return index;
        };
        let mut last = index;
        while self.group_heads.get(last + 1) == Some(&Some(head)) {
            last += 1;
        }
        last
    }

    fn index_p1_deadlines(&mut self) {
        let mut deadlines: Vec<_> = self
            .maneuvers
            .iter()
            .flat_map(|m| std::iter::once(m.anchor).chain(m.roundabout_leave_anchor))
            .map(|a| {
                let along = a.along_route_cm;
                let lead = if self.motorway_approach(along) {
                    20_000
                } else {
                    8_000
                };
                (along, i64::from(self.time_at(along)) - lead)
            })
            .collect();
        deadlines.sort_unstable_by_key(|&(along, _)| along);
        // A following motorway turn can need speech before the preceding city turn.
        let mut earliest = i64::MAX;
        for (_, deadline) in deadlines.iter_mut().rev() {
            earliest = earliest.min(*deadline);
            *deadline = earliest;
        }
        self.p1_deadlines = deadlines;
    }

    pub(crate) fn next_p1_deadline(
        &self,
        passed: Option<u32>,
        progress: u32,
        sampled: u64,
    ) -> Option<u64> {
        let first = passed.map_or(0, |p| {
            self.p1_deadlines.partition_point(|&(along, _)| along <= p)
        });
        let (_, offset) = self.p1_deadlines.get(first)?;
        Some(sampled.saturating_add_signed(offset - i64::from(self.time_at(progress))))
    }

    /// Travel time from the route start to `along_cm` (ms), interpolated inside the edge.
    pub fn time_at(&self, along_cm: u32) -> u32 {
        let i = self.edges.partition_point(|e| e.along_end < along_cm);
        let Some(e) = self.edges.get(i) else {
            return self.eta_ms;
        };
        let t = lerp_u64(
            u64::from(e.time_start),
            u64::from(e.time_end),
            u64::from(along_cm.saturating_sub(e.along_start)),
            u64::from(e.along_end - e.along_start),
        );
        t as u32
    }

    /// Remaining travel time from `along_cm` to the end (ms).
    pub fn remaining_ms(&self, along_cm: u32) -> u32 {
        self.eta_ms.saturating_sub(self.time_at(along_cm))
    }
}
