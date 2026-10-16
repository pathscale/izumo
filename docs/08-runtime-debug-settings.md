# Runtime debug settings

Blitz applications ship two independent, owner-controlled settings:

1. **Enable inspection and agent control** publishes the local MCP Unix socket
   and discovery descriptor. Filesystem permissions restrict access by uid;
   any process running as that uid can use the interface. It enables
   `blitz.agent.control` and the non-intrusive parts of `blitz.diagnostics`
   when compiled.
2. **Enable deep intrusive profiling** activates performance-affecting engine
   clocks, counters, maps, locks, and bounded sample retention. It is for a
   deliberately captured trace, not normal operation.

The two-boolean contract is defined by `ps-blitz-traits::profiling::DebugOptions`
so Tauri and non-Tauri embedders use the same dependency rule.
`izumo` re-exports it as `RuntimeDebugOptions` and applies its own
socket lifecycle with one call:

```rust
izumo::apply_runtime_debug_options(
    izumo::RuntimeDebugOptions {
        inspection_and_agent_control: settings.inspection_enabled,
        deep_intrusive_profiling: settings.deep_profiling_enabled,
    },
)?;
```

Each embedder owns only the UI, persistence, and its local socket lifecycle for
those two booleans. Embedders must not reproduce the dependency
rule. `DebugOptions::effective_deep_profiling` guarantees that collection is
ineffective when inspection/control is off. `apply_runtime_debug_options` also
clears retained samples whenever profiling stops; non-Tauri boundaries must do
the same for the collectors they use.

## Build and runtime contract

- Compile `izumo/diagnostics` into a build that may need a user
  trace. Compilation makes the capability available; it does not activate it.
- Both persisted settings default to false. Without `--blitz-control`, disabled
  inspection creates no agent-control listener, socket, descriptor, or serving
  task. The CLI rescue explicitly starts that interface before setup without
  consulting persisted settings; applying inspection off stops it.
- The built-in relaunch does not forward `--blitz-control`. The replacement
  must enable inspection again through its normal settings or a fresh explicit
  launch argument.
- The socket uses a mode-0700 parent directory and mode-0600 socket and new
  descriptor. There is no bearer token or application authentication.
- These settings govern the Unix socket interface. A separately configured
  legacy HTTP controller may still start through `TAURI_BLITZ_DRIVER`,
  `TAURI_BLITZ_DRIVER_DESCRIPTOR`, and `TAURI_BLITZ_DRIVER_TOKEN` when
  `debug-control` is compiled. See docs/06.
- The deep switch is read once at coarse engine boundaries such as a frame,
  document resolve, or script poll. A disabled section must not read clocks,
  lock sample stores, allocate labels, update maps, or retain samples.
- Inner attribution hooks may observe a mode selected by their enclosing
  section, but must not repeatedly load process configuration or acquire a
  shared lock.
- Turning inspection off also turns effective deep profiling off. Applications
  may additionally clear the persisted deep boolean to keep their UI state
  unsurprising.
- Turning deep profiling back on starts an empty capture window. Old samples
  must never appear in a new user trace.

## Embedder checklist

Every embedder should:

1. Persist two booleans in the application's ordinary settings store. This is
   application configuration, not a WorkTable/schema field.
2. Present two toggles with deep profiling unavailable until inspection is on.
3. Apply both settings once after the Blitz runtime is initialized and again as
   one pair after a live settings change.
4. Roll back the pair if application persistence fails.
5. Build and test three modes: both off, inspection only, and inspection plus
   deep profiling.
6. Measure the shipped binary with deep profiling off and on. The disabled
   result is the performance acceptance case; the enabled result quantifies the
   deliberate profiling tax.
