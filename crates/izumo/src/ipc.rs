use std::sync::{Arc, OnceLock, RwLock};

use blitz_script::ScriptDocument;
use serde_json::Value;

use crate::AppHandle;

const MAX_REQUEST_ID: u64 = 9_007_199_254_740_991;

type RemoteIpcPolicy = dyn Fn(&url::Url) -> bool + Send + Sync + 'static;

static REMOTE_IPC_POLICY: OnceLock<RwLock<Option<Arc<RemoteIpcPolicy>>>> = OnceLock::new();

/// Set the explicit permission policy for HTTP and HTTPS chrome document IPC.
///
/// Remote document IPC is denied until a policy permits its parsed URL.
/// Prefer an exact origin allowlist, including scheme and port, rather than
/// string prefixes. Local chrome and every script it loads must be trusted:
/// admitted IPC can invoke every registered application command.
///
/// Install the policy before running the application. It is checked for every
/// valid IPC request, so replacing it with `|_| false` revokes remote access.
/// The callback runs on the document thread without holding the policy lock.
/// It must not execute JavaScript or reenter the document.
pub fn set_remote_ipc_policy(policy: impl Fn(&url::Url) -> bool + Send + Sync + 'static) {
    *REMOTE_IPC_POLICY
        .get_or_init(|| RwLock::new(None))
        .write()
        .unwrap() = Some(Arc::new(policy));
}

/// The chrome client. Install before executing the document's own scripts.
///
/// `invoke(cmd, args)` returns a promise for the command's JSON reply.
/// `listen(event, callback)` returns a promise for an idempotent unsubscribe
/// function. Callbacks receive `{ event, id, payload }`.
const IPC_SHIM: &str = r#"
(function (global) {
    "use strict";

    const pending = new Map();
    const listeners = new Map();
    let nextRequestId = 1;
    let nextListenerId = 1;

    function invoke(cmd, args) {
        if (typeof cmd !== "string" || cmd.trim().length === 0) {
            return Promise.reject(new TypeError("izumo command must be nonempty"));
        }
        if (nextRequestId > Number.MAX_SAFE_INTEGER) {
            return Promise.reject(new Error("izumo request ids exhausted"));
        }
        const id = nextRequestId++;
        return new Promise(function (resolve, reject) {
            pending.set(id, { resolve: resolve, reject: reject });
            try {
                global.ipc.postMessage(JSON.stringify({
                    cmd: cmd,
                    id: id,
                    args: args === undefined ? {} : args
                }));
            } catch (error) {
                pending.delete(id);
                reject(error);
            }
        });
    }

    function listen(event, callback) {
        if (typeof event !== "string" || event.trim().length === 0) {
            return Promise.reject(new TypeError("izumo event must be nonempty"));
        }
        if (typeof callback !== "function") {
            return Promise.reject(new TypeError("izumo listener must be a function"));
        }
        if (nextListenerId > Number.MAX_SAFE_INTEGER) {
            return Promise.reject(new Error("izumo listener ids exhausted"));
        }
        const id = nextListenerId++;
        let callbacks = listeners.get(event);
        if (!callbacks) {
            callbacks = new Map();
            listeners.set(event, callbacks);
        }
        callbacks.set(id, callback);
        let active = true;
        return Promise.resolve(function () {
            if (!active) {
                return;
            }
            active = false;
            callbacks.delete(id);
            if (callbacks.size === 0) {
                listeners.delete(event);
            }
        });
    }

    function reply(id, success, payload) {
        const request = pending.get(id);
        if (!request) {
            return;
        }
        pending.delete(id);
        if (success) {
            request.resolve(payload);
        } else {
            request.reject(payload);
        }
    }

    function emit(event, payload) {
        const callbacks = listeners.get(event);
        if (!callbacks) {
            return;
        }
        for (const entry of Array.from(callbacks.entries())) {
            const id = entry[0];
            const callback = entry[1];
            if (!callbacks.has(id)) {
                continue;
            }
            try {
                callback({ event: event, id: id, payload: payload });
            } catch (error) {
                if (global.console && typeof global.console.error === "function") {
                    global.console.error("izumo event listener failed", error);
                }
            }
        }
    }

    Object.defineProperty(global, "__IZUMO__", {
        value: Object.freeze({
            invoke: invoke,
            listen: listen,
            _reply: reply,
            _emit: emit
        }),
        writable: false,
        configurable: false,
        enumerable: false
    });
})(window);
"#;

/// Connect Boa's `window.ipc.postMessage` hook to the application's command table.
///
/// Call once on a fresh document, before executing its scripts or polling it.
/// Commands run on the document thread without holding the command table lock.
/// Replies and application events return through the same document-thread queue.
///
/// The chrome document must be local and trusted, including all scripts it
/// executes. HTTP and HTTPS document URLs are denied unless explicitly permitted
/// by [`set_remote_ipc_policy`]. This grants access to all registered commands;
/// it does not provide per-command capabilities or validate script origins.
///
/// The URL is captured when the bridge is attached. Set the document's actual
/// source URL before calling this function and attach a fresh bridge for each
/// replacement document. Missing or non-HTTP(S) URLs are not proof of trust.
pub fn attach_ipc_handler(document: &mut ScriptDocument, handle: AppHandle) {
    attach_ipc_handler_for_url(document, handle, None);
}

pub(crate) fn attach_ipc_handler_for_url(
    document: &mut ScriptDocument,
    handle: AppHandle,
    source_url: Option<url::Url>,
) {
    let remote_urls: Vec<url::Url> = source_url
        .into_iter()
        .chain(document.current_url().cloned())
        .filter(|url| matches!(url.scheme(), "http" | "https"))
        .collect();

    let scripts = handle.script_queue().clone();
    scripts.attach_to(document);

    document.set_ipc_handler(move |body| {
        let mut message = match serde_json::from_str::<Value>(&body) {
            Ok(Value::Object(message)) => message,
            Ok(_) => {
                eprintln!("izumo: IPC request must be an object");
                return;
            }
            Err(error) => {
                eprintln!("izumo: invalid IPC request: {error}");
                return;
            }
        };

        let id = message
            .remove("id")
            .and_then(|value| value.as_u64())
            .filter(|id| *id > 0 && *id <= MAX_REQUEST_ID);
        let Some(id) = id else {
            eprintln!("izumo: IPC request id must be a positive safe integer");
            return;
        };

        if !remote_urls.is_empty() {
            let policy = REMOTE_IPC_POLICY
                .get_or_init(|| RwLock::new(None))
                .read()
                .unwrap()
                .clone();
            let permitted = policy
                .as_ref()
                .is_some_and(|policy| remote_urls.iter().all(|url| policy(url)));
            if !permitted {
                scripts.enqueue_reply(
                    id,
                    Err("IPC from remote HTTP(S) chrome documents is not permitted".into()),
                );
                return;
            }
        }

        let cmd = match message.remove("cmd") {
            Some(Value::String(cmd)) if !cmd.trim().is_empty() => cmd,
            _ => {
                scripts.enqueue_reply(id, Err("izumo command must be nonempty".into()));
                return;
            }
        };
        let args = message
            .remove("args")
            .unwrap_or_else(|| Value::Object(Default::default()));
        scripts.enqueue_reply(id, handle.invoke(&cmd, args));
    });

    document.eval(IPC_SHIM);
}
