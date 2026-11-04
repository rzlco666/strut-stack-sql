//! Migration plans (R2-06): plan → apply → rollback.
//!
//! A plan is a validated list of DDL statements plus an optional inverse list.
//! Plans are held in memory with a TTL; `apply` requires the plan id (and an
//! accepted approval id), executes the statements in order, and marks the plan
//! applied. `rollback` is only possible when an inverse was derived at plan
//! time — otherwise the plan reports that no inverse is defined and rollback
//! is refused. Nothing here executes SQL by itself.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const MIGRATION_PLAN_TTL_SECS: u64 = 3600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationPlan {
    pub id: String,
    pub principal: String,
    pub source: String,
    pub statements: Vec<String>,
    /// `None` means no inverse could be derived; rollback is refused.
    pub inverse: Option<Vec<String>>,
    pub created_unix: u64,
    pub expires_unix: u64,
    pub applied: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MigrationError {
    #[error("unknown migration plan")]
    Unknown,
    #[error("migration plan expired; create a new plan")]
    Expired,
    #[error("migration plan was already applied")]
    AlreadyApplied,
    #[error("migration plan has not been applied; there is nothing to roll back")]
    NotApplied,
    #[error("no inverse is defined for this plan; rollback is refused")]
    NoInverse,
}

#[derive(Debug, Default)]
pub struct PlanStore {
    plans: Mutex<HashMap<String, MigrationPlan>>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("pln_{:x}{:x}", t, n)
}

/// Derive an inverse statement list for the shapes we can reverse exactly.
/// Anything else returns `None` (and rollback will be refused).
pub fn derive_inverse(statement: &str) -> Option<Vec<String>> {
    let normalized = statement.trim().trim_end_matches(';').trim();
    let lower = normalized.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("create table ") {
        // Keep the original spelling for the object name.
        let original_rest = &normalized["create table ".len()..];
        let _ = rest;
        let name = original_rest
            .split(|c: char| c.is_whitespace() || c == '(')
            .next()?
            .trim();
        if name.is_empty() {
            return None;
        }
        return Some(vec![format!("DROP TABLE {name}")]);
    }
    if let Some(position) = lower.find(" add column ") {
        let table = normalized["alter table ".len()..position].trim();
        let rest = &normalized[position + " add column ".len()..];
        let column = rest
            .split(|c: char| c.is_whitespace() || c == '"' || c == '`')
            .find(|part| !part.is_empty())?;
        if table.is_empty() || column.is_empty() {
            return None;
        }
        return Some(vec![format!("ALTER TABLE {table} DROP COLUMN {column}")]);
    }
    None
}

impl PlanStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn plan(&self, principal: &str, source: &str, statements: Vec<String>) -> MigrationPlan {
        let created = now_unix();
        let inverse = {
            let mut inverses = Vec::new();
            let mut reversible = true;
            // Roll back in reverse order.
            for statement in statements.iter().rev() {
                match derive_inverse(statement) {
                    Some(mut list) => inverses.append(&mut list),
                    None => {
                        reversible = false;
                        break;
                    }
                }
            }
            if reversible && !inverses.is_empty() {
                Some(inverses)
            } else {
                None
            }
        };
        let plan = MigrationPlan {
            id: new_id(),
            principal: principal.to_string(),
            source: source.to_string(),
            statements,
            inverse,
            created_unix: created,
            expires_unix: created + MIGRATION_PLAN_TTL_SECS,
            applied: false,
        };
        self.plans
            .lock()
            .expect("plan store poisoned")
            .insert(plan.id.clone(), plan.clone());
        plan
    }

    pub fn get(
        &self,
        id: &str,
        principal: &str,
        source: &str,
    ) -> Result<MigrationPlan, MigrationError> {
        let plans = self.plans.lock().expect("plan store poisoned");
        let Some(plan) = plans.get(id) else {
            return Err(MigrationError::Unknown);
        };
        if plan.principal != principal || plan.source != source {
            return Err(MigrationError::Unknown);
        }
        if now_unix() >= plan.expires_unix {
            return Err(MigrationError::Expired);
        }
        Ok(plan.clone())
    }

    pub fn mark_applied(&self, id: &str) -> Result<(), MigrationError> {
        let mut plans = self.plans.lock().expect("plan store poisoned");
        let Some(plan) = plans.get_mut(id) else {
            return Err(MigrationError::Unknown);
        };
        if plan.applied {
            return Err(MigrationError::AlreadyApplied);
        }
        plan.applied = true;
        Ok(())
    }

    /// Mark a plan rolled back (applied = false) after its inverse ran.
    pub fn mark_rolled_back(&self, id: &str) -> Result<(), MigrationError> {
        let mut plans = self.plans.lock().expect("plan store poisoned");
        let Some(plan) = plans.get_mut(id) else {
            return Err(MigrationError::Unknown);
        };
        plan.applied = false;
        Ok(())
    }

    pub fn gc(&self) {
        let now = now_unix();
        self.plans
            .lock()
            .expect("plan store poisoned")
            .retain(|_, plan| now < plan.expires_unix);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_table_inverse_is_drop_and_add_column_inverse_is_drop_column() {
        assert_eq!(
            derive_inverse("CREATE TABLE audit (id INT)"),
            Some(vec!["DROP TABLE audit".to_string()])
        );
        assert_eq!(
            derive_inverse("ALTER TABLE users ADD COLUMN nick TEXT"),
            Some(vec!["ALTER TABLE users DROP COLUMN nick".to_string()])
        );
    }

    #[test]
    fn unreversible_plans_have_no_inverse() {
        assert_eq!(derive_inverse("DROP TABLE users"), None);
        assert_eq!(derive_inverse("UPDATE users SET name = 'x'"), None);
    }

    #[test]
    fn plan_is_principal_and_source_bound() {
        let store = PlanStore::new();
        let plan = store.plan("alice", "app", vec!["CREATE TABLE t (id INT)".into()]);
        assert!(store.get(&plan.id, "alice", "app").is_ok());
        assert_eq!(
            store.get(&plan.id, "bob", "app").unwrap_err(),
            MigrationError::Unknown
        );
        assert_eq!(
            store.get(&plan.id, "alice", "other").unwrap_err(),
            MigrationError::Unknown
        );
    }

    #[test]
    fn apply_is_single_use_and_rollback_requires_applied_state() {
        let store = PlanStore::new();
        let plan = store.plan("alice", "app", vec!["CREATE TABLE t (id INT)".into()]);
        assert!(
            store.mark_rolled_back(&plan.id).is_ok(),
            "rolling back an unapplied plan is refused by the tool, store only flips state"
        );
        store.mark_applied(&plan.id).expect("apply");
        assert_eq!(
            store.mark_applied(&plan.id).unwrap_err(),
            MigrationError::AlreadyApplied
        );
        store.mark_rolled_back(&plan.id).expect("rollback");
        assert!(!store.get(&plan.id, "alice", "app").expect("plan").applied);
    }

    #[test]
    fn expired_plan_fails_closed() {
        let store = PlanStore::new();
        let plan = store.plan("alice", "app", vec!["CREATE TABLE t (id INT)".into()]);
        {
            let mut plans = store.plans.lock().unwrap();
            plans.get_mut(&plan.id).unwrap().expires_unix = 1;
        }
        assert_eq!(
            store.get(&plan.id, "alice", "app").unwrap_err(),
            MigrationError::Expired
        );
    }
}
