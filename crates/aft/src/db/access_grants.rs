//! Scope-bound capabilities and durable launch parameters on the owning aft.db handle.
//! Rollback to schema 15 requires stopping AFT, dropping access_scopes, access_grants, access_launch_params, and access_materialized, then setting schema_version.version to 15; this discards grants and launch state.
use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
use crate::config::Config;
use crate::db::remote_exec::PolicyKey;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

pub(crate) const MIGRATION: &str = r#"
CREATE TABLE IF NOT EXISTS access_scopes (
 owner TEXT NOT NULL, scope_ref TEXT NOT NULL, epoch TEXT NOT NULL,
 parent_owner TEXT, parent TEXT, parent_epoch TEXT, config TEXT NOT NULL, ended INTEGER NOT NULL DEFAULT 0,
 PRIMARY KEY(owner,scope_ref,epoch)
);
CREATE TABLE IF NOT EXISTS access_grants (
 owner TEXT NOT NULL, scope_ref TEXT NOT NULL, epoch TEXT NOT NULL,
 path TEXT NOT NULL, kind TEXT NOT NULL CHECK(kind IN ('file','subtree')),
 mode TEXT NOT NULL CHECK(mode IN ('read','write')),
 PRIMARY KEY(owner,scope_ref,epoch,path,kind)
);
CREATE TABLE IF NOT EXISTS access_launch_params (
 project_root TEXT NOT NULL, harness TEXT NOT NULL, session TEXT NOT NULL,
 principal TEXT NOT NULL, owner TEXT NOT NULL, scope_ref TEXT NOT NULL,
 epoch TEXT NOT NULL, preset TEXT NOT NULL, params TEXT NOT NULL,
 PRIMARY KEY(project_root,harness,session,principal,owner,scope_ref,epoch,preset)
);
CREATE TABLE IF NOT EXISTS access_materialized (
 owner TEXT NOT NULL, scope_ref TEXT NOT NULL, epoch TEXT NOT NULL,
 PRIMARY KEY(owner,scope_ref,epoch)
);
"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeKey {
    pub owner: String,
    pub scope_ref: String,
    pub epoch: u64,
}

impl ScopeKey {
    pub fn from_stamp(stamp: &subc_protocol::scope::ScopeStamp) -> Self {
        Self {
            owner: serde_json::to_string(&stamp.owner).expect("scope owner serializes"),
            scope_ref: stamp.scope_ref.clone(),
            epoch: stamp.scope_epoch,
        }
    }

    pub fn from_policy(key: &PolicyKey) -> Self {
        Self {
            owner: key.owner.clone(),
            scope_ref: key.scope_ref.clone(),
            epoch: key.epoch,
        }
    }
}

fn decode<T: serde::de::DeserializeOwned>(json: &str) -> rusqlite::Result<T> {
    serde_json::from_str(json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

/// Only a daemon-attested bind may register the ancestry used by grant operations.
/// Ended identities are tombstoned so a delayed catalog cannot revive their grants.
pub fn register_scope(
    conn: &Connection,
    key: &ScopeKey,
    parent: Option<&ScopeKey>,
    config: &Config,
) -> rusqlite::Result<()> {
    conn.execute("INSERT INTO access_scopes(owner,scope_ref,epoch,parent_owner,parent,parent_epoch,config) VALUES (?1,?2,?3,?4,?5,?6,?7)
        ON CONFLICT(owner,scope_ref,epoch) DO UPDATE SET parent_owner=excluded.parent_owner, parent=excluded.parent, parent_epoch=excluded.parent_epoch, config=excluded.config WHERE ended=0",
        params![key.owner,key.scope_ref,key.epoch.to_string(),parent.map(|p|&p.owner),parent.map(|p|&p.scope_ref),parent.map(|p|p.epoch.to_string()),serde_json::to_string(config).expect("config serializes")])?;
    Ok(())
}

pub fn scope_config(conn: &Connection, key: &ScopeKey) -> rusqlite::Result<Option<Config>> {
    let json: Option<String> = conn.query_row("SELECT config FROM access_scopes WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND ended=0",
        params![key.owner,key.scope_ref,key.epoch.to_string()], |r| r.get(0)).optional()?;
    json.map(|json| decode(&json)).transpose()
}

pub fn resolve_scope(
    conn: &Connection,
    owner: &str,
    scope_ref: &str,
) -> rusqlite::Result<Option<ScopeKey>> {
    let epoch: Option<String> = conn.query_row("SELECT epoch FROM access_scopes WHERE owner=?1 AND scope_ref=?2 AND ended=0 ORDER BY length(epoch) DESC, epoch DESC LIMIT 1",
        params![owner,scope_ref], |r| r.get(0)).optional()?;
    epoch
        .map(|epoch| {
            Ok(ScopeKey {
                owner: owner.into(),
                scope_ref: scope_ref.into(),
                epoch: epoch.parse().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
            })
        })
        .transpose()
}

/// The registered parent of a live scope, if it has one.
pub fn parent(conn: &Connection, key: &ScopeKey) -> rusqlite::Result<Option<ScopeKey>> {
    let row: Option<(Option<String>, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT parent_owner,parent,parent_epoch FROM access_scopes WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND ended=0",
            params![key.owner, key.scope_ref, key.epoch.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(match row {
        Some((Some(owner), Some(scope_ref), Some(epoch))) => {
            epoch.parse().ok().map(|epoch| ScopeKey {
                owner,
                scope_ref,
                epoch,
            })
        }
        _ => None,
    })
}

pub fn authorized(
    conn: &Connection,
    caller: &ScopeKey,
    target: &ScopeKey,
) -> rusqlite::Result<bool> {
    if scope_config(conn, caller)?.is_none() {
        return Ok(false);
    }
    let parent: Option<(Option<String>, Option<String>, Option<String>)> = conn.query_row("SELECT parent_owner,parent,parent_epoch FROM access_scopes WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND ended=0",
        params![target.owner,target.scope_ref,target.epoch.to_string()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let current_caller = resolve_scope(conn, &caller.owner, &caller.scope_ref)?;
    Ok(current_caller.as_ref() == Some(caller)
        && parent.is_some_and(|(owner, parent, epoch)| {
            caller == target
                || (owner.as_deref() == Some(&caller.owner)
                    && parent.as_deref() == Some(&caller.scope_ref)
                    && epoch == Some(caller.epoch.to_string()))
        }))
}

pub fn list(conn: &Connection, key: &ScopeKey) -> rusqlite::Result<Vec<AccessGrant>> {
    let mut stmt = conn.prepare("SELECT path,kind,mode FROM access_grants WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 ORDER BY path,kind")?;
    let rows = stmt
        .query_map(
            params![key.owner, key.scope_ref, key.epoch.to_string()],
            |r| {
                Ok(AccessGrant {
                    path: PathBuf::from(r.get::<_, String>(0)?),
                    kind: match r.get::<_, String>(1)?.as_str() {
                        "subtree" => GrantKind::Subtree,
                        _ => GrantKind::File,
                    },
                    operation: match r.get::<_, String>(2)?.as_str() {
                        "write" => AccessOperation::Write,
                        _ => AccessOperation::Read,
                    },
                })
            },
        )?
        .collect();
    rows
}

pub fn insert(conn: &Connection, key: &ScopeKey, grant: &AccessGrant) -> rusqlite::Result<()> {
    if !grant.path.is_absolute() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let canonical = crate::access_policy::policy_path(&grant.path);
    conn.execute("INSERT INTO access_grants(owner,scope_ref,epoch,path,kind,mode)
        SELECT ?1,?2,?3,?4,?5,?6 WHERE EXISTS (SELECT 1 FROM access_scopes WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND ended=0)
        ON CONFLICT(owner,scope_ref,epoch,path,kind) DO UPDATE SET mode=CASE WHEN excluded.mode='write' THEN 'write' ELSE access_grants.mode END",
        params![key.owner,key.scope_ref,key.epoch.to_string(),canonical.to_string_lossy(),match grant.kind {GrantKind::File=>"file",GrantKind::Subtree=>"subtree"},match grant.operation {AccessOperation::Read=>"read",AccessOperation::Write=>"write"}])?;
    Ok(())
}

pub fn revoke(conn: &Connection, key: &ScopeKey, path: &Path) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM access_grants WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND path=?4",
        params![
            key.owner,
            key.scope_ref,
            key.epoch.to_string(),
            path.to_string_lossy()
        ],
    )
}

/// Only the describe-confirmed scope-end path may call this helper. A route
/// close, detach, or missing worktree is not proof that a scope ended. The
/// daemon must report ended, a different live epoch, or not_live with
/// owner_synced=true; not_synced, errors, and timeouts retain all scope state.
pub fn end_scope_grants(conn: &Connection, key: &ScopeKey) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO access_scopes(owner,scope_ref,epoch,config,ended) VALUES (?1,?2,?3,'{}',1)
        ON CONFLICT(owner,scope_ref,epoch) DO UPDATE SET ended=1",
        params![key.owner, key.scope_ref, key.epoch.to_string()],
    )?;
    for table in [
        "access_grants",
        "access_launch_params",
        "access_materialized",
    ] {
        tx.execute(
            &format!("DELETE FROM {table} WHERE owner=?1 AND scope_ref=?2 AND epoch=?3"),
            params![key.owner, key.scope_ref, key.epoch.to_string()],
        )?;
    }
    crate::db::remote_exec::forget_scope(&tx, &key.owner, &key.scope_ref, key.epoch)?;
    tx.commit()
}

pub fn launch_params(
    conn: &Connection,
    key: &PolicyKey,
) -> rusqlite::Result<Option<serde_json::Value>> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let row = super::remote_exec::catalog_params(
        &tx,
        key,
        super::remote_exec::CatalogParams::AccessLaunch,
    )?;
    let params = row.map(|(_, json)| decode(&json)).transpose()?;
    tx.commit()?;
    Ok(params)
}

/// Validation is completed before entering this transaction. The scope-wide
/// marker and the grants commit together, including for an empty launch list.
pub fn materialize(
    conn: &Connection,
    key: &PolicyKey,
    raw: &serde_json::Value,
    grants: &[AccessGrant],
) -> rusqlite::Result<()> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let scope = ScopeKey::from_policy(key);
    if scope_config(&tx, &scope)?.is_none() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    tx.execute("INSERT OR IGNORE INTO access_launch_params(project_root,harness,session,principal,owner,scope_ref,epoch,preset,params) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![key.project_root,key.harness,key.session,key.principal,key.owner,key.scope_ref,key.epoch.to_string(),key.preset,raw.to_string()])?;
    materialize_on(&tx, &scope, grants)?;
    tx.commit()
}

fn materialize_on(
    tx: &rusqlite::Transaction<'_>,
    scope: &ScopeKey,
    grants: &[AccessGrant],
) -> rusqlite::Result<()> {
    let first = tx.execute(
        "INSERT OR IGNORE INTO access_materialized(owner,scope_ref,epoch) VALUES (?1,?2,?3)",
        params![scope.owner, scope.scope_ref, scope.epoch.to_string()],
    )? == 1;
    if first {
        for grant in grants {
            insert(tx, scope, grant)?;
        }
    }
    Ok(())
}

/// A call can restore only persisted launch inputs, never standing consent grants.
/// The immediate transaction serializes the marker check, params carry-forward,
/// current-head validation and grant installation, even across database handles.
pub fn restore_launch(
    conn: &Connection,
    key: &PolicyKey,
    validate: impl FnOnce(&Connection, &serde_json::Value) -> rusqlite::Result<Vec<AccessGrant>>,
) -> rusqlite::Result<bool> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let scope = ScopeKey::from_policy(key);
    let materialized: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM access_materialized WHERE owner=?1 AND scope_ref=?2 AND epoch=?3)",
        params![scope.owner, scope.scope_ref, scope.epoch.to_string()],
        |r| r.get(0),
    )?;
    if materialized {
        return Ok(false);
    }
    let Some((_, json)) = super::remote_exec::catalog_params(
        &tx,
        key,
        super::remote_exec::CatalogParams::AccessLaunch,
    )?
    else {
        // A call before the initial catalog must not freeze an empty launch.
        return Ok(false);
    };
    if scope_config(&tx, &scope)?.is_none() {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let grants = validate(&tx, &decode(&json)?)?;
    materialize_on(&tx, &scope, &grants)?;
    tx.commit()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scope(name: &str, epoch: u64) -> ScopeKey {
        ScopeKey {
            owner: "owner".into(),
            scope_ref: name.into(),
            epoch,
        }
    }
    fn policy(key: &ScopeKey, root: &Path) -> PolicyKey {
        PolicyKey {
            project_root: root.display().to_string(),
            harness: "broca".into(),
            session: "worker".into(),
            principal: "reserved:broca".into(),
            owner: key.owner.clone(),
            scope_ref: key.scope_ref.clone(),
            epoch: key.epoch,
            preset: "worker".into(),
        }
    }
    #[test]
    fn grants_materialization_revoke_restart_reclaim_refetch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let key = scope("worker", u64::MAX);
        let root = dir.path().join("root");
        let bind = policy(&key, &root);
        // Stored grants carry the canonical policy identity (on macOS the
        // temp dir's /var is /private/var), so build the expected grant the
        // same way.
        let grant = AccessGrant {
            path: crate::access_policy::policy_path(&dir.path().join("outside")),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Write,
        };
        let raw = serde_json::json!([{"path": grant.path, "mode": "write"}]);
        {
            let conn = crate::db::open(&path).unwrap();
            register_scope(&conn, &key, Some(&scope("head", 1)), &Config::default()).unwrap();
            materialize(&conn, &bind, &raw, &[grant.clone()]).unwrap();
            assert_eq!(list(&conn, &key).unwrap(), vec![grant.clone()]);
            assert_eq!(revoke(&conn, &key, &grant.path).unwrap(), 1);
        }
        std::fs::create_dir(&root).unwrap();
        std::fs::remove_dir(&root).unwrap();
        std::fs::create_dir(&root).unwrap();
        let conn = crate::db::open(&path).unwrap();
        assert_eq!(launch_params(&conn, &bind).unwrap(), Some(raw.clone()));
        materialize(&conn, &bind, &raw, &[grant.clone()]).unwrap();
        let mut rebound = bind.clone();
        rebound.session = "rebound".into();
        materialize(&conn, &rebound, &raw, &[grant.clone()]).unwrap();
        assert!(list(&conn, &key).unwrap().is_empty());
        let mut access = crate::access_policy::AccessPolicy::from_config(
            &Config::default(),
            &dir.path().join("store"),
            vec![root],
            &dir.path().join("home"),
        );
        access.grants = list(&conn, &key).unwrap();
        assert_eq!(
            access.decide(AccessOperation::Write, &grant.path, false),
            crate::access_policy::AccessDecision::Ask
        );
    }
    #[test]
    fn grants_failed_install_rolls_back_params_and_materialization() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let scope = scope("worker", 7);
        register_scope(&conn, &scope, None, &Config::default()).unwrap();
        let bind = policy(&scope, dir.path());
        let grant = AccessGrant {
            path: crate::access_policy::policy_path(&dir.path().join("future")),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        };
        let raw = serde_json::json!([{"path":grant.path,"mode":"read"}]);
        conn.execute_batch("CREATE TRIGGER fail_install BEFORE INSERT ON access_grants BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
        assert!(materialize(&conn, &bind, &raw, &[grant.clone()]).is_err());
        assert!(launch_params(&conn, &bind).unwrap().is_none());
        assert!(list(&conn, &scope).unwrap().is_empty());
        conn.execute_batch("DROP TRIGGER fail_install;").unwrap();
        materialize(&conn, &bind, &raw, &[grant.clone()]).unwrap();
        assert_eq!(list(&conn, &scope).unwrap(), vec![grant]);
    }

    #[test]
    fn grants_revived_launch_concurrent_first_calls_materialize_once() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Barrier,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aft.db");
        let first = scope("worker", 1);
        let second = scope("worker", 2);
        let bind = policy(&first, dir.path());
        let revived = policy(&second, dir.path());
        let grant = AccessGrant {
            path: crate::access_policy::policy_path(&dir.path().join("outside")),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Write,
        };
        let conn = crate::db::open(&path).unwrap();
        for key in [&first, &second] {
            register_scope(&conn, key, None, &Config::default()).unwrap();
        }
        let raw = serde_json::json!([{"path":grant.path,"mode":"write"}]);
        materialize(&conn, &bind, &raw, &[grant.clone()]).unwrap();
        conn.execute_batch(
            "CREATE TABLE installs(n INTEGER); INSERT INTO installs VALUES(0);
            CREATE TRIGGER count_install BEFORE INSERT ON access_grants WHEN NEW.epoch='2'
            BEGIN UPDATE installs SET n=n+1; END;",
        )
        .unwrap();
        let handles = [
            crate::db::open(&path).unwrap(),
            crate::db::open(&path).unwrap(),
        ];
        let barrier = Arc::new(Barrier::new(2));
        let validations = Arc::new(AtomicUsize::new(0));
        let installed: Vec<bool> = std::thread::scope(|threads| {
            let jobs: Vec<_> = handles
                .into_iter()
                .map(|conn| {
                    let barrier = barrier.clone();
                    let validations = validations.clone();
                    let revived = &revived;
                    let grant = &grant;
                    let raw = &raw;
                    threads.spawn(move || {
                        barrier.wait();
                        restore_launch(&conn, revived, |_, params| {
                            assert_eq!(params, raw, "only stored launch inputs may be validated");
                            validations.fetch_add(1, Ordering::SeqCst);
                            Ok(vec![grant.clone()])
                        })
                        .unwrap()
                    })
                })
                .collect();
            jobs.into_iter().map(|job| job.join().unwrap()).collect()
        });
        assert_eq!(installed.iter().filter(|first| **first).count(), 1);
        assert_eq!(validations.load(Ordering::SeqCst), 1);
        let installs: i64 = conn
            .query_row("SELECT n FROM installs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            installs, 1,
            "upserts must not hide duplicate materializations"
        );
        assert_eq!(list(&conn, &second).unwrap(), vec![grant.clone()]);
        assert_eq!(list(&conn, &first).unwrap(), vec![grant]);
        let markers: i64 = conn
            .query_row("SELECT COUNT(*) FROM access_materialized", [], |r| r.get(0))
            .unwrap();
        assert_eq!(markers, 2);
    }

    #[test]
    fn grants_revived_launch_failed_install_rolls_back_carry_forward() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let first = scope("worker", 1);
        let second = scope("worker", 2);
        for key in [&first, &second] {
            register_scope(&conn, key, None, &Config::default()).unwrap();
        }
        let grant = AccessGrant {
            path: dir.path().join("outside"),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        };
        let raw = serde_json::json!([{"path":grant.path,"mode":"read"}]);
        materialize(&conn, &policy(&first, dir.path()), &raw, &[grant.clone()]).unwrap();
        let revived = policy(&second, dir.path());
        conn.execute_batch("CREATE TRIGGER fail_install BEFORE INSERT ON access_grants BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
        assert!(restore_launch(&conn, &revived, |_, _| Ok(vec![grant.clone()])).is_err());
        for table in [
            "access_launch_params",
            "access_materialized",
            "access_grants",
        ] {
            let count: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE epoch='2'"),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 0, "failed restore must roll back {table}");
        }
        conn.execute_batch("DROP TRIGGER fail_install;").unwrap();
        assert!(restore_launch(&conn, &revived, |_, _| Ok(vec![grant.clone()])).unwrap());
        assert_eq!(list(&conn, &second).unwrap(), vec![grant]);
    }

    #[test]
    fn grants_revived_launch_restoration_isolates_full_identity() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let first = scope("worker", 1);
        register_scope(&conn, &first, None, &Config::default()).unwrap();
        materialize(
            &conn,
            &policy(&first, dir.path()),
            &serde_json::json!([]),
            &[],
        )
        .unwrap();
        for dimension in [
            "ref",
            "owner",
            "session",
            "preset",
            "principal",
            "harness",
            "root",
        ] {
            let mut key = policy(&scope("worker", 2), dir.path());
            match dimension {
                "ref" => key.scope_ref = "other".into(),
                "owner" => key.owner = "other".into(),
                "session" => key.session = "other".into(),
                "preset" => key.preset = "head".into(),
                "principal" => key.principal = "other".into(),
                "harness" => key.harness = "other".into(),
                _ => key.project_root = dir.path().join("other").display().to_string(),
            }
            register_scope(
                &conn,
                &ScopeKey::from_policy(&key),
                None,
                &Config::default(),
            )
            .unwrap();
            assert!(!restore_launch(&conn, &key, |_, _| panic!(
                "crossed {dimension} identity boundary"
            ))
            .unwrap());
        }
    }

    #[test]
    fn grants_revived_worker_carries_launch_params_but_not_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let first = scope("worker", 1);
        let second = scope("worker", 2);
        let head = scope("head", 1);
        for key in [&first, &second] {
            register_scope(&conn, key, Some(&head), &Config::default()).unwrap();
        }
        let bind = policy(&first, dir.path());
        let revived = policy(&second, dir.path());
        let grant = AccessGrant {
            path: crate::access_policy::policy_path(&dir.path().join("outside")),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        };
        let raw = serde_json::json!([{"path":grant.path,"mode":"read"}]);
        materialize(&conn, &bind, &raw, &[grant.clone()]).unwrap();
        assert_eq!(launch_params(&conn, &revived).unwrap(), Some(raw.clone()));
        let copied: String = conn
            .query_row(
                "SELECT params FROM access_launch_params WHERE epoch='2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(copied, raw.to_string());
        assert_eq!(list(&conn, &first).unwrap(), vec![grant]);
        assert!(list(&conn, &second).unwrap().is_empty());
        let markers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM access_materialized WHERE epoch='2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(markers, 0);
        end_scope_grants(&conn, &first).unwrap();
        assert_eq!(launch_params(&conn, &revived).unwrap(), Some(raw));
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM access_launch_params", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn grants_revived_launch_params_use_highest_older_epoch_and_isolate_identity() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let first = scope("worker", 1);
        let second = scope("worker", 2);
        for key in [&first, &second] {
            register_scope(&conn, key, None, &Config::default()).unwrap();
        }
        materialize(
            &conn,
            &policy(&first, dir.path()),
            &serde_json::json!(["first"]),
            &[],
        )
        .unwrap();
        let newest = serde_json::json!(["second"]);
        materialize(&conn, &policy(&second, dir.path()), &newest, &[]).unwrap();
        let revived = policy(&scope("worker", 3), dir.path());
        assert_eq!(launch_params(&conn, &revived).unwrap(), Some(newest));
        for different in 0..8 {
            let mut other = revived.clone();
            match different {
                0 => other.epoch = 0,
                1 => other.principal = "direct".into(),
                2 => other.owner = "other".into(),
                3 => other.scope_ref = "other".into(),
                4 => other.harness = "other".into(),
                5 => other.project_root = "/other".into(),
                6 => other.preset = "head".into(),
                _ => other.session = "two".into(),
            }
            assert!(
                launch_params(&conn, &other).unwrap().is_none(),
                "key dimension {different}"
            );
        }
    }

    #[test]
    fn grants_scope_authorization_and_end_are_epoch_bound() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("aft.db")).unwrap();
        let head = scope("head", 1);
        let worker = scope("worker", 2);
        let sibling = scope("sibling", 2);
        for (key, parent) in [(&head, None), (&worker, Some(&head)), (&sibling, None)] {
            register_scope(&conn, key, parent, &Config::default()).unwrap();
        }
        assert!(authorized(&conn, &head, &head).unwrap());
        assert!(authorized(&conn, &head, &worker).unwrap());
        assert!(!authorized(&conn, &worker, &head).unwrap());
        assert!(!authorized(&conn, &worker, &sibling).unwrap());
        let grant = AccessGrant {
            path: dir.path().join("future"),
            kind: GrantKind::File,
            operation: AccessOperation::Write,
        };
        insert(&conn, &worker, &grant).unwrap();
        assert!(list(&conn, &scope("worker", 3)).unwrap().is_empty());
        end_scope_grants(&conn, &worker).unwrap();
        assert!(list(&conn, &worker).unwrap().is_empty());
        register_scope(&conn, &worker, Some(&head), &Config::default()).unwrap();
        insert(&conn, &worker, &grant).unwrap();
        assert!(list(&conn, &worker).unwrap().is_empty());
        assert!(!authorized(&conn, &head, &worker).unwrap());
    }
}
