use std::path::Path;

const FAKE_CONFIG: &str = r#"{
  "name": "demo",
  "gitlab": { "url": "https://gitlab.com", "projectIds": [1] },
  "host": { "type": "fake" },
  "provider": { "type": "fake" },
  "defaultPool": "ci",
  "pools": [
    { "tag": "ci", "image": "example.invalid/node:24", "stages": ["lint", "test"] },
    { "tag": "build", "image": "example.invalid/kaniko:1", "stages": ["build"], "maxRuns": 2 }
  ],
  "cache": { "type": "s3" }
}"#;

async fn ciburst(dir: &Path, command: &[&str]) -> (i32, String) {
    let config = dir.join("config.json");
    let local = dir.join("local");
    let mut args = vec![
        "ciburst".to_string(),
        "--config".to_string(),
        config.display().to_string(),
        "--local".to_string(),
        local.display().to_string(),
        "--skip-gitlab".to_string(),
    ];
    args.extend(command.iter().map(|s| s.to_string()));
    let mut out = Vec::new();
    let code = ciburst::run(args, &mut out).await.unwrap_or_else(|error| {
        out.extend_from_slice(error.to_string().as_bytes());
        1
    });
    (code, String::from_utf8(out).unwrap())
}

#[tokio::test]
async fn dry_run_is_idempotent_and_destroy_asks_for_the_name() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.json"), FAKE_CONFIG).unwrap();

    let (code, out) = ciburst(dir.path(), &["plan"]).await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("create  project demo"), "{out}");
    assert!(out.contains("create  job definition demo-build"), "{out}");
    assert!(
        out.contains("7 to create, 0 to update, 0 to delete, 0 unchanged"),
        "{out}"
    );

    let (code, out) = ciburst(dir.path(), &["apply"]).await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("outputs: "), "{out}");
    let outputs = std::fs::read_to_string(dir.path().join("local").join("outputs.env")).unwrap();
    assert!(outputs.contains("CIBURST_DISPATCH_URL=https://demo-dispatch.fake.invalid/"));
    assert!(outputs.contains("CIBURST_HOOK_TOKEN="));

    let (code, out) = ciburst(dir.path(), &["plan"]).await;
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("0 to create, 0 to update, 0 to delete, 7 unchanged"),
        "{out}"
    );

    let (code, out) = ciburst(dir.path(), &["status"]).await;
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("dispatcher: https://demo-dispatch.fake.invalid/"),
        "{out}"
    );
    assert!(out.contains("pool build: 0 live of 2"), "{out}");

    let (code, out) = ciburst(dir.path(), &["destroy"]).await;
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("--yes demo"), "{out}");

    let (code, out) = ciburst(dir.path(), &["destroy", "--yes", "demo"]).await;
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("destroyed demo"), "{out}");
    let outputs = std::fs::read_to_string(dir.path().join("local").join("outputs.env")).unwrap();
    assert!(outputs.is_empty());

    let (code, out) = ciburst(dir.path(), &["rotate", "everything"]).await;
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("runner, hook, key, cache"), "{out}");
}

#[tokio::test]
async fn reports_unknown_modules_and_bad_config() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.json"),
        FAKE_CONFIG.replace(
            r#""provider": { "type": "fake" }"#,
            r#""provider": { "type": "moon" }"#,
        ),
    )
    .unwrap();
    let (code, out) = ciburst(dir.path(), &["plan"]).await;
    assert_eq!(code, 1);
    assert!(out.contains("no provider named moon"), "{out}");

    std::fs::write(
        dir.path().join("config.json"),
        FAKE_CONFIG.replace(r#""defaultPool": "ci""#, r#""defaultPool": "nope""#),
    )
    .unwrap();
    let (code, out) = ciburst(dir.path(), &["plan"]).await;
    assert_eq!(code, 1);
    assert!(out.contains("defaultPool: nope is not a pool"), "{out}");

    std::fs::write(
        dir.path().join("config.json"),
        FAKE_CONFIG.replace(r#""maxRuns": 2"#, r#""maxRuns": 2, "vcpu": 64"#),
    )
    .unwrap();
    let (code, out) = ciburst(dir.path(), &["plan"]).await;
    assert_eq!(code, 1);
    assert!(out.contains("above fake maximum 6"), "{out}");
}

#[tokio::test]
async fn help_and_usage_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mut out = Vec::new();
    let code = ciburst::run(vec!["ciburst".into(), "--help".into()], &mut out)
        .await
        .unwrap();
    assert_eq!(code, 0);
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("Serverless per-job GitLab runners")
    );
    let (code, _) = ciburst(dir.path(), &["frobnicate"]).await;
    assert_eq!(code, 2);
}
