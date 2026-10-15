# Unreleased

## Sandbox profiles

- Replace the single stamped-worker shell jail with `sandbox.profile` role choices: `off`, `relaxed`, and `strict`. Stamped workers default to `relaxed`; heads, standalone and unstamped sessions default to `off`. Projects may only harden a profile; untrusted/MCP/unverified routes are always strict.
- macOS relaxed shells allow ordinary caches, sockets, browsers, toolchains and shared compile servers while protecting credentials, CortexKit private data, known sibling checkouts, shell/Git configuration and hook governance. Strict keeps the existing jail and sccache block. The short worker TMPDIR, governed `gh` shim, co-author hooks and stripped child environment remain in place.
- Linux relaxed reports every deny as unenforced because Landlock cannot subtract paths from default-allow grants. Windows names relaxed/strict as not enforced and refuses those launches. Seatbelt still cannot execute setuid `ps`/`top`; use `pgrep`/`pkill`, or select `off` when `ps` is required.
- Retire `sandbox.enabled` through the existing in-memory and `doctor --fix` JSONC migration: `true` maps to strict for both roles, `false` maps to relaxed worker/off head, and explicit canonical role values win. An absent setting uses the same new defaults; heads that explicitly enabled the old jail remain confined.
