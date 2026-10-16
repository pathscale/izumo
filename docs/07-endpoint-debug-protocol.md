# Endpoint agent-control and diagnostics protocol

One endpoint-libs transport carries two deliberately separate protocol planes.
Each length-delimited payload is JSON-RPC 2.0 using the small MCP wire layer in
endpoint-libs 2.1.5. WebDriver remains a temporary adapter until the existing
acceptance coverage runs through the new agent-control plane.

## Agent control plane

This plane is for an AI agent or automation client operating the product:

- a semantic tree keyed by stable node ids, roles, accessible names, values,
  state, visibility, and bounds;
- node-addressed click, double-click and hover; set-value, scroll-into-view,
  scroll-by, and native key/wheel actions;
- window/app lifecycle including quit and restart with automatic reconnect;
- pushed tree/lifecycle changes for efficient observation.

Agent-control code is included by the runtime's default `agent-control` feature.
On Unix, the interface normally starts disabled; embedders apply their
owner-controlled setting with `apply_runtime_debug_options` after initialization.
The explicit `--blitz-control` CLI rescue starts the socket before setup,
without consulting that setting. Applying a disabled setting drops the socket
and descriptor, including one started by the CLI rescue.

The enabled interface uses a Unix domain socket, endpoint-libs framing, and
JSON-RPC 2.0. Calls use the standard MCP `tools/call` method with the
`blitz.agent.control` tool; pushed state uses JSON-RPC notifications.
There is no HTTP listener, bearer token, or exclusive automation session in this
transport. The separately env-gated legacy controller is described in docs/06.

Use W3C WebDriver/BiDi input semantics and WAI-ARIA/accessibility-tree semantics
where they fit. MCP may expose discovery and coarse application tools, while
Prompt Syntax remains the authored intent/execution layer. Neither MCP nor
Prompt Syntax replaces native input or semantic UI state.

## Diagnostics plane

This plane is for debugging the implementation rather than operating the app:

- DOM/layout/computed-style snapshots behind settled revision barriers;
- console and runtime-error streams;
- renderer queue, invalidation, frame-stage, memory, and revision metrics;
- offscreen RGBA capture for a node or the whole frame, tied to the same
  committed revision as its snapshot.

Diagnostics handlers are compiled only with the `diagnostics` feature and
exposed as the separate `blitz.diagnostics` tool. Intrusive engine collectors
are additionally dormant until the owner enables deep profiling at runtime.
Production agent control therefore does not retain frame, script, DOM, or layout
telemetry unless the second setting is explicitly on.

## Contract

- Without the CLI rescue, the local agent-control listener and descriptor are
  absent until enabled by the owner-controlled runtime setting. Disabling that
  setting removes both.
- Deep profiling can be selected only while inspection/control is enabled;
  disabling either clears retained samples.
- The Unix socket has a uid-only trust model. A mode-0700 parent directory is
  established before binding; the socket and new descriptor are mode 0600.
  Any process running as the same uid can use the exposed interface. There is
  no bearer token, application authentication, or exclusive session.
- The descriptor publishes the Unix socket address, PID, instance id, protocol
  version, renderer, and renderer revision.
- In `blitz-control-protocol` 0.7.5, descriptors are published under
  `std::env::temp_dir()/tauri-blitz-agent/<instance-id>.json`; the socket uses
  the same path with the `.sock` extension. The server does not read
  `TAURI_BLITZ_CONTROL_DESCRIPTOR`.
- Multiple clients may observe concurrently. Mutating requests execute on the
  UI thread.
- The built-in relaunch starts the replacement and exits the old instance.
  Its executable path removes every exact `--blitz-control` argument; its
  macOS bundle path forwards no arguments. A replacement publishes a new
  descriptor only if inspection is enabled again. Clients rediscover the
  intended instance and reconnect; the built-in path does not announce a
  replacement instance id. An embedder lifecycle handler owns its own behavior.
- Native input carries logical key, physical code, modifiers, pointer phases,
  and wheel phases; it enters the same runtime path as device input.
- Semantic activation never turns a selected node id back into a screen
  coordinate for hit-testing. The runtime dispatches pointer-down, mouse-down,
  pointer-up, mouse-up and click directly to that node, preserving propagation
  and default actions even when overflow parks it outside the viewport.
- Snapshots and screenshots are tied to one settled revision set.
- Metrics name real stages: input-to-present, style, layout, scene, submit,
  present, total, queue depth, coalesced invalidations, and resident bytes.

The chrome document and all scripts it executes must be trusted. Remote HTTP
and HTTPS chrome IPC is denied by default. An embedder may explicitly permit
parsed URLs through `set_remote_ipc_policy`; admission grants access to every
registered command, not a per-command capability scope.

## Delivery stages

1. Typed MCP-compatible calls, results, notifications, and endpoint-libs
   framing, with no token or session handshake. Implemented.
2. Local Unix-socket listener, mode-0600 descriptor lifecycle, MCP initialize
   and tool discovery, and multi-client connections. Implemented.
3. UI-thread agent and diagnostics handler boundaries. Implemented. The agent
   plane now extracts a semantic tree with ancestor-aware visibility and drives
   node-addressed click/double-click/hover, set-value, scroll-into-view/by,
   physical key, wheel, and modifier input through `ScriptDocument`'s native
   event path. Relaunch uses LaunchServices
   for a macOS app bundle (and the current executable elsewhere), then exits the
   old instance cleanly. An embedder that registers an agent-control handler owns
   the relaunch lifecycle instead; this lets an application drain state and
   delegate replacement to its own supervisor. Diagnostic snapshots, idle barriers,
   computed styles, frame capture and metrics are connected.
4. Renderer instrumentation and pushed metrics/errors. Implemented for the
   current bounded streams and frame windows.
5. WebDriver compatibility adapter backed by the endpoint protocol; delete the
   old bespoke server after its acceptance suite passes unchanged.

The live consumer audit that defined this contract found the old layer could
not inject Command-modified keys or wheel input, treated descendants of
`display:none` retained views as displayed, exposed revision counters without
real timings, allowed only one session, and lost its environment during an
AppKit/LaunchServices relaunch. Each is an acceptance case for this protocol.
