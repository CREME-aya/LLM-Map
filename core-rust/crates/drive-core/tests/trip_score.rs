//! Unit and integration tests for `drive_core::trip_score` (`dc_trip_score`, 05 §6.1).

use contracts::trip_score::{DestinationKind, HistoryDensity, TripScorePolicy, TripScoreRequest};
use contracts::ErrorCode;
use drive_core::trip_score::evaluate;

fn zero_req() -> TripScoreRequest {
    TripScoreRequest {
        distance_m: 10_000,
        eta_s: 900,
        motorway_ratio_permille: 0,
        destination_kind: DestinationKind::Other,
        outside_daily_area: false,
        corridor_history_density: HistoryDensity::Low,
        recently_declined_similar: false,
        is_driving_reroute: false,
        policy: TripScorePolicy::default(),
    }
}

/// Every factor in the 05 §6.1 table awards its exact points independently.
#[test]
fn each_factor_scores_independently() {
    let base = evaluate(&zero_req()).unwrap();
    assert_eq!(base.score, 0);
    assert!(!base.show_card);

    // 1. Distance >= 80 km (+2) and >= 150 km (+3)
    let mut r = zero_req();
    r.distance_m = 80_000;
    let o = evaluate(&r).unwrap();
    assert_eq!(o.breakdown.distance, 2);
    assert_eq!(o.score, 2);

    r.distance_m = 150_000;
    let o = evaluate(&r).unwrap();
    assert_eq!(o.breakdown.distance, 3);
    assert_eq!(o.score, 3);
    assert!(o.show_card);

    // 2. ETA >= 60 min (3600 s) -> +2; 3599 s -> 0
    let mut r = zero_req();
    r.eta_s = 3_599;
    assert_eq!(evaluate(&r).unwrap().breakdown.eta, 0);
    r.eta_s = 3_600;
    assert_eq!(evaluate(&r).unwrap().breakdown.eta, 2);

    // 3. Motorway ratio >= 50% (500 permille) -> +1; 499 -> 0
    let mut r = zero_req();
    r.motorway_ratio_permille = 499;
    assert_eq!(evaluate(&r).unwrap().breakdown.motorway, 0);
    r.motorway_ratio_permille = 500;
    assert_eq!(evaluate(&r).unwrap().breakdown.motorway, 1);

    // 4. Destination kinds: all 5 trip kinds -> +2, other -> 0
    for kind in [
        DestinationKind::Sightseeing,
        DestinationKind::HistoricSite,
        DestinationKind::Castle,
        DestinationKind::Michinoeki,
        DestinationKind::Lodging,
    ] {
        let mut r = zero_req();
        r.destination_kind = kind;
        assert_eq!(evaluate(&r).unwrap().breakdown.destination, 2, "{kind:?}");
    }
    assert_eq!(evaluate(&zero_req()).unwrap().breakdown.destination, 0);

    // 5. Outside daily driving area -> +2
    let mut r = zero_req();
    r.outside_daily_area = true;
    assert_eq!(evaluate(&r).unwrap().breakdown.outside_daily_area, 2);

    // 6. Corridor historical spot density: high -> +1; low / normal -> 0
    for (density, pts) in [
        (HistoryDensity::Low, 0),
        (HistoryDensity::Normal, 0),
        (HistoryDensity::High, 1),
    ] {
        let mut r = zero_req();
        r.corridor_history_density = density;
        assert_eq!(
            evaluate(&r).unwrap().breakdown.corridor_history_density,
            pts,
            "{density:?}"
        );
    }
}

/// Exact boundaries at 80 km (79_999 / 80_000 m) and 150 km (149_999 / 150_000 m).
#[test]
fn distance_80km_and_150km_boundaries() {
    for (dist_m, want) in [
        (0, 0),
        (79_999, 0),
        (80_000, 2),
        (149_999, 2),
        (150_000, 3),
        (300_000, 3),
    ] {
        let mut r = zero_req();
        r.distance_m = dist_m;
        let o = evaluate(&r).unwrap();
        assert_eq!(o.breakdown.distance, want, "distance_m={dist_m}");
        assert_eq!(o.score, want, "distance_m={dist_m}");
    }
}

/// Recent decline on a similar trip deducts -3 points (can drop score below threshold or negative).
/// Explicitly verifies: >= 80 km (+2) with `recently_declined_similar: true` (-3) -> score -1,
/// `show_card = false`.
#[test]
fn recently_declined_similar_deducts_three_points() {
    // 80 km (+2) + recently_declined_similar (-3) -> total -1 < 3 -> show_card = false
    let mut dist_declined = zero_req();
    dist_declined.distance_m = 80_000;
    dist_declined.recently_declined_similar = true;
    let o_dist = evaluate(&dist_declined).unwrap();
    assert_eq!(o_dist.breakdown.distance, 2);
    assert_eq!(o_dist.breakdown.recently_declined_similar, -3);
    assert_eq!(o_dist.score, -1);
    assert!(!o_dist.show_card);

    // Without decline: 80 km (+2) + ETA (+2) + motorway (+1) = 5 -> show_card = true
    let mut r = zero_req();
    r.distance_m = 80_000;
    r.eta_s = 3_600;
    r.motorway_ratio_permille = 500;
    assert_eq!(evaluate(&r).unwrap().score, 5);
    assert!(evaluate(&r).unwrap().show_card);

    // With recent decline: 5 - 3 = 2 < 3 -> show_card = false
    r.recently_declined_similar = true;
    let o = evaluate(&r).unwrap();
    assert_eq!(o.breakdown.recently_declined_similar, -3);
    assert_eq!(o.score, 2);
    assert!(!o.show_card);

    // With all zeros + decline: -3
    let mut neg = zero_req();
    neg.recently_declined_similar = true;
    let o_neg = evaluate(&neg).unwrap();
    assert_eq!(o_neg.score, -3);
    assert!(!o_neg.show_card);
}

/// Exact threshold boundary: score 2 -> false, score 3 -> true; configurable threshold works.
#[test]
fn threshold_boundary_exact_and_configurable() {
    // Score 2 (outside daily area only) -> false
    let mut r = zero_req();
    r.outside_daily_area = true;
    let o2 = evaluate(&r).unwrap();
    assert_eq!(o2.score, 2);
    assert_eq!(o2.threshold, 3);
    assert!(!o2.show_card);

    // Score 3 (+ high history density) -> true
    r.corridor_history_density = HistoryDensity::High;
    let o3 = evaluate(&r).unwrap();
    assert_eq!(o3.score, 3);
    assert!(o3.show_card);

    // Custom threshold 4 makes score 3 -> false
    r.policy.threshold = 4;
    let o3_t4 = evaluate(&r).unwrap();
    assert_eq!(o3_t4.score, 3);
    assert_eq!(o3_t4.threshold, 4);
    assert!(!o3_t4.show_card);
}

/// Driving reroute ALWAYS suppresses the C01 card (`show_card = false`), even at maximum score.
#[test]
fn driving_reroute_suppresses_card_even_with_high_score() {
    let mut r = TripScoreRequest {
        distance_m: 200_000,                            // +3
        eta_s: 7_200,                                   // +2
        motorway_ratio_permille: 800,                   // +1
        destination_kind: DestinationKind::Castle,      // +2
        outside_daily_area: true,                       // +2
        corridor_history_density: HistoryDensity::High, // +1
        recently_declined_similar: false,               // 0 -> total 11
        is_driving_reroute: false,
        policy: TripScorePolicy::default(),
    };
    let pre = evaluate(&r).unwrap();
    assert_eq!(pre.score, 11);
    assert!(pre.show_card);
    assert!(!pre.suppressed_by_reroute);

    r.is_driving_reroute = true;
    let reroute = evaluate(&r).unwrap();
    assert_eq!(reroute.score, 11);
    assert!(!reroute.show_card);
    assert!(reroute.suppressed_by_reroute);
}

/// Invalid policy or out-of-range permille is rejected with E_SCHEMA.
#[test]
fn invalid_inputs_rejected_with_e_schema() {
    let mut bad_ratio = zero_req();
    bad_ratio.motorway_ratio_permille = 1_001;
    assert_eq!(evaluate(&bad_ratio).unwrap_err().code, ErrorCode::Schema);

    let mut bad_policy = zero_req();
    bad_policy.policy.distance_medium_m = 100_000;
    bad_policy.policy.distance_long_m = 50_000;
    assert_eq!(evaluate(&bad_policy).unwrap_err().code, ErrorCode::Schema);
}
