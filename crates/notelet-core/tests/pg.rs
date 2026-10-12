//! Integration test against a real Postgres. Skipped (passes trivially)
//! unless `NOTELET_TEST_DATABASE_URL` is set; CI and local runs point it at
//! a throwaway database. The test uses its own path prefix and cleans up.

use std::sync::Arc;

use notelet_core::{CoreError, NoteService, PgNoteRepo};

#[tokio::test]
async fn round_trip_against_postgres() {
    let Ok(url) = std::env::var("NOTELET_TEST_DATABASE_URL") else {
        eprintln!("NOTELET_TEST_DATABASE_URL not set; skipping Postgres integration test");
        return;
    };
    let repo = PgNoteRepo::connect(&url).await.expect("connect");
    repo.migrate().await.expect("migrate");
    let svc = NoteService::new(Arc::new(repo.clone()));

    let prefix = format!("_test/{}/", std::process::id());
    let p = |s: &str| format!("{prefix}{s}");

    // put / conflict / replace / history
    let n = svc.put(&p("a.md"), "one", None, "t").await.unwrap();
    assert_eq!(n.version, 1);
    let e = svc.put(&p("a.md"), "two", None, "t").await.unwrap_err();
    assert!(matches!(e, CoreError::VersionConflict { current: 1, .. }));
    let n = svc.put(&p("a.md"), "two", Some(1), "t").await.unwrap();
    assert_eq!(n.version, 2);
    let h = svc.history(&p("a.md"), None).await.unwrap();
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].body, "one");

    // append twice lands on separate lines
    svc.append(&p("j.md"), "first", "t").await.unwrap();
    svc.append(&p("j.md"), "second", "t").await.unwrap();
    assert_eq!(svc.get(&p("j.md")).await.unwrap().body, "first\nsecond\n");

    // list with prefix + filter, search, dump
    let l = svc.list(Some(&prefix), &[], None, None).await.unwrap();
    assert_eq!(l.items.len(), 2);
    let s = svc
        .search("SECOND", Some(&prefix), None, None)
        .await
        .unwrap();
    assert_eq!(s.items.len(), 1);
    assert!(s.items[0].snippet.contains("second"));
    let d = svc.dump(Some(&prefix), None, None).await.unwrap();
    assert_eq!(d.items.len(), 2);

    // cleanup
    sqlx::query("DELETE FROM revisions WHERE path LIKE $1")
        .bind(format!("{prefix}%"))
        .execute(repo.pool())
        .await
        .unwrap();
    sqlx::query("DELETE FROM notes WHERE path LIKE $1")
        .bind(format!("{prefix}%"))
        .execute(repo.pool())
        .await
        .unwrap();
}
