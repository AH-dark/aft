//! Durable catalog routing state. All operations use the owning aft.db handle.
//!
//! Routing settings saved by a worker's tool.catalog fetch are session
//! configuration, not disposable cache entries. Only the daemon can establish
//! that the worker's scope has ended; idle time, a missing worktree, and row
//! count are not evidence of that. Delete settings only for an ended scope.
use crate::exec_remote::FrozenParams;
use rusqlite::{params, Connection, OptionalExtension};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyKey {
    pub project_root: String,
    pub harness: String,
    pub session: String,
    pub principal: String,
    pub owner: String,
    pub scope_ref: String,
    pub epoch: u64,
    pub preset: String,
}

impl PolicyKey {
    fn values(&self) -> [&str; 7] {
        [
            &self.project_root,
            &self.harness,
            &self.session,
            &self.principal,
            &self.owner,
            &self.scope_ref,
            &self.preset,
        ]
    }
}

pub fn freeze(
    conn: &Connection,
    key: &PolicyKey,
    policy: &FrozenParams,
    now: u64,
) -> rusqlite::Result<()> {
    let v = key.values();
    conn.execute(
        "INSERT OR IGNORE INTO remote_exec_policies
        (project_root,harness,session,principal,owner,scope_ref,preset,epoch,params,last_used)
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            v[0],
            v[1],
            v[2],
            v[3],
            v[4],
            v[5],
            v[6],
            key.epoch.to_string(),
            serde_json::to_string(policy).expect("routing fields serialize"),
            now as i64
        ],
    )?;
    Ok(())
}

/// The only per-call policy lookup: no files or call arguments can supply it.
pub fn lookup(
    conn: &Connection,
    key: &PolicyKey,
    now: u64,
) -> rusqlite::Result<Option<FrozenParams>> {
    let v = key.values();
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT rowid,params FROM remote_exec_policies
        WHERE project_root=?1 AND harness=?2 AND session=?3 AND principal=?4
          AND owner=?5 AND scope_ref=?6 AND preset=?7 AND epoch=?8",
            params![
                v[0],
                v[1],
                v[2],
                v[3],
                v[4],
                v[5],
                v[6],
                key.epoch.to_string()
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(rowid, json)| {
        conn.execute(
            "UPDATE remote_exec_policies SET last_used=?1 WHERE rowid=?2",
            params![now as i64, rowid],
        )?;
        serde_json::from_str(&json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
        })
    })
    .transpose()
}

/// Called by scope-end handling once it is available. Ordinary route detach
/// must not forget a policy: catalog and calls use different routes.
pub fn forget_scope(
    conn: &Connection,
    owner: &str,
    scope_ref: &str,
    epoch: u64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM remote_exec_policies WHERE owner=?1 AND scope_ref=?2 AND epoch=?3",
        params![owner, scope_ref, epoch.to_string()],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(root: &std::path::Path) -> PolicyKey {
        PolicyKey {
            project_root: root.display().to_string(),
            harness: "broca".into(),
            session: "one".into(),
            principal: "reserved:broca".into(),
            owner: "prefrontal-core".into(),
            scope_ref: "work".into(),
            epoch: 7,
            preset: "worker".into(),
        }
    }
    fn policy(enabled: bool) -> FrozenParams {
        FrozenParams {
            remote_exec: Some(crate::exec_remote::policy::RemoteExecPolicy {
                enabled,
                ..Default::default()
            }),
            ..Default::default()
        }
    }
    #[test]
    fn exec_remote_catalog_policy_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let key = key(dir.path());
        {
            let conn = crate::db::open(&path).unwrap();
            freeze(&conn, &key, &policy(true), 100).unwrap();
            assert!(
                lookup(&conn, &key, 100).unwrap().is_some(),
                "the first process routed before restart"
            );
        }
        let conn = crate::db::open(&path).unwrap();
        assert!(
            lookup(&conn, &key, 200)
                .unwrap()
                .unwrap()
                .remote_exec
                .unwrap()
                .enabled
        );
        forget_scope(&conn, &key.owner, &key.scope_ref, key.epoch).unwrap();
        assert!(lookup(&conn, &key, 200).unwrap().is_none());
    }
    #[test]
    fn exec_remote_policy_key_isolates_epoch_principal_and_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let first = key(dir.path());
        freeze(&conn, &first, &policy(true), 100).unwrap();
        let mut second = first.clone();
        second.session = "two".into();
        freeze(&conn, &second, &policy(false), 100).unwrap();
        assert!(
            lookup(&conn, &first, 200)
                .unwrap()
                .unwrap()
                .remote_exec
                .unwrap()
                .enabled
        );
        assert!(
            !lookup(&conn, &second, 200)
                .unwrap()
                .unwrap()
                .remote_exec
                .unwrap()
                .enabled
        );
        for different in 0..7 {
            let mut other = first.clone();
            match different {
                0 => other.epoch += 1,
                1 => other.principal = "direct".into(),
                2 => other.owner = "other".into(),
                3 => other.scope_ref = "other".into(),
                4 => other.harness = "other".into(),
                5 => other.project_root = "/other".into(),
                _ => other.preset = "head".into(),
            }
            assert!(
                lookup(&conn, &other, 200).unwrap().is_none(),
                "key dimension {different}"
            );
        }
    }

    #[test]
    fn exec_remote_migration_from_previous_version_preserves_existing_data() {
        for previous in [12, 13] {
            let mut conn = Connection::open_in_memory().unwrap();
            crate::db::run_migrations(&mut conn).unwrap();
            conn.execute_batch("DROP TABLE remote_exec_policies; CREATE TABLE unrelated (v TEXT); INSERT INTO unrelated VALUES ('keep');").unwrap();
            conn.execute("UPDATE schema_version SET version=?1", [previous])
                .unwrap();
            assert_eq!(crate::db::run_migrations(&mut conn).unwrap(), 15);
            assert_eq!(
                conn.query_row("SELECT v FROM unrelated", [], |r| r.get::<_, String>(0))
                    .unwrap(),
                "keep"
            );
            let dir = tempfile::tempdir().unwrap();
            let key = key(dir.path());
            freeze(&conn, &key, &policy(true), 100).unwrap();
            assert!(lookup(&conn, &key, 200).unwrap().is_some());
        }
    }
}
