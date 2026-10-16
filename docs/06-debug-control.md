# Debug control contract

The current inspection interface is the local Unix domain socket server owned by
`blitz-control-protocol`. It carries endpoint-libs framed JSON-RPC 2.0 and exposes
MCP discovery and tools. It is not a loopback HTTP or WebDriver endpoint.

The WebDriver requirements and measured Stage 1.5 result below describe the
earlier acceptance contract. They are historical context, not the current socket
wire protocol or authentication model. See [the endpoint protocol](07-endpoint-debug-protocol.md)
and [runtime debug settings](08-runtime-debug-settings.md) for the current interface.

## Process boundary

```text
Local MCP client
      |
      | Unix domain socket, endpoint-libs framing, JSON-RPC 2.0
      v
blitz-control-protocol server on nagoya
      |
      | typed request and response hand-off
      v
UI/runtime thread -> Boa -> DOM/events -> style/layout -> paint
```

The socket server must not access Boa, DOM, layout, or paint state directly.
Requests are handed to the owning UI/runtime thread. Multiple clients may
connect; mutations execute on the UI thread without an exclusive automation
session.

The runtime's default `agent-control` Cargo feature includes this interface on
Unix. Compilation alone does not start it. Embedders normally enable it through
`set_agent_control_enabled` or `apply_runtime_debug_options` after loading owner
settings. The explicit `--blitz-control` CLI rescue starts it before setup,
without consulting those settings. Applying a disabled setting stops it.
The built-in relaunch path does not forward the rescue flag.

The `diagnostics` feature adds diagnostic capabilities. Intrusive profiling
requires its separate runtime permission. See docs/08 for the complete settings
contract.

## Discovery and uid-only trust

For `blitz-control-protocol` 0.7.5, discovery descriptors are published at:

```text
std::env::temp_dir()/tauri-blitz-agent/<instance-id>.json
```

The socket uses the same path with the `.sock` extension. The descriptor
contains `protocolVersion`, `pid`, `instanceId`, `address`, `renderer`, and
`rendererRevision`. The address is `unix://<socket-path>` and protocol version
is 2. There is no token field.

The parent directory is mode 0700 before the socket is bound. The socket and
new descriptor are mode 0600. Access relies on filesystem permissions and the
user identity, with no bearer token, application authentication, or exclusive
session. Any process running as the same uid can connect and use the exposed
tools. This boundary does not isolate applications running as the same user.

A client connects to the Unix socket, performs MCP initialization and tool
discovery, and calls `blitz.agent.control` or, when available,
`blitz.diagnostics` through `tools/call`. It must use endpoint-libs framing;
HTTP requests and WebDriver session creation do not apply.

The descriptor is written after the listener is bound. Dropping the server
removes its socket and descriptor. Clients must handle stale discovery files
and validate the intended process and instance when reconnecting. Version 0.7.5
does not read `TAURI_BLITZ_CONTROL_DESCRIPTOR`; do not rely on that variable to
select a stable descriptor path.

Control can drive trusted chrome and thereby invoke application commands.
Diagnostics can expose scripts, DOM, console data, and screenshots. Enable
inspection only when that access is intended. The chrome document and every
script it executes must be trusted. HTTP and HTTPS chrome IPC is denied unless
the embedder explicitly permits its URL through `set_remote_ipc_policy`.

## Separate legacy HTTP controller

This checkout still contains a separate compatibility controller:
`run()` calls `blitz_script::DebugController::start_from_env` when the
`debug-control` feature is compiled. It uses `TAURI_BLITZ_DRIVER`,
`TAURI_BLITZ_DRIVER_DESCRIPTOR`, and `TAURI_BLITZ_DRIVER_TOKEN`.
The `diagnostics` feature also includes `debug-control`.

That env-gated HTTP controller is distinct from the Unix socket interface and
its runtime setting. Its token and WebDriver session behavior must not be
attributed to `blitz-control-protocol`, and disabling the socket setting is not
a promise that this separately configured controller is disabled.

## Historical minimum WebDriver surface

The earlier WebDriver adapter contract required W3C response envelopes and
error names. Unsupported commands were required to return
`unsupported operation`, not a successful empty response.

Minimum commands:

- status; create and delete session
- current URL and page source
- find one or many elements by CSS selector
- element text, attribute, property, rectangle, displayed state, and enabled state
- element click, focus, and keyboard input
- semantic pointer and keyboard actions
- synchronous and asynchronous JavaScript execution
- screenshot
- window handles, current window, and window selection

Element references contain both the Blitz node ID and a document generation. Removing or
replacing the node makes the reference stale. Never silently retarget a stale reference to
a new node that reused the same numeric ID.

Clicks and keys must enter through Blitz's input/event path. A semantic click is addressed by
node id and dispatches pointer-down, mouse-down, pointer-up, mouse-up and click to that target;
it neither calls JavaScript `element.click()` nor asks coordinate hit-testing to choose the node
again. That preserves propagation bugs while making overflowed controls addressable.

## Historical Blitz diagnostic extensions

The earlier WebDriver adapter specified these vendor-prefixed commands and
`/session/{session id}/blitz/...` HTTP routes. These are not socket routes or
MCP tool names.

| Command | Result |
|---|---|
| `blitz:waitForIdle` | Drain runnable work and commit a frame; return revision counters |
| `blitz:getDomSnapshot` | Node IDs, tree, text, attributes, and document revision |
| `blitz:getLayoutTree` | Node IDs, boxes, clipping, scroll offsets, and layout revision |
| `blitz:getComputedStyle` | Computed values for one node and style revision |
| `blitz:getConsoleEntries` | Ordered console records after a supplied sequence number |
| `blitz:getRuntimeErrors` | Ordered uncaught JS and host errors with stacks |
| `blitz:traceEvent` | Dispatch path, phases, targets, listeners, and propagation outcome |
| `blitz:getRendererMetrics` | Queue sizes, revisions, frame timing, and last paint status |

Console entries and runtime errors live in bounded ring buffers with monotonically increasing
sequence numbers. Report overflow explicitly so a client knows records were lost. JavaScript
errors include source name, line/column when available, Boa backtrace, and the DOM and paint
revisions at which the error escaped.

Polling these ordered buffers is sufficient for the first gate. Do not implement WebDriver
BiDi or a custom WebSocket until a real need for pushed events appears.

## Historical settled-frame acceptance contract

`blitz:waitForIdle` is the reliability primitive. A successful response means:

1. The triggering command has completed.
2. Immediately runnable Boa promise jobs and microtasks have drained.
3. Currently due timers have run.
4. One animation-frame turn has run.
5. Resulting style and layout work has completed.
6. A CPU paint has committed.
7. No new synchronous mutation appeared during the final observation turn.

Do not wait for future timers or require repeating timers to disappear. Apply both a time
budget and work-count budget. If the document keeps mutating or a queue does not drain,
return a timeout with queue sizes and the last revisions rather than claiming the page
is idle.

Every successful response carries the relevant counters:

```json
{
  "documentRevision": 42,
  "styleRevision": 19,
  "layoutRevision": 19,
  "paintRevision": 19
}
```

A screenshot and its associated DOM/layout snapshot must be captured behind the same barrier
and identify the same committed paint revision. This prevents a client from comparing pixels
from one frame with semantic state from another.

## Historical Stage 1.5 acceptance gate

Use the Stage 1 Solid probe as the fixture. The earlier HTTP/WebDriver gate used the Stage 1 Solid probe from a separate
process. Its port, token, status, and session checks below apply only to that
historical transport:

1. Launch with a temporary descriptor path and an OS-selected port.
2. Wait for the descriptor, validate its PID, and call `/status`.
3. Verify a missing or incorrect token cannot create a session.
4. Create a session and confirm advertised capabilities and protocol version.
5. Find the counter button with a CSS selector and read its initial text and rectangle.
6. Click it through the real input path, call `blitz:waitForIdle`, and verify the signal,
   effect log, DOM snapshot, and screenshot all agree.
7. Exercise `<Show>` and `<For>` and verify delegated event trace includes `document`.
8. Trigger a deliberate JavaScript exception and retrieve its message and stack.
9. Delete the session, reconnect, and repeat a read-only command.
10. Terminate the app and verify the descriptor is removed or detected as stale.

Pass only when the test is repeatable without sleeps. Timeouts wait on explicit readiness,
revision, and queue conditions; fixed delays are not an acceptable synchronization strategy.

This gate originally blocked Stage 2 until the debugging channel worked
reliably. It is not evidence that the current socket transport has passed
equivalent acceptance coverage.

### Historical measured result (2026-08-09)

The following records the earlier headless HTTP/WebDriver result. Its token
checks and transport claims do not describe the current Unix socket server.

**PASS.** Blitz commits `80881334`, `17b2350f`, `a79b9ba7`, and `30ffb7b9` implement and
verify the transport boundary, runtime diagnostics, input tracing, standard commands, and the
complete separate-process acceptance gate:

- loopback-only HTTP, random 256-bit token, atomic 0600 descriptor, bounded request size and
  response timeout, one serialized renderer queue, and clean shutdown;
- W3C status/session envelopes, token rejection, session deletion, and reconnect;
- current URL, page source, CSS element lookup, text, rectangle, pointer-path click, DOM
  snapshot, idle/layout barrier, and CPU PNG screenshot;
- process-unique node identities so a reused slab index cannot silently retarget an old element
  reference;
- a separate-process Solid test driven entirely by descriptor readiness and protocol replies,
  with no fixed sleeps;
- synchronous and timer-backed asynchronous JavaScript execution, bounded ordered console
  records, and bounded ordered uncaught-error records with a retained Boa stack;
- pointer hit-test tracing with the full node chain through the document, plus focused text
  entry through Blitz's IME/input event path;
- attributes, properties, displayed/enabled state, focus, window selection, computed style,
  layout tree, renderer metrics, viewport pointer actions, and keyboard actions;
- full Solid transitions through signals, effect logs, `<Show>`, and `<For>`, with DOM,
  layout, style, paint, and changed PNG evidence correlated after explicit idle barriers;
- detached reference rejection, deliberate exception retrieval, session reconnect, clean
  process shutdown, and descriptor removal.

The test uses no fixed sleeps. It passed with Python unavailable, alongside all existing DOM,
Preact, Solid, transport, and doctests. This remains a headless Blitz result, not a Tauri
runtime result, but Stage 2 may now begin.

## Historical growth guidance

Keep the Stage 1.5 surface small. Add network inspection, CSS rule provenance, accessibility
trees, memory/GC statistics, pushed events, or interactive JavaScript breakpoints only in
response to a demonstrated debugging need. Protocol additions are versioned capabilities so
old clients fail clearly rather than misreading new responses.
