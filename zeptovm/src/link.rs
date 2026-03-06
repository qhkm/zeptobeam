use std::{
  collections::{HashMap, HashSet},
  sync::atomic::{AtomicU64, Ordering},
};

use crate::pid::Pid;

static NEXT_MONITOR_REF: AtomicU64 = AtomicU64::new(1);

/// Opaque reference identifying a monitor relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MonitorRef(u64);

impl MonitorRef {
  pub fn new() -> Self {
    Self(NEXT_MONITOR_REF.fetch_add(1, Ordering::Relaxed))
  }
}

/// Tracks bidirectional links and unidirectional monitors between processes.
pub struct LinkTable {
  links: HashMap<Pid, HashSet<Pid>>,
  monitors: HashMap<Pid, Vec<(MonitorRef, Pid)>>,
  monitor_index: HashMap<MonitorRef, (Pid, Pid)>,
}

impl LinkTable {
  pub fn new() -> Self {
    Self {
      links: HashMap::new(),
      monitors: HashMap::new(),
      monitor_index: HashMap::new(),
    }
  }

  /// Create a bidirectional link between two processes.
  pub fn link(&mut self, a: Pid, b: Pid) {
    self.links.entry(a).or_default().insert(b);
    self.links.entry(b).or_default().insert(a);
  }

  /// Remove a bidirectional link.
  pub fn unlink(&mut self, a: Pid, b: Pid) {
    if let Some(set) = self.links.get_mut(&a) {
      set.remove(&b);
      if set.is_empty() {
        self.links.remove(&a);
      }
    }
    if let Some(set) = self.links.get_mut(&b) {
      set.remove(&a);
      if set.is_empty() {
        self.links.remove(&b);
      }
    }
  }

  /// Get all pids linked to the given pid.
  pub fn get_links(&self, pid: &Pid) -> Vec<Pid> {
    self
      .links
      .get(pid)
      .map(|s| s.iter().copied().collect())
      .unwrap_or_default()
  }

  /// Remove all links for a pid. Returns the previously-linked pids.
  pub fn remove_all(&mut self, pid: &Pid) -> Vec<Pid> {
    let linked = self.get_links(pid);
    self.links.remove(pid);
    for other in &linked {
      if let Some(set) = self.links.get_mut(other) {
        set.remove(pid);
        if set.is_empty() {
          self.links.remove(other);
        }
      }
    }
    linked
  }

  /// Create a unidirectional monitor: watcher monitors target.
  pub fn monitor(&mut self, watcher: Pid, target: Pid) -> MonitorRef {
    let mref = MonitorRef::new();
    self
      .monitors
      .entry(target)
      .or_default()
      .push((mref, watcher));
    self.monitor_index.insert(mref, (watcher, target));
    mref
  }

  /// Remove a monitor.
  pub fn demonitor(&mut self, mref: MonitorRef) {
    if let Some((_, target)) = self.monitor_index.remove(&mref) {
      if let Some(monitors) = self.monitors.get_mut(&target) {
        monitors.retain(|(r, _)| *r != mref);
        if monitors.is_empty() {
          self.monitors.remove(&target);
        }
      }
    }
  }

  /// Get all monitors watching the given target pid.
  pub fn get_monitors_of(&self, target: &Pid) -> Vec<(MonitorRef, Pid)> {
    self.monitors.get(target).cloned().unwrap_or_default()
  }

  /// Remove all monitors of a target. Returns watchers to notify.
  pub fn remove_monitors_of(&mut self, target: &Pid) -> Vec<(MonitorRef, Pid)> {
    let monitors = self.monitors.remove(target).unwrap_or_default();
    for (mref, _) in &monitors {
      self.monitor_index.remove(mref);
    }
    monitors
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::pid::Pid;

  #[test]
  fn test_link_bidirectional() {
    let mut table = LinkTable::new();
    let a = Pid::from_raw(1);
    let b = Pid::from_raw(2);
    table.link(a, b);
    assert_eq!(table.get_links(&a), vec![b]);
    assert_eq!(table.get_links(&b), vec![a]);
  }

  #[test]
  fn test_unlink() {
    let mut table = LinkTable::new();
    let a = Pid::from_raw(1);
    let b = Pid::from_raw(2);
    table.link(a, b);
    table.unlink(a, b);
    assert!(table.get_links(&a).is_empty());
    assert!(table.get_links(&b).is_empty());
  }

  #[test]
  fn test_remove_all_links() {
    let mut table = LinkTable::new();
    let a = Pid::from_raw(1);
    let b = Pid::from_raw(2);
    let c = Pid::from_raw(3);
    table.link(a, b);
    table.link(a, c);
    let linked = table.remove_all(&a);
    assert_eq!(linked.len(), 2);
    assert!(table.get_links(&a).is_empty());
    assert!(table.get_links(&b).is_empty());
    assert!(table.get_links(&c).is_empty());
  }

  #[test]
  fn test_monitor_unidirectional() {
    let mut table = LinkTable::new();
    let watcher = Pid::from_raw(1);
    let target = Pid::from_raw(2);
    let mref = table.monitor(watcher, target);
    assert_eq!(table.get_monitors_of(&target), vec![(mref, watcher)]);
  }

  #[test]
  fn test_demonitor() {
    let mut table = LinkTable::new();
    let watcher = Pid::from_raw(1);
    let target = Pid::from_raw(2);
    let mref = table.monitor(watcher, target);
    table.demonitor(mref);
    assert!(table.get_monitors_of(&target).is_empty());
  }
}
