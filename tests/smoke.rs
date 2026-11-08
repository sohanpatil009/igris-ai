//! Week-1 smoke tests (plan.md 1.7) plus regression coverage for the Week-1
//! structured-results/approval work (plan.md 1.1/1.3/1.4):
//! - registry builds and executes a ReadOnly tool (`system_info`)
//! - `read_file` is sandboxed by `PathPolicy` (component-wise, not string-prefix)
//! - Sensitive tools stay gated behind a single-use approval
//! - registry-level JSON schema validation (unknown + missing args)

use std::path::PathBuf;
use std::sync::Arc;

use igris_ai::security::{AuditLog, PermissionPolicy};
use igris_ai::tools::default_registry;
use igris_ai::tools::{PathPolicy, ReadFileTool, Tool, ToolErrorKind, ToolRegistry};

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("opencode")
        .join(format!("igris-smoke-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn test_registry(tag: &str) -> ToolRegistry {
    let base = scratch_dir(tag);
    let policy = Arc::new(PermissionPolicy::load_from(&base));
    let audit = Arc::new(AuditLog::load_from(&base));
    default_registry(policy, audit)
}

#[tokio::test]
async fn registry_builds_and_runs_system_info() {
    let registry = test_registry("registry");

    let names = registry.names();
    assert!(names.contains(&"system_info".to_string()));
    assert!(names.contains(&"read_file".to_string()));

    let out = registry
        .execute("system_info", &serde_json::json!({}))
        .await
        .expect("system_info should succeed");
    assert!(out
        .message
        .contains(&format!("os={}", std::env::consts::OS)));
}

#[tokio::test]
async fn read_file_is_sandboxed() {
    // Build the literal `foo` vs `foo2` sibling case the component-wise check
    // must reject and a naive string-prefix check would accept.
    let parent = scratch_dir("paths");
    let foo = parent.join("foo");
    let foo2 = parent.join("foo2");
    std::fs::create_dir_all(&foo).expect("mkdir foo");
    std::fs::create_dir_all(&foo2).expect("mkdir foo2");
    let inside = foo.join("hello.txt");
    let sibling = foo2.join("secret.txt");
    std::fs::write(&inside, "igris").expect("write inside file");
    std::fs::write(&sibling, "secret").expect("write sibling file");

    let tool = ReadFileTool::new(PathPolicy::from_roots(vec![foo.clone()]));

    // Inside the sandbox -> returns the file content.
    let out = tool
        .execute(&serde_json::json!({ "path": inside.display().to_string() }))
        .await
        .expect("read inside sandbox should succeed");
    assert!(out.message.contains("igris"));

    // `..` traversal -> denied up front.
    let traversal = foo.join("..").join("foo2").join("secret.txt");
    let err = tool
        .execute(&serde_json::json!({ "path": traversal.display().to_string() }))
        .await
        .expect_err("traversal must be denied");
    assert_eq!(err.kind, ToolErrorKind::InvalidArgs);
    // All path denials share one message — no existence oracle for the model.
    assert!(err.message.contains("path denied"), "{}", err.message);

    // Sibling directory with a shared string prefix -> denied.
    let err = tool
        .execute(&serde_json::json!({ "path": sibling.display().to_string() }))
        .await
        .expect_err("sibling directory must be denied");
    assert!(err.message.contains("path denied"), "{}", err.message);
}

#[tokio::test]
async fn registry_rejects_unknown_and_missing_args() {
    let registry = test_registry("schema");

    // Unknown argument rejected (schema declares `query` only).
    let err = registry
        .execute("web_search", &serde_json::json!({ "queryx": "igris" }))
        .await
        .expect_err("unknown arg must be rejected");
    assert_eq!(err.kind, ToolErrorKind::InvalidArgs);
    assert!(err.message.contains("unknown argument"));

    // Missing required argument rejected.
    let err = registry
        .execute("web_search", &serde_json::json!({}))
        .await
        .expect_err("missing required arg must be rejected");
    assert_eq!(err.kind, ToolErrorKind::InvalidArgs);
    assert!(err.message.contains("missing required argument"));

    // Wrong primitive type rejected.
    let err = registry
        .execute("web_search", &serde_json::json!({ "query": 42 }))
        .await
        .expect_err("wrong type must be rejected");
    assert_eq!(err.kind, ToolErrorKind::InvalidArgs);
    assert!(err.message.contains("must be of type string"));

    // Optional args declared in the schema pass (system_info accepts `info`).
    registry
        .execute("system_info", &serde_json::json!({ "info": "os" }))
        .await
        .expect("declared optional arg must be accepted");
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn sensitive_tool_requires_single_use_approval() {
    let registry = test_registry("approval");

    // Ungranted Sensitive tool -> NeedsApproval with a pending id, no run.
    let err = registry
        .execute("shell_run", &serde_json::json!({ "command": "whoami" }))
        .await
        .expect_err("ungranted Sensitive tool must not run");
    assert_eq!(err.kind, ToolErrorKind::NeedsApproval);
    let id = err.pending_id.expect("pending id must be present");
    assert_eq!(registry.list_pending().len(), 1);

    // Approve -> executes once (single-use), message from whoami output.
    let out = registry.approve(&id).await.expect("approve should run it");
    assert!(!out.message.is_empty());
    assert!(registry.list_pending().is_empty());

    // Approving the same id again -> gone (single-use).
    let err = registry
        .approve(&id)
        .await
        .expect_err("second approve must fail");
    assert_eq!(err.kind, ToolErrorKind::NotFound);

    // Re-running still requires a fresh approval; deny clears it.
    let err = registry
        .execute("shell_run", &serde_json::json!({ "command": "whoami" }))
        .await
        .expect_err("re-run must require approval again");
    let second_id = err.pending_id.expect("new pending id");
    assert_ne!(second_id, id);
    assert!(registry.deny(&second_id));
    assert!(registry.list_pending().is_empty());
}

#[cfg(target_os = "windows")]
#[tokio::test]
async fn denied_pending_action_audits_deny() {
    let dir = scratch_dir("deny-audit");
    let registry = {
        let policy = Arc::new(PermissionPolicy::load_from(&dir));
        let audit = Arc::new(AuditLog::load_from(&dir));
        default_registry(policy, audit)
    };
    registry
        .execute("shell_run", &serde_json::json!({ "command": "whoami" }))
        .await
        .expect_err("needs approval");
    let id = registry
        .list_pending()
        .first()
        .map(|a| a.id.clone())
        .expect("pending present");
    assert!(registry.deny(&id));

    // The deny decision and reason must be observable in the audit log —
    // the test's claim is only proven by reading it back.
    let log = std::fs::read_to_string(dir.join("audit.log")).expect("audit log written");
    assert!(log.contains("\"decision\":\"deny\""), "log: {log}");
    assert!(log.contains("rejected by user"), "log: {log}");
}

#[tokio::test]
async fn shell_run_metachar_and_control_char_blocklist() {
    let registry = test_registry("shellblock");
    // The blocklist runs *inside* tool execution, so grant `shell_run` to
    // reach it; forbidden commands must fail before any process spawns.
    registry.grant("shell_run");

    let bad_commands = [
        "echo hi\nwhoami", // newline chaining must not run a second command
        "echo hi\rwhoami", // CR chaining
        "echo %PATH%",     // env expansion re-parses after any check
        "echo hi^x",       // cmd escape lever
        "echo !delayed!",  // delayed-expansion lever
        "dir \u{0000}x",   // NUL
    ];
    for command in bad_commands {
        let err = registry
            .execute("shell_run", &serde_json::json!({ "command": command }))
            .await
            .expect_err("forbidden command must be rejected");
        assert_eq!(
            err.kind,
            ToolErrorKind::InvalidArgs,
            "expected InvalidArgs for {command:?}"
        );
        assert!(
            err.message.contains("unsafe shell metacharacters"),
            "message for {command:?}: {}",
            err.message
        );
        assert!(
            registry.list_pending().is_empty(),
            "rejected args must never mint pending approvals"
        );
    }
}

#[tokio::test]
async fn alias_names_resolve_to_real_tools() {
    let registry = test_registry("alias");

    // `search_web` (router alias) normalizes to `web_search`; the ungranted
    // Standard tool surfaces NeedsApproval — proof the alias lookup hit
    // rather than "unknown tool".
    let err = registry
        .execute("search_web", &serde_json::json!({ "query": "igris" }))
        .await
        .expect_err("ungranted web_search must ask for approval");
    assert_eq!(err.kind, ToolErrorKind::NeedsApproval);
    assert!(registry.deny(&err.pending_id.expect("pending id")));

    // Case-insensitive: normalization lowercases the raw input.
    let err = registry
        .execute("WEB_SEARCH", &serde_json::json!({ "query": "igris" }))
        .await
        .expect_err("ungranted web_search must ask for approval");
    assert_eq!(err.kind, ToolErrorKind::NeedsApproval);
    assert!(registry.deny(&err.pending_id.expect("pending id")));

    // `Close_App` (router alias) normalizes to the now-registered
    // `app_control`: it must resolve and mint a pending approval rather than
    // \"unknown tool\".
    let err = registry
        .execute("Close_App", &serde_json::json!({}))
        .await
        .expect_err("Close_App must resolve to app_control and need approval");
    assert_eq!(err.kind, ToolErrorKind::NeedsApproval);
    assert!(registry.deny(&err.pending_id.expect("pending id")));
}
