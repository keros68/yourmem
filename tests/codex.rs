
#[test]
fn message_text_that_names_the_new_format_marker_is_not_a_marker() {
    let raw = r#"{"type":"event_msg","payload":{"type":"user_message","message":"response_item"}}"#;
    let out = yourmem::adapters::codex::parse_lines(&[(1, raw.into())], false);
    assert!(!out.saw_response_items);
    assert_eq!(out.messages.len(), 1);
}
