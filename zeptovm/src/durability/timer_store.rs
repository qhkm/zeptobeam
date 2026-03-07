use rusqlite::{params, Connection, Result as SqlResult};

use crate::core::timer::{TimerId, TimerKind, TimerSpec};
use crate::pid::Pid;

pub struct TimerStore {
  conn: Connection,
}

impl TimerStore {
  pub fn open_in_memory() -> SqlResult<Self> {
    let conn = Connection::open_in_memory()?;
    let store = Self { conn };
    store.init_schema()?;
    Ok(store)
  }

  pub fn open(path: &str) -> SqlResult<Self> {
    let conn = Connection::open(path)?;
    let store = Self { conn };
    store.init_schema()?;
    Ok(store)
  }

  fn init_schema(&self) -> SqlResult<()> {
    self.conn.execute_batch(
      "CREATE TABLE IF NOT EXISTS timers (
                timer_id INTEGER PRIMARY KEY,
                owner_pid INTEGER NOT NULL,
                kind TEXT NOT NULL,
                deadline_ms INTEGER NOT NULL,
                payload_json TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_timers_owner
                ON timers(owner_pid);",
    )?;
    Ok(())
  }

  pub fn save(&self, spec: &TimerSpec) -> SqlResult<()> {
    let kind_str = match &spec.kind {
      TimerKind::SleepUntil => "sleep_until",
      TimerKind::Timeout => "timeout",
      TimerKind::RetryBackoff => "retry_backoff",
    };
    let payload_json =
      spec.payload.as_ref().map(|v| v.to_string());
    self.conn.execute(
      "INSERT OR REPLACE INTO timers \
             (timer_id, owner_pid, kind, deadline_ms, payload_json) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
      params![
        spec.id.raw() as i64,
        spec.owner.raw() as i64,
        kind_str,
        spec.deadline_ms as i64,
        payload_json,
      ],
    )?;
    Ok(())
  }

  pub fn delete(&self, timer_id: TimerId) -> SqlResult<bool> {
    let rows = self.conn.execute(
      "DELETE FROM timers WHERE timer_id = ?1",
      params![timer_id.raw() as i64],
    )?;
    Ok(rows > 0)
  }

  pub fn load_for_pid(
    &self,
    pid: Pid,
  ) -> SqlResult<Vec<TimerSpec>> {
    let mut stmt = self.conn.prepare(
      "SELECT timer_id, owner_pid, kind, deadline_ms, payload_json \
             FROM timers WHERE owner_pid = ?1",
    )?;
    let specs = stmt
      .query_map(params![pid.raw() as i64], |row| {
        Self::row_to_spec(row)
      })?
      .collect::<SqlResult<Vec<_>>>()?;
    Ok(specs)
  }

  pub fn load_all(&self) -> SqlResult<Vec<TimerSpec>> {
    let mut stmt = self.conn.prepare(
      "SELECT timer_id, owner_pid, kind, deadline_ms, payload_json \
             FROM timers ORDER BY deadline_ms",
    )?;
    let specs = stmt
      .query_map([], |row| Self::row_to_spec(row))?
      .collect::<SqlResult<Vec<_>>>()?;
    Ok(specs)
  }

  fn row_to_spec(
    row: &rusqlite::Row<'_>,
  ) -> rusqlite::Result<TimerSpec> {
    let timer_id: i64 = row.get(0)?;
    let owner_pid: i64 = row.get(1)?;
    let kind_str: String = row.get(2)?;
    let deadline_ms: i64 = row.get(3)?;
    let payload_json: Option<String> = row.get(4)?;

    let kind = match kind_str.as_str() {
      "timeout" => TimerKind::Timeout,
      "retry_backoff" => TimerKind::RetryBackoff,
      _ => TimerKind::SleepUntil,
    };
    let payload =
      payload_json.and_then(|s| serde_json::from_str(&s).ok());

    Ok(TimerSpec {
      id: TimerId::from_raw(timer_id as u64),
      owner: Pid::from_raw(owner_pid as u64),
      kind,
      deadline_ms: deadline_ms as u64,
      payload,
      durable: true,
    })
  }

  pub fn count(&self) -> SqlResult<i64> {
    self
      .conn
      .query_row("SELECT COUNT(*) FROM timers", [], |row| {
        row.get(0)
      })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::core::timer::{TimerKind, TimerSpec};
  use crate::pid::Pid;

  fn make_durable(
    pid: Pid,
    deadline: u64,
    kind: TimerKind,
  ) -> TimerSpec {
    TimerSpec::new(pid, kind, deadline).with_durable(true)
  }

  #[test]
  fn test_timer_store_save_and_load() {
    let store = TimerStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);
    let spec = make_durable(pid, 5000, TimerKind::SleepUntil);
    store.save(&spec).unwrap();
    let loaded = store.load_for_pid(pid).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].deadline_ms, 5000);
    assert_eq!(loaded[0].kind, TimerKind::SleepUntil);
  }

  #[test]
  fn test_timer_store_delete() {
    let store = TimerStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);
    let spec = make_durable(pid, 5000, TimerKind::Timeout);
    let id = spec.id;
    store.save(&spec).unwrap();
    assert!(store.delete(id).unwrap());
    assert_eq!(store.load_for_pid(pid).unwrap().len(), 0);
  }

  #[test]
  fn test_timer_store_load_all() {
    let store = TimerStore::open_in_memory().unwrap();
    let pid1 = Pid::from_raw(1);
    let pid2 = Pid::from_raw(2);
    store
      .save(&make_durable(pid1, 100, TimerKind::SleepUntil))
      .unwrap();
    store
      .save(&make_durable(pid2, 200, TimerKind::Timeout))
      .unwrap();
    store
      .save(&make_durable(
        pid1,
        300,
        TimerKind::RetryBackoff,
      ))
      .unwrap();
    let all = store.load_all().unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].deadline_ms, 100);
    assert_eq!(all[2].deadline_ms, 300);
  }

  #[test]
  fn test_timer_store_with_payload() {
    let store = TimerStore::open_in_memory().unwrap();
    let pid = Pid::from_raw(1);
    let spec = make_durable(pid, 5000, TimerKind::SleepUntil)
      .with_payload(serde_json::json!({"msg": "wake"}));
    store.save(&spec).unwrap();
    let loaded = store.load_for_pid(pid).unwrap();
    assert_eq!(
      loaded[0].payload.as_ref().unwrap()["msg"],
      "wake"
    );
  }

  #[test]
  fn test_timer_store_count() {
    let store = TimerStore::open_in_memory().unwrap();
    assert_eq!(store.count().unwrap(), 0);
    let pid = Pid::from_raw(1);
    store
      .save(&make_durable(pid, 100, TimerKind::SleepUntil))
      .unwrap();
    store
      .save(&make_durable(pid, 200, TimerKind::Timeout))
      .unwrap();
    assert_eq!(store.count().unwrap(), 2);
  }

  #[test]
  fn test_timer_store_isolates_pids() {
    let store = TimerStore::open_in_memory().unwrap();
    let pid1 = Pid::from_raw(1);
    let pid2 = Pid::from_raw(2);
    store
      .save(&make_durable(pid1, 100, TimerKind::SleepUntil))
      .unwrap();
    store
      .save(&make_durable(pid2, 200, TimerKind::Timeout))
      .unwrap();
    assert_eq!(store.load_for_pid(pid1).unwrap().len(), 1);
    assert_eq!(store.load_for_pid(pid2).unwrap().len(), 1);
  }
}
