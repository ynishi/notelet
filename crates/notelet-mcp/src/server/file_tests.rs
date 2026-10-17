//! Tests of the local file projection through the tools (see [`super`] and
//! [`crate::files`]).

use super::*;
use crate::files::scratch;
use notelet_core::domain::sha256_hex;
use notelet_core::MemoryNoteRepo;
use serde_json::{json, Value};

const P: &str = "projects/x/doc.md";

struct Fixture {
    s: NoteletMcp,
    svc: Arc<NoteService>,
    dir: std::path::PathBuf,
}

fn fixture(name: &str) -> Fixture {
    let dir = scratch(name);
    let svc = Arc::new(NoteService::new(Arc::new(MemoryNoteRepo::default())));
    let s = NoteletMcp::local(svc.clone(), "test".into(), FileRoot::new(dir.clone()));
    Fixture { s, svc, dir }
}

fn text_of(r: &CallToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("")
}

fn ok(r: &CallToolResult) -> Value {
    assert_ne!(r.is_error, Some(true), "{}", text_of(r));
    serde_json::from_str(&text_of(r)).unwrap()
}

fn err(r: &CallToolResult) -> Value {
    assert_eq!(r.is_error, Some(true), "{}", text_of(r));
    serde_json::from_str(&text_of(r)).unwrap()
}

async fn get_to(s: &NoteletMcp, path: &str, to: &str) -> CallToolResult {
    s.note_get(Parameters(GetParams {
        path: path.into(),
        tail_lines: None,
        max_bytes: None,
        to_file: Some(to.into()),
    }))
    .await
    .unwrap()
}

/// A wire string as the enum it names; panics on a string the schema does not allow.
fn wire<T: serde::de::DeserializeOwned>(s: &str) -> T {
    serde_json::from_value(serde_json::Value::String(s.into())).unwrap()
}

async fn put(
    s: &NoteletMcp,
    body: Option<&str>,
    from: Option<&str>,
    expected: Option<i64>,
    on_conflict: Option<&str>,
) -> CallToolResult {
    s.note_put(
        Parameters(PutParams {
            path: P.into(),
            body: body.map(Into::into),
            from_file: from.map(Into::into),
            expected_version: expected,
            on_conflict: on_conflict.map(wire),
        }),
        Extensions::default(),
    )
    .await
    .unwrap()
}

async fn restore(s: &NoteletMcp, file: &str, source: &str) -> CallToolResult {
    s.note_restore(Parameters(RestoreParams {
        file: file.into(),
        source: wire(source),
    }))
    .await
    .unwrap()
}

/// A note at version 2 whose version 1 body is `base`, with `ours` in
/// `work.md`; the caller read version 1.
async fn conflicted(f: &Fixture, base: &str, theirs: &str, ours: &str) {
    ok(&put(&f.s, Some(base), None, None, None).await);
    ok(&put(&f.s, Some(theirs), None, Some(1), None).await);
    std::fs::write(f.dir.join("work.md"), ours).unwrap();
}

fn read(f: &Fixture, name: &str) -> String {
    std::fs::read_to_string(f.dir.join(name)).unwrap()
}

#[tokio::test]
async fn get_to_file_edit_put_from_file_round_trips() {
    let f = fixture("roundtrip");
    ok(&put(&f.s, Some("héllo\n"), None, None, None).await);

    let got = ok(&get_to(&f.s, P, "out/deep/doc.md").await);
    assert!(got.get("body").is_none(), "{got}");
    assert_eq!(got["version"], 1);
    assert_eq!(got["size"], 7);
    assert_eq!(got["sha256"], sha256_hex("héllo\n").as_str());
    assert_eq!(read(&f, "out/deep/doc.md"), "héllo\n");

    std::fs::write(f.dir.join("out/deep/doc.md"), "héllo\nedited\n").unwrap();
    let w = ok(&put(&f.s, None, Some("out/deep/doc.md"), Some(1), None).await);
    assert_eq!(w["version"], 2);
    assert_eq!(w["changed"], true);
    let on_disk = read(&f, "out/deep/doc.md");
    assert_eq!(w["sha256"], sha256_hex(&on_disk).as_str());
    assert_eq!(f.svc.get(P).await.unwrap().body, "héllo\nedited\n");
}

#[tokio::test]
async fn a_write_between_get_and_put_is_a_conflict_and_writes_nothing() {
    let f = fixture("race");
    ok(&put(&f.s, Some("one\n"), None, None, None).await);
    ok(&get_to(&f.s, P, "w.md").await);
    ok(&put(&f.s, Some("theirs\n"), None, Some(1), None).await);
    std::fs::write(f.dir.join("w.md"), "mine\n").unwrap();

    let e = err(&put(&f.s, None, Some("w.md"), Some(1), None).await);
    assert_eq!(e["kind"], "Conflict");
    assert_eq!(e["current_version"], 2);
    assert_eq!(e["expected_version"], 1);
    assert!(e.get("marked").is_none(), "{e}");
    assert_eq!(f.svc.get(P).await.unwrap().body, "theirs\n");
    assert_eq!(read(&f, "w.md"), "mine\n", "the file is left alone");
}

#[tokio::test]
async fn from_file_without_a_version_only_creates() {
    let f = fixture("create");
    std::fs::write(f.dir.join("w.md"), "new\n").unwrap();
    ok(&put(&f.s, None, Some("w.md"), None, None).await);
    let e = err(&put(&f.s, None, Some("w.md"), None, None).await);
    assert_eq!(e["kind"], "Conflict");
    assert!(e["expected_version"].is_null());
    assert_eq!(e["current_version"], 1);
}

#[tokio::test]
async fn names_outside_the_root_are_refused() {
    let f = fixture("escape");
    let outside = scratch("escape-outside");
    std::fs::write(outside.join("secret.md"), "s").unwrap();
    ok(&put(&f.s, Some("x"), None, None, None).await);
    let mut names = vec![
        "/etc/hostname".to_string(),
        "../x.md".to_string(),
        "a/../../x.md".to_string(),
        outside.join("secret.md").to_str().unwrap().to_string(),
    ];
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, f.dir.join("link")).unwrap();
        names.push("link/secret.md".into());
        names.push("link/new.md".into());
    }
    for n in &names {
        let e = err(&get_to(&f.s, P, n).await);
        assert_eq!(e["kind"], "Validation", "{n}: {e}");
        let e = err(&put(&f.s, None, Some(n), None, None).await);
        assert_eq!(e["kind"], "Validation", "{n}: {e}");
        let e = err(&restore(&f.s, n, "ours").await);
        assert_eq!(e["kind"], "Validation", "{n}: {e}");
    }
    assert!(!outside.join("new.md").exists());
    assert_eq!(f.svc.get(P).await.unwrap().body, "x");
}

#[tokio::test]
async fn to_file_with_a_tail_is_refused() {
    let f = fixture("tail");
    ok(&put(&f.s, Some("a\nb\n"), None, None, None).await);
    for (l, m) in [(Some(1), None), (None, Some(2)), (Some(1), Some(2))] {
        let r =
            f.s.note_get(Parameters(GetParams {
                path: P.into(),
                tail_lines: l,
                max_bytes: m,
                to_file: Some("t.md".into()),
            }))
            .await
            .unwrap();
        let e = err(&r);
        assert_eq!(e["kind"], "Validation");
        assert!(e["message"].as_str().unwrap().contains("tail_lines"));
    }
    assert!(!f.dir.join("t.md").exists());
}

#[tokio::test]
async fn body_and_from_file_are_exclusive_and_on_conflict_needs_a_file() {
    let f = fixture("exclusive");
    std::fs::write(f.dir.join("w.md"), "x").unwrap();
    for r in [
        put(&f.s, Some("a"), Some("w.md"), None, None).await,
        put(&f.s, None, None, None, None).await,
        put(&f.s, Some("a"), None, None, Some("mark")).await,
    ] {
        assert_eq!(err(&r)["kind"], "Validation");
    }
    assert!(f.svc.get(P).await.is_err(), "nothing was written");
}

#[test]
fn on_conflict_other_than_fail_or_mark_does_not_deserialize() {
    assert!(serde_json::from_value::<OnConflict>(serde_json::json!("merge")).is_err());
    assert_eq!(wire::<OnConflict>("fail"), OnConflict::Fail);
    assert_eq!(wire::<OnConflict>("mark"), OnConflict::Mark);
}

#[tokio::test]
async fn a_file_that_is_not_utf8_is_refused() {
    let f = fixture("utf8");
    std::fs::write(f.dir.join("bin.md"), [0xff, 0xfe, 0x00]).unwrap();
    let e = err(&put(&f.s, None, Some("bin.md"), None, None).await);
    assert_eq!(e["kind"], "Validation");
    assert!(e["message"].as_str().unwrap().contains("UTF-8"));
    let e = err(&put(&f.s, None, Some("missing.md"), None, None).await);
    assert_eq!(e["kind"], "File");
}

#[tokio::test]
async fn mark_writes_ours_base_and_theirs_exactly() {
    let f = fixture("mark3");
    // none of the three bodies ends with a newline
    conflicted(&f, "base", "theirs", "mine").await;
    let e = err(&put(&f.s, None, Some("work.md"), Some(1), Some("mark")).await);
    assert_eq!(e["kind"], "Conflict");
    assert_eq!(e["marked"], true);
    assert_eq!(e["current_version"], 2);
    assert_eq!(e["expected_version"], 1);
    assert_eq!(e["base_version"], 1);
    assert_eq!(e["file"], "work.md");
    assert!(e["message"]
        .as_str()
        .unwrap()
        .contains("expected_version=2"));
    assert_eq!(
        read(&f, "work.md"),
        "<<<<<<< ours (local, 1 lines, no-eol)\nmine\n||||||| base (v1, 1 lines, no-eol)\nbase\n=======\ntheirs\n>>>>>>> theirs (v2, 1 lines, no-eol)\n"
    );
    assert_eq!(
        f.svc.get(P).await.unwrap().body,
        "theirs",
        "the note is untouched"
    );

    // with trailing newlines nothing is flagged
    let g = fixture("mark3-nl");
    conflicted(&g, "base\n", "theirs\n", "mine\n").await;
    err(&put(&g.s, None, Some("work.md"), Some(1), Some("mark")).await);
    assert_eq!(
        read(&g, "work.md"),
        "<<<<<<< ours (local, 1 lines)\nmine\n||||||| base (v1, 1 lines)\nbase\n=======\ntheirs\n>>>>>>> theirs (v2, 1 lines)\n"
    );
}

#[tokio::test]
async fn mark_without_a_readable_base_writes_two_sections() {
    // no expected_version
    let f = fixture("mark2a");
    conflicted(&f, "base\n", "theirs\n", "mine\n").await;
    let e = err(&put(&f.s, None, Some("work.md"), None, Some("mark")).await);
    assert_eq!(e["marked"], true);
    assert!(e["expected_version"].is_null());
    assert!(e.get("base_version").is_none(), "{e}");
    assert_eq!(
        read(&f, "work.md"),
        "<<<<<<< ours (local, 1 lines)\nmine\n=======\ntheirs\n>>>>>>> theirs (v2, 1 lines)\n"
    );

    // an expected_version whose revision does not exist
    let g = fixture("mark2b");
    conflicted(&g, "base\n", "theirs\n", "mine\n").await;
    let e = err(&put(&g.s, None, Some("work.md"), Some(5), Some("mark")).await);
    assert_eq!(e["marked"], true);
    assert!(e.get("base_version").is_none(), "{e}");
    assert_eq!(
        read(&g, "work.md"),
        "<<<<<<< ours (local, 1 lines)\nmine\n=======\ntheirs\n>>>>>>> theirs (v2, 1 lines)\n"
    );
}

#[tokio::test]
async fn the_base_is_found_by_version_among_several_revisions() {
    let f = fixture("base-deep");
    ok(&put(&f.s, Some("v1\n"), None, None, None).await);
    for v in 1..4 {
        ok(&put(&f.s, Some(&format!("v{}\n", v + 1)), None, Some(v), None).await);
    }
    std::fs::write(f.dir.join("work.md"), "mine\n").unwrap();
    let e = err(&put(&f.s, None, Some("work.md"), Some(2), Some("mark")).await);
    assert_eq!(e["base_version"], 2);
    assert_eq!(e["current_version"], 4);
    ok(&restore(&f.s, "work.md", "base").await);
    assert_eq!(read(&f, "work.md"), "v2\n");
}

#[tokio::test]
async fn mark_accepts_bodies_with_any_marker_like_line() {
    let nasty = "a\n=======\n<<<<<<< ours (local, 1 lines)\n||||||| base (v1, 1 lines)\n>>>>>>> theirs (v2, 1 lines)\n<<<<<<< HEAD\n>>>>>>> x";
    let f = fixture("nasty");
    // base and theirs are the nasty ones; ours is plain
    ok(&put(&f.s, Some(nasty), None, None, None).await);
    ok(&put(&f.s, Some(nasty), None, Some(1), None).await); // unchanged
    ok(&put(&f.s, Some(&format!("{nasty}\nmore")), None, Some(1), None).await);
    std::fs::write(f.dir.join("work.md"), "mine\n").unwrap();
    let e = err(&put(&f.s, None, Some("work.md"), Some(1), Some("mark")).await);
    assert_eq!(e["marked"], true, "{e}");
    for (source, want) in [
        ("theirs", format!("{nasty}\nmore")),
        ("base", nasty.to_string()),
    ] {
        let g = read(&f, "work.md");
        let r = ok(&restore(&f.s, "work.md", source).await);
        assert_eq!(r["expected_version"], 2);
        assert_eq!(read(&f, "work.md"), want, "{source}");
        std::fs::write(f.dir.join("work.md"), g).unwrap();
    }
}

#[tokio::test]
async fn a_failed_mark_leaves_from_file_byte_identical() {
    let f = fixture("mark-fails");
    conflicted(&f, "base\n", "theirs\n", "mine\r\nunsaved edit").await;
    crate::files::FAIL_RENAME.with(|c| c.set(true));
    let e = err(&put(&f.s, None, Some("work.md"), Some(1), Some("mark")).await);
    crate::files::FAIL_RENAME.with(|c| c.set(false));
    assert_eq!(e["kind"], "Conflict");
    assert_eq!(e["marked"], false);
    assert_eq!(e["current_version"], 2);
    assert!(e["message"].as_str().unwrap().contains("not marked"), "{e}");
    assert_eq!(
        std::fs::read(f.dir.join("work.md")).unwrap(),
        b"mine\r\nunsaved edit"
    );
    let tmp: Vec<_> = std::fs::read_dir(&f.dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("tmp")
        })
        .collect();
    assert!(tmp.is_empty(), "no temporary file is left");
}

#[tokio::test]
async fn a_note_with_a_setext_heading_can_be_put_from_a_file() {
    let f = fixture("setext");
    std::fs::write(
        f.dir.join("w.md"),
        "Title\n=======\ntext\n```\n<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> branch\n```\n",
    )
    .unwrap();
    ok(&put(&f.s, None, Some("w.md"), None, None).await);
    assert!(f
        .svc
        .get(P)
        .await
        .unwrap()
        .body
        .starts_with("Title\n=======\n"));
}

#[tokio::test]
async fn a_file_with_notelet_headers_is_not_put() {
    let f = fixture("guard");
    ok(&put(&f.s, Some("one\n"), None, None, None).await);
    for bad in [
        "a\n<<<<<<< ours (local, 1 lines)\nb\n",
        "a\n||||||| base (v1, 2 lines)\n",
        "a\n>>>>>>> theirs (v2, 0 lines)",
    ] {
        std::fs::write(f.dir.join("w.md"), bad).unwrap();
        let e = err(&put(&f.s, None, Some("w.md"), Some(1), None).await);
        assert_eq!(e["kind"], "Validation");
        assert!(e["message"].as_str().unwrap().contains("conflict markers"));
    }
    // a whole marked file is refused too
    let g = fixture("guard-marked");
    conflicted(&g, "x\n", "y\n", "z\n").await;
    err(&put(&g.s, None, Some("work.md"), Some(1), Some("mark")).await);
    let e = err(&put(&g.s, None, Some("work.md"), Some(2), None).await);
    assert_eq!(e["kind"], "Validation");
}

#[tokio::test]
async fn to_file_does_not_overwrite_an_unresolved_conflict() {
    let f = fixture("tofile-guard");
    conflicted(&f, "x\n", "y\n", "z\n").await;
    err(&put(&f.s, None, Some("work.md"), Some(1), Some("mark")).await);
    let before = read(&f, "work.md");
    let e = err(&get_to(&f.s, P, "work.md").await);
    assert_eq!(e["kind"], "Validation");
    assert!(e["message"].as_str().unwrap().contains("note_restore"));
    assert_eq!(read(&f, "work.md"), before);
    // an ordinary file is overwritten
    std::fs::write(f.dir.join("plain.md"), "old").unwrap();
    ok(&get_to(&f.s, P, "plain.md").await);
    assert_eq!(read(&f, "plain.md"), "y\n");
}

#[tokio::test]
async fn restore_gives_back_the_exact_original_bytes() {
    let cases = [
        ("base", "theirs", "mine"),
        ("base\n", "theirs\n", "mine\n"),
        ("", "t", "\n"),
        ("b\r\n", "t\r", "m\n\n"),
        ("b\r", "t\r\nx", "m\r"),
    ];
    for (base, theirs, ours) in cases {
        for (source, want, section) in [
            ("ours", ours, json!(null)),
            ("theirs", theirs, json!(2)),
            ("base", base, json!(1)),
        ] {
            let f = fixture("restore");
            conflicted(&f, base, theirs, ours).await;
            err(&put(&f.s, None, Some("work.md"), Some(1), Some("mark")).await);
            let r = ok(&restore(&f.s, "work.md", source).await);
            assert_eq!(r["file"], "work.md");
            assert_eq!(r["source"], source);
            assert_eq!(r["expected_version"], 2, "{source}");
            assert_eq!(r["section_version"], section, "{source}");
            assert!(r.get("version").is_none());
            let bytes = std::fs::read(f.dir.join("work.md")).unwrap();
            assert_eq!(
                bytes,
                want.as_bytes(),
                "{source} of {base:?}/{theirs:?}/{ours:?}"
            );
        }
    }
}

#[tokio::test]
async fn restore_without_a_base_has_no_base_to_give() {
    let f = fixture("restore-ours-null");
    conflicted(&f, "b\n", "t\n", "m\n").await;
    err(&put(&f.s, None, Some("work.md"), None, Some("mark")).await);
    let r = ok(&restore(&f.s, "work.md", "ours").await);
    assert!(r["section_version"].is_null());
    assert_eq!(r["expected_version"], 2);
    assert_eq!(read(&f, "work.md"), "m\n");

    let g = fixture("restore-no-base");
    conflicted(&g, "b\n", "t\n", "m\n").await;
    err(&put(&g.s, None, Some("work.md"), None, Some("mark")).await);
    let before = read(&g, "work.md");
    let e = err(&restore(&g.s, "work.md", "base").await);
    assert_eq!(e["kind"], "Validation");
    assert_eq!(read(&g, "work.md"), before);
}

#[tokio::test]
async fn restore_any_side_then_put_with_expected_version_succeeds() {
    // theirs: edit, then put
    let f = fixture("resolve-theirs");
    conflicted(&f, "base\n", "theirs\n", "mine\n").await;
    err(&put(&f.s, None, Some("work.md"), Some(1), Some("mark")).await);
    let r = ok(&restore(&f.s, "work.md", "theirs").await);
    std::fs::write(f.dir.join("work.md"), "theirs\nplus mine\n").unwrap();
    let v = r["expected_version"].as_i64().unwrap();
    let w = ok(&put(&f.s, None, Some("work.md"), Some(v), None).await);
    assert_eq!(w["version"], 3);
    assert_eq!(f.svc.get(P).await.unwrap().body, "theirs\nplus mine\n");

    // ours: overwrites theirs, as git checkout --ours does
    let g = fixture("resolve-ours");
    conflicted(&g, "base\n", "theirs\n", "mine\n").await;
    err(&put(&g.s, None, Some("work.md"), Some(1), Some("mark")).await);
    let r = ok(&restore(&g.s, "work.md", "ours").await);
    let v = r["expected_version"].as_i64().unwrap();
    let w = ok(&put(&g.s, None, Some("work.md"), Some(v), None).await);
    assert_eq!(w["version"], 3);
    assert_eq!(g.svc.get(P).await.unwrap().body, "mine\n");

    // base: the put goes back to the version the caller had read
    let h = fixture("resolve-base");
    conflicted(&h, "base\n", "theirs\n", "mine\n").await;
    err(&put(&h.s, None, Some("work.md"), Some(1), Some("mark")).await);
    let r = ok(&restore(&h.s, "work.md", "base").await);
    assert_eq!(r["section_version"], 1);
    let v = r["expected_version"].as_i64().unwrap();
    let w = ok(&put(&h.s, None, Some("work.md"), Some(v), None).await);
    assert_eq!(w["version"], 3);
    assert_eq!(h.svc.get(P).await.unwrap().body, "base\n");
}

#[tokio::test]
async fn restore_on_a_malformed_file_errors_and_leaves_it_alone() {
    let f = fixture("restore-bad");
    let good = crate::files::Marked {
        ours: "o\n".into(),
        base: Some((1, "b\n".into())),
        theirs: (2, "t\n".into()),
    }
    .render();
    let truncated = good.replace(">>>>>>> theirs (v2, 1 lines)\n", "");
    let two_mids = good.replace("||||||| base (v1, 1 lines)\n", "=======\n");
    let miscounted = good.replace("ours (local, 1 lines)", "ours (local, 2 lines)");
    let miscounted_theirs = good.replace("theirs (v2, 1 lines)", "theirs (v2, 3 lines)");
    for (i, bad) in [
        "just text\n",
        "",
        truncated.as_str(),
        two_mids.as_str(),
        miscounted.as_str(),
        miscounted_theirs.as_str(),
        "<<<<<<< ours (local, 1 lines)\nx\n=======\ny\n>>>>>>> theirs (vx, 1 lines)\n",
    ]
    .iter()
    .enumerate()
    {
        let name = format!("bad{i}.md");
        std::fs::write(f.dir.join(&name), bad).unwrap();
        let e = err(&restore(&f.s, &name, "ours").await);
        assert_eq!(e["kind"], "Validation", "{bad:?}");
        assert_eq!(read(&f, &name), *bad);
    }
    std::fs::write(f.dir.join("ok.md"), &good).unwrap();
    // a source outside ours / theirs / base does not deserialize, so the
    // handler is never reached
    assert!(serde_json::from_value::<RestoreSource>(serde_json::json!("mine")).is_err());
    assert_eq!(read(&f, "ok.md"), good);
    let e = err(&restore(&f.s, "nothing.md", "ours").await);
    assert_eq!(e["kind"], "File");
}

#[test]
fn local_mode_needs_loopback_a_root_and_a_token() {
    use crate::auth::{digest, Actor, TokenTable};
    let cfg = |bind: &str, root: Option<&str>, token: bool| {
        let mut tokens = TokenTable::default();
        if token {
            assert!(tokens.insert(digest("t"), Actor::new("alice")));
        }
        Config {
            tokens,
            bind: bind.parse().unwrap(),
            actor: "a".into(),
            file_root: root.map(Into::into),
        }
    };
    assert!(local_root(&cfg("127.0.0.1:1", Some("/r"), true)).is_ok());
    assert!(local_root(&cfg("[::1]:1", Some("/r"), true)).is_ok());
    for (c, why) in [
        (cfg("127.0.0.1:1", None, true), "NOTELET_FILE_ROOT"),
        (cfg("127.0.0.1:1", Some("/r"), false), "token"),
        (cfg("10.0.0.1:1", Some("/r"), true), "loopback"),
    ] {
        let reason = local_root(&c).unwrap_err();
        assert!(reason.contains(why), "{reason}");
    }
}

fn schema_text(t: &rmcp::model::Tool) -> String {
    serde_json::to_string(&*t.input_schema).unwrap() + t.description.as_deref().unwrap_or_default()
}

#[test]
fn outside_local_mode_no_file_parameter_is_listed() {
    let public = NoteletMcp::router_for(false);
    let tools = public.list_all();
    assert!(tools.iter().all(|t| t.name != LOCAL_ONLY_TOOL));
    assert_eq!(tools.len(), 8);
    for t in &tools {
        let text = schema_text(t);
        for word in LOCAL_ONLY_PARAMS
            .iter()
            .chain(&[LOCAL_ONLY_TOOL, "local mode"])
        {
            assert!(!text.contains(word), "{} mentions {word}", t.name);
        }
    }
    let put = public.get("note_put").unwrap();
    assert_eq!(put.input_schema["required"], json!(["path", "body"]));
    assert_eq!(put.input_schema["properties"]["body"]["type"], "string");

    let local = NoteletMcp::router_for(true);
    assert_eq!(local.list_all().len(), 9);
    for param in LOCAL_ONLY_PARAMS {
        assert!(
            local
                .list_all()
                .iter()
                .any(|t| property_names_of(t).iter().any(|n| n == param)),
            "{param}"
        );
    }
    assert_eq!(
        local.get("note_put").unwrap().input_schema["required"],
        json!(["path"])
    );
    assert!(local.get(LOCAL_ONLY_TOOL).is_some());
}

fn property_names_of(t: &rmcp::model::Tool) -> Vec<String> {
    t.input_schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

#[tokio::test]
async fn outside_local_mode_the_file_parameters_are_refused_not_ignored() {
    let svc = Arc::new(NoteService::new(Arc::new(MemoryNoteRepo::default())));
    let s = NoteletMcp::new(svc.clone(), "test".into());
    let r = put(&s, Some("a"), Some("w.md"), None, None).await;
    assert_eq!(err(&r)["kind"], "Validation");
    let r = put(&s, Some("a"), None, None, Some("mark")).await;
    assert_eq!(err(&r)["kind"], "Validation");
    let r = put(&s, None, None, None, None).await;
    assert!(err(&r)["message"]
        .as_str()
        .unwrap()
        .contains("body is required"));
    let r = get_to(&s, P, "a.md").await;
    assert_eq!(err(&r)["kind"], "Validation");
    let r = restore(&s, "a.md", "ours").await;
    assert_eq!(err(&r)["kind"], "Validation");
    assert!(svc.get(P).await.is_err());
}

/// The projection flow with the notes in another notelet: a loopback
/// `serve` over a memory store is the remote, and `RemoteNoteRepo` the
/// store of the local, file-capable server.
#[tokio::test]
async fn the_flow_runs_through_the_remote_backend() {
    use crate::auth::{digest, Actor, TokenTable};
    use crate::remote::RemoteNoteRepo;
    use notelet_core::NoteRepo;

    const TOKEN: &str = "throwaway-remote-token";
    let mem = Arc::new(MemoryNoteRepo::default());
    let mut tokens = TokenTable::default();
    assert!(tokens.insert(digest(TOKEN), Actor::new("alice")));
    let cfg = Config {
        tokens,
        bind: "127.0.0.1:0".parse().unwrap(),
        actor: "fallback".into(),
        file_root: None,
    };
    let router = build_router(Arc::new(NoteService::new(mem.clone())), &cfg);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let repo = RemoteNoteRepo::connect(&format!("http://{addr}/mcp"), Some(TOKEN))
        .await
        .unwrap();

    let dir = scratch("remote-flow");
    let svc = Arc::new(NoteService::new(Arc::new(repo)));
    let s = NoteletMcp::local(svc, "test".into(), FileRoot::new(dir.clone()));

    ok(&put(&s, Some("one\n"), None, None, None).await);
    let got = ok(&get_to(&s, P, "w.md").await);
    assert_eq!(got["version"], 1);
    assert_eq!(got["sha256"], sha256_hex("one\n").as_str());
    std::fs::write(dir.join("w.md"), "mine\n").unwrap();
    let w = ok(&put(&s, None, Some("w.md"), Some(1), None).await);
    assert_eq!(w["version"], 2);
    assert_eq!(w["sha256"], sha256_hex("mine\n").as_str());

    // someone else writes through the remote store directly
    mem.put(
        &notelet_core::NotePath::parse(P).unwrap(),
        "theirs\n",
        Some(2),
        "bob",
    )
    .await
    .unwrap();
    std::fs::write(dir.join("w.md"), "mine again\n").unwrap();
    let e = err(&put(&s, None, Some("w.md"), Some(2), Some("mark")).await);
    assert_eq!(e["marked"], true);
    assert_eq!(e["current_version"], 3);
    assert_eq!(e["base_version"], 2);
    assert_eq!(
        std::fs::read_to_string(dir.join("w.md")).unwrap(),
        "<<<<<<< ours (local, 1 lines)\nmine again\n||||||| base (v2, 1 lines)\nmine\n=======\ntheirs\n>>>>>>> theirs (v3, 1 lines)\n"
    );
    let r = ok(&restore(&s, "w.md", "theirs").await);
    assert_eq!(r["expected_version"], 3);
    assert_eq!(r["section_version"], 3);
    std::fs::write(dir.join("w.md"), "theirs\nmerged\n").unwrap();
    let w = ok(&put(&s, None, Some("w.md"), Some(3), None).await);
    assert_eq!(w["version"], 4);
}
