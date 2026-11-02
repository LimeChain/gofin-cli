//! Run against b6fcb06 with the companion finite protected configuration.
use gofin_store::Store;
use serde_json::json;
fn main() {
    let mut store = Store::open(std::env::args().nth(1).unwrap()).unwrap();
    let t = "2026-11-01T00:00:00Z";
    let run = json!({"purpose":null,"max_movement_usd_micros":"50000000","id":"synthetic-over-limit-run","status":"active","baseline":{"state":{"holdings":[],"positions":[],"authorities":[],"in_flight":[]},"observed_at":t,"complete":true,"coverage":[],"pending":[]},"initial_allocation_usd_micros":"100000000","borrowed_proceeds_reserve_usd_micros":"0","return_cost_reserve_usd_micros":"0","planned_operations":[],"unwind":[],"route_proofs":[],"asset_prices":[],"settled_route_steps":[],"route_invalidated":false,"allocations":[],"available":[],"reservations":{},"costs":[],"spent_cost_usd_micros":"90000000","borrowed_allocation_used_usd_micros":"0","final_state":null,"final_observed_at":null,"differences":{},"provenance":[],"provenance_events":[],"supply_accruals":[],"position_anchors":[],"route_revisions":[],"net_conversion_deltas":[]});
    store
        .create_run("synthetic-over-limit-run", &run, t)
        .unwrap();
}
