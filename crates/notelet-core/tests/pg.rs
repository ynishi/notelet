//! Integration test against a real Postgres. Skipped (passes trivially)
//! unless `NOTELET_TEST_DATABASE_URL` is set. Local runs (`just test-pg`)
//! default it to the throwaway container from `just pg-up`; CI points it at
//! its service container. The test uses its own path prefix and cleans up.

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
    // a path hit (different case) with no body match: snippet is the body start
    let s = svc.search("J.MD", Some(&prefix), None, None).await.unwrap();
    assert_eq!(s.items.len(), 1);
    assert!(s.items[0].snippet.starts_with("first"));
    let d = svc.dump(Some(&prefix), None, None).await.unwrap();
    assert_eq!(d.items.len(), 2);

    // delete keeps the body as the last revision; re-creating continues the
    // version so a later replace does not collide with the kept revisions
    let e = svc.delete(&p("a.md"), 9, "t").await.unwrap_err();
    assert!(matches!(e, CoreError::VersionConflict { current: 2, .. }));
    let w = svc.delete(&p("a.md"), 2, "t").await.unwrap();
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
    let w = svc.delete(&p("j.md"), 2, "t").await.unwrap();
    assert_eq!(w.version, 2);
    let w = svc.append(&p("j.md"), "again", "t").await.unwrap();
    assert_eq!(w.version, 3);
    assert_eq!(svc.get(&p("j.md")).await.unwrap().body, "again\n");
    let w = svc.put(&p("j.md"), "replaced", Some(3), "t").await.unwrap();
    assert_eq!(w.version, 4);
    let w = svc.delete(&p("j.md"), 4, "t").await.unwrap();
    assert_eq!(w.version, 4);
    let History::Meta(h) = svc.history(&p("j.md"), None, false).await.unwrap() else {
        panic!("meta history expected")
    };
    let versions: Vec<i64> = h.iter().map(|r| r.version).collect();
    assert_eq!(versions, vec![4, 3, 2]);
    assert_eq!(h[0].size, 8);

    // concurrent create without a version on a new path: one wins, the
    // other answers VersionConflict (not Internal)
    let svc = Arc::new(svc);
    let path = p("race.md");
    let mut tasks = Vec::new();
    for i in 0..8 {
        let svc = svc.clone();
        let path = path.clone();
        tasks.push(tokio::spawn(async move {
            svc.put(&path, &format!("body {i}"), None, "t").await
        }));
    }
    let mut won = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(w) => {
                assert_eq!(w.version, 1);
                won += 1;
            }
            Err(e) => assert!(
                matches!(
                    e,
                    CoreError::VersionConflict {
                        expected: None,
                        current: 1,
                        ..
                    }
                ),
                "unexpected error: {e:?}"
            ),
        }
    }
    assert_eq!(won, 1);

    // force the unique-violation branch: an open transaction has inserted
    // the row (invisible to the put's pre-check), the put blocks on the
    // unique index, and the commit makes it the loser
    let forced = p("forced.md");
    let mut tx = repo.pool().begin().await.unwrap();
    sqlx::query("INSERT INTO notes (path, body, version, updated_by) VALUES ($1, 'first', 1, 't')")
        .bind(&forced)
        .execute(&mut *tx)
        .await
        .unwrap();
    let loser = {
        let svc = svc.clone();
        let forced = forced.clone();
        tokio::spawn(async move { svc.put(&forced, "second", None, "t").await })
    };
    // wait until the put's INSERT is blocked on the open transaction, so the
    // unique-violation branch (not the pre-check) is what decides it
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE wait_event_type = 'Lock' AND query LIKE 'INSERT INTO notes%' \
             AND pid <> pg_backend_pid()",
        )
        .fetch_one(repo.pool())
        .await
        .unwrap();
        if waiting > 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the put never blocked on the open insert"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    tx.commit().await.unwrap();
    let e = loser.await.unwrap().unwrap_err();
    assert!(
        matches!(
            e,
            CoreError::VersionConflict {
                expected: None,
                current: 1,
                ..
            }
        ),
        "unexpected: {e:?}"
    );
    assert_eq!(svc.get(&forced).await.unwrap().body, "first");

    // delete names the version it read: a stale one is refused and the note
    // stays (a delete without a version is unrepresentable here; the MCP
    // tool answers it as Validation)
    let e = svc.delete(&path, 7, "t").await.unwrap_err();
    assert!(matches!(
        e,
        CoreError::VersionConflict {
            expected: Some(7),
            current: 1,
            ..
        }
    ));
    assert_eq!(svc.get(&path).await.unwrap().version, 1);

    // updated_at never runs backwards against version
    let w1 = svc.put(&p("clock.md"), "a", None, "t").await.unwrap();
    let w2 = svc.put(&p("clock.md"), "b", Some(1), "t").await.unwrap();
    let w3 = svc.append(&p("clock.md"), "c", "t").await.unwrap();
    assert!(w1.updated_at <= w2.updated_at && w2.updated_at <= w3.updated_at);

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
