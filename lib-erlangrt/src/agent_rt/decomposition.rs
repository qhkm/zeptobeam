//! Goal decomposition: prompt building and LLM response parsing.

use crate::agent_rt::types::IoOp;

/// Default provider used for decomposition when none is configured.
pub const DEFAULT_DECOMPOSE_PROVIDER: &str = "openrouter";
/// Default model for decomposition.
pub const DEFAULT_DECOMPOSE_MODEL: &str = "anthropic/claude-sonnet-4";

/// Build a decomposition prompt from a goal description.
/// Returns the full system prompt and user prompt as a tuple.
pub fn build_decomposition_prompt(goal: &serde_json::Value) -> (String, String) {
  let goal_text = match goal {
    serde_json::Value::String(s) => s.clone(),
    other => {
      serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string())
    }
  };

  let system_prompt = "You are a task decomposition engine. Given a goal, break it into \
        independent sub-tasks that can be executed in parallel by AI agents. \
        Respond ONLY with a JSON object: {\"tasks\": [{\"task_id\": \"t1\", \"goal\": \"...\", \
        \"depends_on\": []}, ...]}. Each task needs a unique task_id, a goal description, \
        and an optional depends_on array of task_ids it must wait for. \
        Keep tasks atomic and actionable. Aim for 2-6 tasks."
    .to_string();

  let user_prompt = format!("Decompose this goal into sub-tasks:\n\n{}", goal_text);

  (system_prompt, user_prompt)
}

/// Build an `IoOp::AgentChat` for goal decomposition.
pub fn build_decomposition_chat_op(
  goal: &serde_json::Value,
  provider: Option<&str>,
  model: Option<&str>,
) -> IoOp {
  let (system_prompt, user_prompt) = build_decomposition_prompt(goal);
  IoOp::AgentChat {
    provider: provider.unwrap_or(DEFAULT_DECOMPOSE_PROVIDER).to_string(),
    model: Some(model.unwrap_or(DEFAULT_DECOMPOSE_MODEL).to_string()),
    system_prompt: Some(system_prompt),
    prompt: user_prompt,
    tools: None,
    max_iterations: Some(1),
    timeout_ms: Some(60_000),
  }
}

/// Parse an LLM response string into a tasks JSON value.
/// Tries to extract a JSON object with a "tasks" array.
/// Falls back to wrapping the entire goal as a single task.
pub fn parse_decomposition_response(
  response: &str,
  goal: &serde_json::Value,
) -> serde_json::Value {
  let json_str = extract_json_block(response);

  if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) {
    if let Some(tasks) = parsed.get("tasks") {
      if tasks.is_array() && !tasks.as_array().unwrap().is_empty() {
        return serde_json::json!({ "tasks": tasks });
      }
    }
    if parsed.is_array() && !parsed.as_array().unwrap().is_empty() {
      return serde_json::json!({ "tasks": parsed });
    }
  }

  // Fallback: single task with the original goal
  serde_json::json!({
      "tasks": [{
          "task_id": "task-0",
          "goal": goal.clone(),
      }]
  })
}

/// Extract JSON from a string that might have markdown fences.
fn extract_json_block(s: &str) -> &str {
  if let Some(start) = s.find("```json") {
    let after_fence = &s[start + 7..];
    if let Some(end) = after_fence.find("```") {
      return after_fence[..end].trim();
    }
  }
  if let Some(start) = s.find("```") {
    let after_fence = &s[start + 3..];
    if let Some(end) = after_fence.find("```") {
      return after_fence[..end].trim();
    }
  }
  // Try to find a JSON object or array in the raw string
  let obj_range = s
    .find('{')
    .and_then(|start| s.rfind('}').filter(|&end| end > start).map(|end| (start, end)));
  let arr_range = s
    .find('[')
    .and_then(|start| s.rfind(']').filter(|&end| end > start).map(|end| (start, end)));

  // Pick whichever starts first (array or object)
  match (obj_range, arr_range) {
    (Some((os, oe)), Some((as_, ae))) => {
      if as_ < os {
        return &s[as_..=ae];
      } else {
        return &s[os..=oe];
      }
    }
    (Some((os, oe)), None) => return &s[os..=oe],
    (None, Some((as_, ae))) => return &s[as_..=ae],
    (None, None) => {}
  }
  s.trim()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_build_decomposition_prompt_string_goal() {
    let goal = serde_json::json!("Build a REST API for todo items");
    let (system, user) = build_decomposition_prompt(&goal);
    assert!(system.contains("task decomposition"));
    assert!(user.contains("Build a REST API"));
  }

  #[test]
  fn test_build_decomposition_prompt_json_goal() {
    let goal =
      serde_json::json!({"description": "Research topic X", "depth": "deep"});
    let (_, user) = build_decomposition_prompt(&goal);
    assert!(user.contains("Research topic X"));
  }

  #[test]
  fn test_build_decomposition_chat_op() {
    let goal = serde_json::json!("test goal");
    let op = build_decomposition_chat_op(&goal, None, None);
    match op {
      IoOp::AgentChat {
        provider,
        model,
        system_prompt,
        prompt,
        max_iterations,
        ..
      } => {
        assert_eq!(provider, DEFAULT_DECOMPOSE_PROVIDER);
        assert_eq!(model, Some(DEFAULT_DECOMPOSE_MODEL.to_string()));
        assert!(system_prompt.is_some());
        assert!(prompt.contains("test goal"));
        assert_eq!(max_iterations, Some(1));
      }
      _ => panic!("Expected AgentChat"),
    }
  }

  #[test]
  fn test_build_decomposition_chat_op_custom_provider() {
    let goal = serde_json::json!("test");
    let op =
      build_decomposition_chat_op(&goal, Some("openai"), Some("gpt-4o"));
    match op {
      IoOp::AgentChat {
        provider, model, ..
      } => {
        assert_eq!(provider, "openai");
        assert_eq!(model, Some("gpt-4o".to_string()));
      }
      _ => panic!("Expected AgentChat"),
    }
  }

  #[test]
  fn test_parse_valid_json_response() {
    let response = r#"{"tasks": [{"task_id": "t1", "goal": "Do X"}, {"task_id": "t2", "goal": "Do Y"}]}"#;
    let goal = serde_json::json!("original");
    let result = parse_decomposition_response(response, &goal);
    let tasks = result["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0]["task_id"], "t1");
  }

  #[test]
  fn test_parse_markdown_fenced_response() {
    let response = "Here are the tasks:\n```json\n{\"tasks\": [{\"task_id\": \"t1\", \"goal\": \"Do X\"}]}\n```\nDone!";
    let goal = serde_json::json!("original");
    let result = parse_decomposition_response(response, &goal);
    let tasks = result["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1);
  }

  #[test]
  fn test_parse_array_response() {
    let response = r#"[{"task_id": "t1", "goal": "Do X"}, {"task_id": "t2", "goal": "Do Y"}]"#;
    let goal = serde_json::json!("original");
    let result = parse_decomposition_response(response, &goal);
    let tasks = result["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 2);
  }

  #[test]
  fn test_parse_garbage_falls_back_to_single_task() {
    let response = "I don't understand the request, sorry!";
    let goal = serde_json::json!("Build an API");
    let result = parse_decomposition_response(response, &goal);
    let tasks = result["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["task_id"], "task-0");
    assert_eq!(tasks[0]["goal"], "Build an API");
  }

  #[test]
  fn test_parse_empty_tasks_falls_back() {
    let response = r#"{"tasks": []}"#;
    let goal = serde_json::json!("Build an API");
    let result = parse_decomposition_response(response, &goal);
    let tasks = result["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1);
  }

  #[test]
  fn test_extract_json_block_raw() {
    assert_eq!(extract_json_block(r#"{"a": 1}"#), r#"{"a": 1}"#);
  }

  #[test]
  fn test_extract_json_block_markdown() {
    let input = "text\n```json\n{\"a\": 1}\n```\nmore";
    assert_eq!(extract_json_block(input), "{\"a\": 1}");
  }
}
