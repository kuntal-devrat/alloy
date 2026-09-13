use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use alloy_core::value::{Value, VmHost, SYMBOL_ITERATOR, SYMBOL_TO_STRING_TAG};
use crate::vm::core::Vm;
use crate::vm::stack::{CallFrame, OperandStack};
use crate::vm::ops_async::{Handler, ThrowResult};

#[derive(Clone)]
pub struct GeneratorState {
    pub stack: Vec<Value>,
    pub call_stack: Vec<CallFrame>,
    pub cells_stack: Vec<Vec<Rc<RefCell<Value>>>>,
    pub handlers: Vec<Handler>,
    pub pc: usize,
    pub program_id: u32,
    pub done: bool,
    pub yielded: bool,
    pub is_initial: bool,
    pub return_value: Value,
}

impl Vm {
    pub(crate) fn create_generator_object(&mut self, gen_id: u64) -> Value {
        let next_fn = Value::native(Arc::new(move |args, vm| {
            let val = args.first().cloned().unwrap_or(Value::undefined());
            vm.generator_step(gen_id, val, false)
        }));
        let return_fn = Value::native(Arc::new(move |args, vm| {
            let val = args.first().cloned().unwrap_or(Value::undefined());
            vm.generator_return(gen_id, val)
        }));
        let throw_fn = Value::native(Arc::new(move |args, vm| {
            let err = args.first().cloned().unwrap_or(Value::undefined());
            vm.generator_step(gen_id, err, true)
        }));
        let iter_fn = Value::native(Arc::new(|_args, vm| {
            vm.this_value()
        }));

        let mut props = hashbrown::HashMap::new();
        props.insert("next".to_string(), next_fn);
        props.insert("return".to_string(), return_fn);
        props.insert("throw".to_string(), throw_fn);
        props.insert(format!("\0sym_{}", SYMBOL_ITERATOR), iter_fn);
        props.insert(
            format!("\0sym_{}", SYMBOL_TO_STRING_TAG),
            Value::string("Generator".to_string()),
        );
        Value::object(props)
    }

    pub(crate) fn generator_return(&mut self, gen_id: u64, val: Value) -> Value {
        if let Some(state_rc) = self.generators.get(&gen_id).cloned() {
            let mut st = state_rc.borrow_mut();
            st.done = true;
            st.return_value = val.clone();
        }
        let mut res = hashbrown::HashMap::new();
        res.insert("value".to_string(), val);
        res.insert("done".to_string(), Value::bool(true));
        Value::object(res)
    }

    pub(crate) fn generator_step(&mut self, gen_id: u64, input: Value, is_throw: bool) -> Value {
        let state_rc = match self.generators.get(&gen_id).cloned() {
            Some(s) => s,
            None => {
                let mut res = hashbrown::HashMap::new();
                res.insert("value".to_string(), Value::undefined());
                res.insert("done".to_string(), Value::bool(true));
                return Value::object(res);
            }
        };

        if state_rc.borrow().done {
            if is_throw {
                self.throw_exception(input);
                return Value::undefined();
            }
            let mut res = hashbrown::HashMap::new();
            res.insert("value".to_string(), Value::undefined());
            res.insert("done".to_string(), Value::bool(true));
            return Value::object(res);
        }

        let is_initial = state_rc.borrow().is_initial;
        let (gen_stack, gen_call_stack, gen_cells, gen_handlers, gen_pc, gen_prog) = {
            let mut st = state_rc.borrow_mut();
            st.is_initial = false;
            (
                std::mem::take(&mut st.stack),
                std::mem::take(&mut st.call_stack),
                std::mem::take(&mut st.cells_stack),
                std::mem::take(&mut st.handlers),
                st.pc,
                st.program_id,
            )
        };

        // Save active generator & context
        let saved_active_gen = self.active_generator;
        self.active_generator = Some(gen_id);

        let mut saved_stack = std::mem::replace(&mut self.stack, OperandStack::new());
        self.stack.restore(gen_stack);
        let saved_call_stack = std::mem::replace(&mut self.call_stack, gen_call_stack);
        let saved_cells = std::mem::replace(&mut self.cells_stack, gen_cells);
        let saved_handlers = std::mem::replace(&mut self.handlers, gen_handlers);
        let saved_program = self.program_id;
        self.load_program(gen_prog);

        let mut start_pc = gen_pc;
        if is_throw {
            match self.throw_value(input) {
                ThrowResult::Jump(p) => start_pc = p,
                ThrowResult::EndDispatch | ThrowResult::Abort => {
                    state_rc.borrow_mut().done = true;
                    self.stack = saved_stack;
                    self.call_stack = saved_call_stack;
                    self.cells_stack = saved_cells;
                    self.handlers = saved_handlers;
                    self.load_program(saved_program);
                    self.active_generator = saved_active_gen;
                    return Value::undefined();
                }
            }
        } else if !is_initial {
            // Push the result of the yield expression that just resumed
            self.push(input);
        }

        let result = self.dispatch(start_pc);

        self.active_generator = saved_active_gen;

        // Check if an uncaught exception occurred in generator
        if let Some(err) = self.uncaught_exception.take() {
            state_rc.borrow_mut().done = true;
            self.stack = saved_stack;
            self.call_stack = saved_call_stack;
            self.cells_stack = saved_cells;
            self.handlers = saved_handlers;
            self.load_program(saved_program);
            self.throw_exception(err);
            return Value::undefined();
        }

        let is_yielded = state_rc.borrow().yielded;
        let is_done = state_rc.borrow().done;

        // Restore outer state
        self.stack = saved_stack;
        self.call_stack = saved_call_stack;
        self.cells_stack = saved_cells;
        self.handlers = saved_handlers;
        self.load_program(saved_program);

        let mut res = hashbrown::HashMap::new();
        res.insert("value".to_string(), result);
        res.insert("done".to_string(), Value::bool(!is_yielded || is_done));
        Value::object(res)
    }

    pub(crate) fn drain_iterator(&mut self, iter_obj: &Value) -> Value {
        if iter_obj.is_array() {
            return iter_obj.clone();
        }
        let sym_key = format!("\0sym_{}", SYMBOL_ITERATOR);
        let actual_iter = if let Some(od) = iter_obj.as_object() {
            if od.borrow().get("next").is_some() {
                iter_obj.clone()
            } else if let Some(im) = od.borrow().get(&sym_key).cloned() {
                if im.is_function() || im.is_native() {
                    self.call_value_with_this(&im, Some(iter_obj.clone()), &[])
                } else {
                    iter_obj.clone()
                }
            } else {
                iter_obj.clone()
            }
        } else {
            iter_obj.clone()
        };

        let mut items = Vec::new();
        let max_iterations = 100_000;
        let mut count = 0;
        while count < max_iterations {
            count += 1;
            let next_fn = if let Some(od) = actual_iter.as_object() {
                od.borrow().get("next").cloned()
            } else {
                None
            };
            let Some(next_fn) = next_fn else {
                break;
            };
            let step_res = self.call_value_with_this(&next_fn, Some(actual_iter.clone()), &[]);
            if let Some(err) = self.uncaught_exception.take() {
                self.throw_exception(err);
                return Value::undefined();
            }
            let (val, done) = if let Some(obj) = step_res.as_object() {
                let b = obj.borrow();
                let v = b.get("value").cloned().unwrap_or(Value::undefined());
                let d = b.get("done").map(|d| d.is_truthy()).unwrap_or(false);
                (v, d)
            } else {
                (step_res, true)
            };
            if done {
                break;
            }
            items.push(val);
        }
        Value::array(items)
    }

    pub(crate) fn prepare_spread_values(&mut self, mut vals: Vec<Value>, mask: u16) -> Vec<Value> {
        let sym_key = format!("\0sym_{}", SYMBOL_ITERATOR);
        for (i, v) in vals.iter_mut().enumerate() {
            if mask & (1 << i) != 0 {
                if let Some(od) = v.as_object() {
                    let c = od.borrow().container;
                    if c == 0 {
                        let has_next = od.borrow().get("next").is_some();
                        let has_iter = od.borrow().get(&sym_key).is_some();
                        if has_next || has_iter {
                            *v = self.drain_iterator(v);
                        }
                    }
                }
            }
        }
        vals
    }
}
