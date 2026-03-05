use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::agent_rt::error::AgentRtError;
use crate::agent_rt::ets::AccessType;
use crate::agent_rt::types::AgentPid;

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct DetsHeader {
  schema_version: u32,
  #[serde(rename = "type")]
  file_type: String,
  table: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct DetsEntry {
  key: String,
  value: serde_json::Value,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct DetsLogEntry {
  op: String, // "put" or "delete"
  key: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  value: Option<serde_json::Value>,
}

pub struct DetsTable {
  pub name: String,
  dir: PathBuf,
  pub access: AccessType,
  pub owner: AgentPid,
  pub heir: Option<AgentPid>,
  cache: HashMap<String, serde_json::Value>,
  max_entries: usize,
  dirty_count: usize,
  compact_threshold: usize,
  log_writer: Option<BufWriter<fs::File>>,
}

impl DetsTable {
  pub fn open(
    name: &str,
    dir: &Path,
    access: AccessType,
    owner: AgentPid,
    heir: Option<AgentPid>,
    max_entries: usize,
    compact_threshold: usize,
  ) -> Result<Self, AgentRtError> {
    fs::create_dir_all(dir)
      .map_err(|e| AgentRtError::Ets(format!("create dir: {}", e)))?;
    let mut cache = HashMap::new();

    let dat_path = dir.join(format!("{}.dat", name));
    let log_path = dir.join(format!("{}.log", name));

    // Load .dat if exists
    if dat_path.exists() {
      let file = fs::File::open(&dat_path)
        .map_err(|e| AgentRtError::Ets(format!("open dat: {}", e)))?;
      let reader = BufReader::new(file);
      let mut first = true;
      for line in reader.lines() {
        let line = line
          .map_err(|e| AgentRtError::Ets(format!("read dat: {}", e)))?;
        if line.trim().is_empty() {
          continue;
        }
        if first {
          // Verify schema header
          let header: DetsHeader = serde_json::from_str(&line)
            .map_err(|e| {
              AgentRtError::Ets(format!("bad dat header: {}", e))
            })?;
          if header.schema_version > SCHEMA_VERSION {
            return Err(AgentRtError::Ets(format!(
              "schema version {} not supported, max supported: {}",
              header.schema_version, SCHEMA_VERSION
            )));
          }
          first = false;
          continue;
        }
        let entry: DetsEntry = serde_json::from_str(&line)
          .map_err(|e| {
            AgentRtError::Ets(format!("bad dat entry: {}", e))
          })?;
        cache.insert(entry.key, entry.value);
      }
    }

    // Replay .log if exists
    if log_path.exists() {
      let file = fs::File::open(&log_path)
        .map_err(|e| AgentRtError::Ets(format!("open log: {}", e)))?;
      let reader = BufReader::new(file);
      for line in reader.lines() {
        let line = line
          .map_err(|e| AgentRtError::Ets(format!("read log: {}", e)))?;
        if line.trim().is_empty() {
          continue;
        }
        let entry: DetsLogEntry = serde_json::from_str(&line)
          .map_err(|e| {
            AgentRtError::Ets(format!("bad log entry: {}", e))
          })?;
        match entry.op.as_str() {
          "put" => {
            if let Some(val) = entry.value {
              cache.insert(entry.key, val);
            }
          }
          "delete" => {
            cache.remove(&entry.key);
          }
          _ => {}
        }
      }
    }

    // Open log for appending
    let log_file = fs::OpenOptions::new()
      .create(true)
      .append(true)
      .open(&log_path)
      .map_err(|e| {
        AgentRtError::Ets(format!("open log for append: {}", e))
      })?;
    let log_writer = Some(BufWriter::new(log_file));

    Ok(Self {
      name: name.to_string(),
      dir: dir.to_path_buf(),
      access,
      owner,
      heir,
      cache,
      max_entries,
      dirty_count: 0,
      compact_threshold,
      log_writer,
    })
  }

  pub fn get(
    &self,
    key: &str,
    caller: AgentPid,
  ) -> Result<Option<serde_json::Value>, AgentRtError> {
    self.check_read(caller)?;
    Ok(self.cache.get(key).cloned())
  }

  pub fn put(
    &mut self,
    key: &str,
    value: &serde_json::Value,
    caller: AgentPid,
  ) -> Result<(), AgentRtError> {
    self.check_write(caller)?;
    let is_new = !self.cache.contains_key(key);
    if is_new && self.cache.len() >= self.max_entries {
      return Err(AgentRtError::Ets("table full".into()));
    }

    // Append to log
    let entry = DetsLogEntry {
      op: "put".into(),
      key: key.to_string(),
      value: Some(value.clone()),
    };
    self.append_log(&entry)?;

    // Update cache
    self.cache.insert(key.to_string(), value.clone());
    self.dirty_count += 1;

    if self.dirty_count >= self.compact_threshold {
      self.compact()?;
    }

    Ok(())
  }

  pub fn delete_key(
    &mut self,
    key: &str,
    caller: AgentPid,
  ) -> Result<(), AgentRtError> {
    self.check_write(caller)?;
    let entry = DetsLogEntry {
      op: "delete".into(),
      key: key.to_string(),
      value: None,
    };
    self.append_log(&entry)?;
    self.cache.remove(key);
    self.dirty_count += 1;

    if self.dirty_count >= self.compact_threshold {
      self.compact()?;
    }

    Ok(())
  }

  pub fn scan(
    &self,
    prefix: &str,
    caller: AgentPid,
  ) -> Result<Vec<(String, serde_json::Value)>, AgentRtError> {
    self.check_read(caller)?;
    Ok(
      self
        .cache
        .iter()
        .filter(|(k, _)| k.starts_with(prefix))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect(),
    )
  }

  pub fn filter(
    &self,
    caller: AgentPid,
    predicate: &dyn Fn(&str, &serde_json::Value) -> bool,
  ) -> Result<Vec<(String, serde_json::Value)>, AgentRtError> {
    self.check_read(caller)?;
    Ok(
      self
        .cache
        .iter()
        .filter(|(k, v)| predicate(k.as_str(), v))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect(),
    )
  }

  pub fn count(&self, caller: AgentPid) -> Result<usize, AgentRtError> {
    self.check_read(caller)?;
    Ok(self.cache.len())
  }

  pub fn compact(&mut self) -> Result<(), AgentRtError> {
    let dat_tmp = self.dir.join(format!("{}.dat.tmp", self.name));
    let dat_path = self.dir.join(format!("{}.dat", self.name));
    let log_path = self.dir.join(format!("{}.log", self.name));

    // Write .dat.tmp from cache snapshot
    let file = fs::File::create(&dat_tmp)
      .map_err(|e| AgentRtError::Ets(format!("create dat.tmp: {}", e)))?;
    let mut writer = BufWriter::new(file);
    let header = DetsHeader {
      schema_version: SCHEMA_VERSION,
      file_type: "dets".into(),
      table: self.name.clone(),
    };
    writeln!(
      writer,
      "{}",
      serde_json::to_string(&header)
        .map_err(|e| AgentRtError::Ets(format!("serialize header: {}", e)))?
    )
    .map_err(|e| AgentRtError::Ets(format!("write header: {}", e)))?;
    for (key, value) in &self.cache {
      let entry = DetsEntry {
        key: key.clone(),
        value: value.clone(),
      };
      writeln!(
        writer,
        "{}",
        serde_json::to_string(&entry)
          .map_err(|e| AgentRtError::Ets(format!("serialize entry: {}", e)))?
      )
      .map_err(|e| AgentRtError::Ets(format!("write entry: {}", e)))?;
    }
    writer
      .flush()
      .map_err(|e| AgentRtError::Ets(format!("flush: {}", e)))?;
    writer
      .get_ref()
      .sync_all()
      .map_err(|e| AgentRtError::Ets(format!("sync: {}", e)))?;
    drop(writer);

    // Atomic rename
    fs::rename(&dat_tmp, &dat_path)
      .map_err(|e| AgentRtError::Ets(format!("rename: {}", e)))?;
    fsync_dir(&self.dir)?;

    // Truncate log
    let log_file = fs::OpenOptions::new()
      .create(true)
      .write(true)
      .truncate(true)
      .open(&log_path)
      .map_err(|e| AgentRtError::Ets(format!("truncate log: {}", e)))?;
    self.log_writer = Some(BufWriter::new(log_file));
    self.dirty_count = 0;

    Ok(())
  }

  pub fn close(&mut self) -> Result<(), AgentRtError> {
    if let Some(ref mut writer) = self.log_writer {
      writer
        .flush()
        .map_err(|e| AgentRtError::Ets(format!("flush on close: {}", e)))?;
    }
    self.log_writer = None;
    Ok(())
  }

  fn append_log(
    &mut self,
    entry: &DetsLogEntry,
  ) -> Result<(), AgentRtError> {
    if let Some(ref mut writer) = self.log_writer {
      let line = serde_json::to_string(entry)
        .map_err(|e| AgentRtError::Ets(format!("serialize log: {}", e)))?;
      writeln!(writer, "{}", line)
        .map_err(|e| AgentRtError::Ets(format!("write log: {}", e)))?;
      writer
        .flush()
        .map_err(|e| AgentRtError::Ets(format!("flush log: {}", e)))?;
    }
    Ok(())
  }

  fn check_write(&self, caller: AgentPid) -> Result<(), AgentRtError> {
    match self.access {
      AccessType::Public => Ok(()),
      AccessType::Protected | AccessType::Private => {
        if caller == self.owner {
          Ok(())
        } else {
          Err(AgentRtError::Ets("write access denied".into()))
        }
      }
    }
  }

  fn check_read(&self, caller: AgentPid) -> Result<(), AgentRtError> {
    match self.access {
      AccessType::Public | AccessType::Protected => Ok(()),
      AccessType::Private => {
        if caller == self.owner {
          Ok(())
        } else {
          Err(AgentRtError::Ets("read access denied".into()))
        }
      }
    }
  }
}

fn fsync_dir(dir: &Path) -> Result<(), AgentRtError> {
  let d = std::fs::File::open(dir)
    .map_err(|e| AgentRtError::Ets(format!("open dir for fsync: {}", e)))?;
  d.sync_all()
    .map_err(|e| AgentRtError::Ets(format!("fsync dir: {}", e)))?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::agent_rt::types::AgentPid;

  #[test]
  fn test_dets_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let owner = AgentPid::new();
    let mut table = DetsTable::open(
      "test",
      dir.path(),
      AccessType::Public,
      owner,
      None,
      100_000,
      100,
    )
    .unwrap();
    table
      .put("key1", &serde_json::json!("val1"), owner)
      .unwrap();
    assert_eq!(
      table.get("key1", owner).unwrap(),
      Some(serde_json::json!("val1"))
    );
    table.close().unwrap();

    // Reopen and verify persistence
    let table = DetsTable::open(
      "test",
      dir.path(),
      AccessType::Public,
      owner,
      None,
      100_000,
      100,
    )
    .unwrap();
    assert_eq!(
      table.get("key1", owner).unwrap(),
      Some(serde_json::json!("val1"))
    );
  }

  #[test]
  fn test_dets_delete_persists() {
    let dir = tempfile::tempdir().unwrap();
    let owner = AgentPid::new();
    let mut table = DetsTable::open(
      "test",
      dir.path(),
      AccessType::Public,
      owner,
      None,
      100_000,
      100,
    )
    .unwrap();
    table.put("k", &serde_json::json!(42), owner).unwrap();
    table.delete_key("k", owner).unwrap();
    table.close().unwrap();

    let table = DetsTable::open(
      "test",
      dir.path(),
      AccessType::Public,
      owner,
      None,
      100_000,
      100,
    )
    .unwrap();
    assert_eq!(table.get("k", owner).unwrap(), None);
  }

  #[test]
  fn test_dets_capacity_limit() {
    let dir = tempfile::tempdir().unwrap();
    let owner = AgentPid::new();
    let mut table = DetsTable::open(
      "test",
      dir.path(),
      AccessType::Public,
      owner,
      None,
      2,
      100,
    )
    .unwrap();
    table.put("a", &serde_json::json!(1), owner).unwrap();
    table.put("b", &serde_json::json!(2), owner).unwrap();
    assert!(table.put("c", &serde_json::json!(3), owner).is_err());
  }

  #[test]
  fn test_dets_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let owner = AgentPid::new();
    let mut table = DetsTable::open(
      "test",
      dir.path(),
      AccessType::Public,
      owner,
      None,
      100_000,
      3,
    )
    .unwrap();
    // 3 mutations triggers compaction
    table.put("a", &serde_json::json!(1), owner).unwrap();
    table.put("b", &serde_json::json!(2), owner).unwrap();
    table.put("c", &serde_json::json!(3), owner).unwrap(); // triggers compact
    table.close().unwrap();

    // After compaction, .log should be empty/gone, .dat has all data
    let table = DetsTable::open(
      "test",
      dir.path(),
      AccessType::Public,
      owner,
      None,
      100_000,
      3,
    )
    .unwrap();
    assert_eq!(table.get("a", owner).unwrap(), Some(serde_json::json!(1)));
    assert_eq!(table.get("b", owner).unwrap(), Some(serde_json::json!(2)));
    assert_eq!(table.get("c", owner).unwrap(), Some(serde_json::json!(3)));
  }

  #[test]
  fn test_dets_recovery_from_dat_plus_log() {
    let dir = tempfile::tempdir().unwrap();
    let owner = AgentPid::new();
    let mut table = DetsTable::open(
      "test",
      dir.path(),
      AccessType::Public,
      owner,
      None,
      100_000,
      100,
    )
    .unwrap();
    table.put("a", &serde_json::json!(1), owner).unwrap();
    // Force compact to write .dat
    table.compact().unwrap();
    // Add more entries (goes to .log only)
    table.put("b", &serde_json::json!(2), owner).unwrap();
    drop(table); // simulate crash -- no close()

    // Recovery: load .dat + replay .log
    let table = DetsTable::open(
      "test",
      dir.path(),
      AccessType::Public,
      owner,
      None,
      100_000,
      100,
    )
    .unwrap();
    assert_eq!(table.get("a", owner).unwrap(), Some(serde_json::json!(1)));
    assert_eq!(table.get("b", owner).unwrap(), Some(serde_json::json!(2)));
  }

  #[test]
  fn test_dets_scan_and_filter() {
    let dir = tempfile::tempdir().unwrap();
    let owner = AgentPid::new();
    let mut table = DetsTable::open(
      "test",
      dir.path(),
      AccessType::Public,
      owner,
      None,
      100_000,
      100,
    )
    .unwrap();
    table
      .put("user:1", &serde_json::json!({"age": 25}), owner)
      .unwrap();
    table
      .put("user:2", &serde_json::json!({"age": 30}), owner)
      .unwrap();
    table
      .put("task:1", &serde_json::json!({"done": true}), owner)
      .unwrap();

    let users = table.scan("user:", owner).unwrap();
    assert_eq!(users.len(), 2);

    let older = table
      .filter(owner, &|_k, v| {
        v.get("age")
          .and_then(|a| a.as_i64())
          .map_or(false, |a| a > 27)
      })
      .unwrap();
    assert_eq!(older.len(), 1);
  }

  #[test]
  fn test_dets_private_read_access() {
    let dir = tempfile::tempdir().unwrap();
    let owner = AgentPid::new();
    let other = AgentPid::new();
    let mut table = DetsTable::open(
      "test",
      dir.path(),
      AccessType::Private,
      owner,
      None,
      100_000,
      100,
    )
    .unwrap();
    table.put("k", &serde_json::json!(1), owner).unwrap();
    // Other cannot read
    assert!(table.get("k", other).is_err());
    assert!(table.scan("", other).is_err());
    assert!(table.filter(other, &|_, _| true).is_err());
    assert!(table.count(other).is_err());
    // Owner can read
    assert!(table.get("k", owner).is_ok());
    assert!(table.scan("", owner).is_ok());
    assert!(table.filter(owner, &|_, _| true).is_ok());
    assert!(table.count(owner).is_ok());
  }

  #[test]
  fn test_dets_protected_read_access() {
    let dir = tempfile::tempdir().unwrap();
    let owner = AgentPid::new();
    let other = AgentPid::new();
    let mut table = DetsTable::open(
      "test",
      dir.path(),
      AccessType::Protected,
      owner,
      None,
      100_000,
      100,
    )
    .unwrap();
    table.put("k", &serde_json::json!(1), owner).unwrap();
    // Other can read (Protected)
    assert!(table.get("k", other).is_ok());
    // Other cannot write
    assert!(table.put("k2", &serde_json::json!(2), other).is_err());
  }

  #[test]
  fn test_dets_schema_version_header() {
    let dir = tempfile::tempdir().unwrap();
    let owner = AgentPid::new();
    let mut table = DetsTable::open(
      "test",
      dir.path(),
      AccessType::Public,
      owner,
      None,
      100_000,
      100,
    )
    .unwrap();
    table.put("k", &serde_json::json!(1), owner).unwrap();
    table.compact().unwrap();

    // Verify .dat starts with schema header
    let dat_path = dir.path().join("test.dat");
    let content = std::fs::read_to_string(&dat_path).unwrap();
    let first_line: serde_json::Value =
      serde_json::from_str(content.lines().next().unwrap()).unwrap();
    assert_eq!(first_line["schema_version"], 1);
    assert_eq!(first_line["type"], "dets");
  }
}
