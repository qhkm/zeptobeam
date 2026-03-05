use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::agent_rt::error::AgentRtError;
use crate::agent_rt::types::Message;

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct WalHeader {
  schema_version: u32,
  #[serde(rename = "type")]
  file_type: String,
  stable_id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WalEntry {
  pub seq: u64,
  pub ts: i64,
  pub msg: Message,
}

pub struct DurableMailbox {
  stable_id: String,
  wal_path: PathBuf,
  ack_path: PathBuf,
  dir: PathBuf,
  writer: Option<BufWriter<fs::File>>,
  sequence: u64,
  last_acked_seq: u64,
}

impl DurableMailbox {
  pub fn open(stable_id: &str, dir: &Path) -> Result<Self, AgentRtError> {
    fs::create_dir_all(dir)
      .map_err(|e| AgentRtError::DurableMailbox(format!("create dir: {}", e)))?;
    let wal_path = dir.join(format!("{}.wal", stable_id));
    let ack_path = dir.join(format!("{}.ack", stable_id));

    // Read last acked seq
    let last_acked_seq = if ack_path.exists() {
      let content = fs::read_to_string(&ack_path)
        .map_err(|e| AgentRtError::DurableMailbox(format!("read ack: {}", e)))?;
      content
        .trim()
        .parse::<u64>()
        .map_err(|e| AgentRtError::DurableMailbox(format!("bad ack file: {}", e)))?
    } else {
      0
    };

    // Find max seq from existing WAL
    let mut max_seq = 0u64;
    if wal_path.exists() {
      let file = fs::File::open(&wal_path)
        .map_err(|e| AgentRtError::DurableMailbox(format!("open wal: {}", e)))?;
      let reader = BufReader::new(file);
      let mut first = true;
      for line in reader.lines() {
        let line = line
          .map_err(|e| AgentRtError::DurableMailbox(format!("read wal: {}", e)))?;
        if line.trim().is_empty() {
          continue;
        }
        if first {
          first = false;
          continue; // skip header
        }
        if let Ok(entry) = serde_json::from_str::<WalEntry>(&line) {
          if entry.seq > max_seq {
            max_seq = entry.seq;
          }
        }
      }
    }

    // Open WAL for appending (or create with header)
    let writer = if wal_path.exists() {
      let file = fs::OpenOptions::new()
        .append(true)
        .open(&wal_path)
        .map_err(|e| {
          AgentRtError::DurableMailbox(format!("open wal append: {}", e))
        })?;
      Some(BufWriter::new(file))
    } else {
      let file = fs::File::create(&wal_path)
        .map_err(|e| AgentRtError::DurableMailbox(format!("create wal: {}", e)))?;
      let mut writer = BufWriter::new(file);
      let header = WalHeader {
        schema_version: SCHEMA_VERSION,
        file_type: "wal".into(),
        stable_id: stable_id.to_string(),
      };
      writeln!(
        writer,
        "{}",
        serde_json::to_string(&header).map_err(|e| {
          AgentRtError::DurableMailbox(format!("serialize header: {}", e))
        })?
      )
      .map_err(|e| AgentRtError::DurableMailbox(format!("write header: {}", e)))?;
      writer
        .flush()
        .map_err(|e| AgentRtError::DurableMailbox(format!("flush header: {}", e)))?;
      Some(writer)
    };

    Ok(Self {
      stable_id: stable_id.to_string(),
      wal_path,
      ack_path,
      dir: dir.to_path_buf(),
      writer,
      sequence: max_seq,
      last_acked_seq,
    })
  }

  pub fn append(&mut self, msg: &Message) -> Result<u64, AgentRtError> {
    self.sequence += 1;
    let entry = WalEntry {
      seq: self.sequence,
      ts: std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64,
      msg: msg.clone(),
    };
    if let Some(ref mut writer) = self.writer {
      let line = serde_json::to_string(&entry)
        .map_err(|e| AgentRtError::DurableMailbox(format!("serialize: {}", e)))?;
      writeln!(writer, "{}", line)
        .map_err(|e| AgentRtError::DurableMailbox(format!("write: {}", e)))?;
      writer
        .flush()
        .map_err(|e| AgentRtError::DurableMailbox(format!("flush: {}", e)))?;
      writer
        .get_ref()
        .sync_all()
        .map_err(|e| AgentRtError::DurableMailbox(format!("fsync: {}", e)))?;
    }
    Ok(self.sequence)
  }

  pub fn replay(&self) -> Result<Vec<WalEntry>, AgentRtError> {
    let mut entries = Vec::new();
    if !self.wal_path.exists() {
      return Ok(entries);
    }
    let file = fs::File::open(&self.wal_path)
      .map_err(|e| AgentRtError::DurableMailbox(format!("open replay: {}", e)))?;
    let reader = BufReader::new(file);
    let mut first = true;
    for line in reader.lines() {
      let line = line
        .map_err(|e| AgentRtError::DurableMailbox(format!("read replay: {}", e)))?;
      if line.trim().is_empty() {
        continue;
      }
      if first {
        first = false;
        continue; // skip header
      }
      let entry: WalEntry = serde_json::from_str(&line)
        .map_err(|e| AgentRtError::DurableMailbox(format!("bad entry: {}", e)))?;
      if entry.seq > self.last_acked_seq {
        entries.push(entry);
      }
    }
    Ok(entries)
  }

  pub fn ack(&mut self, seq: u64) -> Result<(), AgentRtError> {
    let tmp_path = self.dir.join(format!("{}.ack.tmp", self.stable_id));
    fs::write(&tmp_path, seq.to_string())
      .map_err(|e| AgentRtError::DurableMailbox(format!("write ack tmp: {}", e)))?;
    // fsync tmp
    let f = fs::File::open(&tmp_path)
      .map_err(|e| AgentRtError::DurableMailbox(format!("open ack tmp: {}", e)))?;
    f.sync_all()
      .map_err(|e| AgentRtError::DurableMailbox(format!("fsync ack: {}", e)))?;
    drop(f);
    // atomic rename
    fs::rename(&tmp_path, &self.ack_path)
      .map_err(|e| AgentRtError::DurableMailbox(format!("rename ack: {}", e)))?;
    self.last_acked_seq = seq;
    Ok(())
  }

  pub fn truncate(&mut self) -> Result<(), AgentRtError> {
    // Rewrite WAL excluding entries <= last_acked_seq
    let entries = self.replay_all()?;
    let kept: Vec<_> = entries
      .iter()
      .filter(|e| e.seq > self.last_acked_seq)
      .collect();

    let tmp_path = self.dir.join(format!("{}.wal.tmp", self.stable_id));
    let file = fs::File::create(&tmp_path)
      .map_err(|e| AgentRtError::DurableMailbox(format!("create wal tmp: {}", e)))?;
    let mut writer = BufWriter::new(file);
    let header = WalHeader {
      schema_version: SCHEMA_VERSION,
      file_type: "wal".into(),
      stable_id: self.stable_id.clone(),
    };
    writeln!(
      writer,
      "{}",
      serde_json::to_string(&header).map_err(|e| {
        AgentRtError::DurableMailbox(format!("serialize header: {}", e))
      })?
    )
    .map_err(|e| AgentRtError::DurableMailbox(format!("write header: {}", e)))?;
    for entry in kept {
      writeln!(
        writer,
        "{}",
        serde_json::to_string(entry).map_err(|e| {
          AgentRtError::DurableMailbox(format!("serialize entry: {}", e))
        })?
      )
      .map_err(|e| AgentRtError::DurableMailbox(format!("write entry: {}", e)))?;
    }
    writer
      .flush()
      .map_err(|e| AgentRtError::DurableMailbox(format!("flush: {}", e)))?;
    writer
      .get_ref()
      .sync_all()
      .map_err(|e| AgentRtError::DurableMailbox(format!("sync: {}", e)))?;
    drop(writer);

    fs::rename(&tmp_path, &self.wal_path)
      .map_err(|e| AgentRtError::DurableMailbox(format!("rename wal: {}", e)))?;

    // Reopen writer
    let file = fs::OpenOptions::new()
      .append(true)
      .open(&self.wal_path)
      .map_err(|e| AgentRtError::DurableMailbox(format!("reopen wal: {}", e)))?;
    self.writer = Some(BufWriter::new(file));

    Ok(())
  }

  pub fn last_acked_seq(&self) -> u64 {
    self.last_acked_seq
  }

  pub fn current_seq(&self) -> u64 {
    self.sequence
  }

  fn replay_all(&self) -> Result<Vec<WalEntry>, AgentRtError> {
    let mut entries = Vec::new();
    if !self.wal_path.exists() {
      return Ok(entries);
    }
    let file = fs::File::open(&self.wal_path)
      .map_err(|e| AgentRtError::DurableMailbox(format!("open replay all: {}", e)))?;
    let reader = BufReader::new(file);
    let mut first = true;
    for line in reader.lines() {
      let line = line
        .map_err(|e| AgentRtError::DurableMailbox(format!("read: {}", e)))?;
      if line.trim().is_empty() {
        continue;
      }
      if first {
        first = false;
        continue;
      }
      if let Ok(entry) = serde_json::from_str::<WalEntry>(&line) {
        entries.push(entry);
      }
    }
    Ok(entries)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::agent_rt::types::Message;

  #[test]
  fn test_append_and_replay() {
    let dir = tempfile::tempdir().unwrap();
    let mut mb = DurableMailbox::open("proc-1", dir.path()).unwrap();
    mb.append(&Message::Text("hello".into())).unwrap();
    mb.append(&Message::Text("world".into())).unwrap();
    drop(mb);

    let mb = DurableMailbox::open("proc-1", dir.path()).unwrap();
    let msgs = mb.replay().unwrap();
    assert_eq!(msgs.len(), 2);
    match &msgs[0].msg {
      Message::Text(s) => assert_eq!(s, "hello"),
      _ => panic!("expected Text"),
    }
  }

  #[test]
  fn test_ack_and_replay_skips_acked() {
    let dir = tempfile::tempdir().unwrap();
    let mut mb = DurableMailbox::open("proc-2", dir.path()).unwrap();
    mb.append(&Message::Text("a".into())).unwrap();
    mb.append(&Message::Text("b".into())).unwrap();
    mb.append(&Message::Text("c".into())).unwrap();
    mb.ack(2).unwrap(); // ack first two
    drop(mb);

    let mb = DurableMailbox::open("proc-2", dir.path()).unwrap();
    let msgs = mb.replay().unwrap();
    assert_eq!(msgs.len(), 1); // only "c" remains
    match &msgs[0].msg {
      Message::Text(s) => assert_eq!(s, "c"),
      _ => panic!("expected Text"),
    }
  }

  #[test]
  fn test_truncate_removes_acked() {
    let dir = tempfile::tempdir().unwrap();
    let mut mb = DurableMailbox::open("proc-3", dir.path()).unwrap();
    mb.append(&Message::Text("a".into())).unwrap();
    mb.append(&Message::Text("b".into())).unwrap();
    mb.ack(1).unwrap();
    mb.truncate().unwrap();
    drop(mb);

    // After truncate, WAL only has seq > 1
    let mb = DurableMailbox::open("proc-3", dir.path()).unwrap();
    let msgs = mb.replay().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].seq, 2);
  }

  #[test]
  fn test_sequence_monotonic() {
    let dir = tempfile::tempdir().unwrap();
    let mut mb = DurableMailbox::open("proc-4", dir.path()).unwrap();
    let s1 = mb.append(&Message::Text("a".into())).unwrap();
    let s2 = mb.append(&Message::Text("b".into())).unwrap();
    assert_eq!(s1, 1);
    assert_eq!(s2, 2);
  }

  #[test]
  fn test_wal_schema_header() {
    let dir = tempfile::tempdir().unwrap();
    let mut mb = DurableMailbox::open("proc-5", dir.path()).unwrap();
    mb.append(&Message::Text("test".into())).unwrap();
    drop(mb);

    let wal_path = dir.path().join("proc-5.wal");
    let content = std::fs::read_to_string(&wal_path).unwrap();
    let first_line: serde_json::Value =
      serde_json::from_str(content.lines().next().unwrap()).unwrap();
    assert_eq!(first_line["schema_version"], 1);
    assert_eq!(first_line["type"], "wal");
    assert_eq!(first_line["stable_id"], "proc-5");
  }

  #[test]
  fn test_ack_uses_atomic_rename() {
    let dir = tempfile::tempdir().unwrap();
    let mut mb = DurableMailbox::open("proc-6", dir.path()).unwrap();
    mb.append(&Message::Text("a".into())).unwrap();
    mb.ack(1).unwrap();

    // .ack file should exist, no .ack.tmp
    assert!(dir.path().join("proc-6.ack").exists());
    assert!(!dir.path().join("proc-6.ack.tmp").exists());
  }

  #[test]
  fn test_reopen_preserves_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let mut mb = DurableMailbox::open("proc-7", dir.path()).unwrap();
    mb.append(&Message::Text("a".into())).unwrap();
    mb.append(&Message::Text("b".into())).unwrap();
    drop(mb);

    let mut mb = DurableMailbox::open("proc-7", dir.path()).unwrap();
    let s3 = mb.append(&Message::Text("c".into())).unwrap();
    assert_eq!(s3, 3); // continues from last seq
  }
}
