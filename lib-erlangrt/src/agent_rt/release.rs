use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::agent_rt::error::AgentRtError;

const MAX_SCHEMA_VERSION: u32 = 1;
const IN_PROGRESS_FILENAME: &str = ".in_progress.json";

/// Resolve a dotted key path (e.g. "runtime.worker_count") into a nested
/// JSON value. Falls back to a flat key lookup if no dots are present.
fn resolve_dotted_key<'a>(
    config: &'a serde_json::Value,
    key: &str,
) -> Option<&'a serde_json::Value> {
    let parts: Vec<&str> = key.split('.').collect();
    let mut current = config;
    for part in &parts {
        match current.get(*part) {
            Some(v) => current = v,
            None => return None,
        }
    }
    Some(current)
}

fn fsync_dir(dir: &Path) -> Result<(), AgentRtError> {
    let d = std::fs::File::open(dir)
        .map_err(|e| AgentRtError::Release(format!("open dir for fsync: {}", e)))?;
    d.sync_all()
        .map_err(|e| AgentRtError::Release(format!("fsync dir: {}", e)))?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseManifest {
    pub schema_version: u32,
    pub version: String,
    #[serde(default)]
    pub previous: Option<String>,
    pub behaviors: Vec<BehaviorUpgrade>,
    #[serde(default)]
    pub config_changes: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehaviorUpgrade {
    pub name: String,
    pub version: String,
    pub upgrade_from: String,
    #[serde(default)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReleaseStep {
    UpgradeBehavior {
        name: String,
        from: String,
        to: String,
        extra: serde_json::Value,
    },
    ApplyConfig {
        key: String,
        old_value: serde_json::Value,
        new_value: serde_json::Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleasePlan {
    pub steps: Vec<ReleaseStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InProgressRelease {
    pub schema_version: u32,
    pub manifest_version: String,
    pub plan: ReleasePlan,
    pub completed_steps: usize,
}

impl ReleaseManifest {
    pub fn from_json(json: &str) -> Result<Self, AgentRtError> {
        let manifest: Self = serde_json::from_str(json)
            .map_err(|e| AgentRtError::Release(format!("invalid manifest: {}", e)))?;
        if manifest.schema_version > MAX_SCHEMA_VERSION {
            return Err(AgentRtError::Release(format!(
                "schema version {} not supported, max supported: {}",
                manifest.schema_version, MAX_SCHEMA_VERSION
            )));
        }
        Ok(manifest)
    }

    pub fn from_file(path: &str) -> Result<Self, AgentRtError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| AgentRtError::Release(format!("read manifest: {}", e)))?;
        Self::from_json(&content)
    }
}

impl ReleasePlan {
    pub fn from_manifest(
        manifest: &ReleaseManifest,
        current_config: &serde_json::Value,
    ) -> Self {
        let mut steps = Vec::new();

        // Behavior upgrades first
        for b in &manifest.behaviors {
            steps.push(ReleaseStep::UpgradeBehavior {
                name: b.name.clone(),
                from: b.upgrade_from.clone(),
                to: b.version.clone(),
                extra: b.extra.clone(),
            });
        }

        // Config changes
        for (key, new_value) in &manifest.config_changes {
            let old_value = resolve_dotted_key(current_config, key)
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            steps.push(ReleaseStep::ApplyConfig {
                key: key.clone(),
                old_value,
                new_value: new_value.clone(),
            });
        }

        Self { steps }
    }
}

/// Trait for executing release steps. Callers implement this to
/// wire behavior upgrades and config changes into the runtime.
pub trait StepHandler {
    fn apply_step(&mut self, step: &ReleaseStep) -> Result<(), AgentRtError>;
    fn rollback_step(&mut self, step: &ReleaseStep) -> Result<(), AgentRtError>;
}

pub struct ReleaseManager {
    pub current: Option<ReleaseManifest>,
    pub history: Vec<ReleaseManifest>,
    max_history: usize,
}

impl ReleaseManager {
    pub fn new(max_history: usize) -> Self {
        Self {
            current: None,
            history: Vec::new(),
            max_history,
        }
    }

    /// Execute a release plan transactionally. If any step fails,
    /// all previously completed steps are rolled back in reverse order.
    pub fn apply(
        &mut self,
        manifest: ReleaseManifest,
        plan: &ReleasePlan,
        handler: &mut dyn StepHandler,
    ) -> Result<(), AgentRtError> {
        let mut completed: Vec<&ReleaseStep> = Vec::new();

        for step in &plan.steps {
            match handler.apply_step(step) {
                Ok(()) => {
                    completed.push(step);
                }
                Err(e) => {
                    // Rollback in reverse order
                    for done_step in completed.iter().rev() {
                        if let Err(rb_err) = handler.rollback_step(done_step) {
                            return Err(AgentRtError::Release(format!(
                                "apply failed: {}; rollback also failed: {}",
                                e, rb_err
                            )));
                        }
                    }
                    return Err(e);
                }
            }
        }

        self.record_apply(manifest);
        Ok(())
    }

    /// Roll back the current release by reversing all steps using the
    /// provided plan. On success, removes the current release from
    /// history tracking.
    pub fn rollback(
        &mut self,
        plan: &ReleasePlan,
        handler: &mut dyn StepHandler,
    ) -> Result<(), AgentRtError> {
        if self.current.is_none() {
            return Err(AgentRtError::Release("no current release to rollback".into()));
        }
        // Rollback steps in reverse order
        for step in plan.steps.iter().rev() {
            handler.rollback_step(step)?;
        }
        // Remove current, restore previous from history
        self.current = self.history.pop();
        Ok(())
    }

    pub fn record_apply(&mut self, manifest: ReleaseManifest) {
        if let Some(prev) = self.current.take() {
            self.history.push(prev);
            if self.history.len() > self.max_history {
                self.history.remove(0);
            }
        }
        self.current = Some(manifest);
    }

    pub fn current(&self) -> Option<&ReleaseManifest> {
        self.current.as_ref()
    }

    pub fn history(&self) -> &[ReleaseManifest] {
        &self.history
    }
}

// ---- In-progress persistence (Task 11) ----

pub fn write_in_progress(
    dir: &Path,
    ip: &InProgressRelease,
) -> Result<(), AgentRtError> {
    let tmp = dir.join(".in_progress.json.tmp");
    let target = dir.join(IN_PROGRESS_FILENAME);
    let bytes = serde_json::to_vec_pretty(ip)
        .map_err(|e| AgentRtError::Release(format!("serialize in_progress: {}", e)))?;
    std::fs::write(&tmp, &bytes)
        .map_err(|e| AgentRtError::Release(format!("write tmp: {}", e)))?;
    // fsync
    let f = std::fs::File::open(&tmp)
        .map_err(|e| AgentRtError::Release(format!("open tmp: {}", e)))?;
    f.sync_all()
        .map_err(|e| AgentRtError::Release(format!("fsync tmp: {}", e)))?;
    drop(f);
    std::fs::rename(&tmp, &target)
        .map_err(|e| AgentRtError::Release(format!("rename: {}", e)))?;
    fsync_dir(dir)?;
    Ok(())
}

pub fn read_in_progress(dir: &Path) -> Result<Option<InProgressRelease>, AgentRtError> {
    let path = dir.join(IN_PROGRESS_FILENAME);
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| AgentRtError::Release(format!("read in_progress: {}", e)))?;
    let ip: InProgressRelease = serde_json::from_str(&content)
        .map_err(|e| AgentRtError::Release(format!("bad in_progress: {}", e)))?;
    Ok(Some(ip))
}

pub fn delete_in_progress(dir: &Path) -> Result<(), AgentRtError> {
    let path = dir.join(IN_PROGRESS_FILENAME);
    if path.exists() {
        std::fs::remove_file(&path)
            .map_err(|e| AgentRtError::Release(format!("delete in_progress: {}", e)))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Task 10 tests
    #[test]
    fn test_parse_manifest() {
        let json = r#"{
            "schema_version": 1,
            "version": "1.2.0",
            "previous": "1.1.0",
            "behaviors": [
                {"name": "worker", "version": "v2", "upgrade_from": "v1", "extra": {}}
            ],
            "config_changes": {"runtime.worker_count": 16}
        }"#;
        let manifest = ReleaseManifest::from_json(json).unwrap();
        assert_eq!(manifest.version, "1.2.0");
        assert_eq!(manifest.previous, Some("1.1.0".into()));
        assert_eq!(manifest.behaviors.len(), 1);
        assert_eq!(manifest.behaviors[0].name, "worker");
    }

    #[test]
    fn test_parse_manifest_unsupported_schema() {
        let json = r#"{"schema_version": 99, "version": "1.0.0", "behaviors": []}"#;
        assert!(ReleaseManifest::from_json(json).is_err());
    }

    #[test]
    fn test_build_plan() {
        let manifest = ReleaseManifest {
            schema_version: 1,
            version: "1.2.0".into(),
            previous: Some("1.1.0".into()),
            behaviors: vec![BehaviorUpgrade {
                name: "worker".into(),
                version: "v2".into(),
                upgrade_from: "v1".into(),
                extra: serde_json::json!({}),
            }],
            config_changes: {
                let mut m = HashMap::new();
                m.insert("runtime.worker_count".into(), serde_json::json!(16));
                m
            },
        };
        let plan = ReleasePlan::from_manifest(
            &manifest,
            &serde_json::json!({"runtime.worker_count": 4}),
        );
        assert_eq!(plan.steps.len(), 2); // 1 behavior + 1 config
    }

    #[test]
    fn test_plan_captures_old_values() {
        let manifest = ReleaseManifest {
            schema_version: 1,
            version: "1.0.0".into(),
            previous: None,
            behaviors: vec![],
            config_changes: {
                let mut m = HashMap::new();
                m.insert("key".into(), serde_json::json!("new"));
                m
            },
        };
        let current_config = serde_json::json!({"key": "old"});
        let plan = ReleasePlan::from_manifest(&manifest, &current_config);
        match &plan.steps[0] {
            ReleaseStep::ApplyConfig {
                old_value,
                new_value,
                ..
            } => {
                assert_eq!(old_value, &serde_json::json!("old"));
                assert_eq!(new_value, &serde_json::json!("new"));
            }
            _ => panic!("expected ApplyConfig"),
        }
    }

    #[test]
    fn test_dotted_key_resolution() {
        let manifest = ReleaseManifest {
            schema_version: 1,
            version: "1.0.0".into(),
            previous: None,
            behaviors: vec![],
            config_changes: {
                let mut m = HashMap::new();
                m.insert("runtime.worker_count".into(), serde_json::json!(16));
                m
            },
        };
        let current_config = serde_json::json!({"runtime": {"worker_count": 4}});
        let plan = ReleasePlan::from_manifest(&manifest, &current_config);
        match &plan.steps[0] {
            ReleaseStep::ApplyConfig { old_value, .. } => {
                assert_eq!(old_value, &serde_json::json!(4));
            }
            _ => panic!("expected ApplyConfig"),
        }
    }

    #[test]
    fn test_dotted_key_missing_returns_null() {
        let manifest = ReleaseManifest {
            schema_version: 1,
            version: "1.0.0".into(),
            previous: None,
            behaviors: vec![],
            config_changes: {
                let mut m = HashMap::new();
                m.insert("runtime.missing_key".into(), serde_json::json!(1));
                m
            },
        };
        let current_config = serde_json::json!({"runtime": {"worker_count": 4}});
        let plan = ReleasePlan::from_manifest(&manifest, &current_config);
        match &plan.steps[0] {
            ReleaseStep::ApplyConfig { old_value, .. } => {
                assert_eq!(old_value, &serde_json::Value::Null);
            }
            _ => panic!("expected ApplyConfig"),
        }
    }

    #[test]
    fn test_in_progress_serialization() {
        let plan = ReleasePlan {
            steps: vec![ReleaseStep::UpgradeBehavior {
                name: "w".into(),
                from: "v1".into(),
                to: "v2".into(),
                extra: serde_json::json!({}),
            }],
        };
        let ip = InProgressRelease {
            schema_version: 1,
            manifest_version: "1.0.0".into(),
            plan,
            completed_steps: 0,
        };
        let json = serde_json::to_string(&ip).unwrap();
        let parsed: InProgressRelease = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.completed_steps, 0);
        assert_eq!(parsed.plan.steps.len(), 1);
    }

    // Task 11 tests
    #[test]
    fn test_write_and_read_in_progress() {
        let dir = tempfile::tempdir().unwrap();
        let ip = InProgressRelease {
            schema_version: 1,
            manifest_version: "1.0.0".into(),
            plan: ReleasePlan { steps: vec![] },
            completed_steps: 0,
        };
        write_in_progress(dir.path(), &ip).unwrap();
        let loaded = read_in_progress(dir.path()).unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().manifest_version, "1.0.0");
    }

    #[test]
    fn test_delete_in_progress() {
        let dir = tempfile::tempdir().unwrap();
        let ip = InProgressRelease {
            schema_version: 1,
            manifest_version: "1.0.0".into(),
            plan: ReleasePlan { steps: vec![] },
            completed_steps: 0,
        };
        write_in_progress(dir.path(), &ip).unwrap();
        delete_in_progress(dir.path()).unwrap();
        assert!(read_in_progress(dir.path()).unwrap().is_none());
    }

    #[test]
    fn test_update_completed_steps() {
        let dir = tempfile::tempdir().unwrap();
        let mut ip = InProgressRelease {
            schema_version: 1,
            manifest_version: "1.0.0".into(),
            plan: ReleasePlan {
                steps: vec![ReleaseStep::ApplyConfig {
                    key: "k".into(),
                    old_value: serde_json::json!(1),
                    new_value: serde_json::json!(2),
                }],
            },
            completed_steps: 0,
        };
        write_in_progress(dir.path(), &ip).unwrap();
        ip.completed_steps = 1;
        write_in_progress(dir.path(), &ip).unwrap();

        let loaded = read_in_progress(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.completed_steps, 1);
    }

    struct TestHandler {
        applied: Vec<String>,
        fail_on: Option<String>,
    }

    impl TestHandler {
        fn new() -> Self {
            Self { applied: Vec::new(), fail_on: None }
        }
        fn failing_on(step_key: &str) -> Self {
            Self { applied: Vec::new(), fail_on: Some(step_key.to_string()) }
        }
    }

    impl StepHandler for TestHandler {
        fn apply_step(&mut self, step: &ReleaseStep) -> Result<(), AgentRtError> {
            let key = match step {
                ReleaseStep::UpgradeBehavior { name, .. } => name.clone(),
                ReleaseStep::ApplyConfig { key, .. } => key.clone(),
            };
            if self.fail_on.as_deref() == Some(&key) {
                return Err(AgentRtError::Release(format!("fail on {}", key)));
            }
            self.applied.push(key);
            Ok(())
        }
        fn rollback_step(&mut self, step: &ReleaseStep) -> Result<(), AgentRtError> {
            let key = match step {
                ReleaseStep::UpgradeBehavior { name, .. } => name.clone(),
                ReleaseStep::ApplyConfig { key, .. } => key.clone(),
            };
            self.applied.retain(|k| k != &key);
            Ok(())
        }
    }

    #[test]
    fn test_apply_success() {
        let mut mgr = ReleaseManager::new(5);
        let manifest = ReleaseManifest {
            schema_version: 1,
            version: "1.0.0".into(),
            previous: None,
            behaviors: vec![BehaviorUpgrade {
                name: "worker".into(),
                version: "v2".into(),
                upgrade_from: "v1".into(),
                extra: serde_json::json!({}),
            }],
            config_changes: {
                let mut m = HashMap::new();
                m.insert("key".into(), serde_json::json!(42));
                m
            },
        };
        let plan = ReleasePlan::from_manifest(&manifest, &serde_json::json!({}));
        let mut handler = TestHandler::new();
        mgr.apply(manifest, &plan, &mut handler).unwrap();
        assert_eq!(handler.applied.len(), 2);
        assert_eq!(mgr.current().unwrap().version, "1.0.0");
    }

    #[test]
    fn test_apply_rollback_on_failure() {
        let mut mgr = ReleaseManager::new(5);
        let manifest = ReleaseManifest {
            schema_version: 1,
            version: "1.0.0".into(),
            previous: None,
            behaviors: vec![BehaviorUpgrade {
                name: "worker".into(),
                version: "v2".into(),
                upgrade_from: "v1".into(),
                extra: serde_json::json!({}),
            }],
            config_changes: {
                let mut m = HashMap::new();
                m.insert("fail_key".into(), serde_json::json!(1));
                m
            },
        };
        let plan = ReleasePlan::from_manifest(&manifest, &serde_json::json!({}));
        let mut handler = TestHandler::failing_on("fail_key");
        let result = mgr.apply(manifest, &plan, &mut handler);
        assert!(result.is_err());
        // Behavior step was rolled back
        assert!(handler.applied.is_empty());
        // No current release was set
        assert!(mgr.current().is_none());
    }

    #[test]
    fn test_rollback_current_release() {
        let mut mgr = ReleaseManager::new(5);
        let manifest = ReleaseManifest {
            schema_version: 1,
            version: "1.0.0".into(),
            previous: None,
            behaviors: vec![],
            config_changes: HashMap::new(),
        };
        let plan = ReleasePlan { steps: vec![
            ReleaseStep::ApplyConfig {
                key: "k".into(),
                old_value: serde_json::json!(1),
                new_value: serde_json::json!(2),
            },
        ]};
        let mut handler = TestHandler::new();
        mgr.apply(manifest, &plan, &mut handler).unwrap();
        assert!(mgr.current().is_some());
        mgr.rollback(&plan, &mut handler).unwrap();
        assert!(mgr.current().is_none());
    }

    #[test]
    fn test_rollback_without_current_fails() {
        let mut mgr = ReleaseManager::new(5);
        let plan = ReleasePlan { steps: vec![] };
        let mut handler = TestHandler::new();
        assert!(mgr.rollback(&plan, &mut handler).is_err());
    }

    #[test]
    fn test_release_manager_history_cap() {
        let mut mgr = ReleaseManager::new(2);
        for i in 0..5 {
            mgr.record_apply(ReleaseManifest {
                schema_version: 1,
                version: format!("v{}", i),
                previous: None,
                behaviors: vec![],
                config_changes: HashMap::new(),
            });
        }
        assert_eq!(mgr.history().len(), 2); // capped at 2
        assert_eq!(mgr.current().unwrap().version, "v4");
    }
}
