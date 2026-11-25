//! Integer-only plane geometry for on-route alignment (docs 04 §3.1).
//!
//! No floating point and no trigonometry: C++ computed the per-chunk factors and the
//! segment bearings, Kotlin rounded the fix to integers, so everything here is integer
//! arithmetic and gives the same result on x86 and arm64. Squared distances and dot /
//! cross products are i64; square roots are integer square roots (floor).
//! `tests/no_float.rs` checks this file (and route / align / horizon) stays float-free.
#![deny(clippy::float_arithmetic, clippy::cast_precision_loss)]

use contracts::tick::BEARING_CDEG_FULL;

/// `round_q16(v) = (v + 2^15) >> 16`: arithmetic shift, so exact halves go toward +inf
/// (`0.5 → 1`, `-0.5 → 0`, `-1.5 → -1`).
pub const fn round_q16(v: i64) -> i64 {
    (v + (1 << 15)) >> 16
}

/// Chunk-local plane: origin in 1e-7 degrees and cm per 1e-7 degree in Q16.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalFrame {
    pub origin_lat_e7: i32,
    pub origin_lon_e7: i32,
    pub k_lat_q16: u32,
    pub k_lon_q16: u32,
}

impl LocalFrame {
    /// `(dx_cm, dy_cm)` of a point given in 1e-7 degrees (x east, y north).
    /// `|delta| <= 3.6e9` and `k <= 2^20` keep the product below 2^62.
    pub fn to_local(&self, lat_e7: i32, lon_e7: i32) -> (i64, i64) {
        let dlat = i64::from(lat_e7) - i64::from(self.origin_lat_e7);
        let dlon = i64::from(lon_e7) - i64::from(self.origin_lon_e7);
        (
            round_q16(dlon * i64::from(self.k_lon_q16)),
            round_q16(dlat * i64::from(self.k_lat_q16)),
        )
    }
}

/// Floor of the square root (`isqrt(n)^2 <= n < (isqrt(n)+1)^2`).
pub fn isqrt(n: u64) -> u64 {
    if n < 2 {
        return n;
    }
    // Newton's method from an over-estimate; converges monotonically downwards.
    let mut x = 1u64 << ((64 - n.leading_zeros()).div_ceil(2));
    loop {
        let y = (x + n / x) / 2;
        if y >= x {
            return x;
        }
        x = y;
    }
}

/// Smallest angle between two bearings in centidegrees (`0..=18000`):
/// `d = |a - b| mod 36000`, `min(d, 36000 - d)`.
pub fn bearing_diff_cdeg(a: u16, b: u16) -> u16 {
    let full = i32::from(BEARING_CDEG_FULL);
    let d = (i32::from(a) - i32::from(b)).rem_euclid(full);
    // d < 36000, so both values fit in u16.
    d.min(full - d) as u16
}

/// Projection of point `p` onto segment `a → b` (all chunk-local cm).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Projection {
    /// Distance from `p` to the closest point of the segment (cm, rounded down).
    pub lateral_cm: u64,
    /// Plane distance from `a` to that closest point (cm, `0..=seg_len_cm`).
    pub along_cm: u64,
    /// Plane length of the segment (cm, `isqrt` of the squared length).
    pub seg_len_cm: u64,
}

/// Closest point of segment `a → b` to `p`. Coordinates must stay within a few 1e7 cm
/// (chunk extent + 1 km margin), which keeps every product inside i64.
pub fn project(p: (i64, i64), a: (i64, i64), b: (i64, i64)) -> Projection {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let (wx, wy) = (p.0 - a.0, p.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let seg_len_cm = isqrt(len2 as u64);
    let dist = |x: i64, y: i64| isqrt((x * x + y * y) as u64);
    if len2 == 0 {
        return Projection {
            lateral_cm: dist(wx, wy),
            along_cm: 0,
            seg_len_cm: 0,
        };
    }
    let dot = wx * dx + wy * dy;
    if dot <= 0 {
        return Projection {
            lateral_cm: dist(wx, wy),
            along_cm: 0,
            seg_len_cm,
        };
    }
    if dot >= len2 {
        return Projection {
            lateral_cm: dist(p.0 - b.0, p.1 - b.1),
            along_cm: seg_len_cm,
            seg_len_cm,
        };
    }
    let cross = (wx * dy - wy * dx).unsigned_abs();
    // seg_len_cm >= 1 here (len2 >= 1).
    Projection {
        lateral_cm: cross / seg_len_cm,
        along_cm: (dot as u64 / seg_len_cm).min(seg_len_cm),
        seg_len_cm,
    }
}

/// Linear interpolation `a + (b - a) * num / den` on u64 (den = 0 → a). `b >= a`.
pub fn lerp_u64(a: u64, b: u64, num: u64, den: u64) -> u64 {
    if den == 0 {
        return a;
    }
    a + (b - a) * num.min(den) / den
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_q16_halves_toward_plus_infinity() {
        let h = 1i64 << 15;
        assert_eq!(round_q16(h), 1); // 0.5 → 1
        assert_eq!(round_q16(h - 1), 0);
        assert_eq!(round_q16(-h), 0); // -0.5 → 0
        assert_eq!(round_q16(-h - 1), -1);
        assert_eq!(round_q16(-3 * h), -1); // -1.5 → -1
        assert_eq!(round_q16(3 * h), 2); // 1.5 → 2
        assert_eq!(round_q16(65_536 * 7), 7);
        assert_eq!(round_q16(-65_536 * 7), -7);
    }

    #[test]
    fn isqrt_is_floor() {
        for n in 0..10_000u64 {
            let r = isqrt(n);
            assert!(r * r <= n && (r + 1) * (r + 1) > n, "{n}");
        }
        for n in [u64::MAX, u64::MAX - 1, 1 << 62, (1 << 32) * ((1 << 32) - 1)] {
            let r = isqrt(n) as u128;
            assert!(r * r <= n as u128 && (r + 1) * (r + 1) > n as u128, "{n}");
        }
    }

    #[test]
    fn bearing_diff_wraps() {
        assert_eq!(bearing_diff_cdeg(0, 35_999), 1);
        assert_eq!(bearing_diff_cdeg(35_999, 0), 1);
        assert_eq!(bearing_diff_cdeg(9_000, 27_000), 18_000);
        assert_eq!(bearing_diff_cdeg(1_000, 7_000), 6_000);
        assert_eq!(bearing_diff_cdeg(35_000, 5_000), 6_000);
        assert_eq!(bearing_diff_cdeg(12_345, 12_345), 0);
    }

    #[test]
    fn projection_cases() {
        let a = (0, 0);
        let b = (10_000, 0);
        assert_eq!(
            project((5_000, 3_000), a, b),
            Projection {
                lateral_cm: 3_000,
                along_cm: 5_000,
                seg_len_cm: 10_000
            }
        );
        assert_eq!(project((-300, 400), a, b).lateral_cm, 500);
        assert_eq!(project((-300, 400), a, b).along_cm, 0);
        assert_eq!(project((10_300, -400), a, b).lateral_cm, 500);
        assert_eq!(project((10_300, -400), a, b).along_cm, 10_000);
        assert_eq!(project((3, 4), a, a).lateral_cm, 5);
    }

    #[test]
    fn local_frame_uses_q16_rounding() {
        let f = LocalFrame {
            origin_lat_e7: 350_000_000,
            origin_lon_e7: 1_390_000_000,
            k_lat_q16: 72_955,
            k_lon_q16: 59_762,
        };
        assert_eq!(f.to_local(350_000_000, 1_390_000_000), (0, 0));
        // 2695 * 72955 / 65536 = 3000.13 → 3000; 2696 → 3001.24 → 3001
        assert_eq!(f.to_local(350_002_695, 1_390_000_000).1, 3_000);
        assert_eq!(f.to_local(350_002_696, 1_390_000_000).1, 3_001);
        assert_eq!(f.to_local(349_997_305, 1_390_000_000).1, -3_000);
        // extremes stay inside i64
        let far = f.to_local(-900_000_000, -1_800_000_000);
        assert!(far.0 < 0 && far.1 < 0);
    }
}
