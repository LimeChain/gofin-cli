//! Foreground observation conveniences. No execution or approval path is reachable here.
use super::{BrokerRequest, Client, GoFinError, Value};
use std::time::Duration;
use tokio::time::Instant;

pub(super) async fn status(
    client: &dyn Client,
    id: String,
    wait: Option<u64>,
    side: Option<String>,
) -> Result<Value, GoFinError> {
    let request = side.map_or_else(
        || BrokerRequest::Status { id: id.clone() },
        |side| BrokerRequest::StatusSide {
            id: id.clone(),
            side,
        },
    );
    let Some(seconds) = wait.filter(|seconds| *seconds > 0) else {
        return client.call(request).await;
    };
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut latest = tokio::time::timeout_at(deadline, client.call(request.clone()))
        .await
        .map_err(|_| {
            GoFinError::new(
                "OBSERVATION_TIMEOUT",
                "observation",
                "The observation did not finish within the requested wait.",
                "Check status for the original operation again. No financial write was requested.",
            )
        })??;
    let initial_phase = latest["outcome"]["phase"].as_str().map(str::to_owned);
    if initial_phase.is_none() || !is_open(&latest) {
        return Ok(latest);
    }
    loop {
        let delay = latest["outcome"]["next_check_after"]
            .as_str()
            .and_then(|timestamp| chrono::DateTime::parse_from_rfc3339(timestamp).ok())
            .and_then(|at| {
                (at.with_timezone(&chrono::Utc) - chrono::Utc::now())
                    .to_std()
                    .ok()
            })
            .unwrap_or(Duration::from_secs(1))
            .max(Duration::from_millis(100));
        let wake = (Instant::now() + delay).min(deadline);
        tokio::time::sleep_until(wake).await;
        if Instant::now() >= deadline {
            return Ok(latest);
        }
        match tokio::time::timeout_at(deadline, client.call(request.clone())).await {
            Ok(Ok(observed)) => latest = observed,
            // A later transport failure cannot erase the last established facts.
            Ok(Err(error)) => {
                latest["observation_error"] = serde_json::to_value(error).map_err(|_| {
                    GoFinError::validation("Observation error could not be encoded.")
                })?;
                return Ok(latest);
            }
            Err(_) => return Ok(latest),
        }
        if latest["outcome"]["phase"].as_str() != initial_phase.as_deref() || !is_open(&latest) {
            return Ok(latest);
        }
    }
}

fn is_open(record: &Value) -> bool {
    if record["human_dismissed"] == true {
        return false;
    }
    let outcome = &record["outcome"];
    matches!(
        outcome["phase"].as_str(),
        Some("in_progress" | "needs_you" | "unconfirmed")
    ) || matches!(
        outcome["transfer"].as_str(),
        Some("source_pending" | "in_transit" | "unknown")
    )
}

#[cfg(test)]
pub(super) fn retain_open(records: &mut Value) -> Result<(), GoFinError> {
    let records = records.as_array_mut().ok_or_else(|| {
        GoFinError::new(
            "BROKER_PROTOCOL_ERROR",
            "observation",
            "Operation history is not a list.",
            "Check the broker version and retry the read.",
        )
    })?;
    if records
        .iter()
        .any(|record| record["outcome"]["version"] != 1)
    {
        return Err(GoFinError::new(
            "OUTCOME_VERSION_UNSUPPORTED",
            "observation",
            "The broker does not provide the supported operation outcome contract.",
            "Update the matching CLI and broker before filtering open operations.",
        ));
    }
    records.retain(is_open);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;

    struct Observer(Mutex<Vec<Value>>);
    #[async_trait]
    impl Client for Observer {
        async fn call(&self, request: BrokerRequest) -> Result<Value, GoFinError> {
            assert!(matches!(request, BrokerRequest::Status { id } if id == "original"));
            let mut values = self.0.lock().unwrap();
            Ok(if values.len() > 1 {
                values.remove(0)
            } else {
                values[0].clone()
            })
        }
        async fn execute(&self, _: gofin_domain::OperationRequest) -> Result<Value, GoFinError> {
            panic!("observation must never execute")
        }
    }

    #[tokio::test(start_paused = true)]
    async fn wait_stops_on_phase_change_without_execution() {
        let client = Observer(Mutex::new(vec![
            json!({"outcome":{"phase":"in_progress","dispatch":"uncertain"}}),
            json!({"outcome":{"phase":"done","result":"executed"}}),
        ]));
        let value = status(&client, "original".into(), Some(30), None)
            .await
            .unwrap();
        assert_eq!(value["outcome"]["result"], "executed");
    }

    #[tokio::test(start_paused = true)]
    async fn wait_returns_pending_when_budget_expires() {
        let client = Observer(Mutex::new(vec![json!({"outcome":{"phase":"in_progress"}})]));
        let before = Instant::now();
        let value = status(&client, "original".into(), Some(3), None)
            .await
            .unwrap();
        assert_eq!(value["outcome"]["phase"], "in_progress");
        assert_eq!(before.elapsed(), Duration::from_secs(3));
    }

    #[test]
    fn open_list_keeps_failed_transfers_and_excludes_dismissed_records() {
        let mut records = json!([
            {"id":"refund","outcome":{"version":1,"phase":"failed","transfer":"in_transit"}},
            {"id":"done","outcome":{"version":1,"phase":"done"}},
            {"id":"dismissed","human_dismissed":true,"outcome":{"version":1,"phase":"unconfirmed"}}
        ]);
        retain_open(&mut records).unwrap();
        assert_eq!(records.as_array().unwrap().len(), 1);
        assert_eq!(records[0]["id"], "refund");
    }
}
