//! Contract check against the same Swift renderer used by the native helper.
#![cfg(target_os = "macos")]
#![allow(dead_code)]
use gofin_domain::{AuthorizationChallenge, PreparedOperation, approval_review::approval_review};
use std::{
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    sync::OnceLock,
};

fn renderer() -> &'static PathBuf {
    static RENDERER: OnceLock<PathBuf> = OnceLock::new();
    RENDERER.get_or_init(|| {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let executable =
            std::env::temp_dir().join(format!("gofin-approval-contract-{}", std::process::id()));
        let output = Command::new("xcrun")
            .arg("swiftc")
            .arg(root.join("native/ApprovalPresentation.swift"))
            .arg(root.join("native/ApprovalPresentationTests.swift"))
            .arg("-o")
            .arg(&executable)
            .output()
            .expect("run Swift compiler");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        executable
    })
}

pub fn assert_prepared_renders(prepared: &PreparedOperation) -> String {
    let review =
        approval_review(prepared).unwrap_or_else(|e| panic!("{} review: {e}", prepared.operation));
    assert_review_renders(
        review,
        "financial_write",
        &prepared.operation,
        gofin_domain::authorization_binding(prepared),
        &prepared.summary,
    )
}

pub fn assert_review_renders(
    review: gofin_domain::approval_review::ApprovalReview,
    purpose: &str,
    operation: &str,
    binding: serde_json::Value,
    summary: &str,
) -> String {
    let challenge = AuthorizationChallenge {
        review: Some(review),
        purpose: purpose.into(),
        operation_id: operation.into(),
        binding,
        binding_hash: String::new(),
        summary: summary.into(),
        nonce: "synthetic".into(),
        expires_at: "2099-01-01T00:00:00Z".into(),
    };
    let envelope =
        gofin_authorization::helper_challenge(&challenge).expect("production helper challenge");
    let binding = envelope["canonicalBinding"]
        .as_str()
        .expect("canonical binding");
    let mut child = Command::new(renderer())
        .arg("--render-binding")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start Swift renderer");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(binding.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "Swift renderer failed for {}",
        operation
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result["status"],
        "ok",
        "{}: review {}, result {}",
        operation,
        challenge
            .review
            .as_ref()
            .map(|r| serde_json::to_string(r).unwrap())
            .unwrap(),
        result
    );
    result["reason"]
        .as_str()
        .expect("rendered reason")
        .to_owned()
}
