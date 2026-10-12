//! The names a host registers AFT's command tool and patch tool under.
//!
//! The engine renders reply text that tells the agent which tool to call next
//! ("Prefer `read` tool over bash.", "use apply_patch for this destructive
//! change"). Most hosts register those two tools as `bash` and `apply_patch`,
//! but OpenCode 2 registers them as `shell` and `patch`, the names its own
//! timeline renders natively.
//!
//! The names belong to the caller, not to the project: one engine process (the
//! subc daemon's module, or a standalone bridge) serves every head and session
//! bound to a root, and an OpenCode 1 head, an OpenCode 2 session and a Pi
//! session can all be bound to the same root at once. So the names travel on
//! each request, in the `host_tool_names` envelope field beside `session_id`
//! and `worker_session`, set by the plugin and never taken from the agent's
//! arguments. A request without the field gets the default names, so every
//! host that does not send it renders exactly the text it always did.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The default names, used by every host that does not say otherwise.
pub const DEFAULT_SHELL_TOOL: &str = "bash";
pub const DEFAULT_PATCH_TOOL: &str = "apply_patch";

/// Host-facing names for AFT's command tool (engine command `bash`) and patch
/// tool (engine command `apply_patch`). Only reply text uses them; the engine
/// commands keep their own names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostToolNames {
    pub bash: String,
    pub apply_patch: String,
}

impl Default for HostToolNames {
    fn default() -> Self {
        Self {
            bash: DEFAULT_SHELL_TOOL.to_string(),
            apply_patch: DEFAULT_PATCH_TOOL.to_string(),
        }
    }
}

/// Whether `name` is a tool name OpenCode 1 and 2 accept for a registration:
/// at most 64 characters, an ASCII letter first, then ASCII letters, digits,
/// `_` or `-`.
fn valid_tool_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    name.len() <= 64
        && first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Request envelope field that carries the caller's tool names; see the
/// module documentation.
pub const HOST_TOOL_NAMES_FIELD: &str = "host_tool_names";

impl HostToolNames {
    /// Parse the `host_tool_names` envelope field of one request.
    ///
    /// Absent means the default names. The field is an object whose keys are
    /// the engine command names (`bash`, `apply_patch`); a key it leaves out
    /// keeps that tool's default name. Any other key, a non-string value, or
    /// a name the hosts would refuse to register is an error.
    pub fn from_request_field(value: Option<&Value>) -> Result<Self, String> {
        let mut names = Self::default();
        let Some(value) = value else {
            return Ok(names);
        };
        let Some(object) = value.as_object() else {
            return Err("host_tool_names must be an object".to_string());
        };
        for (key, entry) in object {
            let slot = match key.as_str() {
                "bash" => &mut names.bash,
                "apply_patch" => &mut names.apply_patch,
                other => {
                    return Err(format!(
                        "host_tool_names has unknown key `{other}`; expected `bash` or `apply_patch`"
                    ));
                }
            };
            let Some(name) = entry.as_str() else {
                return Err(format!("host_tool_names.{key} must be a string"));
            };
            if !valid_tool_name(name) {
                return Err(format!(
                    "host_tool_names.{key} is not a valid tool name: {name:?}"
                ));
            }
            *slot = name.to_string();
        }
        Ok(names)
    }

    /// The name the agent calls AFT's command tool by.
    pub fn shell(&self) -> &str {
        &self.bash
    }

    /// The name the agent calls AFT's patch tool by.
    pub fn patch(&self) -> &str {
        &self.apply_patch
    }

    /// Rewrite the hashline steering sentences that tell the agent to fall back
    /// to the patch tool, so they name the tool the host registered.
    ///
    /// The steering text is a fixed table shared with the transport contract,
    /// so it is rendered with the default name and renamed here, at the one
    /// place a rejection becomes a reply. Only the phrase "use apply_patch" is
    /// a reference to the tool; the rest of the sentence is left untouched.
    pub fn name_patch_tool_in(&self, text: &str) -> String {
        if self.apply_patch == DEFAULT_PATCH_TOOL {
            return text.to_string();
        }
        text.replace(
            &format!("use {DEFAULT_PATCH_TOOL}"),
            &format!("use {}", self.apply_patch),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn absent_parameter_keeps_the_default_names() {
        let names = HostToolNames::from_request_field(None).unwrap();
        assert_eq!(names.shell(), "bash");
        assert_eq!(names.patch(), "apply_patch");
    }

    #[test]
    fn opencode_2_names_are_accepted() {
        let names = HostToolNames::from_request_field(Some(
            &json!({"bash": "shell", "apply_patch": "patch"}),
        ))
        .unwrap();
        assert_eq!(names.shell(), "shell");
        assert_eq!(names.patch(), "patch");
    }

    #[test]
    fn a_missing_key_keeps_that_tools_default() {
        let names = HostToolNames::from_request_field(Some(&json!({"bash": "shell"}))).unwrap();
        assert_eq!(names.shell(), "shell");
        assert_eq!(names.patch(), "apply_patch");
    }

    #[test]
    fn malformed_parameters_are_refused() {
        for bad in [
            json!("shell"),
            json!({"shell": "shell"}),
            json!({"bash": 1}),
            json!({"bash": ""}),
            json!({"bash": "has space"}),
            json!({"bash": "9lives"}),
        ] {
            assert!(
                HostToolNames::from_request_field(Some(&bad)).is_err(),
                "{bad} should be refused"
            );
        }
    }

    fn request(params: serde_json::Value) -> crate::protocol::RawRequest {
        serde_json::from_value(params).expect("raw request")
    }

    #[test]
    fn a_request_reads_its_own_envelope_field() {
        let names = json!({"bash": "shell", "apply_patch": "patch"});
        let top = request(json!({"id": "1", "command": "bash", "host_tool_names": names}));
        assert_eq!(top.host_tool_names().shell(), "shell");
        assert!(top.sent_host_tool_names().is_some());
        // The bridge nests a bash request's arguments under `params`.
        let nested =
            request(json!({"id": "2", "command": "bash", "params": {"host_tool_names": names}}));
        assert_eq!(nested.host_tool_names().patch(), "patch");
        let absent = request(json!({"id": "3", "command": "bash"}));
        assert_eq!(absent.host_tool_names(), HostToolNames::default());
        assert!(absent.sent_host_tool_names().is_none());
        // A bad value only changes wording, so it falls back instead of failing.
        let bad = request(json!({"id": "4", "command": "bash", "host_tool_names": "shell"}));
        assert_eq!(bad.host_tool_names(), HostToolNames::default());
    }

    #[test]
    fn patch_steering_is_renamed_only_for_a_non_default_name() {
        let steer = "enable backups or use apply_patch for this destructive change";
        assert_eq!(HostToolNames::default().name_patch_tool_in(steer), steer);
        let v2 = HostToolNames {
            bash: "shell".into(),
            apply_patch: "patch".into(),
        };
        assert_eq!(
            v2.name_patch_tool_in(steer),
            "enable backups or use patch for this destructive change"
        );
    }
}
