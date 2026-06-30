use super::*;

fn registry() -> RoleRegistry {
    RoleRegistry::defaults("worker-base")
}

fn step(id: &str, deps: &[&str], status: StepStatus) -> Step {
    Step {
        id: id.to_string(),
        role: "researcher".to_string(),
        task: "t".to_string(),
        deps: deps.iter().map(|s| s.to_string()).collect(),
        review: false,
        status,
        result: None,
        attempts: 0,
        review_cycles: 0,
    }
}

#[test]
fn manifest_extracted_from_bare_json_and_fenced() {
    // Bare JSON object.
    let files = extract_file_manifest(
            r#"{"files":[{"path":"index.html","content":"<h1>hi</h1>"},{"path":"game.js","content":"x=1"}]}"#,
        )
        .expect("bare json manifest");
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].path, "index.html");

    // Fenced, with chatter around it.
    let fenced = "Here you go:\n```json\n{\"files\":[{\"path\":\"a.py\",\"content\":\"print(1)\"}]}\n```\nDone.";
    let files = extract_file_manifest(fenced).expect("fenced manifest");
    assert_eq!(files[0].path, "a.py");
}

#[test]
fn channel_delivery_resolution_is_honest_when_unreachable() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("maturana-deliver-{}-{}", std::process::id(), nanos));
    std::fs::create_dir_all(&root).unwrap();
    let home = MaturanaHome::new(root.clone());

    // No live bridges → telegram resolution fails with a clear reason, never panics.
    let err = resolve_channel_delivery(&home, "telegram", None).unwrap_err();
    assert!(err.contains("Telegram"), "got: {err}");

    // A channel with no host-side push path is reported honestly (and the
    // `channel:agent` form still parses to the channel arm).
    let err = resolve_channel_delivery(&home, "slack:claude-firecracker", None).unwrap_err();
    assert!(err.contains("no host-side push destination"), "got: {err}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn prose_is_not_a_manifest() {
    assert!(extract_file_manifest("The answer is 42. Paris has ~2.1M people.").is_none());
    // A JSON object without a files array is prose, not a manifest.
    assert!(extract_file_manifest(r#"{"answer":"42"}"#).is_none());
    // An empty files array is not a deliverable.
    assert!(extract_file_manifest(r#"{"files":[]}"#).is_none());
}

#[test]
fn safe_relative_path_blocks_traversal_and_absolutes() {
    assert_eq!(
        safe_relative_path("a/b.txt").unwrap(),
        PathBuf::from("a/b.txt")
    );
    // Leading slash, .. and . components are stripped — nothing escapes the dir.
    assert_eq!(
        safe_relative_path("/etc/passwd").unwrap(),
        PathBuf::from("etc/passwd")
    );
    assert_eq!(safe_relative_path("../../x").unwrap(), PathBuf::from("x"));
    assert_eq!(
        safe_relative_path("src/./main.rs").unwrap(),
        PathBuf::from("src/main.rs")
    );
    // Nothing usable left.
    assert!(safe_relative_path("../..").is_none());
    assert!(safe_relative_path("").is_none());
}

#[test]
fn out_basename_is_the_last_segment() {
    assert_eq!(
        out_basename("/workspace/maturana-out-run-123"),
        "maturana-out-run-123"
    );
    assert_eq!(
        out_basename("/workspace/maturana-out-run-123/"),
        "maturana-out-run-123"
    );
}

#[test]
fn copy_tree_preserves_layout_and_counts_files() {
    let base = std::env::temp_dir().join(format!("orch-copytree-{}", std::process::id()));
    let src = base.join("src");
    let dst = base.join("dst");
    std::fs::create_dir_all(src.join("css")).unwrap();
    std::fs::write(src.join("index.html"), "<h1>hi</h1>").unwrap();
    std::fs::write(src.join("css/style.css"), "body{}").unwrap();
    assert_eq!(count_files(&src), 2);
    assert_eq!(count_files(&base.join("nope")), 0);

    let mut names = copy_tree(&src, &dst).unwrap();
    names.sort();
    assert_eq!(
        names,
        vec!["css/style.css".to_string(), "index.html".to_string()]
    );
    // The real bytes are copied, not regenerated.
    assert_eq!(
        std::fs::read_to_string(dst.join("index.html")).unwrap(),
        "<h1>hi</h1>"
    );
    assert_eq!(
        std::fs::read_to_string(dst.join("css/style.css")).unwrap(),
        "body{}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn ready_steps_respects_dependencies() {
    let plan = Plan {
        goal: "g".to_string(),
        steps: vec![
            step("s1", &[], StepStatus::Done),
            step("s2", &["s1"], StepStatus::Waiting), // ready: dep done
            step("s3", &["s2"], StepStatus::Waiting), // not ready: dep waiting
        ],
    };
    let ready: Vec<&str> = plan.ready_steps().iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ready, vec!["s2"]);
}

#[test]
fn validate_rejects_cycles_unknown_roles_and_bad_deps() {
    let reg = registry();
    // A -> B -> A cycle.
    let cyclic = Plan {
        goal: "g".to_string(),
        steps: vec![
            step("a", &["b"], StepStatus::Waiting),
            step("b", &["a"], StepStatus::Waiting),
        ],
    };
    assert!(cyclic.validate(&reg).is_err(), "cycle must be rejected");

    // Unknown role.
    let mut bad_role = Plan {
        goal: "g".to_string(),
        steps: vec![step("s1", &[], StepStatus::Waiting)],
    };
    bad_role.steps[0].role = "wizard".to_string();
    assert!(
        bad_role.validate(&reg).is_err(),
        "unknown role must be rejected"
    );

    // Dangling dependency.
    let dangling = Plan {
        goal: "g".to_string(),
        steps: vec![step("s1", &["ghost"], StepStatus::Waiting)],
    };
    assert!(
        dangling.validate(&reg).is_err(),
        "unknown dep must be rejected"
    );

    // A legal linear plan passes.
    let ok = Plan {
        goal: "g".to_string(),
        steps: vec![
            step("s1", &[], StepStatus::Waiting),
            step("s2", &["s1"], StepStatus::Waiting),
        ],
    };
    assert!(ok.validate(&reg).is_ok());
}

#[test]
fn parse_plan_extracts_json_from_prose() {
    let reg = registry();
    let reply = "Sure! Here is the plan:\n```json\n\
            {\"steps\":[{\"id\":\"s1\",\"role\":\"researcher\",\"task\":\"find X\",\"deps\":[],\"review\":false}]}\n\
            ```\nHope that helps.";
    let plan = parse_plan("the goal", reply, &reg).expect("should parse");
    assert_eq!(plan.steps.len(), 1);
    assert_eq!(plan.steps[0].role, "researcher");
    assert_eq!(plan.steps[0].status, StepStatus::Waiting);
}

#[test]
fn dependency_context_includes_upstream_results() {
    let mut plan = Plan {
        goal: "g".to_string(),
        steps: vec![
            step("s1", &[], StepStatus::Done),
            step("s2", &["s1"], StepStatus::Waiting),
        ],
    };
    plan.steps[0].result = Some("the answer is 42".to_string());
    let ctx = plan.dependency_context(&plan.steps[1].clone());
    assert!(ctx.contains("the answer is 42"));
    assert!(ctx.contains("s1"));
}

#[test]
fn review_verdict_is_read_from_markers() {
    assert!(matches!(
        parse_review("looks good [[REVIEW: APPROVE]]"),
        ReviewVerdict::Approve
    ));
    match parse_review("[[REVIEW: REVISE]] fix the title") {
        ReviewVerdict::Revise(fb) => assert_eq!(fb, "fix the title"),
        _ => panic!("expected revise"),
    }
    assert!(matches!(
        parse_review("no marker here"),
        ReviewVerdict::Unclear
    ));
}
