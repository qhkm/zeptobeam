use rusqlite::{params, Connection, Result as SqlResult};

use crate::core::effect::{EffectId, EffectResult};

pub struct IdempotencyStore {
    conn: Connection,
}

impl IdempotencyStore {
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
            "CREATE TABLE IF NOT EXISTS idempotency (
                effect_id TEXT PRIMARY KEY,
                result_json TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );",
        )?;
        Ok(())
    }

    pub fn check(
        &self,
        effect_id: &EffectId,
    ) -> SqlResult<Option<EffectResult>> {
        let mut stmt = self.conn.prepare(
            "SELECT result_json FROM idempotency WHERE effect_id = ?1",
        )?;
        let key = format!("{}", effect_id);
        let mut rows = stmt.query_map(params![key], |row| {
            let json: String = row.get(0)?;
            Ok(json)
        })?;

        match rows.next() {
            Some(Ok(json)) => {
                let result: EffectResult =
                    serde_json::from_str(&json).map_err(|e| {
                        rusqlite::Error::ToSqlConversionFailure(
                            Box::new(e),
                        )
                    })?;
                Ok(Some(result))
            }
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }

    pub fn record(
        &self,
        effect_id: &EffectId,
        result: &EffectResult,
    ) -> SqlResult<()> {
        let key = format!("{}", effect_id);
        let json = serde_json::to_string(result).map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(e))
        })?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        self.conn.execute(
            "INSERT OR REPLACE INTO idempotency \
             (effect_id, result_json, created_at) VALUES (?1, ?2, ?3)",
            params![key, json, now],
        )?;
        Ok(())
    }

    pub fn expire_before(
        &self,
        timestamp_ms: u64,
    ) -> SqlResult<usize> {
        self.conn.execute(
            "DELETE FROM idempotency WHERE created_at < ?1",
            params![timestamp_ms as i64],
        )
    }

    pub fn count(&self) -> SqlResult<i64> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM idempotency",
            [],
            |row| row.get(0),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::effect::{EffectId, EffectResult, EffectStatus};

    #[test]
    fn test_idempotency_check_miss() {
        let store = IdempotencyStore::open_in_memory().unwrap();
        let id = EffectId::new();
        assert!(store.check(&id).unwrap().is_none());
    }

    #[test]
    fn test_idempotency_record_and_check() {
        let store = IdempotencyStore::open_in_memory().unwrap();
        let id = EffectId::new();
        let result =
            EffectResult::success(id, serde_json::json!("ok"));
        store.record(&id, &result).unwrap();

        let cached = store.check(&id).unwrap().unwrap();
        assert_eq!(cached.effect_id, id);
        assert_eq!(cached.status, EffectStatus::Succeeded);
    }

    #[test]
    fn test_idempotency_expire() {
        let store = IdempotencyStore::open_in_memory().unwrap();
        let id = EffectId::new();
        let result =
            EffectResult::success(id, serde_json::json!("ok"));
        store.record(&id, &result).unwrap();
        assert_eq!(store.count().unwrap(), 1);

        let future_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 100_000;
        store.expire_before(future_ms).unwrap();
        assert_eq!(store.count().unwrap(), 0);
    }

    #[test]
    fn test_idempotency_failure_cached() {
        let store = IdempotencyStore::open_in_memory().unwrap();
        let id = EffectId::new();
        let result = EffectResult::failure(id, "network error");
        store.record(&id, &result).unwrap();

        let cached = store.check(&id).unwrap().unwrap();
        assert_eq!(cached.status, EffectStatus::Failed);
        assert_eq!(cached.error.as_deref(), Some("network error"));
    }

    #[test]
    fn test_idempotency_count() {
        let store = IdempotencyStore::open_in_memory().unwrap();
        assert_eq!(store.count().unwrap(), 0);
        for _ in 0..3 {
            let id = EffectId::new();
            store
                .record(
                    &id,
                    &EffectResult::success(
                        id,
                        serde_json::json!("ok"),
                    ),
                )
                .unwrap();
        }
        assert_eq!(store.count().unwrap(), 3);
    }
}
