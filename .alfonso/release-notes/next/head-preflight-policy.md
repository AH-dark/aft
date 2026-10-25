## Fixes

- Worker-plan catalog preflights no longer save routing or confinement policies on head scopes. Head routes ignore old worker-policy rows, preventing false access-policy disagreements that could refuse codemode calls. Worker-scope persistence and confinement remain unchanged.
