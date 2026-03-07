use reqwest::Client;
use crate::core::effect::{EffectId, EffectResult};

pub struct HttpWorker {
  client: Client,
}

impl HttpWorker {
  pub fn new() -> Self {
    Self { client: Client::new() }
  }

  pub async fn execute(
    &self,
    effect_id: EffectId,
    input: &serde_json::Value,
    timeout: std::time::Duration,
  ) -> EffectResult {
    let method = input
      .get("method")
      .and_then(|v| v.as_str())
      .unwrap_or("GET")
      .to_uppercase();
    let url = match input.get("url").and_then(|v| v.as_str()) {
      Some(u) => u,
      None => {
        return EffectResult::failure(
          effect_id,
          "missing 'url' in HTTP effect input",
        )
      }
    };

    let mut req = match method.as_str() {
      "GET" => self.client.get(url),
      "POST" => self.client.post(url),
      "PUT" => self.client.put(url),
      "DELETE" => self.client.delete(url),
      "PATCH" => self.client.patch(url),
      "HEAD" => self.client.head(url),
      other => {
        return EffectResult::failure(
          effect_id,
          format!("unsupported method: {other}"),
        )
      }
    };

    req = req.timeout(timeout);

    if let Some(headers) =
      input.get("headers").and_then(|v| v.as_object())
    {
      for (key, val) in headers {
        if let Some(v) = val.as_str() {
          req = req.header(key.as_str(), v);
        }
      }
    }

    if let Some(body) = input.get("body") {
      req = req.json(body);
    }

    match req.send().await {
      Ok(resp) => {
        let status = resp.status().as_u16();
        let headers: serde_json::Map<String, serde_json::Value> =
          resp
            .headers()
            .iter()
            .filter_map(|(k, v)| {
              v.to_str().ok().map(|v| {
                (
                  k.to_string(),
                  serde_json::Value::String(v.to_string()),
                )
              })
            })
            .collect();
        match resp.text().await {
          Ok(body) => EffectResult::success(
            effect_id,
            serde_json::json!({
              "status": status,
              "headers": headers,
              "body": body,
            }),
          ),
          Err(e) => EffectResult::failure(
            effect_id,
            format!("body read error: {e}"),
          ),
        }
      }
      Err(e) if e.is_timeout() => EffectResult {
        effect_id,
        status: crate::core::effect::EffectStatus::TimedOut,
        output: None,
        error: Some(format!("request timed out: {e}")),
      },
      Err(e) => EffectResult::failure(
        effect_id,
        format!("request error: {e}"),
      ),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_http_worker_new() {
    let _worker = HttpWorker::new();
  }

  #[tokio::test]
  async fn test_http_worker_missing_url() {
    let worker = HttpWorker::new();
    let id = EffectId::new();
    let result = worker
      .execute(
        id,
        &serde_json::json!({}),
        std::time::Duration::from_secs(5),
      )
      .await;
    assert_eq!(
      result.status,
      crate::core::effect::EffectStatus::Failed
    );
    assert!(
      result.error.as_deref().unwrap().contains("missing 'url'")
    );
  }

  #[tokio::test]
  async fn test_http_worker_unsupported_method() {
    let worker = HttpWorker::new();
    let id = EffectId::new();
    let result = worker
      .execute(
        id,
        &serde_json::json!({
          "url": "https://example.com",
          "method": "FOOBAR",
        }),
        std::time::Duration::from_secs(5),
      )
      .await;
    assert_eq!(
      result.status,
      crate::core::effect::EffectStatus::Failed
    );
    assert!(result
      .error
      .as_deref()
      .unwrap()
      .contains("unsupported method"));
  }
}
