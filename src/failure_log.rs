use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn log_failure(event: &str, data: Value) {
    eprintln!("{}", format_failure(event, data));
}

fn format_failure(event: &str, data: Value) -> String {
    json!({
        "ts_ms": now_millis(),
        "level": "error",
        "event": event,
        "data": data,
    })
    .to_string()
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::format_failure;
    use serde_json::json;

    #[test]
    fn formats_json_line() {
        let line = format_failure("request_failed", json!({"path": "/npm/pkg"}));
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["level"], "error");
        assert_eq!(value["event"], "request_failed");
        assert_eq!(value["data"]["path"], "/npm/pkg");
        assert!(value["ts_ms"].as_u64().is_some());
    }
}
