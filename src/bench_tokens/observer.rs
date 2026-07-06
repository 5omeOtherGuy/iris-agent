//! Shared benchmark instrumentation. The `BenchObserver` (rich per-run metrics)
//! and `ZeroPromptGate` (auto-preset approval gate that must never be consulted)
//! now live in the non-test `crate::harness` façade so the real-provider harness
//! and this `#[cfg(test)]` replay bench share ONE definition. This module just
//! re-exports them plus keeps the observer's unit test.

pub(crate) use crate::harness::{BenchObserver, ZeroPromptGate};

#[cfg(test)]
mod tests {
    use super::BenchObserver;
    use crate::nexus::{AgentEvent, ToolCall};
    use serde_json::json;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "c".to_string(),
            thought_signature: None,
            name: name.to_string(),
            arguments: json!({}),
        }
    }

    #[test]
    fn observer_captures_extended_schema() {
        use crate::nexus::AgentObserver;
        let obs = BenchObserver::default();
        obs.on_event(AgentEvent::ToolStarted(call("grep"))).unwrap();
        obs.on_event(AgentEvent::ToolResult {
            call: call("grep"),
            content: "hello".to_string(),
            exit_code: None,
            duration: None,
        })
        .unwrap();
        obs.on_event(AgentEvent::ToolStarted(call("bash"))).unwrap();
        obs.on_event(AgentEvent::ToolResult {
            call: call("bash"),
            content: "boom".to_string(),
            exit_code: Some(2),
            duration: None,
        })
        .unwrap();
        obs.on_event(AgentEvent::ToolAutoApprovedDangerous(call("bash")))
            .unwrap();
        obs.on_event(AgentEvent::ToolError {
            call: call("edit"),
            message: "old_string not found".to_string(),
        })
        .unwrap();

        assert_eq!(obs.tool_sequence.borrow().as_slice(), &["grep", "bash"]);
        // "hello" (5) + "boom" (4) = 9 result bytes into context.
        assert_eq!(obs.tool_result_bytes.get(), 9);
        assert_eq!(obs.tool_result_bytes_by_tool.borrow()["bash"], 4);
        assert_eq!(obs.bash_exit_codes.borrow().as_slice(), &[2]);
        assert_eq!(obs.dangerous_approvals.get(), 1);
        assert_eq!(obs.tool_errors.borrow().len(), 1);
        assert_eq!(obs.tool_errors.borrow()[0].0, "edit");
    }
}
