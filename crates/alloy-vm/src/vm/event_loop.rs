use std::sync::{Arc, Mutex};

use alloy_core::value::{PromiseState, PromiseStatus, RcDirtyRef, Value, VmHost};

use super::core::Vm;
use super::ops_async::{Continuation, Microtask, ThrowResult, Timer};
use super::spawn::decode_spawn_value;

impl Vm {
    pub(crate) fn new_promise_arc(&self) -> Arc<Mutex<PromiseState>> {
        Arc::new(Mutex::new(PromiseState {
            status: PromiseStatus::Pending,
            continuations: Vec::new(),
            owner: Some(self.wake_tx.clone()),
        }))
    }

    pub(crate) fn drive_event_loop(&mut self) {
        loop {
            self.drain_python_completions();
            self.drain_spawn_completions();
            self.drain_cross_thread_inbox();
            while let Some(addr) = self.microtasks.pop_front() {
                let mt = unsafe { self.microtask_arena.read_at(addr as *const Microtask) };
                self.resume(mt);
            }
            self.microtask_arena.reset();
            self.retain_cross_waiters();
            let now = self.epoch.elapsed().as_secs_f64() * 1000.0;
            let due: Vec<(u64, Value, Option<f64>)> = self
                .timers
                .iter()
                .filter(|t| t.when <= now)
                .map(|t| (t.id, t.callback.clone(), t.period))
                .collect();
            if !due.is_empty() {
                self.timers.retain(|t| t.when > now);
                for (id, cb, period) in due {
                    if let Some(p) = period {
                        let when = self.epoch.elapsed().as_secs_f64() * 1000.0 + p.max(0.0);
                        let seq = self.next_cont_id;
                        self.next_cont_id += 1;
                        self.timers.push(Timer {
                            when,
                            seq,
                            id,
                            period: Some(p),
                            callback: cb.clone(),
                        });
                        self.timers.sort_by(|a, b| {
                            a.when
                                .partial_cmp(&b.when)
                                .unwrap_or(std::cmp::Ordering::Equal)
                                .then(a.seq.cmp(&b.seq))
                        });
                    }
                    let saved_handlers = std::mem::take(&mut self.handlers);
                    self.call_value(&cb, &[]);
                    self.handlers = saved_handlers;
                    if let Some(err) = self.uncaught_exception.take() {
                        eprintln!("uncaught exception in timer callback: {}", err);
                    }
                }
                continue;
            }
            let pending_work = self.python_inflight > 0
                || self.spawn_pending > 0
                || !self.cross_waiters.is_empty();
            if self.timers.is_empty() && !pending_work {
                break;
            }
            let next = self
                .timers
                .iter()
                .map(|t| t.when)
                .fold(f64::INFINITY, f64::min);
            let wait = ((next - now).max(0.0)).min(if pending_work { 2.0 } else { 1000.0 }) as u64;
            if wait > 0 {
                if !self.cross_waiters.is_empty() {
                    let _ = self
                        .wake_rx
                        .recv_timeout(std::time::Duration::from_millis(wait));
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(wait));
                }
            }
        }
        self.stack.clear();
        self.call_stack.clear();
        self.cells_stack.clear();
        self.handlers.clear();
    }

    pub(crate) fn drain_cross_thread_inbox(&mut self) {
        for (promise, bytes) in self.wake_tx.take_deliveries() {
            let mut pos = 0;
            let value = decode_spawn_value(&bytes, &mut pos);
            self.resolve_promise(&promise, value);
        }
    }

    pub(crate) fn retain_cross_waiters(&mut self) {
        self.cross_waiters.retain(|p| {
            p.as_promise()
                .map(|pr| {
                    matches!(
                        pr.lock().unwrap_or_else(|g| g.into_inner()).status,
                        PromiseStatus::Pending
                    )
                })
                .unwrap_or(false)
        });
    }

    pub(crate) fn pump_async_once(&mut self) {
        self.drain_python_completions();
        self.drain_spawn_completions();
        self.drain_cross_thread_inbox();
        while let Some(addr) = self.microtasks.pop_front() {
            let mt = unsafe { self.microtask_arena.read_at(addr as *const Microtask) };
            self.resume(mt);
        }
        self.microtask_arena.reset();
        self.retain_cross_waiters();
    }

    pub(crate) fn drive_pending_inner(&mut self) {
        loop {
            self.pump_async_once();
            if self.python_inflight == 0
                && self.spawn_pending == 0
                && self.microtasks.is_empty()
                && self.cross_waiters.is_empty()
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    pub(crate) fn resume(&mut self, mt: Microtask) {
        let Microtask {
            id,
            value,
            rejected,
        } = mt;
        match self.continuations.remove(&id) {
            Some(Continuation::Suspended {
                stack,
                frames,
                cells,
                handlers,
                pc,
                program_id,
            }) => {
                self.stack.restore(stack);
                self.call_stack = frames;
                self.cells_stack = cells;
                self.handlers = handlers;
                self.load_program(program_id);
                if rejected {
                    match self.throw_value(value) {
                        ThrowResult::Jump(p) => {
                            self.dispatch(p);
                        }
                        ThrowResult::EndDispatch => {}
                        ThrowResult::Abort => {}
                    }
                } else {
                    self.push(value);
                    self.dispatch(pc);
                }
            }
            Some(Continuation::Callback {
                callback,
                on_rejected,
                promise,
            }) => {
                let handler = if rejected { on_rejected } else { callback };
                let handler = match handler {
                    Some(h) => h,
                    None => {
                        if rejected {
                            self.reject_promise(&promise, value);
                        } else {
                            self.resolve_promise(&promise, value);
                        }
                        return;
                    }
                };
                let saved_handlers = std::mem::take(&mut self.handlers);
                let result = self.call_value(&handler, &[value]);
                self.handlers = saved_handlers;
                if let Some(err) = self.uncaught_exception.take() {
                    self.reject_promise(&promise, err);
                } else {
                    self.resolve_promise(&promise, result);
                }
            }
            None => {}
        }
    }

    pub(crate) fn throw_value(&mut self, exc: Value) -> ThrowResult {
        if let Some(h) = self.handlers.last().cloned() {
            if let Some(bi) = ((h.frame_depth + 1)..self.call_stack.len())
                .rev()
                .find(|&i| self.call_stack[i].promise_slot.is_some())
            {
                return self.reject_at_boundary(bi, exc);
            }
            while self.call_stack.len() > h.frame_depth {
                let f = self.call_stack.pop().unwrap();
                self.stack.truncate(f.base_slot);
                self.cells_stack.truncate(f.cells_len);
                self.handlers.truncate(f.handlers_len);
            }
            if h.program != self.program_id {
                self.load_program(h.program);
            }
            let floor = match self.call_stack.last() {
                Some(f) => f.locals_end,
                None => self.top_locals_end,
            };
            self.stack.truncate(h.stack_depth.max(floor));
            self.push(exc);
            self.handlers.pop();
            return ThrowResult::Jump(h.handler_pc);
        }
        if let Some(bi) = (0..self.call_stack.len())
            .rev()
            .find(|&i| self.call_stack[i].promise_slot.is_some())
        {
            return self.reject_at_boundary(bi, exc);
        }
        self.uncaught_exception = Some(exc);
        ThrowResult::Abort
    }

    pub(crate) fn reject_at_boundary(&mut self, bi: usize, exc: Value) -> ThrowResult {
        let b = self.call_stack[bi].clone();
        let promise = self
            .stack
            .at(b.base_slot + b.promise_slot.unwrap() as usize)
            .clone();
        if let Some(p) = promise.as_promise() {
            self.reject_promise(p, exc);
        }
        while self.call_stack.len() > bi {
            let f = self.call_stack.pop().unwrap();
            self.stack.truncate(f.base_slot);
            self.cells_stack.truncate(f.cells_len);
            self.handlers.truncate(f.handlers_len);
        }
        if b.return_program != self.program_id {
            self.load_program(b.return_program);
        }
        self.stack.truncate(b.base_slot);
        self.push(promise);
        if b.resumed {
            return ThrowResult::EndDispatch;
        }
        ThrowResult::Jump(b.return_addr)
    }

    pub(crate) fn resolve_promise(&mut self, promise: &Arc<Mutex<PromiseState>>, value: Value) {
        if let Some(inner) = value.as_promise() {
            let inner_status = inner
                .lock()
                .unwrap_or_else(|g| g.into_inner())
                .status
                .clone();
            match inner_status {
                PromiseStatus::Fulfilled(v) => {
                    return self.resolve_promise(promise, v);
                }
                PromiseStatus::Rejected(v) => {
                    return self.reject_promise(promise, v);
                }
                PromiseStatus::Pending => {
                    let conts = {
                        let mut ps = promise.lock().unwrap_or_else(|g| g.into_inner());
                        std::mem::take(&mut ps.continuations)
                    };
                    inner
                        .lock()
                        .unwrap_or_else(|g| g.into_inner())
                        .continuations
                        .extend(conts);
                    return;
                }
            }
        }
        self.note_rc_dirty(RcDirtyRef::Promise(promise.clone()));
        let mut ps = promise.lock().unwrap_or_else(|g| g.into_inner());
        ps.status = PromiseStatus::Fulfilled(value.clone());
        let conts = std::mem::take(&mut ps.continuations);
        drop(ps);
        for id in conts {
            self.enqueue_microtask(id, value.clone(), false);
        }
    }

    pub(crate) fn enqueue_microtask(&mut self, id: u64, value: Value, rejected: bool) {
        let ptr = self.microtask_arena.alloc_at(Microtask {
            id,
            value,
            rejected,
        });
        self.microtasks.push_back(ptr as usize);
    }

    #[cfg(test)]
    pub(crate) fn microtask_arena_used(&self) -> usize {
        self.microtask_arena.used()
    }

    pub(crate) fn reject_promise(&mut self, promise: &Arc<Mutex<PromiseState>>, value: Value) {
        self.note_rc_dirty(RcDirtyRef::Promise(promise.clone()));
        let mut ps = promise.lock().unwrap_or_else(|g| g.into_inner());
        ps.status = PromiseStatus::Rejected(value.clone());
        let conts = std::mem::take(&mut ps.continuations);
        drop(ps);
        for id in conts {
            self.enqueue_microtask(id, value.clone(), true);
        }
    }

    pub(crate) fn then(
        &mut self,
        promise: &Value,
        callback: Value,
        on_rejected: Option<Value>,
    ) -> Value {
        let src = match promise.as_promise() {
            Some(p) => p.clone(),
            None => return Value::undefined(),
        };
        let is_fn = |v: &Value| v.is_function() || v.is_native();
        let callback = if is_fn(&callback) {
            Some(callback)
        } else {
            None
        };
        let on_rejected = on_rejected.filter(&is_fn);
        let chained = self.new_promise_arc();
        let id = self.next_cont_id;
        self.next_cont_id += 1;
        self.continuations.insert(
            id,
            Continuation::Callback {
                callback,
                on_rejected,
                promise: chained.clone(),
            },
        );
        let mut flush: Option<(Value, bool)> = None;
        {
            let mut st = src.lock().unwrap_or_else(|g| g.into_inner());
            match &st.status {
                PromiseStatus::Pending => st.continuations.push(id),
                PromiseStatus::Fulfilled(v) => flush = Some((v.clone(), false)),
                PromiseStatus::Rejected(v) => flush = Some((v.clone(), true)),
            }
        }
        if let Some((v, rejected)) = flush {
            self.enqueue_microtask(id, v, rejected);
        }
        Value::promise(chained)
    }

    pub(crate) fn schedule_timer(&mut self, callback: Value, ms: f64, period: Option<f64>) -> u64 {
        let when = self.epoch.elapsed().as_secs_f64() * 1000.0 + ms.max(0.0);
        let seq = self.next_cont_id;
        let id = self.next_cont_id;
        self.next_cont_id += 1;
        self.timers.push(Timer {
            when,
            seq,
            id,
            period,
            callback,
        });
        self.timers.sort_by(|a, b| {
            a.when
                .partial_cmp(&b.when)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.seq.cmp(&b.seq))
        });
        id
    }

    pub(crate) fn clear_timer(&mut self, id: u64) {
        self.timers.retain(|t| t.id != id);
    }
}
