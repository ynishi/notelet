//! Integration test against a real Postgres. Skipped (passes trivially)
//! unless `NOTELET_TEST_DATABASE_URL` is set; CI and local runs point it at
//! a throwaway database. The test uses its own path prefix and cleans up.

use std::sync::Arc;

use notelet_core::{CoreError, History, NoteService, PgNoteRepo};

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
    assert!(n.changed);
    let History::Full(h) = svc.history(&p("a.md"), None, true).await.unwrap() else {
        panic!("full history expected")
    };
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].body, "one");

    // the same body again is a no-op: no bump, no revision
    let n = svc.put(&p("a.md"), "two", Some(2), "t").await.unwrap();
    assert!(!n.changed);
    assert_eq!(n.version, 2);
    let History::Meta(h) = svc.history(&p("a.md"), None, false).await.unwrap() else {
        panic!("meta history expected")
    };
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].size, 3);

    // append twice lands on separate lines
    svc.append(&p("j.md"), "first", "t").await.unwrap();
    svc.append(&p("j.md"), "second", "t").await.unwrap();
    assert_eq!(svc.get(&p("j.md")).await.unwrap().body, "first\nsecond\n");

    // a tail read cuts the end and reports the whole size
    let t = svc.read(&p("j.md"), Some(1), None).await.unwrap();
    assert_eq!(t.body, "second\n");
    assert!(t.truncated);
    assert_eq!(t.size, 13);
    let w = svc.read(&p("j.md"), None, None).await.unwrap();
    assert_eq!(w.body, "first\nsecond\n");
    assert!(!w.truncated);

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

    // delete keeps the body as the last revision; re-creating continues the
    // version so a later replace does not collide with the kept revisions
    let e = svc.delete(&p("a.md"), Some(9), "t").await.unwrap_err();
    assert!(matches!(e, CoreError::VersionConflict { current: 2, .. }));
    let w = svc.delete(&p("a.md"), Some(2), "t").await.unwrap();
    assert_eq!(w.version, 2);
    assert!(matches!(
        svc.get(&p("a.md")).await.unwrap_err(),
        CoreError::NotFound(_)
    ));
    let History::Full(h) = svc.history(&p("a.md"), None, true).await.unwrap() else {
        panic!("full history expected")
    };
    assert_eq!(h[0].version, 2);
    assert_eq!(h[0].body, "two");
    let n = svc.put(&p("a.md"), "three", None, "t").await.unwrap();
    assert_eq!(n.version, 3);
    let n = svc.put(&p("a.md"), "four", Some(3), "t").await.unwrap();
    assert_eq!(n.version, 4);

    // append re-creates a deleted note one past its highest revision, and a
    // later replace / delete does not collide with the kept revisions
    let w = svc.delete(&p("j.md"), None, "t").await.unwrap();
    assert_eq!(w.version, 2);
    let w = svc.append(&p("j.md"), "again", "t").await.unwrap();
    assert_eq!(w.version, 3);
    assert_eq!(svc.get(&p("j.md")).await.unwrap().body, "again\n");
    let w = svc.put(&p("j.md"), "replaced", Some(3), "t").await.unwrap();
    assert_eq!(w.version, 4);
    let w = svc.delete(&p("j.md"), Some(4), "t").await.unwrap();
    assert_eq!(w.version, 4);
    let History::Meta(h) = svc.history(&p("j.md"), None, false).await.unwrap() else {
        panic!("meta history expected")
    };
    let versions: Vec<i64> = h.iter().map(|r| r.version).collect();
    assert_eq!(versions, vec![4, 3, 2]);
    assert_eq!(h[0].size, 8);

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
