//! `Replay`: a fake `tdjson` that plays JSON fixtures. Compiled
//! in tests and debug builds only.
//!
//! A fixture is `{ "steps": [...], "execute": {...} }`. Each step waits for a request whose
//! `@type` is `expect` (the first unused step of that type wins), then queues after
//! `delay_ms`: the `before` updates, the `reply` (given the request's `@extra`) and the
//! `after` updates. A step without `reply` is never answered, like TDLib with a request sent
//! before a `setTdlibParameters` that failed. Every queued message gets the
//! request's `@client_id`. `execute` maps a synchronous request type to its answer
//! (`{"@type":"ok"}` by default).

use std::collections::{HashMap, VecDeque};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use super::bindings::TdBindings;
use crate::transport::TransportError;

#[derive(Clone, Debug, Deserialize)]
pub struct Step {
    pub expect: String,
    #[serde(default)]
    pub reply: Option<Value>,
    #[serde(default)]
    pub before: Vec<Value>,
    #[serde(default)]
    pub after: Vec<Value>,
    #[serde(default)]
    pub delay_ms: u64,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Fixture {
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub execute: HashMap<String, Value>,
}

struct Queued {
    due: Instant,
    text: String,
}

#[derive(Default)]
struct State {
    steps: VecDeque<Step>,
    execute: HashMap<String, Value>,
    outbox: VecDeque<Queued>,
    /// `create`, `execute:<type>`, `send:<type>` in call order.
    calls: Vec<String>,
    sent: Vec<Value>,
    executed: Vec<Value>,
    unexpected: Vec<String>,
    next_client_id: i32,
}

pub struct Replay {
    state: Mutex<State>,
    wake: Condvar,
}

impl Replay {
    pub fn new(fixture: Fixture) -> Self {
        Self {
            state: Mutex::new(State {
                steps: fixture.steps.into(),
                execute: fixture.execute,
                next_client_id: 1,
                ..State::default()
            }),
            wake: Condvar::new(),
        }
    }

    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        Ok(Self::new(serde_json::from_str(text)?))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Adds steps after the current ones (a test that drives the flow in phases).
    pub fn push_steps(&self, steps: Vec<Step>) {
        self.lock().steps.extend(steps);
    }

    /// Queues a message now, as if TDLib emitted it (a renewed QR link, garbage…).
    pub fn inject(&self, text: impl Into<String>) {
        let mut state = self.lock();
        state.outbox.push_back(Queued {
            due: Instant::now(),
            text: text.into(),
        });
        self.wake.notify_all();
    }

    pub fn calls(&self) -> Vec<String> {
        self.lock().calls.clone()
    }

    pub fn sent(&self) -> Vec<Value> {
        self.lock().sent.clone()
    }

    pub fn sent_of(&self, kind: &str) -> Vec<Value> {
        self.sent()
            .into_iter()
            .filter(|request| request["@type"] == kind)
            .collect()
    }

    pub fn executed(&self) -> Vec<Value> {
        self.lock().executed.clone()
    }

    /// Request types no step expected (they were never answered).
    pub fn unexpected(&self) -> Vec<String> {
        self.lock().unexpected.clone()
    }

    /// `expect` of the steps not used yet.
    pub fn remaining(&self) -> Vec<String> {
        self.lock().steps.iter().map(|s| s.expect.clone()).collect()
    }
}

fn kind_of(request: &Value) -> String {
    request["@type"].as_str().unwrap_or("<no @type>").to_owned()
}

fn with_client(mut message: Value, client_id: i32) -> String {
    if let Some(object) = message.as_object_mut() {
        object.insert("@client_id".into(), json!(client_id));
    }
    message.to_string()
}

impl TdBindings for Replay {
    fn create_client_id(&self) -> Result<i32, TransportError> {
        let mut state = self.lock();
        let id = state.next_client_id;
        state.next_client_id += 1;
        state.calls.push("create".into());
        Ok(id)
    }

    fn send(&self, client_id: i32, request: &str) -> Result<(), TransportError> {
        let request: Value = serde_json::from_str(request)
            .map_err(|_| TransportError::Protocol("replay: request is not JSON".into()))?;
        let kind = kind_of(&request);
        let mut state = self.lock();
        state.calls.push(format!("send:{kind}"));
        state.sent.push(request.clone());
        let Some(index) = state.steps.iter().position(|step| step.expect == kind) else {
            state.unexpected.push(kind);
            return Ok(());
        };
        let Some(step) = state.steps.remove(index) else {
            return Ok(());
        };
        let due = Instant::now() + Duration::from_millis(step.delay_ms);
        let mut messages: Vec<String> = step
            .before
            .into_iter()
            .map(|update| with_client(update, client_id))
            .collect();
        if let Some(mut reply) = step.reply {
            if let (Some(object), Some(extra)) = (reply.as_object_mut(), request.get("@extra")) {
                object.insert("@extra".into(), extra.clone());
            }
            messages.push(with_client(reply, client_id));
        }
        messages.extend(
            step.after
                .into_iter()
                .map(|update| with_client(update, client_id)),
        );
        state
            .outbox
            .extend(messages.into_iter().map(|text| Queued { due, text }));
        self.wake.notify_all();
        Ok(())
    }

    fn receive(&self, timeout: Duration) -> Result<Option<String>, TransportError> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        loop {
            let now = Instant::now();
            // Earliest due message; equal due times keep their queue order.
            let next = state
                .outbox
                .iter()
                .enumerate()
                .min_by_key(|(index, queued)| (queued.due, *index))
                .map(|(index, queued)| (index, queued.due));
            if let Some((index, due)) = next
                && due <= now
            {
                return Ok(state.outbox.remove(index).map(|queued| queued.text));
            }
            if now >= deadline {
                return Ok(None);
            }
            let until = next.map_or(deadline, |(_, due)| due.min(deadline));
            state = self
                .wake
                .wait_timeout(state, until.saturating_duration_since(now))
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }

    fn execute(&self, request: &str) -> Result<Option<String>, TransportError> {
        let request: Value = serde_json::from_str(request)
            .map_err(|_| TransportError::Protocol("replay: request is not JSON".into()))?;
        let kind = kind_of(&request);
        let mut state = self.lock();
        state.calls.push(format!("execute:{kind}"));
        state.executed.push(request);
        let answer = state
            .execute
            .get(&kind)
            .cloned()
            .unwrap_or_else(|| json!({ "@type": "ok" }));
        Ok(Some(answer.to_string()))
    }
}
