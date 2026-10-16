# Notes for the next release

- 0.60.1: Worker file tools can read Cargo registry sources and Rust toolchains without permission cards; writes, credential/cache paths, permanent deny floors, and shell confinement are unchanged.
- Permissions: grep, outline, zoom, callgraph, AST search, inspect and OpenCode glob now recognize exact session-owned bash output without OpenCode's external-directory prompts or Pi's root-restriction denials; directory-wide searches, other sessions' output and mutations stay protected.
- Daemon health: scheduler contention now preserves the last measured census and timestamp, marks it stale with an increasing age, and reports unmeasured startup metrics as null instead of empty/zero/available. The health sentinel treats stale or unknown census data as an instrument failure without losing watcher baselines or disabling live liveness checks.
- Bash compression: command lists with top-level separators (`cargo test && cargo clippy`, `git add . ; git commit`) now use generic compression instead of the head command's specialized compressor. Previously the specialized compressor silently deleted every later command's output from the response on success — 26.65% of real bash traffic (7-day sample) was affected. Chained commands are now more verbose but complete.
- aft_outline / aft_zoom: signature lines no longer carry the redundant visibility/kind prefix (`E fn `) — the signature text itself is the information. A minimal `E` marker remains only where exported-ness is not visible in the signature (e.g. TypeScript export lists, Go uppercase exports). ~5.5% mean output reduction on outline, compounding on zoom menus.
- apply_patch: updates no longer eat one terminal newline from files ending with two or more.
- Windows LSP: percent-escaped file URIs (paths with spaces, non-ASCII) now decode correctly, so diagnostics/goto/rename work on those paths.
- edit: empty-string mode sentinels sent by some hosts no longer reject valid edits[] calls (issue #171).
- Workflow hints ship inside the host's system entry — fixes "System message must be at the beginning" on Qwen-family endpoints (issue #150).
- Checkpoints under restrict_to_project_root now snapshot a symlink itself rather than its target.
- Callgraph stores sweep orphaned build temporaries (age-based, both layouts).
- OpenCode 2 now requires 2.0.29 or newer. Permission prompts run in-process through OpenCode's own permission check, including on standalone hosts and hosts without a readable server password, and edit prompts keep the built-in diff view. The `opencode.server_url` and `opencode.server_password_env` settings are removed; `npx @cortexkit/aft doctor --fix` deletes them.
