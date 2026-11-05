use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::stream::{SseEvent, StreamContext};
use crate::kiro::model::events::{Event, ToolUseEvent};

fn context(mapping: HashMap<String, String>) -> StreamContext {
    let mut context =
        StreamContext::new_with_thinking("test-model", 1, false, mapping, HashSet::new());
    context.generate_initial_events();
    context
}

fn tool(id: &str, name: &str, input: &str, stop: bool) -> Event {
    Event::ToolUse(ToolUseEvent {
        name: name.to_string(),
        tool_use_id: id.to_string(),
        input: input.to_string(),
        stop,
    })
}

fn tool_index(events: &[SseEvent], id: &str) -> Value {
    events
        .iter()
        .find(|event| event.data["content_block"]["id"] == id)
        .unwrap()
        .data["index"]
        .clone()
}

fn input_for(events: &[SseEvent], index: &Value) -> Value {
    let input: String = events
        .iter()
        .filter(|event| {
            event.data["index"] == *index && event.data["delta"]["type"] == "input_json_delta"
        })
        .map(|event| event.data["delta"]["partial_json"].as_str().unwrap())
        .collect();
    serde_json::from_str(&input).unwrap()
}

#[test]
fn raw_interleaved_tool_fragments_keep_ids_names_and_json_boundaries() {
    let mut context = context(HashMap::from([(
        "short_tool".to_string(),
        "original_long_tool".to_string(),
    )]));
    let mut events =
        context.process_kiro_event(&tool("first", "Write", "{\"content\":\"中", false));
    events.extend(context.process_kiro_event(&tool(
        "second",
        "short_tool",
        "{\"enabled\":tr",
        false,
    )));
    let first = tool_index(&events, "first");
    let second = tool_index(&events, "second");
    assert_ne!(first, second);
    assert!(
        events
            .iter()
            .any(|event| event.data["content_block"]["name"] == "original_long_tool")
    );
    events.extend(context.process_kiro_event(&tool("first", "", "文\\n\\\"quoted\\\"\"}", true)));
    events.extend(context.process_kiro_event(&tool("second", "", "ue}", true)));
    events.extend(context.generate_final_events());
    assert_eq!(
        input_for(&events, &first),
        json!({"content":"中文\n\"quoted\""})
    );
    assert_eq!(input_for(&events, &second), json!({"enabled":true}));
    for index in [first, second] {
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event == "content_block_stop" && event.data["index"] == index)
                .count(),
            1
        );
    }
    assert_eq!(events.last().unwrap().event, "message_stop");
}

#[test]
fn legacy_mapped_tool_waits_for_rewrite_and_restores_client_keys() {
    let mut context = context(HashMap::from([(
        "fs_write".to_string(),
        "Write".to_string(),
    )]));
    let first = context.process_kiro_event(&tool(
        "mapped",
        "fs_write",
        "{\"path\":\"/example\",\"text\":\"hello",
        false,
    ));
    assert!(
        !first
            .iter()
            .any(|event| event.data["delta"]["type"] == "input_json_delta")
    );
    let mut events = context.process_kiro_event(&tool("mapped", "", " world\"}", true));
    events.extend(context.generate_final_events());
    let index = tool_index(&events, "mapped");
    assert_eq!(
        input_for(&events, &index),
        json!({"file_path":"/example","content":"hello world"})
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.data["delta"]["type"] == "input_json_delta")
            .count(),
        1
    );
}

#[test]
fn streamed_invalid_or_truncated_tool_never_signals_completion() {
    for end in [None, Some("not-json")] {
        let mut context = context(HashMap::new());
        let mut events =
            context.process_kiro_event(&tool("broken", "PowerShell", "{\"command\":", false));
        let index = tool_index(&events, "broken");
        if let Some(end) = end {
            events.extend(context.process_kiro_event(&tool("broken", "", end, true)));
        }
        events.extend(context.generate_final_events());
        assert!(
            !events
                .iter()
                .any(|event| event.event == "content_block_stop" && event.data["index"] == index)
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.event.as_str(), "message_delta" | "message_stop"))
        );
        assert_eq!(
            events.last().unwrap().data["error"]["type"],
            "upstream_tool_json_error"
        );
    }
}

#[test]
fn transport_failure_does_not_complete_streamed_tool_arguments() {
    let mut context = context(HashMap::new());
    let mut events =
        context.process_kiro_event(&tool("broken", "PowerShell", "{\"command\":\"wait", false));
    let index = tool_index(&events, "broken");
    events.extend(context.generate_error_events("api_error", "upstream disconnected"));
    assert!(
        !events
            .iter()
            .any(|event| event.event == "content_block_stop" && event.data["index"] == index)
    );
    assert_eq!(events.last().unwrap().event, "error");
}

#[test]
fn refusal_after_partial_tool_does_not_complete_or_execute_the_call() {
    let mut context = context(HashMap::new());
    let mut events =
        context.process_kiro_event(&tool("refused", "PowerShell", "{\"command\":\"wait", false));
    let index = tool_index(&events, "refused");
    events.extend(context.process_kiro_event(&Event::Metadata(
        serde_json::from_value(json!({"stopReason":"CONTENT_FILTERED"})).unwrap(),
    )));
    events.extend(context.generate_final_events());
    assert!(
        !events
            .iter()
            .any(|event| event.event == "content_block_stop" && event.data["index"] == index)
    );
    assert!(!events.iter().any(|event| event.event == "message_stop"));
    assert_eq!(
        events.last().unwrap().data["error"]["type"],
        "permission_error"
    );
}

#[test]
fn tool_output_estimate_is_independent_of_fragment_boundaries() {
    let mut context = context(HashMap::new());
    let input = "{\"value\":\"abcd\"}";
    for (position, character) in input.chars().enumerate() {
        context.process_kiro_event(&tool(
            "counted",
            "custom_tool",
            &character.to_string(),
            position + 1 == input.len(),
        ));
    }
    context.generate_final_events();
    assert_eq!(context.resolved_output_tokens(), 4);
}

#[test]
fn late_tool_name_and_empty_stop_preserve_all_input_once() {
    let mut context = context(HashMap::new());
    assert!(
        context
            .process_kiro_event(&tool("late", "", "{\"value\":", false))
            .is_empty()
    );
    let mut events = context.process_kiro_event(&tool("late", "custom_tool", "123}", false));
    let index = tool_index(&events, "late");
    events.extend(context.process_kiro_event(&tool("late", "", "", true)));
    events.extend(context.generate_final_events());
    assert_eq!(input_for(&events, &index), json!({"value":123}));
    assert_eq!(
        events
            .iter()
            .filter(|event| event.data["delta"]["type"] == "input_json_delta")
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event == "content_block_stop" && event.data["index"] == index)
            .count(),
        1
    );
}
