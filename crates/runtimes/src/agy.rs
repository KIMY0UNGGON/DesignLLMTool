use std::fs;
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;

use designllm_bridge::catalog::{tools_for, Scope};
use designllm_contracts::{DltError, DltResult, ErrorCode};
use serde_json::{json, Value};

use crate::attempt::{read_bounded_line, MAX_LINE_BYTES};
use crate::invocation::{LaunchSpec, MCP_SERVER_NAME};

// ACP updates can embed a rendered image and the accompanying tool result.
const MAX_ACP_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

pub(crate) struct AgyProfile {
    pub directory: PathBuf,
    name: String,
    log: PathBuf,
    tools: Vec<String>,
}

impl AgyProfile {
    pub fn create() -> DltResult<Self> {
        let name = format!("designllm-{}", designllm_contracts::new_id());
        let directory = std::env::temp_dir().join(&name);
        fs::create_dir(&directory)?;
        let tools = tools_for(Scope::JobBound).into_iter().map(|tool| format!("mcp_{MCP_SERVER_NAME}_{}", tool.name)).collect();
        let profile = Self { log: directory.join("agy.log"), directory, name, tools };
        fs::create_dir_all(profile.directory.join(".agents/agents"))?;
        // A JSON list is also valid YAML within the agent frontmatter.
        let tools = json!(profile.tools);
        fs::write(profile.agent_path(), format!(
            "---\nname: {}\ndescription: Edit a DesignLLMTool workspace through its job-bound MCP tools.\nmainAgent: true\nsubagent: false\ncommandExecutionPolicy: \"off\"\ntools: {tools}\n---\n\nUse only the DesignLLMTool MCP tools supplied to this job.\n", profile.name
        ))?;
        Ok(profile)
    }

    pub fn args(&self) -> [String; 4] {
        ["--agent".into(), self.name.clone(), "--log-file".into(), self.log.to_string_lossy().into_owned()]
    }

    fn agent_path(&self) -> PathBuf { self.directory.join(".agents/agents").join(format!("{}.md", self.name)) }

    fn require_selected(&self) -> DltResult<()> {
        let mut log = String::new();
        let selected = fs::File::open(&self.log).and_then(|file| file.take(1024 * 1024).read_to_string(&mut log)).is_ok()
            && log.lines().any(|line| line.contains("Creating new cascade trajectory (agentScript=true)"))
            && !log.lines().any(|line| line.contains("falling back to default") && line.contains(&self.name));
        if !selected { return Err(DltError::unsupported("AGY did not confirm the DesignLLMTool tool policy")); }
        Ok(())
    }
}

impl Drop for AgyProfile {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.agent_path());
        let _ = fs::remove_file(&self.log);
        let _ = fs::remove_dir(self.directory.join(".agents/agents"));
        let _ = fs::remove_dir(self.directory.join(".agents"));
        let _ = fs::remove_dir(&self.directory);
    }
}

pub(crate) fn run_turn(
    mut input: impl BufRead,
    mut output: impl Write,
    spec: &LaunchSpec,
    profile: &AgyProfile,
    prompt: &str,
    mut progress: impl FnMut(&str),
    mut model_confirmed: impl FnMut(&str),
) -> DltResult<()> {
    let mut request = |id, method, params| {
        exchange(&mut input, &mut output, id, method, params, &profile.tools, &mut progress)
    };
    let hello = request(1, "initialize", json!({
        "protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {"name": "DesignLLMTool", "version": env!("CARGO_PKG_VERSION")}
    }))?;
    if hello["protocolVersion"] != 1 {
        return Err(DltError::new(ErrorCode::ProtocolMismatch, "AGY did not accept ACP version 1"));
    }
    let session = request(2, "session/new", json!({
        "cwd": profile.directory, "mcpServers": [{"name": MCP_SERVER_NAME, "command": spec.mcp.program, "args": spec.mcp.args, "env": []}]
    }))?;
    profile.require_selected()?;
    let session_id = session["sessionId"].as_str().ok_or_else(|| DltError::unsupported("AGY did not return an ACP session"))?;
    if session.pointer("/modes/currentModeId").and_then(Value::as_str) != Some("default") {
        request(3, "session/set_mode", json!({"sessionId": session_id, "modeId": "default"}))?;
    }
    for (id, key, value) in [(4, "model", &spec.request.model), (5, "thought_level", &spec.request.effort)] {
        let configured = request(id, "session/set_config_option", json!({"sessionId": session_id, "configId": key, "value": value}))?;
        let exact = configured["configOptions"].as_array().is_some_and(|options| {
            options.iter().any(|option| option["id"] == key && option["currentValue"].as_str() == Some(value))
        });
        if !exact { return Err(DltError::unsupported(format!("AGY did not confirm the requested {key}"))); }
    }
    model_confirmed(&spec.request.model);
    let reply = request(6, "session/prompt", json!({
        "sessionId": session_id, "prompt": [{"type":"text", "text":prompt}]
    }))?;
    if reply["stopReason"].as_str() != Some("end_turn") {
        return Err(DltError::new(ErrorCode::Internal, "AGY did not finish the turn"));
    }
    Ok(())
}

fn send(output: &mut impl Write, value: Value) -> DltResult<()> {
    writeln!(output, "{value}")?;
    output.flush()?;
    Ok(())
}

fn exchange(input: &mut impl BufRead, output: &mut impl Write, id: u32, method: &str, params: Value, tools: &[String], progress: &mut impl FnMut(&str)) -> DltResult<Value> {
    send(output, json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}))?;
    while let Some(line) = read_bounded_line(input, MAX_ACP_MESSAGE_BYTES)? {
        if line.len() > MAX_ACP_MESSAGE_BYTES {
            return Err(DltError::new(ErrorCode::ProtocolMismatch, "AGY ACP message exceeded 32 MiB"));
        }
        let message: Value = serde_json::from_str(&line).map_err(|_| DltError::new(ErrorCode::ProtocolMismatch, "AGY returned invalid ACP JSON"))?;
        if line.len() <= MAX_LINE_BYTES {
            progress(&line);
        } else {
            progress(&json!({"method":message["method"], "id":message["id"], "message_bytes":line.len()}).to_string());
        }
        if let Some(server_method) = message["method"].as_str() {
            if let Some(server_id) = message.get("id") {
                let reply = if server_method == "session/request_permission" {
                    json!({"result":{"outcome":permission(&message["params"], tools)}})
                } else {
                    json!({"error":{"code":-32601,"message":"Client method unavailable"}})
                };
                let mut reply = reply;
                reply["jsonrpc"] = json!("2.0");
                reply["id"] = server_id.clone();
                send(output, reply)?;
            }
        } else if message["id"] == id {
            return message.get("result").cloned().ok_or_else(|| {
                let code = message.pointer("/error/code").and_then(Value::as_i64).unwrap_or_default();
                DltError::unsupported(format!("AGY rejected {method} (ACP {code})"))
            });
        }
    }
    Err(DltError::new(ErrorCode::Unavailable, format!("AGY closed while waiting for {method}")))
}

fn permission(params: &Value, tools: &[String]) -> Value {
    let own_mcp = params.pointer("/toolCall/name").and_then(Value::as_str)
        .is_some_and(|name| tools.iter().any(|tool| tool == name));
    let needed = if own_mcp { "allow_once" } else { "reject_once" };
    params["options"].as_array().into_iter().flatten().find_map(|option| {
        (option["kind"] == needed).then(|| option["optionId"].as_str()).flatten()
    }).map_or_else(|| json!({"outcome":"cancelled"}), |id| json!({"outcome":"selected","optionId":id}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_is_once_and_scoped_to_the_job_mcp() {
        let mut request = json!({"toolCall":{"name":"mcp_designllm_artifact_inspect"},"options":[
            {"kind":"allow_always","optionId":"forever"}, {"kind":"allow_once","optionId":"allow"},
            {"kind":"reject_once","optionId":"deny"}
        ]});
        let tools = ["mcp_designllm_artifact_inspect".to_owned()];
        assert_eq!(permission(&request, &tools)["optionId"], "allow");
        request["toolCall"]["name"] = json!("run_command");
        assert_eq!(permission(&request, &tools)["optionId"], "deny");
        request["toolCall"]["name"] = json!("mcp_designllm_job_start");
        assert_eq!(permission(&request, &tools)["optionId"], "deny");
    }

    #[test]
    fn a_large_tool_update_does_not_interrupt_the_turn() {
        let update = json!({"jsonrpc":"2.0", "method":"session/update", "params":{"update":{
            "sessionUpdate":"tool_call_update", "image":"a".repeat(MAX_LINE_BYTES + 1)
        }}});
        let mut input = std::io::Cursor::new(format!("{update}\n{{\"jsonrpc\":\"2.0\",\"id\":6,\"result\":{{\"stopReason\":\"end_turn\"}}}}\n"));
        let mut output = Vec::new();
        let mut progress = Vec::new();
        let result = exchange(&mut input, &mut output, 6, "session/prompt", json!({}), &[], &mut |line| progress.push(line.len())).unwrap();
        assert_eq!(result["stopReason"], "end_turn");
        assert_eq!(progress.len(), 2);
        assert!(progress.iter().all(|&len| len < 256));
    }

    #[test]
    fn the_generic_agent_cannot_receive_the_job_prompt() {
        let profile = AgyProfile::create().unwrap();
        let definition = fs::read_to_string(profile.agent_path()).unwrap();
        assert!(definition.contains("mcp_designllm_artifact_inspect"));
        assert!(!definition.contains("run_command"));
        assert!(profile.require_selected().is_err());
        fs::write(&profile.log, "Creating new cascade trajectory (agentScript=false)\n").unwrap();
        assert!(profile.require_selected().is_err());
        fs::write(&profile.log, "Creating new cascade trajectory (agentScript=true)\n").unwrap();
        assert!(profile.require_selected().is_ok());
    }
}
