use gofin_store::Store;
use serde_json::json;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let mut store = Store::open(path).unwrap();
    let record = json!({"id":"synthetic-boundary","request_id":"boundary-request","request_fingerprint":"boundary-hash","intent_id":"synthetic-intent","status":"active","expires_at":"2099-01-01T00:00:00Z","approval":{"source":"touch_id","binding_hash":"synthetic-binding"}});
    store
        .create_boundary_authorization(
            "synthetic-boundary",
            "boundary-request",
            "synthetic-intent",
            "boundary-hash",
            &record,
            "2026-11-01T00:00:00Z",
        )
        .unwrap();
}
