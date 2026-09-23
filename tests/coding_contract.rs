use orbit::coding_agent::completion_instructions;

#[test]
fn coding_contract_requires_feedback_without_claiming_verification() {
    let contract = completion_instructions("/workspace", &["shell".into()]);
    for expectation in [
        "inspect your resulting changes",
        "applicable project validation, build, and test commands",
        "Repair failures caused by your changes",
        "If validation cannot be performed, report that explicitly",
        "independent Orbit validation remains authoritative",
        "isolated Git repository at the pinned baseline",
        "provided terminal/shell tool",
        "within this workspace",
    ] {
        assert!(contract.contains(expectation), "missing: {expectation}");
    }
    for language_specific in ["cargo", "pytest", "npm", "go test", "Antigravity", "Codex"] {
        assert!(!contract.contains(language_specific));
    }
}

#[test]
fn coding_contract_does_not_advertise_ungranted_terminal() {
    let contract = completion_instructions("/orbit/home/workspace", &["read_file".into()]);
    assert!(contract.contains("Terminal execution is unavailable"));
    assert!(!contract.contains("You can execute repository-local commands"));
    assert!(contract.contains("/orbit/home/workspace"));
}

#[test]
fn codex_base_instructions_use_the_shared_completion_contract() {
    let tools = vec!["read_file".into(), "write_file".into(), "shell".into()];
    let thread = orbit::codex_bridge::thread_start(
        orbit::codex_bridge::CODEX_VERSION,
        "fixture-model",
        Some("high"),
        std::path::Path::new("/orbit/home"),
        &tools,
    )
    .unwrap();
    assert!(
        thread["baseInstructions"]
            .as_str()
            .unwrap()
            .starts_with(&completion_instructions(
                orbit::acp_runtime::WORKSPACE,
                &tools
            ))
    );
    assert_eq!(thread["environments"], serde_json::json!([]));
    assert_eq!(thread["dynamicTools"].as_array().unwrap().len(), 3);
}
