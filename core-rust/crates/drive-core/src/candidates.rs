//! `CandidatePlanner` (03 §2 / §3.1): `TripPlanSnapshot` → ordered `CandidateRequest[]`.
//! Pure and deterministic. Total requests are capped at 5 and at `budget.max_requests`.
//!
//! Generated today: `baseline` (with up to 2 `engine_alternate`s when there are no vias)
//! then the two `costing_variant`s.
//! TODO(drive-core): the trigger conditions for additional searches (diversity lacking, a
//! remaining opposing right turn, explicit comparison request; 03 §3.1 item 2) need the
//! baseline facts, and `through_anchor` needs the hard spots found in them. Neither is
//! available before searching, so both variants are always queued and no `through_anchor`
//! is produced yet.

use contracts::plan::{CandidateLabel, CandidateRequest, CostingVariant, TripPlanSnapshot};
use contracts::tick::SearchPriority;

/// 03 §3.1 item 3: 5 requests in total per plan.
pub const MAX_REQUESTS: u32 = 5;
/// 03 §3.1 item 1: at most 2 engine alternates, only without vias.
pub const MAX_ENGINE_ALTERNATES: u8 = 2;

pub fn candidate_requests(snapshot: &TripPlanSnapshot) -> Vec<CandidateRequest> {
    let cap = snapshot.budget.max_requests.clamp(1, MAX_REQUESTS) as usize;
    let mut out = vec![CandidateRequest {
        candidate_id: "c1".into(),
        label: CandidateLabel::Baseline,
        priority: SearchPriority::ExplicitPlan,
        engine_alternates: if snapshot.vias.is_empty() {
            MAX_ENGINE_ALTERNATES
        } else {
            0
        },
        costing_variant: None,
    }];
    for v in [CostingVariant::PreferTrunk, CostingVariant::ServicePenalty] {
        if out.len() >= cap {
            break;
        }
        out.push(CandidateRequest {
            candidate_id: format!("c{}", out.len() + 1),
            label: CandidateLabel::CostingVariant,
            priority: SearchPriority::Comparison,
            engine_alternates: 0,
            costing_variant: Some(v),
        });
    }
    out
}
