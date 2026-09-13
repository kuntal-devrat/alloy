use std::cell::RefCell;
use alloy_core::heap::{KIND_ARRAY, KIND_OBJECT, PromoteMap};
use alloy_core::value::{
    ArrayData, ChannelItem, MarkState, ObjectData, PromiseStatus, RcDirtyRef, Value,
    sweep_old_mark_sweep, sweep_young, walk_cell, walk_container_entries, walk_value,
};

use super::core::{MAJOR_THRESHOLD_MAX, MAJOR_THRESHOLD_MIN, Vm};
use super::ops_async::{Continuation, Microtask};

impl Vm {
    pub(crate) fn walk_roots(
        &mut self,
        map: &mut PromoteMap,
        visited: &mut std::collections::HashSet<usize>,
        mut mark: Option<&mut MarkState>,
        result: Option<&mut Value>,
    ) {
        for i in 0..self.stack.sp {
            walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), &mut self.stack.slots[i]);
        }
        for g in &mut self.globals {
            walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), g);
        }
        for g in &mut self.stable_globals {
            walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), g);
        }
        for m in self.python_modules.values_mut() {
            walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), m);
        }
        for (e, _, _) in self.require_cache.values_mut() {
            walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), e);
        }
        for m in self.modules.values_mut() {
            for g in m.globals.iter_mut() {
                walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), g);
            }
        }
        for cs in &mut self.cells_stack {
            for c in cs.iter() {
                walk_cell(&mut self.heap, map, visited, mark.as_deref_mut(), c);
            }
        }
        for w in &mut self.cross_waiters {
            walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), w);
        }
        for t in &mut self.timers {
            walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), &mut t.callback);
        }
        for cont in self.continuations.values_mut() {
            match cont {
                Continuation::Suspended { stack, cells, .. } => {
                    for v in stack.iter_mut() {
                        walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), v);
                    }
                    for cs in cells {
                        for c in cs {
                            walk_cell(&mut self.heap, map, visited, mark.as_deref_mut(), c);
                        }
                    }
                }
                Continuation::Callback { callback, on_rejected, promise } => {
                    if let Some(cb) = callback.as_mut() {
                        walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), cb);
                    }
                    if let Some(cb) = on_rejected.as_mut() {
                        walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), cb);
                    }
                    let mut ps = promise.lock().unwrap_or_else(|g| g.into_inner());
                    if let PromiseStatus::Fulfilled(val) | PromiseStatus::Rejected(val) =
                        &mut ps.status
                    {
                        walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), val);
                    }
                }
            }
        }
        let mts: Vec<usize> = self.microtasks.iter().copied().collect();
        for addr in mts {
            let mut mt = unsafe { self.microtask_arena.read_at(addr as *const Microtask) };
            walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), &mut mt.value);
            unsafe {
                std::ptr::write(addr as *mut Microtask, mt);
            }
        }
        if let Some(r) = result {
            walk_value(&mut self.heap, map, visited, mark, r);
        }
    }

    pub(crate) fn promote_and_reclaim(&mut self, result: Option<&mut Value>) {
        let mut mark = self.mark.take();
        if mark.is_none()
            && self.heap.old_alloc_total().saturating_sub(self.last_major_alloc)
                >= self.major_threshold
        {
            mark = Some(MarkState::new());
        }
        let result_opt = result;
        let mut map = PromoteMap::new();
        let mut visited = std::collections::HashSet::new();
        self.walk_roots(&mut map, &mut visited, mark.as_mut(), result_opt);
        self.scan_dirty_old_boxes(&mut map, &mut visited, mark.as_mut());
        if let Some(m) = &mut mark {
            let batch: Vec<usize> = {
                let n = self.mark_budget.min(m.worklist.len());
                m.worklist.split_off(m.worklist.len() - n)
            };
            for addr in batch {
                self.trace_box(addr, &mut map, &mut visited, Some(&mut *m));
            }
            let dirty = std::mem::take(&mut m.dirty_rc);
            let mut v2 = std::collections::HashSet::new();
            for d in dirty {
                self.trace_rc_dirty(d, &mut map, &mut v2, Some(&mut *m));
            }
        }
        if let Some(m) = &mut mark {
            for &addr in map.values() {
                m.insert_box(&self.heap, addr);
            }
        }
        sweep_young(&mut self.heap, &map);
        if let Some(m) = mark {
            if m.is_done() {
                let free_after = {
                    sweep_old_mark_sweep(&mut self.heap, &m.set);
                    self.heap.free_bytes()
                };
                self.last_major_alloc = self.heap.old_alloc_total();
                let used = self.heap.used_old();
                let t = self.major_threshold;
                if free_after * 4 < used {
                    self.major_threshold = (t * 2).min(MAJOR_THRESHOLD_MAX.max(t));
                } else if free_after * 2 > used {
                    self.major_threshold = (t / 2).max(MAJOR_THRESHOLD_MIN.min(t));
                }
                self.mark = None;
            } else {
                self.mark = Some(m);
            }
        }
    }

    pub(crate) fn scan_dirty_old_boxes(
        &mut self,
        map: &mut PromoteMap,
        visited: &mut std::collections::HashSet<usize>,
        mut mark: Option<&mut MarkState>,
    ) {
        let dirty = self.heap.collect_dirty_old_boxes();
        for (addr, kind) in dirty {
            match kind as u64 {
                KIND_ARRAY => {
                    let inner = unsafe { &*(addr as *const RefCell<ArrayData>) };
                    match &mut *inner.borrow_mut() {
                        ArrayData::Ints(_) => {}
                        ArrayData::Values(v) => {
                            for e in v.iter_mut() {
                                walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), e);
                            }
                        }
                    }
                }
                KIND_OBJECT => {
                    let inner = unsafe { &*(addr as *const RefCell<ObjectData>) };
                    let mut od = inner.borrow_mut();
                    for e in od.values.iter_mut() {
                        walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), e);
                    }
                    if let Some(cd) = od.entries.as_mut() {
                        walk_container_entries(cd, &mut self.heap, map, visited, mark.as_deref_mut());
                    }
                }
                _ => {}
            }
        }
    }

    pub(crate) fn trace_box(
        &mut self,
        addr: usize,
        map: &mut PromoteMap,
        visited: &mut std::collections::HashSet<usize>,
        mut mark: Option<&mut MarkState>,
    ) {
        let kind = self.heap.kind_of(addr);
        match kind as u64 {
            KIND_ARRAY => {
                let inner = unsafe { &*(addr as *const RefCell<ArrayData>) };
                match &mut *inner.borrow_mut() {
                    ArrayData::Ints(_) => {}
                    ArrayData::Values(v) => {
                        for e in v.iter_mut() {
                            walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), e);
                        }
                    }
                }
            }
            KIND_OBJECT => {
                let inner = unsafe { &*(addr as *const RefCell<ObjectData>) };
                let mut od = inner.borrow_mut();
                walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), &mut od.proto);
                for e in od.values.iter_mut() {
                    walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), e);
                }
                if let Some(cd) = od.entries.as_mut() {
                    walk_container_entries(cd, &mut self.heap, map, visited, mark);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn trace_rc_dirty(
        &mut self,
        d: RcDirtyRef,
        map: &mut PromoteMap,
        visited: &mut std::collections::HashSet<usize>,
        mut mark: Option<&mut MarkState>,
    ) {
        match d {
            RcDirtyRef::Cell(cell) => {
                walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), &mut cell.borrow_mut());
            }
            RcDirtyRef::Promise(p) => {
                let mut ps = p.lock().unwrap_or_else(|g| g.into_inner());
                if let PromiseStatus::Fulfilled(val) | PromiseStatus::Rejected(val) = &mut ps.status
                {
                    walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), val);
                }
            }
            RcDirtyRef::Channel(st) => {
                let mut g = st.lock().unwrap_or_else(|g| g.into_inner());
                for m in g.queue.iter_mut() {
                    if let ChannelItem::Raw(v) = m {
                        walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), v);
                    }
                }
                for w in g.waiters.iter_mut() {
                    walk_value(&mut self.heap, map, visited, mark.as_deref_mut(), w);
                }
            }
        }
    }

    pub fn heap_stats(&self) -> (usize, usize, usize, usize) {
        (
            self.heap.used_young(),
            self.heap.used_old(),
            self.heap.used(),
            self.heap.capacity(),
        )
    }

    #[cfg(test)]
    pub(crate) fn heap_used_young(&self) -> usize {
        self.heap.used_young()
    }
}
