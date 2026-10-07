use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Waker};

use blitz_script::ScriptDocument;

/// Thread-safe ingress for application command replies, events and UI work.
///
/// `ScriptDocument` is intentionally single-threaded. Dispatchers clone this queue and push from
/// any thread; the native event loop drains it into Boa on the document thread.
type EvalCallback = Box<dyn FnOnce(String) + Send + 'static>;
type DocumentTask = Box<dyn FnOnce() + Send + 'static>;

enum ScriptTask {
    Evaluate(String),
    EvaluateWithCallback(String, EvalCallback),
    RunOnDocumentThread(DocumentTask),
}

#[derive(Default)]
struct ScriptQueueState {
    tasks: VecDeque<ScriptTask>,
    waker: Option<Waker>,
}

/// Queue shared by the application handle and the Boa document thread.
#[derive(Clone, Default)]
pub struct ScriptQueue(Arc<Mutex<ScriptQueueState>>);

impl fmt::Debug for ScriptQueue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScriptQueue")
            .field("pending", &self.pending())
            .finish()
    }
}

impl ScriptQueue {
    fn push(&self, task: ScriptTask) {
        let waker = {
            let mut state = self.0.lock().unwrap();
            state.tasks.push_back(task);
            state.waker.clone()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    pub fn enqueue(&self, script: impl Into<String>) {
        self.push(ScriptTask::Evaluate(script.into()));
    }

    /// Serialize replies as JSON values before inserting them into evaluated JavaScript.
    pub(crate) fn enqueue_reply(&self, id: u64, result: crate::CommandResult) {
        let (success, payload) = match result {
            Ok(payload) => (true, payload),
            Err(error) => (false, serde_json::Value::String(error)),
        };
        self.enqueue(format!(
            "window.__IZUMO__._reply({id},{success},{payload});"
        ));
    }

    pub fn enqueue_with_callback(
        &self,
        script: impl Into<String>,
        callback: impl FnOnce(String) + Send + 'static,
    ) {
        self.push(ScriptTask::EvaluateWithCallback(
            script.into(),
            Box::new(callback),
        ));
    }

    pub fn enqueue_task(&self, task: impl FnOnce() + Send + 'static) {
        self.push(ScriptTask::RunOnDocumentThread(Box::new(task)));
    }

    pub fn pending(&self) -> usize {
        self.0.lock().unwrap().tasks.len()
    }

    /// Attach this queue to the document's native poll cycle.
    pub fn attach_to(&self, document: &mut ScriptDocument) {
        let queue = self.clone();
        document.add_poll_hook(move |document, task_context| queue.poll(document, task_context));
    }

    fn poll(&self, document: &mut ScriptDocument, task_context: Option<&TaskContext<'_>>) -> bool {
        if let Some(task_context) = task_context {
            let mut state = self.0.lock().unwrap();
            let stale = state
                .waker
                .as_ref()
                .map(|old| !old.will_wake(task_context.waker()))
                .unwrap_or(true);
            if stale {
                state.waker = Some(task_context.waker().clone());
            }
        }
        self.drain_into(document) > 0
    }

    pub fn drain_into(&self, document: &mut ScriptDocument) -> usize {
        let tasks: Vec<ScriptTask> = self.0.lock().unwrap().tasks.drain(..).collect();
        let count = tasks.len();
        for task in tasks {
            match task {
                ScriptTask::Evaluate(script) => document.eval(&script),
                ScriptTask::EvaluateWithCallback(script, callback) => {
                    let result = document
                        .eval_json(&script)
                        .map(|value| value.to_string())
                        .unwrap_or_else(|_| "null".into());
                    callback(result);
                }
                ScriptTask::RunOnDocumentThread(task) => task(),
            }
        }
        count
    }
}
