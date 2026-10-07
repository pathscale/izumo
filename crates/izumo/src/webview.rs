use blitz_script::ScriptDocument;

use crate::{AppHandle, attach_ipc_handler};

/// Prepare the chrome document before its own scripts execute.
///
/// The runtime calls this immediately after the document factory returns and
/// before registering the document with blitz-shell. The factory must return
/// a document whose scripts have not yet executed.
///
/// IPC uses the application's existing command table and ScriptQueue. The
/// bridge installs the native postMessage hook, attaches the queue without
/// replacing existing poll hooks, and evaluates the __IZUMO__ shim immediately.
pub(crate) fn prepare_document(
    mut document: ScriptDocument,
    handle: &AppHandle,
) -> ScriptDocument {
    attach_ipc_handler(&mut document, handle.clone());
    document
}
