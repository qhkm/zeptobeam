use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::agent_rt::error::AgentRtError;
use crate::agent_rt::types::AgentPid;

static NEXT_TABLE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessType {
    Public,
    Protected,
    Private,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TableId {
    Ets(u64),
    Dets(u64),
}

impl TableId {
    fn next_ets() -> Self {
        TableId::Ets(NEXT_TABLE_ID.fetch_add(1, Ordering::Relaxed))
    }
}

pub struct EtsTable {
    pub name: String,
    pub access: AccessType,
    pub owner: AgentPid,
    pub heir: Option<AgentPid>,
    data: HashMap<String, serde_json::Value>,
    max_entries: usize,
}

impl EtsTable {
    fn new(
        name: String,
        access: AccessType,
        owner: AgentPid,
        heir: Option<AgentPid>,
        max_entries: usize,
    ) -> Self {
        Self {
            name,
            access,
            owner,
            heir,
            data: HashMap::new(),
            max_entries,
        }
    }

    fn check_write_access(&self, caller: AgentPid) -> Result<(), AgentRtError> {
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

    fn check_read_access(&self, caller: AgentPid) -> Result<(), AgentRtError> {
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

pub struct EtsRegistry {
    tables: HashMap<TableId, EtsTable>,
    max_tables: usize,
    max_entries_per_table: usize,
}

impl EtsRegistry {
    pub fn new(max_tables: usize, max_entries_per_table: usize) -> Self {
        Self {
            tables: HashMap::new(),
            max_tables,
            max_entries_per_table,
        }
    }

    pub fn create(
        &mut self,
        name: &str,
        access: AccessType,
        owner: AgentPid,
        heir: Option<AgentPid>,
    ) -> Result<TableId, AgentRtError> {
        if self.tables.len() >= self.max_tables {
            return Err(AgentRtError::Ets("max tables exceeded".into()));
        }
        let tid = TableId::next_ets();
        let table = EtsTable::new(
            name.to_string(),
            access,
            owner,
            heir,
            self.max_entries_per_table,
        );
        self.tables.insert(tid, table);
        Ok(tid)
    }

    pub fn destroy(
        &mut self,
        tid: &TableId,
        caller: AgentPid,
    ) -> Result<(), AgentRtError> {
        let table = self
            .tables
            .get(tid)
            .ok_or_else(|| AgentRtError::Ets("table not found".into()))?;
        if table.owner != caller {
            return Err(AgentRtError::Ets(
                "only owner can destroy table".into(),
            ));
        }
        self.tables.remove(tid);
        Ok(())
    }

    pub fn get(
        &self,
        tid: &TableId,
        key: &str,
    ) -> Result<Option<serde_json::Value>, AgentRtError> {
        let table = self
            .tables
            .get(tid)
            .ok_or_else(|| AgentRtError::Ets("table not found".into()))?;
        Ok(table.data.get(key).cloned())
    }

    pub fn put(
        &mut self,
        tid: &TableId,
        key: &str,
        value: &serde_json::Value,
        caller: AgentPid,
    ) -> Result<(), AgentRtError> {
        let table = self
            .tables
            .get(tid)
            .ok_or_else(|| AgentRtError::Ets("table not found".into()))?;
        table.check_write_access(caller)?;
        let is_new = !table.data.contains_key(key);
        if is_new && table.data.len() >= table.max_entries {
            return Err(AgentRtError::Ets("table full".into()));
        }
        // Re-borrow mutably
        let table = self.tables.get_mut(tid).unwrap();
        table.data.insert(key.to_string(), value.clone());
        Ok(())
    }

    pub fn delete_key(
        &mut self,
        tid: &TableId,
        key: &str,
        caller: AgentPid,
    ) -> Result<(), AgentRtError> {
        let table = self
            .tables
            .get(tid)
            .ok_or_else(|| AgentRtError::Ets("table not found".into()))?;
        table.check_write_access(caller)?;
        let table = self.tables.get_mut(tid).unwrap();
        table.data.remove(key);
        Ok(())
    }

    pub fn count(&self, tid: &TableId) -> Result<usize, AgentRtError> {
        let table = self
            .tables
            .get(tid)
            .ok_or_else(|| AgentRtError::Ets("table not found".into()))?;
        Ok(table.data.len())
    }

    pub fn keys(
        &self,
        tid: &TableId,
    ) -> Result<Vec<String>, AgentRtError> {
        let table = self
            .tables
            .get(tid)
            .ok_or_else(|| AgentRtError::Ets("table not found".into()))?;
        Ok(table.data.keys().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_rt::types::AgentPid;

    #[test]
    fn test_create_and_get() {
        let mut reg = EtsRegistry::new(256, 100_000);
        let owner = AgentPid::new();
        let tid = reg
            .create("test", AccessType::Public, owner, None)
            .unwrap();
        reg.put(&tid, "key1", &serde_json::json!("value1"), owner)
            .unwrap();
        let val = reg.get(&tid, "key1").unwrap();
        assert_eq!(val, Some(serde_json::json!("value1")));
    }

    #[test]
    fn test_delete_key() {
        let mut reg = EtsRegistry::new(256, 100_000);
        let owner = AgentPid::new();
        let tid = reg
            .create("test", AccessType::Public, owner, None)
            .unwrap();
        reg.put(&tid, "k", &serde_json::json!(42), owner).unwrap();
        reg.delete_key(&tid, "k", owner).unwrap();
        assert_eq!(reg.get(&tid, "k").unwrap(), None);
    }

    #[test]
    fn test_count_and_keys() {
        let mut reg = EtsRegistry::new(256, 100_000);
        let owner = AgentPid::new();
        let tid = reg
            .create("t", AccessType::Public, owner, None)
            .unwrap();
        reg.put(&tid, "a", &serde_json::json!(1), owner).unwrap();
        reg.put(&tid, "b", &serde_json::json!(2), owner).unwrap();
        assert_eq!(reg.count(&tid).unwrap(), 2);
        let mut keys = reg.keys(&tid).unwrap();
        keys.sort();
        assert_eq!(keys, vec!["a", "b"]);
    }

    #[test]
    fn test_destroy_table() {
        let mut reg = EtsRegistry::new(256, 100_000);
        let owner = AgentPid::new();
        let tid = reg
            .create("t", AccessType::Public, owner, None)
            .unwrap();
        reg.destroy(&tid, owner).unwrap();
        assert!(reg.get(&tid, "k").is_err());
    }

    #[test]
    fn test_destroy_non_owner_fails() {
        let mut reg = EtsRegistry::new(256, 100_000);
        let owner = AgentPid::new();
        let other = AgentPid::new();
        let tid = reg
            .create("t", AccessType::Public, owner, None)
            .unwrap();
        assert!(reg.destroy(&tid, other).is_err());
    }

    #[test]
    fn test_max_tables_enforced() {
        let mut reg = EtsRegistry::new(2, 100_000);
        let owner = AgentPid::new();
        reg.create("t1", AccessType::Public, owner, None).unwrap();
        reg.create("t2", AccessType::Public, owner, None).unwrap();
        assert!(
            reg.create("t3", AccessType::Public, owner, None).is_err()
        );
    }

    #[test]
    fn test_max_entries_enforced() {
        let mut reg = EtsRegistry::new(256, 2);
        let owner = AgentPid::new();
        let tid = reg
            .create("t", AccessType::Public, owner, None)
            .unwrap();
        reg.put(&tid, "a", &serde_json::json!(1), owner).unwrap();
        reg.put(&tid, "b", &serde_json::json!(2), owner).unwrap();
        assert!(
            reg.put(&tid, "c", &serde_json::json!(3), owner).is_err()
        );
    }
}
