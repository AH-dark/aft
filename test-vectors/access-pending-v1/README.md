# `access_pending` tool-provider vector v1

`worker-read-outside.json` is captured from a real stamped worker `read` call whose target is outside that worker's worktree. The test submits the call through the subc tool-provider route: it runs through `run_tool_call` and its response-finalization hook, consent turns the held access denial into `access_pending`, and the final response is formatted and serialized into the provider frame. The test compares the emitted body bytes with this file after replacing only the volatile values below.

## Fields in the wire body

- `content`: the generic MCP content array. This example contains one text block.
- `content[0].type`: `text`, the block type.
- `content[0].text`: agent-visible pending text. The reusable text pattern is `Waiting for permission to {operation} {path}. The request has gone to your head. End your turn now; you will be resumed with the answer, and then you should run this same call again.` A provider should substitute its operation and path while preserving the wording.
- `isError`: `true`, because this is a terminal error-shaped reply while permission is pending.
- `structuredContent`: AFT's first-party structured response payload; generic MCP hosts consume `content` and `isError` instead.
- `structuredContent.id`: AFT's response/request identifier. `${REQUEST_ID}` replaces the volatile value (`subc-41-7` in the capture).
- `structuredContent.success`: `false`, the AFT response success flag.
- `structuredContent.code`: `access_pending`, the machine-readable error code. There is no separate top-level `errorCode` field in this wire reply.
- `structuredContent.message`: AFT's error message, with the same pending text pattern as the content block.
- `structuredContent.ask`: AFT-specific access-grant data describing a filesystem operation; another provider should not copy this body as-is.
- `structuredContent.ask.path`: `${HEAD_ROOT}/shared.txt` replaces the temporary absolute filesystem path. It is volatile and appears in the text as well as in the grant.
- `structuredContent.ask.kind`: `file`, the AFT grant's filesystem object kind.
- `structuredContent.ask.mode`: `read`, the requested filesystem operation.
- `structuredContent.text`: the rendered agent-facing text, duplicating the text block and message in this capture.

## What another provider should reuse

The provider-neutral contract shown here is `isError: true`, the `access_pending` code, and the pending text pattern above. AFT's `ask` grant is provider-specific filesystem context, and the structured AFT response fields are not required for another provider's own tool-result protocol.

This reply contains **no `elicitation_id`**. A model or caller cannot obtain an ID to use for resumption from this tool response. AFT's internal Cingulate elicitation ID is not included on the tool-provider wire; do not add one to this vector or infer one from the request ID. There are no timestamps in the captured reply.
