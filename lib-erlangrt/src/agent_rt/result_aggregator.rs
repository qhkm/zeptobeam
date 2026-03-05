use std::collections::HashMap;

pub trait ResultAggregator: Send + Sync {
  fn aggregate(&self, results: &[serde_json::Value]) -> serde_json::Value;
}

pub struct ConcatAggregator;

pub struct VoteAggregator {
  pub field: String,
}

pub struct MergeAggregator;

impl ResultAggregator for ConcatAggregator {
  fn aggregate(&self, results: &[serde_json::Value]) -> serde_json::Value {
    serde_json::Value::Array(results.to_vec())
  }
}

impl ResultAggregator for VoteAggregator {
  fn aggregate(&self, results: &[serde_json::Value]) -> serde_json::Value {
    let mut counts: HashMap<String, i64> = HashMap::new();

    for result in results {
      if let Some(value) = result.get(&self.field) {
        let key = value.to_string();
        *counts.entry(key).or_insert(0) += 1;
      }
    }

    let winner = counts
      .iter()
      .max_by_key(|&(_, count)| count)
      .map(|(value, _)| {
        // Remove quotes if it's a string value
        value.trim_matches('"').to_string()
      })
      .unwrap_or_default();

    let votes: serde_json::Map<String, serde_json::Value> = counts
      .into_iter()
      .map(|(k, v)| {
        let key = k.trim_matches('"').to_string();
        (key, serde_json::Value::Number(v.into()))
      })
      .collect();

    let mut result = serde_json::Map::new();
    result.insert("winner".to_string(), serde_json::Value::String(winner));
    result.insert("votes".to_string(), serde_json::Value::Object(votes));

    serde_json::Value::Object(result)
  }
}

impl ResultAggregator for MergeAggregator {
  fn aggregate(&self, results: &[serde_json::Value]) -> serde_json::Value {
    let mut merged = serde_json::Map::new();

    for result in results {
      if let serde_json::Value::Object(map) = result {
        for (key, value) in map.iter() {
          match merged.get_mut(key) {
            Some(existing) => {
              // If both are arrays, concatenate
              if let (Some(existing_arr), Some(new_arr)) =
                (existing.as_array_mut(), value.as_array())
              {
                existing_arr.extend(new_arr.iter().cloned());
              } else {
                // Later value wins
                *existing = value.clone();
              }
            }
            None => {
              merged.insert(key.clone(), value.clone());
            }
          }
        }
      }
    }

    serde_json::Value::Object(merged)
  }
}

/// Build an aggregator from a strategy name.
pub fn build_aggregator(strategy: &str) -> Box<dyn ResultAggregator> {
  match strategy {
    "concat" => Box::new(ConcatAggregator),
    "vote" => Box::new(VoteAggregator {
      field: "decision".to_string(),
    }),
    "merge" => Box::new(MergeAggregator),
    _ => Box::new(ConcatAggregator), // Default to ConcatAggregator
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn test_concat_aggregator() {
    let agg = ConcatAggregator;
    let results = vec![json!({"a": 1}), json!({"b": 2})];
    let out = agg.aggregate(&results);
    assert_eq!(out, json!([{"a": 1}, {"b": 2}]));
  }

  #[test]
  fn test_concat_empty() {
    let agg = ConcatAggregator;
    assert_eq!(agg.aggregate(&[]), json!([]));
  }

  #[test]
  fn test_vote_aggregator() {
    let agg = VoteAggregator {
      field: "decision".into(),
    };
    let results = vec![
      json!({"decision": "approve"}),
      json!({"decision": "reject"}),
      json!({"decision": "approve"}),
    ];
    let out = agg.aggregate(&results);
    assert_eq!(out["winner"], "approve");
    assert_eq!(out["votes"]["approve"], 2);
    assert_eq!(out["votes"]["reject"], 1);
  }

  #[test]
  fn test_merge_aggregator() {
    let agg = MergeAggregator;
    let results = vec![
      json!({"name": "alice", "tags": ["fast"]}),
      json!({"age": 30, "tags": ["smart"]}),
    ];
    let out = agg.aggregate(&results);
    assert_eq!(out["name"], "alice");
    assert_eq!(out["age"], 30);
    assert_eq!(out["tags"], json!(["fast", "smart"]));
  }

  #[test]
  fn test_build_aggregator_concat() {
    let agg = build_aggregator("concat");
    let out = agg.aggregate(&[json!(1), json!(2)]);
    assert_eq!(out, json!([1, 2]));
  }

  #[test]
  fn test_build_aggregator_unknown_defaults_concat() {
    let agg = build_aggregator("nonexistent");
    let out = agg.aggregate(&[json!(1)]);
    assert_eq!(out, json!([1]));
  }
}
