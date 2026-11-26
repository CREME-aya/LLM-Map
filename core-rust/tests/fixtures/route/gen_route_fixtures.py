#!/usr/bin/env python3
"""Generates the on-route alignment fixtures in this directory (GPX-like fix tracks).

This script plays the roles that are NOT Rust (docs 04 §3.1):
- C++ (route side): splits each leg into chunks, picks the chunk origin, computes the
  fixed-point factors k_lat_q16 / k_lon_q16 with cos() once, the integer points, the
  per-point along distance and the per-segment bearing_cdeg (atan2), the edges and the
  maneuvers.
- Kotlin (fix side): rounds each simulated Location exactly once
  (lat_e7 = Math.round(deg * 1e7), bearing_cdeg = Math.round(deg * 100) % 36000, cm, cm/s).
Floating point is used only here; Rust consumes the integers.

Run from anywhere:  python3 "core-rust/tests/fixtures/route/gen_route_fixtures.py"
The output is deterministic (no randomness beyond a fixed LCG); commit the JSON files.
"""
import json
import math
import os

HERE = os.path.dirname(os.path.abspath(__file__))
TICK_DIR = os.path.join(HERE, "..", "tick")

EARTH_R_M = 6378137.0
CM_PER_E7_LAT = 2 * math.pi * EARTH_R_M / 360 / 1e7 * 100  # ~1.11319 cm
M_PER_DEG_LAT = CM_PER_E7_LAT * 1e7 / 100
REF = (35.0, 139.0)  # local metric frame reference for the simulated geometry
GRAPH = "jp-2026.11.0"


def kround(x):
    """Java/Kotlin Math.round (half up), not Python's banker's rounding."""
    return math.floor(x + 0.5)


def round_q16(v):
    return (v + (1 << 15)) >> 16  # Python >> on ints is arithmetic, like Rust i64


def m_to_e7(x_m, y_m):
    lat = REF[0] + y_m / M_PER_DEG_LAT
    lon = REF[1] + x_m / (M_PER_DEG_LAT * math.cos(math.radians(REF[0])))
    return kround(lat * 1e7), kround(lon * 1e7)


def route_key(rev, trip="t1", trip_rev="3"):
    return {"trip_id": trip, "trip_revision": trip_rev, "route_revision": str(rev), "graph_version": GRAPH}


# A bundle passed to dc_route_commit always carries route_revision "0" (team decision
# 2026-11-26): Rust assigns current + 1 (the first route of a session: 1) and rewrites
# every route_key inside the bundle. BUNDLE_KEY is that "0" key.
BUNDLE_KEY = route_key(0)


def committed(rk, rev):
    """The key Rust assigns when the bundle with key rk becomes route revision rev."""
    return dict(rk, route_revision=str(rev))


def evidence_unknown():
    return {"kind": "unknown", "reason": "missing"}


def maneuver(rk, leg_index, ordinal, shape_index, along, arrow):
    return {
        "id": {"route_key": rk, "leg_index": leg_index, "ordinal": ordinal},
        "anchor": {"leg_index": leg_index, "leg_shape_index": shape_index, "along_route_cm": along},
        "arrow": arrow,
        "side_of_traffic": "left",
        "crossing": evidence_unknown(),
        "approach_signal": evidence_unknown(),
        "right_turn_protection": evidence_unknown(),
        "approach_width_mm": evidence_unknown(),
        "departure_width_mm": evidence_unknown(),
        "advisory_flags": [],
    }


def densify(pts_m, step_m):
    """Inserts vertices so that no segment is longer than step_m."""
    out = [pts_m[0]]
    for (x0, y0), (x1, y1) in zip(pts_m, pts_m[1:]):
        n = max(1, math.ceil(math.hypot(x1 - x0, y1 - y0) / step_m))
        for i in range(1, n + 1):
            out.append((x0 + (x1 - x0) * i / n, y0 + (y1 - y0) * i / n))
    return out


def build_bundle(rk, legs, speed_mps, chunk_len_m=20000.0, pts_per_edge=4, flags_at=None,
                 eta_ms=None):
    """legs: list of {"pts": [(x_m, y_m)], "maneuvers": [(shape_index, arrow)]}.
    flags_at(x_m, y_m) -> edge flags for an edge starting there. eta_ms rescales times."""
    out_legs = []
    route_along = 0
    raw_edges = []
    for li, leg in enumerate(legs):
        e7 = [m_to_e7(x, y) for x, y in leg["pts"]]
        # --- chunks (C++): cut at vertices every ~chunk_len_m, boundary point shared
        chunks = []
        start = 0
        acc = 0.0
        for i in range(1, len(e7)):
            (x0, y0), (x1, y1) = leg["pts"][i - 1], leg["pts"][i]
            acc += math.hypot(x1 - x0, y1 - y0)
            if acc >= chunk_len_m or i == len(e7) - 1:
                chunks.append((start, i))
                start, acc = i, 0.0
        shape_along = [0]
        out_chunks = []
        for (a, b) in chunks:
            olat, olon = e7[a]
            k_lat = kround(CM_PER_E7_LAT * 65536)
            k_lon = kround(CM_PER_E7_LAT * math.cos(math.radians(olat / 1e7)) * 65536)
            pts = [[round_q16((lon - olon) * k_lon), round_q16((lat - olat) * k_lat)] for lat, lon in e7[a:b + 1]]
            seg_along = [shape_along[-1]]
            bearings = []
            for (px, py), (qx, qy) in zip(pts, pts[1:]):
                seg_along.append(seg_along[-1] + kround(math.hypot(qx - px, qy - py)))
                bearings.append(kround(math.degrees(math.atan2(qx - px, qy - py)) * 100) % 36000)
            shape_along.extend(seg_along[1:])
            out_chunks.append({
                "first_shape_index": a,
                "origin_e7": {"lat": olat, "lon": olon},
                "k_lat_q16": k_lat,
                "k_lon_q16": k_lon,
                "points_cm": pts,
                "seg_along_cm": seg_along,
                "bearing_cdeg": bearings,
            })
        # --- edges: every pts_per_edge segments
        edges = []
        last = len(e7) - 1
        p = 0
        while p < last:
            q = min(p + pts_per_edge, last)
            x, y = leg["pts"][p]
            edges.append({"from_pt": p, "to_pt": q, "length_cm": shape_along[q] - shape_along[p],
                          "flags": flags_at(x, y) if flags_at else 0,
                          "_along_end": route_along + shape_along[q]})
            p = q
        raw_edges.extend(edges)
        mans = []
        for ordinal, (si, arrow) in enumerate(leg.get("maneuvers", [])):
            mans.append(maneuver(rk, li, ordinal, si, route_along + shape_along[si], arrow))
        out_legs.append({"leg_index": li, "along_route_start_cm": route_along,
                         "length_cm": shape_along[-1], "chunks": out_chunks, "edges": edges,
                         "maneuvers": mans})
        route_along += shape_along[-1]
    total_ms = eta_ms if eta_ms is not None else kround(route_along / (speed_mps * 100) * 1000)
    for i, e in enumerate(raw_edges):
        e["edge_id"] = str(1000 + i)
        e["cum_time_ms"] = route_along and (total_ms * e["_along_end"]) // route_along
        del e["_along_end"]
    raw_edges[-1]["cum_time_ms"] = total_ms
    # key order for readability
    for leg in out_legs:
        leg["edges"] = [{k: e[k] for k in ("edge_id", "from_pt", "to_pt", "length_cm", "cum_time_ms", "flags")}
                        for e in leg["edges"]]
    return {"route_key": rk, "eta_ms": total_ms, "length_cm": route_along, "legs": out_legs}


def fix(t_ms, x_m, y_m, acc_m=5.0, bearing_deg=None, speed_mps=None):
    lat_e7, lon_e7 = m_to_e7(x_m, y_m)
    f = {"at_mono_ms": t_ms, "lat_e7": lat_e7, "lon_e7": lon_e7, "acc_cm": kround(acc_m * 100)}
    if bearing_deg is not None:
        f["bearing_cdeg"] = kround((bearing_deg % 360) * 100) % 36000
    if speed_mps is not None:
        f["speed_cm_s"] = kround(speed_mps * 100)
    return f


def tick(t_ms, f=None, **expect):
    t = {"input": {"now_mono_ms": t_ms}}
    if f is not None:
        t["input"]["fix"] = f
    if expect:
        t["expect"] = expect
    return t


class Lcg:
    """Deterministic noise (no dependency on Python's random)."""

    def __init__(self, seed):
        self.s = seed

    def unit(self):  # [-1, 1)
        self.s = (self.s * 6364136223846793005 + 1442695040888963407) % (1 << 64)
        return ((self.s >> 11) / float(1 << 53)) * 2 - 1


def drive(path_m, speed_mps, t0_ms, dt_ms=1000, lateral=None, acc=None, heading_noise=None):
    """Yields (t_ms, x, y, bearing_deg) every dt_ms along a polyline at constant speed.
    lateral(i, s) -> offset (m, left positive) applied perpendicular to the path."""
    seglens = [math.hypot(b[0] - a[0], b[1] - a[1]) for a, b in zip(path_m, path_m[1:])]
    total = sum(seglens)
    i = 0
    s = 0.0
    while s <= total + 1e-9:
        # locate s on the path
        rem = s
        k = 0
        while k < len(seglens) - 1 and rem > seglens[k]:
            rem -= seglens[k]
            k += 1
        (x0, y0), (x1, y1) = path_m[k], path_m[k + 1]
        ux, uy = (x1 - x0) / seglens[k], (y1 - y0) / seglens[k]
        x, y = x0 + ux * rem, y0 + uy * rem
        off = lateral(i, s) if lateral else 0.0
        x, y = x - uy * off, y + ux * off
        brg = math.degrees(math.atan2(ux, uy))
        if heading_noise:
            brg += heading_noise(i)
        yield t0_ms + i * dt_ms, x, y, brg
        i += 1
        s += speed_mps * dt_ms / 1000


def write(name, obj, directory=HERE):
    path = os.path.join(directory, name)
    with open(path, "w", encoding="utf-8") as f:
        json.dump(obj, f, ensure_ascii=False, separators=(",", ":"))
        f.write("\n")
    print("wrote", os.path.relpath(path, HERE))


def straight(length_m, step_m=250.0):
    return densify([(0.0, 0.0), (length_m, 0.0)], step_m)


# ---------------------------------------------------------------------------------------
def gen_straight_base():
    """Base route for tests that only need a route with a known remaining ETA."""
    rk = BUNDLE_KEY
    pts = straight(3000.0)
    b = build_bundle(rk, [{"pts": pts, "maneuvers": [(len(pts) - 1, "arrive")]}], speed_mps=10.0)
    write("straight-3km.json", {
        "description": "Straight 3 km route east (one leg, one chunk, 250 m segments, 4 segments per edge, 10 m/s). Base for planner tests that need a remaining ETA: tests rescale cum_time_ms / eta_ms and use legs[0].chunks[0].origin_e7 as the start fix.",
        "bundle": b,
    })


def gen_chunk_boundary():
    """Two legs, several chunks, bends; continuous along distance across boundaries."""
    rk = BUNDLE_KEY
    leg0 = densify([(0, 0), (1200, 900), (2400, 900), (3000, 2200)], 100.0)
    leg1 = densify([(3000, 2200), (4200, 2600), (5200, 1800)], 100.0)
    # maneuvers at the bends (shape indices of the vertices)
    def vertex_index(pts, v):
        return min(range(len(pts)), key=lambda i: math.hypot(pts[i][0] - v[0], pts[i][1] - v[1]))
    m0 = [(vertex_index(leg0, (1200, 900)), "right"), (vertex_index(leg0, (2400, 900)), "left")]
    m1 = [(vertex_index(leg1, (4200, 2600)), "right"), (len(leg1) - 1, "arrive")]
    b = build_bundle(rk, [{"pts": leg0, "maneuvers": m0}, {"pts": leg1, "maneuvers": m1}],
                     speed_mps=15.0, chunk_len_m=1500.0)
    path = leg0 + leg1[1:]
    ticks = [tick(0)]
    for t, x, y, brg in drive(path, 20.0, 1000, lateral=lambda i, s: 2.0 * math.sin(i / 7.0)):
        ticks.append(tick(t, fix(t, x, y, 5.0, brg, 20.0)))
    # one last fix 5 m past the destination (projection clamps to the route end)
    t_end = ticks[-1]["input"]["now_mono_ms"] + 1000
    (xa, ya), (xb, yb) = path[-2], path[-1]
    d = math.hypot(xb - xa, yb - ya)
    xe, ye = xb + (xb - xa) / d * 5, yb + (yb - ya) / d * 5
    brg = math.degrees(math.atan2(xb - xa, yb - ya))
    ticks.append(tick(t_end, fix(t_end, xe, ye, 5.0, brg, 20.0), status="tracking"))
    t_end += 1000
    ticks.append(tick(t_end, fix(t_end, xe, ye, 5.0, brg, 20.0), status="finished"))
    ticks[1]["expect"] = {"status": "tracking"}
    write("gate7-chunk-boundary.json", {
        "description": "Gate 7 / 07 §2.4 case 1 (long-distance chunk boundary, scaled down): 2 legs, 1.5 km chunks (the 20 km target is exercised by the Rust unit test), bends with right/left maneuvers, 20 m/s with +-2 m lateral wobble. Expect tracking all the way, progress continuous across chunk and leg boundaries (no jump > 21 m per 1 s fix), 0 off-route, finished at the end.",
        "bundle": b, "ticks": ticks,
    })


def main_line_bundle(rev=0, length_m=4000.0, maneuver_xs=(1500.0, 2500.0, 3500.0)):
    rk = route_key(rev)
    pts = straight(length_m, 100.0)
    mans = [(int(x // 100), arrow) for x, arrow in zip(maneuver_xs, ("left", "right", "left"))]
    mans.append((len(pts) - 1, "arrive"))
    return build_bundle(rk, [{"pts": pts, "maneuvers": mans}], speed_mps=15.0)


def gen_side_road_20m():
    b = main_line_bundle()
    ticks = [tick(0)]
    path = [(0, 0), (300, 0), (400, 20), (2000, 20), (2100, 0), (4000, 0)]
    for t, x, y, brg in drive(path, 15.0, 1000):
        if x > 3000:
            break
        ticks.append(tick(t, fix(t, x, y, 5.0, brg, 15.0), status="tracking", reroute=False))
    write("gate7-side-road-20m.json", {
        "description": "Gate 7 (added 2026-11-25): a frontage road 20 m from the main line in the same direction (x 400..2000 m). 20 m < lateral_tol 30 m, so it is NOT detected (recorded as a known limit): status stays tracking, no reroute. The vehicle returns to the main line at x 2100 m.",
        "bundle": b, "ticks": ticks,
    })


def gen_lateral_boundary():
    b = main_line_bundle()
    c = b["legs"][0]["chunks"][0]
    olat, olon = c["origin_e7"]["lat"], c["origin_e7"]["lon"]
    k_lat = c["k_lat_q16"]
    # smallest lat offset n (e7) whose dy rounds to exactly 3000 cm, and the next one (3001)
    n30 = next(n for n in range(2600, 2800) if round_q16(n * k_lat) == 3000)
    n31 = next(n for n in range(n30, n30 + 10) if round_q16(n * k_lat) == 3001)
    lon_at = lambda x_m: m_to_e7(x_m, 0.0)[1]
    def f(t, x, n):
        return {"at_mono_ms": t, "lat_e7": olat + n, "lon_e7": lon_at(x), "acc_cm": 500,
                "bearing_cdeg": 9000, "speed_cm_s": 1000}
    ticks = [tick(0), tick(1000, f(1000, 100, 0), status="tracking")]
    t = 2000
    x = 110.0
    for _ in range(5):  # exactly 30.00 m: on the route
        ticks.append(tick(t, f(t, x, n30), status="tracking", off_run=0, reroute=False))
        t += 1000; x += 10
    for i in range(3):  # 30.01 m: off; confirmed at the 3rd
        last = i == 2
        ticks.append(tick(t, f(t, x, n31), off_run=i + 1, reroute=last,
                          status="rerouting" if last else "tracking"))
        t += 1000; x += 10
    write("gate7-lateral-30m-boundary.json", {
        "description": f"Gate 7 (added 2026-11-25): lateral offset exactly at lateral_tol_cm. dy = round_q16(n * k_lat_q16): n={n30} -> 3000 cm (on the route, <= tol), n={n31} -> 3001 cm (off). 5 fixes at 30.00 m keep tracking; 3 fixes at 30.01 m confirm off-route at the 3rd and start the reroute with that fix.",
        "bundle": b, "ticks": ticks,
    })


def gen_branch_40m():
    b = main_line_bundle()
    ticks = [tick(0)]
    path = [(0, 0), (1000, 0), (1200, 40), (2500, 40)]
    for t, x, y, brg in drive(path, 15.0, 1000):
        ticks.append(tick(t, fix(t, x, y, 5.0, brg, 15.0)))
    write("gate7-branch-40m.json", {
        "description": "Gate 7 (added 2026-11-25): a side road branching off at x 1000 m and reaching 40 m from the main line at x 1200 m (then parallel). 15 m/s, 1 Hz. The test records how many fixes after the branch point the off-route is confirmed (lateral > 30 m for 3 consecutive fixes) and checks that the reroute start carries the confirming fix.",
        "bundle": b, "ticks": ticks,
    })


def gen_jitter_main():
    b = main_line_bundle()
    noise = Lcg(20261125)
    ticks = [tick(0)]
    path = [(0, 0), (3900, 0)]
    for t, x, y, brg in drive(path, 15.0, 1000, lateral=lambda i, s: 25.0 * noise.unit(),
                              heading_noise=lambda i: 20.0 * noise.unit()):
        i = (t - 1000) // 1000
        acc = 8.0 + 7.0 * abs(noise.unit())
        if i % 37 == 5:
            acc = 45.0  # occasional poor-accuracy fix (> max_acc_cm): not used
        ticks.append(tick(t, fix(t, x, y, acc, brg, 15.0), reroute=False))
    write("gate7-jitter-main.json", {
        "description": "Gate 7 (added 2026-11-25): the vehicle stays on the main line while the position jitters (lateral noise up to +-25 m, heading noise up to +-20 deg, accuracy 8..15 m, an occasional 45 m accuracy fix). Expect 0 off-route detections and no reroute.",
        "bundle": b, "ticks": ticks,
    })


def gen_off_run_reset():
    b = main_line_bundle()
    ticks = [tick(0), tick(1000, fix(1000, 100, 0, 5.0, 90, 10.0), status="tracking")]
    ticks.append(tick(2000, fix(2000, 110, 60, 5.0, 90, 10.0), off_run=1, reroute=False))
    ticks.append(tick(3000, fix(3000, 120, 60, 5.0, 90, 10.0), off_run=2, reroute=False))
    ticks.append(tick(4000, fix(4000, 130, 60, 40.0, 90, 10.0), off_run=0, low_accuracy_run=1, reroute=False))
    ticks.append(tick(5000, fix(5000, 140, 60, 40.0, 90, 10.0), off_run=0, low_accuracy_run=2, reroute=False))
    ticks.append(tick(6000, fix(6000, 150, 60, 5.0, 90, 10.0), off_run=1, low_accuracy_run=0, reroute=False))
    write("gate7-off-run-reset.json", {
        "description": "Gate 7 (added 2026-11-25): valid off-route fix x2 -> low-accuracy fix (acc 40 m > max_acc_cm 30 m) x2 -> valid off-route fix x1. A low-accuracy fix resets the off-route run to 0 (the run must be confirm_fixes consecutive valid fixes), so at the end the run is 1 and no reroute start is emitted.",
        "bundle": b, "ticks": ticks,
    })


def gen_off_lowacc_alternating():
    b = main_line_bundle()
    ticks = [tick(0), tick(1000, fix(1000, 100, 0, 5.0, 90, 10.0), status="tracking")]
    t, x = 2000, 110.0
    for i in range(30):  # 30 s: valid off-route fix, low-accuracy fix, valid off-route fix, ...
        low = i % 2 == 1
        ticks.append(tick(t, fix(t, x, 60, 40.0 if low else 5.0, 90, 10.0),
                          off_run=0 if low else 1, reroute=False))
        t += 1000; x += 10
    write("gate7-off-lowacc-alternating.json", {
        "description": "Gate 7 / 07 §2.4 case 7 (added 2026-11-25): valid off-route fixes (60 m from the route) alternating with low-accuracy fixes (acc 40 m > max_acc_cm) for 30 s at 1 Hz. Each low-accuracy fix resets the off-route run, so off-route is never confirmed (known trade-off of the reset rule; the test records the elapsed time and fix count). The status becomes uncertain once the last match is older than 2.5 s; no reroute start.",
        "bundle": b, "ticks": ticks,
    })


def gen_parallel_ambiguous():
    rk = BUNDLE_KEY
    # out on y=0 to x=2000, U-turn 20 m north, back west on y=20 (two carriageways 20 m apart)
    pts = densify([(0, 0), (2000, 0), (2000, 20), (0, 20)], 100.0)
    mans = [(20, "u_turn_left"), (len(pts) - 1, "arrive")]
    b = build_bundle(rk, [{"pts": pts, "maneuvers": mans}], speed_mps=10.0)
    ticks = [tick(0)]
    # first fix after commit: between both carriageways, standing still (bearing unusable)
    ticks.append(tick(1000, fix(1000, 1000, 10, 5.0, 90, 0.5), status="uncertain"))
    ticks.append(tick(2000, fix(2000, 1001, 10, 5.0, 90, 1.0), status="uncertain"))
    # starts moving east at 10 m/s: the bearing (90 deg) excludes the westbound part
    ticks.append(tick(3000, fix(3000, 1010, 9, 5.0, 90, 10.0), status="tracking"))
    ticks.append(tick(4000, fix(4000, 1020, 9, 5.0, 90, 10.0), status="tracking"))
    write("gate7-parallel-ambiguous.json", {
        "description": "Gate 7 / 07 §2.4 case 2 (parallel roads): the route goes out on one carriageway and back on the other, 20 m apart. The first fix lies between them at 0.5 m/s (< 1.4 m/s: bearing not used): both route parts are candidates and cannot be told apart -> uncertain (no progress / ETA). Once the vehicle moves east at 10 m/s the bearing excludes the westbound part -> tracking.",
        "bundle": b, "ticks": ticks,
    })


def gen_tunnel():
    rk = BUNDLE_KEY
    pts = straight(6000.0, 100.0)
    mans = [(25, "left"), (50, "right"), (55, "keep_left"), (len(pts) - 1, "arrive")]
    b = build_bundle(rk, [{"pts": pts, "maneuvers": mans}], speed_mps=25.0, pts_per_edge=5,
                     flags_at=lambda x, y: 1 if 2000 <= x < 4500 else 0)
    ticks = [tick(0)]
    t = 1000
    x = 1000.0
    while x <= 2000.0:  # approach at 25 m/s
        ticks.append(tick(t, fix(t, x, 0.0, 5.0, 90.0, 25.0), status="tracking"))
        t += 1000; x += 25.0
    last_fix_t = t - 1000
    # inside the tunnel: no fixes; Kotlin ticks at next_wake / periodically
    ticks.append(tick(last_fix_t + 2500, status="tracking"))
    ticks.append(tick(last_fix_t + 2501, status="uncertain"))
    for k in range(1, 10):
        ticks.append(tick(last_fix_t + 2501 + k * 10000, status="uncertain"))
    # exit at x = 4520 m after 100.8 s: first fix far beyond the local window -> grid
    t = last_fix_t + 100800
    x = 4520.0
    ticks.append(tick(t, fix(t, x, 3.0, 12.0, 90.0, 25.0), status="tracking", reacquired=True))
    for _ in range(5):
        t += 1000; x += 25.0
        ticks.append(tick(t, fix(t, x, 0.0, 5.0, 90.0, 25.0), status="tracking"))
    write("gate7-tunnel-recovery.json", {
        "description": "Gate 7 / 07 §2.4 case 3 (tunnel recovery): straight 6 km at 25 m/s, tunnel edges (flag 1) from 2.0 to 4.5 km. Fixes stop at 2.0 km; 2.5 s later (07 §1 freshness) the status becomes uncertain without new input (next_wake). The first fix after the exit (4.52 km, 100.8 s later) is outside the local window: re-acquired through the chunk grid, back to tracking, the maneuvers inside the tunnel are passed.",
        "bundle": b, "ticks": ticks,
    })


def gen_adhoc_distance_class():
    """Planner fixture (tick/ dir): distance class from the remaining ETA of the alignment."""
    def bundle(rev, eta_ms):
        pts = straight(3000.0)
        return build_bundle(route_key(rev), [{"pts": pts, "maneuvers": [(len(pts) - 1, "arrive")]}],
                            speed_mps=10.0, eta_ms=eta_ms)
    a, b2 = bundle(0, 1_799_000), bundle(0, 1_800_000)
    start = fix(900, 0.0, 0.0, 5.0, 90.0, 0.0)
    rk_a, rk_b = committed(a["route_key"], 1), committed(b2["route_key"], 2)
    def fin(t, req, rk):
        return {"at_mono_ms": t, "kind": "search_finished", "req_id": req, "route_key": rk,
                "graph_version": GRAPH, "outcome": "error", "engine_ms": 1000}
    steps = [
        {"step": "route_commit", "req_id": None, "expected_route_key": None, "bundle": a},
        {"step": "tick", "input": {"now_mono_ms": 900, "fix": start}},
        {"step": "request", "now_mono_ms": 1000, "priority": "reroute",
         "expect_starts": [{"req_id": "r1", "priority": "reroute", "distance_class": "short"}],
         "expect_watchdog": 6000},
        {"step": "tick", "input": {"now_mono_ms": 2000, "events": [fin(2000, "r1", rk_a)]}},
        # r1 ended with an error: the replacement stands in for another commit
        {"step": "issue_commit", "req_id": "x1", "route_key": rk_a},
        {"step": "route_commit", "req_id": "x1", "expected_route_key": rk_a, "bundle": b2},
        {"step": "tick", "input": {"now_mono_ms": 2900, "fix": dict(start, at_mono_ms=2900)}},
        {"step": "request", "now_mono_ms": 3000, "priority": "avoid_chip",
         "expect_starts": [{"req_id": "r2", "priority": "avoid_chip", "distance_class": "long"}],
         "expect_watchdog": 13000},
        {"step": "tick", "input": {"now_mono_ms": 4000, "events": [fin(4000, "r2", rk_b)]}},
        {"step": "plan", "now_mono_ms": 5000, "plan_request_id": "p1", "eta_estimate_s": 1799},
        {"step": "tick", "input": {"now_mono_ms": 5000, "fix": dict(start, at_mono_ms=5000)}},
    ]
    # three valid fixes 1 km north of the route: off-route confirmed at the third
    for i, t in enumerate((5100, 5600, 6000)):
        f = fix(t, 10.0 * i, 1000.0, 5.0, 90.0, 0.0)
        st = {"step": "tick", "input": {"now_mono_ms": t, "fix": f}}
        if t == 6000:
            st["expect_starts"] = [{"req_id": "r3", "priority": "reroute", "distance_class": "long"}]
            st["expect_watchdog"] = 16000
        steps.append(st)
    write("gate7-adhoc-distance-class.json", {
        "description": "Gate 7: distance_class of searches outside a plan, fixed when the search starts from the valid remaining ETA of the on-route alignment (edges.cum_time_ms at the matched position; replaces the former route_eta steps): < 1800 s -> short; >= 1800 s, unknown or invalidated -> long; the latest plan's class is never inherited. Route A (eta 29:59) matched at its start -> r1 short (watchdog 1000 + 5000). Route B (eta 30:00) -> r2 long (3000 + 10000). Plan p1 is short (estimate 1799 s), but 3 fixes 1 km off the route confirm off-route, invalidate the remaining ETA and start the reroute r3 itself (long, 6000 + 10000) with the confirming fix.",
        "route_key": rk_a,
        "steps": steps,
    }, directory=TICK_DIR)


def _parallel_bundle(rev):
    pts = densify([(0, 0), (2000, 0), (2000, 20), (0, 20)], 100.0)
    mans = [(20, "u_turn_left"), (len(pts) - 1, "arrive")]
    return build_bundle(route_key(rev), [{"pts": pts, "maneuvers": mans}], speed_mps=10.0)


def _held_reroute_steps():
    """Shared start of the hold fixtures: tracking, off-route -> reroute r1, then an
    ambiguous fix between the parallel carriageways -> uncertain while r1 runs."""
    a = _parallel_bundle(0)
    rk_a = committed(a["route_key"], 1)
    steps = [
        {"step": "route_commit", "req_id": None, "expected_route_key": None, "bundle": a},
        {"step": "tick", "input": {"now_mono_ms": 1000, "fix": fix(1000, 100.0, 0.0, 5.0, 90.0, 10.0)}},
    ]
    for i, t in enumerate((2000, 3000, 4000)):
        st = {"step": "tick", "input": {"now_mono_ms": t, "fix": fix(t, 110.0 + 10.0 * i, 1000.0, 5.0, 90.0, 10.0)}}
        if t == 4000:
            st["expect_starts"] = [{"req_id": "r1", "priority": "reroute", "distance_class": "long"}]
        steps.append(st)
    steps.append({"step": "tick", "input": {"now_mono_ms": 5000, "fix": fix(5000, 1000.0, 10.0, 5.0, 90.0, 0.5)}})
    ok = {"at_mono_ms": 6000, "kind": "search_finished", "req_id": "r1", "route_key": rk_a,
          "graph_version": GRAPH, "outcome": "ok", "facts_ref": "facts-r1", "engine_ms": 2000}
    steps.append({"step": "tick", "input": {"now_mono_ms": 6000, "events": [ok]}, "expect_cmds": []})
    return a, rk_a, steps


def gen_reroute_held_uncertain():
    """Planner fixture (tick/ dir): a reroute ok returned while uncertain is held."""
    a, rk_a, steps = _held_reroute_steps()
    b = _parallel_bundle(0)
    steps += [
        {"step": "tick", "input": {"now_mono_ms": 6500}, "expect_cmds": []},
        # moving east at 10 m/s: the bearing decides -> tracking -> the held commit
        {"step": "tick", "input": {"now_mono_ms": 7000, "fix": fix(7000, 1010.0, 9.0, 5.0, 90.0, 10.0)},
         "expect_cmds": [{"cmd": "commit", "req_id": "r1", "route_key": rk_a}]},
        {"step": "route_commit", "req_id": "r1", "expected_route_key": rk_a, "bundle": b},
        {"step": "tick", "input": {"now_mono_ms": 8000, "fix": fix(8000, 1020.0, 9.0, 5.0, 90.0, 10.0)},
         "expect_cmds": []},
    ]
    write("gate7-reroute-held-uncertain.json", {
        "description": "Gate 7 / 04 §3 (hold while uncertain): parallel carriageways 20 m apart (as gate7-parallel-ambiguous). Tracking at 1000; three valid fixes 1 km off confirm off-route and start the reroute r1 (long: the remaining ETA is invalidated). At 5000 a fix between both carriageways at 0.5 m/s is ambiguous -> uncertain while r1 runs. r1 returns ok at 6000: not committed and not discarded but held (commit_held_uncertain), no second reroute starts. At 7000 the vehicle moves east at 10 m/s -> tracking -> commit{r1, route_key at the start (rev 1)}, once. Kotlin's dc_route_commit(expected rev 1, bundle rev \"0\") succeeds and Rust assigns rev 2.",
        "route_key": rk_a,
        "steps": steps,
    }, directory=TICK_DIR)


def gen_reroute_hold_expired():
    """Planner fixture (tick/ dir): the hold ends after commit_hold_ms (10 s)."""
    a, rk_a, steps = _held_reroute_steps()
    steps += [
        # 10 s after the hold (inclusive) it is still held; Kotlin wakes at 16001
        {"step": "tick", "input": {"now_mono_ms": 16000}, "expect_cmds": []},
        {"step": "tick", "input": {"now_mono_ms": 16001}, "expect_cmds": []},
        {"step": "tick", "input": {"now_mono_ms": 17000, "fix": fix(17000, 1010.0, 9.0, 5.0, 90.0, 10.0)},
         "expect_cmds": []},
    ]
    write("gate7-reroute-hold-expired.json", {
        "description": "Gate 7 / 04 §3 (hold limit, provisional 10 s = SessionConfig commit_hold_ms): same start as gate7-reroute-held-uncertain (r1 ok held at 6000 while uncertain). The status stays uncertain (no fix); at 16000 the result is still held, at 16001 it is discarded_hold_expired (next_wake_mono_ms). Back to tracking at 17000: no commit. Whether an expired hold restarts the reroute is undecided (04 §3, Gate 7); nothing is restarted.",
        "route_key": rk_a,
        "steps": steps,
    }, directory=TICK_DIR)


def gen_commit_route_revision():
    """Planner fixture (tick/ dir): dc_route_commit assigns route_revision (current + 1)."""
    def bundle(rev=0, trip_rev="3", graph=GRAPH):
        pts = straight(3000.0)
        rk = dict(route_key(rev, trip_rev=trip_rev), graph_version=graph)
        return build_bundle(rk, [{"pts": pts, "maneuvers": [(len(pts) - 1, "arrive")]}],
                            speed_mps=10.0)
    a = bundle()
    rk_a = committed(a["route_key"], 1)
    rk_other_trip = dict(rk_a, trip_revision="4")
    rk_other_graph = dict(rk_a, graph_version="jp-2026.12.0")
    steps = [
        # the first route: rev "0" in, rev 1 assigned
        {"step": "route_commit", "req_id": None, "expected_route_key": None, "bundle": a, "expect_route_key": rk_a},
        # stands in for a commit{c1, rk_a} the fixture does not drive (test hook)
        {"step": "issue_commit", "req_id": "c1", "route_key": rk_a},
        # the req_id must name a commit Rust issued (provisional): unknown / null -> E_STALE
        {"step": "route_commit", "req_id": "c2", "expected_route_key": rk_a, "bundle": bundle(), "expect_error": "E_STALE"},
        {"step": "route_commit", "req_id": None, "expected_route_key": rk_a, "bundle": bundle(), "expect_error": "E_STALE"},
        # a bundle must carry "0": any other revision is E_SCHEMA (nothing replaced)
        {"step": "route_commit", "req_id": "c1", "expected_route_key": rk_a, "bundle": bundle(2), "expect_error": "E_SCHEMA"},
        {"step": "route_commit", "req_id": "c1", "expected_route_key": rk_a, "bundle": bundle(1), "expect_error": "E_SCHEMA"},
        # bundle trip_revision / graph_version must equal the current route's
        {"step": "route_commit", "req_id": "c1", "expected_route_key": rk_a, "bundle": bundle(trip_rev="4"), "expect_error": "E_STALE"},
        {"step": "route_commit", "req_id": "c1", "expected_route_key": rk_a, "bundle": bundle(graph="jp-2026.12.0"), "expect_error": "E_STALE"},
        # the guard compares the whole RouteKey
        {"step": "route_commit", "req_id": "c1", "expected_route_key": rk_other_trip, "bundle": bundle(), "expect_error": "E_STALE"},
        {"step": "route_commit", "req_id": "c1", "expected_route_key": rk_other_graph, "bundle": bundle(), "expect_error": "E_STALE"},
        {"step": "tick", "input": {"now_mono_ms": 1000, "fix": fix(1000, 100.0, 0.0, 5.0, 90.0, 10.0)}},
        {"step": "route_commit", "req_id": "c1", "expected_route_key": rk_a, "bundle": bundle(),
         "expect_route_key": committed(a["route_key"], 2)},
        # c1 is settled: the same req_id again is E_STALE
        {"step": "route_commit", "req_id": "c1", "expected_route_key": committed(a["route_key"], 2),
         "bundle": bundle(), "expect_error": "E_STALE"},
        {"step": "tick", "input": {"now_mono_ms": 2000, "fix": fix(2000, 110.0, 0.0, 5.0, 90.0, 10.0)}},
    ]
    write("gate7-commit-route-revision.json", {
        "description": "Gate 7 (commit versioning, team decision 2026-11-26): every bundle carries route_revision \"0\" and Rust assigns current + 1 (the first route: 1), rewriting the bundle's route_key and every ManeuverId.route_key. A bundle with another revision (2, 1) is E_SCHEMA. While a route is current, a bundle whose trip_revision or graph_version differs from it is E_STALE. The guard expected_route_key compares the whole RouteKey (04 §3): a guard differing only in trip_revision or graph_version is E_STALE. The req_id must name a commit Rust issued for that guard (provisional; issue_commit stands in for one): an unknown or null req_id is E_STALE, and once a route is loaded the same req_id is E_STALE. Nothing is replaced on any error. The valid replacement becomes rev 2; the next horizon carries rev 2.",
        "route_key": rk_a,
        "steps": steps,
    }, directory=TICK_DIR)


if __name__ == "__main__":
    gen_straight_base()
    gen_chunk_boundary()
    gen_side_road_20m()
    gen_lateral_boundary()
    gen_branch_40m()
    gen_jitter_main()
    gen_off_run_reset()
    gen_off_lowacc_alternating()
    gen_parallel_ambiguous()
    gen_tunnel()
    gen_adhoc_distance_class()
    gen_reroute_held_uncertain()
    gen_reroute_hold_expired()
    gen_commit_route_revision()
