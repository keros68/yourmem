#[test]
fn message_text_that_names_the_new_format_marker_is_not_a_marker() {
    let raw = r#"{"type":"event_msg","payload":{"type":"user_message","message":"response_item"}}"#;
    let out = yourmem::adapters::codex::parse_lines(&[(1, raw.into())], false);
    assert!(!out.saw_response_items);
    assert_eq!(out.messages.len(), 1);
}

#[test]
fn apply_patch_emits_each_file_and_rename_with_native_call_id() {
    let patch="*** Begin Patch\n*** Update File: src/old.rs\n*** Move to: src/new.rs\n*** Add File: README.md\n*** Delete File: obsolete.txt\n*** End Patch";
    let raw=serde_json::json!({"type":"response_item","payload":{"type":"function_call","name":"apply_patch","call_id":"call-patch-1","arguments":patch}}).to_string();
    let out = yourmem::adapters::codex::parse_lines(&[(7, raw)], false);
    assert_eq!(out.artifacts.len(), 3);
    assert_eq!(out.artifacts[0].operation, "rename");
    assert_eq!(out.artifacts[0].from_path.as_deref(), Some("src/old.rs"));
    assert_eq!(out.artifacts[0].path, "src/new.rs");
    assert_eq!(out.artifacts[1].operation, "create");
    assert_eq!(out.artifacts[2].operation, "delete");
    assert!(out
        .artifacts
        .iter()
        .all(|a| a.native_tool_call_id.as_deref() == Some("call-patch-1") && a.line_no == 7));
}
