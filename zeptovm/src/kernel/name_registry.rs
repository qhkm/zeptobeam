use std::collections::HashMap;

use crate::pid::Pid;

/// Global flat name-based process registry.
///
/// Provides O(1) lookup in both directions (name->Pid, Pid->name).
/// Names are unique — registering a duplicate returns an error.
pub struct NameRegistry {
  names: HashMap<String, Pid>,
  pid_names: HashMap<Pid, String>,
}

impl NameRegistry {
  pub fn new() -> Self {
    Self {
      names: HashMap::new(),
      pid_names: HashMap::new(),
    }
  }

  /// Register a name for a pid. Fails if name already taken.
  pub fn register(
    &mut self,
    name: String,
    pid: Pid,
  ) -> Result<(), String> {
    if self.names.contains_key(&name) {
      return Err(format!(
        "name '{}' already registered",
        name
      ));
    }
    self.names.insert(name.clone(), pid);
    self.pid_names.insert(pid, name);
    Ok(())
  }

  /// Unregister a name. Returns the old pid if it existed.
  pub fn unregister(&mut self, name: &str) -> Option<Pid> {
    if let Some(pid) = self.names.remove(name) {
      self.pid_names.remove(&pid);
      Some(pid)
    } else {
      None
    }
  }

  /// Unregister by pid (for auto-cleanup on exit).
  /// Returns the name if it was registered.
  pub fn unregister_pid(
    &mut self,
    pid: Pid,
  ) -> Option<String> {
    if let Some(name) = self.pid_names.remove(&pid) {
      self.names.remove(&name);
      Some(name)
    } else {
      None
    }
  }

  /// Lookup a pid by name.
  pub fn whereis(&self, name: &str) -> Option<Pid> {
    self.names.get(name).copied()
  }

  /// Reverse lookup: get the registered name for a pid.
  pub fn registered_name(
    &self,
    pid: Pid,
  ) -> Option<&str> {
    self.pid_names.get(&pid).map(|s| s.as_str())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_register_and_whereis() {
    let mut reg = NameRegistry::new();
    let pid = Pid::from_raw(1);
    reg.register("logger".into(), pid).unwrap();
    assert_eq!(reg.whereis("logger"), Some(pid));
  }

  #[test]
  fn test_register_duplicate_name_fails() {
    let mut reg = NameRegistry::new();
    let pid1 = Pid::from_raw(1);
    let pid2 = Pid::from_raw(2);
    reg.register("logger".into(), pid1).unwrap();
    let err = reg.register("logger".into(), pid2);
    assert!(err.is_err());
  }

  #[test]
  fn test_unregister() {
    let mut reg = NameRegistry::new();
    let pid = Pid::from_raw(1);
    reg.register("logger".into(), pid).unwrap();
    let old = reg.unregister("logger");
    assert_eq!(old, Some(pid));
    assert_eq!(reg.whereis("logger"), None);
  }

  #[test]
  fn test_unregister_unknown_returns_none() {
    let mut reg = NameRegistry::new();
    assert_eq!(reg.unregister("nope"), None);
  }

  #[test]
  fn test_unregister_by_pid() {
    let mut reg = NameRegistry::new();
    let pid = Pid::from_raw(1);
    reg.register("logger".into(), pid).unwrap();
    let name = reg.unregister_pid(pid);
    assert_eq!(name, Some("logger".to_string()));
    assert_eq!(reg.whereis("logger"), None);
  }

  #[test]
  fn test_registered_name() {
    let mut reg = NameRegistry::new();
    let pid = Pid::from_raw(1);
    reg.register("logger".into(), pid).unwrap();
    assert_eq!(
      reg.registered_name(pid),
      Some("logger")
    );
  }

  #[test]
  fn test_reregister_after_unregister() {
    let mut reg = NameRegistry::new();
    let pid1 = Pid::from_raw(1);
    let pid2 = Pid::from_raw(2);
    reg.register("logger".into(), pid1).unwrap();
    reg.unregister("logger");
    reg.register("logger".into(), pid2).unwrap();
    assert_eq!(reg.whereis("logger"), Some(pid2));
  }
}
