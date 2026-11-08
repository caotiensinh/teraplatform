//! The same contract the in-memory store passes, against a real PostgreSQL.
//! Set `TF_TEST_DATABASE_URL` (a throwaway database; every test uses its own
//! schema and drops it). Without it the cases print SKIPPED and pass, so check
//! the count of SKIPPED lines before trusting a green run.

use std::sync::Arc;
use tf_core::*;
use tf_crypto::Crypto;
use tf_store::{PgStore, Store, StoreError};

fn url() -> Option<String> {
    std::env::var("TF_TEST_DATABASE_URL").ok().filter(|s| !s.is_empty())
}

static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

async fn fresh() -> Option<(PgStore, String, String)> {
    let u = url()?;
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let schema = format!("t_{}_{}", std::process::id(), n);
    let s = PgStore::connect_in_schema(&u, Some(&schema), Crypto::ephemeral()).await.expect("connect and migrate");
    Some((s, u, schema))
}

async fn make() -> Option<Arc<dyn Store>> {
    let (s, _, _) = fresh().await?;
    Some(Arc::new(s))
}

tf_store::all_contract_tests!(make);

#[tokio::test]
async fn nothing_sensitive_rests_in_plaintext_in_postgres() {
    let Some((s, u, schema)) = fresh().await else {
        eprintln!("SKIPPED: TF_TEST_DATABASE_URL not set");
        return;
    };
    let user = s.create_user("a@example.com", "h", "A").await.unwrap();
    let org = s.create_org("acme", &user.id).await.unwrap();
    let ws = s.create_workspace(&org.id, "prod").await.unwrap();
    let cv = s.create_config_version(&ws.id, "cfg", "t").await.unwrap();
    let run = s.create_run(&ws.id, &cv.id, Operation::PlanAndApply, "", false, "t").await.unwrap();
    s.finalize_state(&ws.id, &run.id, "{\"password\":\"hunter2\"}", 1).await.unwrap();
    s.set_plan(&run.id, PlanSummary::default(), vec![], b"plan-hunter2".to_vec()).await.unwrap();
    s.set_variable(
        &ws.id,
        NewVariable { key: "TOKEN".into(), category: "env".into(), hcl: false, sensitive: true, value: "hunter2".into(), description: String::new() },
    )
    .await
    .unwrap();

    for raw in [
        s.raw_scalar("select content from state_versions where workspace_id = $1", &ws.id).await.unwrap(),
        s.raw_scalar("select plan_blob from runs where id = $1", &run.id).await.unwrap(),
        s.raw_scalar("select value from variables where workspace_id = $1", &ws.id).await.unwrap(),
    ] {
        assert!(raw.starts_with("enc:v1:") && !raw.contains("hunter2"), "stored form must be sealed: {raw}");
    }
    PgStore::drop_schema(&u, &schema).await;
}

#[tokio::test]
async fn the_database_itself_refuses_plaintext_sensitive_variables_and_a_second_executing_run() {
    let Some((s, u, schema)) = fresh().await else {
        eprintln!("SKIPPED: TF_TEST_DATABASE_URL not set");
        return;
    };
    let user = s.create_user("a@example.com", "h", "A").await.unwrap();
    let org = s.create_org("acme", &user.id).await.unwrap();
    let ws = s.create_workspace(&org.id, "prod").await.unwrap();
    let cv = s.create_config_version(&ws.id, "cfg", "t").await.unwrap();

    // bypass the store with raw SQL on a second connection in the same schema
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .after_connect({
            let schema = schema.clone();
            move |c, _| {
                let schema = schema.clone();
                Box::pin(async move {
                    sqlx::query(&format!("set search_path to \"{schema}\"")).execute(c).await?;
                    Ok(())
                })
            }
        })
        .connect(&u)
        .await
        .unwrap();
    let bad = sqlx::query("insert into variables (workspace_id, key, category, sensitive, value) values ($1,'K','env',true,'plaintext')")
        .bind(&ws.id)
        .execute(&pool)
        .await;
    assert!(bad.is_err(), "check constraint must reject a plaintext sensitive value");

    for i in 0..2 {
        let r = sqlx::query(
            "insert into runs (workspace_id, config_version_id, status, operation, source) values ($1,$2,'planning','plan_and_apply','t')",
        )
        .bind(&ws.id)
        .bind(&cv.id)
        .execute(&pool)
        .await;
        assert_eq!(r.is_ok(), i == 0, "second lock-holding run on one workspace must violate the unique index");
    }
    // many speculative plans are fine
    for _ in 0..3 {
        sqlx::query("insert into runs (workspace_id, config_version_id, status, operation, source) values ($1,$2,'planning','plan_only','t')")
            .bind(&ws.id)
            .bind(&cv.id)
            .execute(&pool)
            .await
            .unwrap();
    }
    drop(pool);
    assert!(matches!(s.get_workspace("nope").await, Err(StoreError::NotFound(_))));
    PgStore::drop_schema(&u, &schema).await;
}

#[tokio::test]
async fn one_poisoned_workspace_cannot_starve_the_queue() {
    let Some((s, u, schema)) = fresh().await else {
        eprintln!("SKIPPED: TF_TEST_DATABASE_URL not set");
        return;
    };
    let user = s.create_user("a@example.com", "h", "A").await.unwrap();
    let org = s.create_org("acme", &user.id).await.unwrap();
    let bad = s.create_workspace(&org.id, "bad").await.unwrap();
    let good = s.create_workspace(&org.id, "good").await.unwrap();
    let cv_bad = s.create_config_version(&bad.id, "cfg", "t").await.unwrap();
    let cv_good = s.create_config_version(&good.id, "cfg", "t").await.unwrap();

    // a crashed run left its workspace unlocked while the run still looks active
    let orphan = s.create_run(&bad.id, &cv_bad.id, Operation::PlanAndApply, "orphan", false, "t").await.unwrap();
    s.claim_next().await.unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .after_connect({
            let schema = schema.clone();
            move |c, _| {
                let schema = schema.clone();
                Box::pin(async move {
                    sqlx::query(&format!("set search_path to \"{schema}\"")).execute(c).await?;
                    Ok(())
                })
            }
        })
        .connect(&u)
        .await
        .unwrap();
    sqlx::query("update workspaces set lock_owner = null where id = $1").bind(&bad.id).execute(&pool).await.unwrap();
    let stuck = s.create_run(&bad.id, &cv_bad.id, Operation::PlanAndApply, "stuck", false, "t").await.unwrap();
    let healthy = s.create_run(&good.id, &cv_good.id, Operation::PlanAndApply, "healthy", false, "t").await.unwrap();

    let claim = s.claim_next().await.expect("claiming must not fail because of another workspace");
    match claim {
        Some(tf_store::Claim::Plan(r)) => assert_eq!(r.id, healthy.id, "the healthy workspace's run is served"),
        other => panic!("expected the healthy run, got {other:?}"),
    }
    assert_eq!(s.get_run(&stuck.id).await.unwrap().status, RunStatus::Pending);
    assert_eq!(s.get_run(&orphan.id).await.unwrap().status, RunStatus::Planning);
    drop(pool);
    PgStore::drop_schema(&u, &schema).await;
}
