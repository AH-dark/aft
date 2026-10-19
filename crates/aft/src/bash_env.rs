use std::collections::HashMap;

pub(crate) const BLOCKED_ENV_VARS: &[&str] = &[
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "BASH_ENV",
    "ENV",
    "IFS",
    "PATH",
];

pub(crate) fn is_blocked_env_key(key: &str) -> bool {
    BLOCKED_ENV_VARS.iter().any(|blocked| {
        #[cfg(windows)]
        {
            key.eq_ignore_ascii_case(blocked)
        }
        #[cfg(not(windows))]
        {
            key == *blocked
        }
    }) || is_preexec_hijack_env_key(key)
}

pub(crate) fn first_blocked_env_key(env: &HashMap<String, String>) -> Option<&str> {
    env.keys()
        .find(|key| is_blocked_env_key(key))
        .map(String::as_str)
}

/// Startup hooks that execute code before a command can enter its sandbox.
/// These names must be rejected for user-configured and per-call environments.
pub(crate) fn is_preexec_hijack_env_key(key: &str) -> bool {
    #[cfg(windows)]
    let upper_key = key.to_ascii_uppercase();
    #[cfg(windows)]
    let key = upper_key.as_str();

    if key.starts_with("LD_") || key.starts_with("DYLD_") || key.starts_with("BASH_FUNC_") {
        return true;
    }
    matches!(
        key,
        "BASH_ENV"
            | "ENV"
            | "SHELLOPTS"
            | "BASHOPTS"
            | "PROMPT_COMMAND"
            | "PS4"
            | "IFS"
            | "PYTHONSTARTUP"
            | "PYTHONPATH"
            | "PYTHONHOME"
            | "PERL5OPT"
            | "PERL5LIB"
            | "PERLLIB"
            | "PERL5DB"
            | "RUBYOPT"
            | "RUBYLIB"
            | "NODE_OPTIONS"
            | "GCONV_PATH"
            | "LOCPATH"
            | "NLSPATH"
            | "HOSTALIASES"
            | "RESOLV_HOST_CONF"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_environment_rejects_both_blocklists() {
        for key in ["PATH", "LD_CUSTOM", "BASH_FUNC_echo%%", "NODE_OPTIONS"] {
            assert!(is_blocked_env_key(key), "{key}");
        }
        assert!(!is_blocked_env_key("TZ"));
    }
}
