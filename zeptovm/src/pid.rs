use std::{
  fmt,
  sync::atomic::{AtomicU64, Ordering},
};

static NEXT_PID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Pid(u64);

impl Pid {
  pub fn new() -> Self {
    Self(NEXT_PID.fetch_add(1, Ordering::Relaxed))
  }

  pub fn from_raw(raw: u64) -> Self {
    Self(raw)
  }

  pub fn raw(&self) -> u64 {
    self.0
  }
}

impl fmt::Display for Pid {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "<0.{}>", self.0)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_pid_unique() {
    let a = Pid::new();
    let b = Pid::new();
    assert_ne!(a, b);
  }

  #[test]
  fn test_pid_raw_roundtrip() {
    let pid = Pid::new();
    let raw = pid.raw();
    let restored = Pid::from_raw(raw);
    assert_eq!(pid, restored);
  }

  #[test]
  fn test_pid_display() {
    let pid = Pid::from_raw(42);
    assert_eq!(format!("{pid}"), "<0.42>");
  }

  #[test]
  fn test_pid_is_copy() {
    let a = Pid::new();
    let b = a; // copy
    assert_eq!(a, b);
  }
}
