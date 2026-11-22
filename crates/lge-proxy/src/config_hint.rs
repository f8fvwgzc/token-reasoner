//! The "Configuration entry" column of the page: what to add to Codex's
//! `config.toml` to turn a group of tools off. Advice only: lge never writes
//! the file, and every entry changes what the agent can do. Entries were
//! checked against Codex 0.160.0 (source commit 9f2b874); a key that no
//! version is known to read is never invented, so a tool without a switch gets
//! an empty cell.
use std::path::Path;

/// Tools of the multi-agent groups: version 2 (namespace `collaboration`) and
/// version 1 (`multi_agent_v1`).
const MULTI_AGENT_TOOLS: [&str; 9] = [
    "spawn_agent",
    "wait_agent",
    "send_message",
    "followup_task",
    "interrupt_agent",
    "list_agents",
    "send_input",
    "resume_agent",
    "close_agent",
];

/// A group needs this many of the tools above to count as sub-agent tools
/// (the namespace is configurable, so the tools tell, not the group's name).
const MULTI_AGENT_MIN_MATCHES: usize = 3;

const AGENTS_KEY: &str = "[agents] enabled = false";
const AGENTS_NOTE: &str = "turns off every sub-agent tool: the agent can no longer spawn or message sub-agents (Codex desktop parallel agents); top-level key, not available in a profile; applies to new sessions only";
const ASK_KEY: &str = "[tools.experimental_request_user_input] enabled = false";
const ASK_NOTE: &str =
    "the agent can no longer ask you structured questions (Plan mode choices); applies to new sessions only";

fn is_tool(name: &str, wanted: &str) -> bool {
    // A tool is named by its namespace and its name, joined without a separator.
    name == wanted || name.ends_with(wanted)
}

fn is_multi_agent_group(tool_names: &[String]) -> bool {
    MULTI_AGENT_TOOLS
        .iter()
        .filter(|wanted| tool_names.iter().any(|n| is_tool(n, wanted)))
        .count()
        >= MULTI_AGENT_MIN_MATCHES
}

/// The configuration entry that turns off `tool` (or, with `tool` `None`, the
/// whole group) and what it does, or an empty string when no switch is known.
/// `tool_names`: every tool of the group. `home`: Codex's home folder, read
/// (never written) for an MCP server's entry.
pub fn config_hint(group: &str, tool: Option<&str>, mcp: bool, tool_names: &[String], home: Option<&Path>) -> String {
    if mcp {
        return mcp_hint(group, home);
    }
    if is_multi_agent_group(tool_names) {
        let in_group = match tool {
            None => true,
            Some(t) => MULTI_AGENT_TOOLS.iter().any(|w| is_tool(t, w)),
        };
        if in_group {
            return format!("{AGENTS_KEY} ({AGENTS_NOTE})");
        }
    }
    match tool {
        Some(t) if is_tool(t, "request_user_input") => format!("{ASK_KEY} ({ASK_NOTE})"),
        _ => String::new(),
    }
}

/// An MCP server's entry: its own table when `config.toml` has one, else the
/// plugin whose `.mcp.json` declares it; nothing when neither is found.
fn mcp_hint(server: &str, home: Option<&Path>) -> String {
    // Without a home folder there is nothing to check against: the usual table.
    let Some(home) = home else {
        return format!("[mcp_servers.{server}]");
    };
    if config_declares(home, server) {
        return format!("[mcp_servers.{server}]");
    }
    match plugin_declaring(home, server) {
        Some(plugin) => format!(
            "[plugins.\"{plugin}\"] enabled = false (plugin-provided; this also turns off the plugin's other tools; applies to new sessions only)"
        ),
        None => String::new(),
    }
}

fn config_declares(home: &Path, server: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(home.join("config.toml")) else {
        return false;
    };
    let Ok(value) = text.parse::<toml::Table>() else {
        return false;
    };
    value
        .get("mcp_servers")
        .and_then(|t| t.as_table())
        .is_some_and(|t| t.contains_key(server))
}

/// `<plugin>@<marketplace>` of the plugin under `plugins/cache/<marketplace>/<plugin>/<version>/`
/// whose `.mcp.json` declares `server`.
fn plugin_declaring(home: &Path, server: &str) -> Option<String> {
    let cache = home.join("plugins").join("cache");
    let dirs = |p: &Path| -> Vec<std::path::PathBuf> {
        let mut v: Vec<_> = std::fs::read_dir(p)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        v.sort();
        v
    };
    for market in dirs(&cache) {
        for plugin in dirs(&market) {
            for version in dirs(&plugin) {
                let Ok(text) = std::fs::read_to_string(version.join(".mcp.json")) else {
                    continue;
                };
                let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                let servers = json.get("mcpServers").unwrap_or(&json);
                if servers.get(server).is_some() {
                    let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().to_string());
                    return Some(format!("{}@{}", name(&plugin)?, name(&market)?));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn sub_agent_tools_are_found_by_their_names_and_turn_off_with_the_agents_key() {
        let tools = names(&[
            "collaborationspawn_agent",
            "collaborationwait_agent",
            "collaborationsend_message",
            "collaborationfollowup_task",
            "collaborationinterrupt_agent",
            "collaborationlist_agents",
        ]);
        let group = config_hint("collaboration", None, false, &tools, None);
        assert!(group.contains("[agents] enabled = false"), "{group}");
        assert!(group.contains("new sessions only"));
        let one = config_hint("collaboration", Some("collaborationsend_message"), false, &tools, None);
        assert!(one.contains("[agents] enabled = false"), "{one}");
        // The namespace is configurable: the tools tell, not the name.
        assert!(config_hint("team", None, false, &tools, None).contains("[agents]"));
        // A group with one tool of that name is not the sub-agent group.
        assert_eq!(
            config_hint("tools", None, false, &names(&["send_message", "exec_command"]), None),
            ""
        );
    }

    #[test]
    fn the_structured_question_tool_has_its_own_switch_and_a_clock_has_none() {
        let built_in = names(&["exec_command", "request_user_input", "write_stdin"]);
        assert!(
            config_hint("built-in", Some("request_user_input"), false, &built_in, None)
                .contains("[tools.experimental_request_user_input] enabled = false")
        );
        assert_eq!(config_hint("built-in", None, false, &built_in, None), "");
        assert_eq!(
            config_hint(
                "clock",
                Some("clockcurr_time"),
                false,
                &names(&["clockcurr_time", "clocksleep"]),
                None
            ),
            ""
        );
    }

    #[test]
    fn a_built_in_tool_without_a_known_switch_gets_no_invented_key() {
        let built_in = names(&["exec_command", "write_stdin", "apply_patch"]);
        for tool in &built_in {
            assert_eq!(config_hint("built-in", Some(tool), false, &built_in, None), "");
        }
        assert_eq!(config_hint("built-in", None, false, &built_in, None), "");
    }

    #[test]
    fn an_mcp_server_is_named_by_its_table_in_config_toml_or_by_the_plugin_that_declares_it() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path();
        std::fs::write(path.join("config.toml"), "[mcp_servers.db]\ncommand = \"db-mcp\"\n").unwrap();
        assert_eq!(config_hint("db", None, true, &[], Some(path)), "[mcp_servers.db]");
        assert_eq!(
            config_hint("cua_repl", None, true, &[], Some(path)),
            "",
            "declared nowhere"
        );
        let plugin = path.join("plugins/cache/m/p/1");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join(".mcp.json"),
            r#"{"mcpServers":{"cua_repl":{"command":"x"}}}"#,
        )
        .unwrap();
        let hint = config_hint("cua_repl", Some("cua_repljs"), true, &[], Some(path));
        assert!(hint.starts_with("[plugins.\"p@m\"] enabled = false"), "{hint}");
        assert!(hint.contains("other tools"));
        assert_eq!(
            config_hint("db", None, true, &[], None),
            "[mcp_servers.db]",
            "no home folder to check"
        );
        // The file is only read.
        assert_eq!(
            std::fs::read_to_string(path.join("config.toml")).unwrap(),
            "[mcp_servers.db]\ncommand = \"db-mcp\"\n"
        );
    }
}
