use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use alloy_core::regex;
use alloy_core::value::{
    to_string_js, ArrayData, ChannelItem, FunctionData, ObjectData, PromiseStatus, RcDirtyRef,
    Value, VmHost, SYMBOL_ITERATOR, SYMBOL_TO_STRING_TAG,
};
use crate::opcode::Opcode;

use super::alu::*;
use super::builtins::arrays::*;
use super::builtins::containers::*;
use super::builtins::numbers::*;
use super::builtins::strings::*;
use super::cache::{ic_slot, CallIcEntry, IcEntry};
use super::core::{op_hist, unwrap_cell, FRAME_BUDGET, MAX_CALL_DEPTH, STACK_SIZE, Vm};
use super::spawn::{decode_spawn_value, write_spawn_value};
use super::ops_async::{Continuation, Handler, ThrowResult};
use super::stack::{CallFrame, KIND_INT, KIND_NUMBER, KIND_OTHER, kind_of_value};

impl Vm {
    pub(crate) fn dispatch(&mut self, pc: usize) -> Value {
        match (self.instruction_budget.is_some(), self.op_hist_on) {
            (true, true) => self.dispatch_inner::<true, true>(pc),
            (true, false) => self.dispatch_inner::<true, false>(pc),
            (false, true) => self.dispatch_inner::<false, true>(pc),
            (false, false) => self.dispatch_inner::<false, false>(pc),
        }
    }

    #[inline(always)]
    pub(crate) fn dispatch_inner<const HAS_BUDGET: bool, const OP_HIST: bool>(
        &mut self,
        mut pc: usize,
    ) -> Value {
        let mut budget: u64 = if HAS_BUDGET {
            self.instruction_budget.unwrap_or(u64::MAX)
        } else {
            0
        };
        loop {
            if pc >= self.bytecode.len() {
                break;
            }
            if HAS_BUDGET {
                if budget == 0 {
                    self.budget_exhausted = true;
                    break;
                }
                budget -= 1;
            }

            // Hot loop uses unchecked byte fetch; length checked at top.
            // SAFETY: pc < len checked above, so index is in-bounds.
            let op_byte = unsafe { *self.bytecode.get_unchecked(pc) };
            // Cached flag (set once at Vm construction) — no OnceLock/mutex
            // traffic on the hot path when profiling is off.
            if OP_HIST {
                if let Some(h) = op_hist() {
                    if let Ok(mut g) = h.lock() {
                        g[op_byte as usize] += 1;
                    }
                }
            }
            // Back-edge counter for the baseline-JIT hypervisor: only taken
            // backwards jumps pay the HashMap increment.
            // (Forward jumps and fall-through cost one predictable branch.)
            let op = match Opcode::from_u8(op_byte) {
                Some(o) => o,
                None => { pc += 1; continue; }
            };
            match op {
                Opcode::Halt => break,

                Opcode::Nop => { pc += 1; }

                Opcode::LoadConst => {
                    let idx = self.read_u16(pc + 1);
                    let val = self.constants[idx as usize].clone();
                    self.push(val);
                    pc += 3;
                }
                Opcode::LoadInt => {
                    let val = self.read_u32(pc + 1) as i64;
                    self.push(Value::int(val));
                    pc += 5;
                }
                Opcode::LoadTrue => { self.push(Value::bool(true)); pc += 1; }
                Opcode::LoadFalse => { self.push(Value::bool(false)); pc += 1; }
                Opcode::LoadNull => { self.push(Value::null()); pc += 1; }
                Opcode::LoadUndefined => { self.push(Value::undefined()); pc += 1; }

                Opcode::LoadLocal => {
                    let slot = self.bytecode[pc + 1] as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let val = if idx < self.stack.len() {
                        // Fast: a slot known to hold a direct int/number is
                        // never a cell — raw word, no probe, no clone dispatch.
                        match self.stack.kind_of(idx) {
                            KIND_INT | KIND_NUMBER => {
                                Value::from_raw_word(self.stack.at(idx).bits())
                            }
                            _ => {
                                let v = self.stack.at(idx).clone();
                                match v.as_cell() {
                                    Some(c) => c.borrow().clone(),
                                    None => v,
                                }
                            }
                        }
                    } else {
                        Value::undefined()
                    };
                    self.push(val);
                    pc += 2;
                }
                Opcode::StoreLocal => {
                    let slot = self.bytecode[pc + 1] as usize;
                    let val = self.pop();
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    self.store_slot(base + slot, val);
                    pc += 2;
                }
                Opcode::LoadGlobal => {
                    let idx = self.read_u16(pc + 1) as usize;
                    let missing = idx >= self.globals.len()
                        || idx >= self.global_defined.len()
                        || !self.global_defined[idx];
                    if missing {
                        // JS: reading an undeclared identifier is a
                        // ReferenceError (a `let` that was never assigned is
                        // defined; a name that was never declared is not).
                        let name = self
                            .programs[self.program_id as usize]
                            .globals
                            .get(idx)
                            .cloned()
                            .unwrap_or_else(|| "<unknown>".to_string());
                        match self.throw_value(Value::string(format!(
                            "ReferenceError: {} is not defined",
                            name
                        ))) {
                            ThrowResult::Jump(p) => pc = p,
                            ThrowResult::EndDispatch => break,
                            ThrowResult::Abort => break,
                        }
                        continue;
                    }
                    // A slot may hold a live-import cell (module exports
                    // aliased into this scope): read through to the current
                    // value so imported bindings observe later mutations.
                    let v = self.globals[idx].clone();
                    let v = match v.as_cell() {
                        Some(c) => c.borrow().clone(),
                        None => v,
                    };
                    self.push(v);
                    pc += 3;
                }
                Opcode::TypeOfGlobal => {
                    // `typeof g` on a never-declared global is "undefined",
                    // not a ReferenceError.
                    let idx = self.read_u16(pc + 1) as usize;
                    let v = if idx < self.globals.len() {
                        self.globals[idx].clone()
                    } else {
                        Value::undefined()
                    };
                    self.push(Value::string(v.type_name().to_string()));
                    pc += 3;
                }
                Opcode::StoreGlobal => {
                    let idx = self.read_u16(pc + 1) as usize;
                    let val = self.pop();
                    if idx >= self.globals.len() {
                        self.globals.resize_with(idx + 1, Value::undefined);
                    }
                    // A live-import cell in the slot (a module export aliased
                    // into this scope, or the module's own exported global):
                    // write through to the shared cell so every alias — the
                    // exports object, other importers, the module itself —
                    // sees the new value. The cell stays in the slot.
                    match self.globals[idx].as_cell() {
                        Some(c) => *c.borrow_mut() = val.clone(),
                        None => self.globals[idx] = val.clone(),
                    }
                    if idx >= self.global_defined.len() {
                        self.global_defined.resize_with(idx + 1, || false);
                    }
                    self.global_defined[idx] = true;
                    // Mirror into the stable table so every program sees it —
                    // but NOT for modules: their globals live in the isolated
                    // `modules` view and must never leak into the requirer's
                    // namespace (or vice versa).
                    if !self.modules.contains_key(&self.program_id) {
                        if let Some(name) = self
                            .programs[self.program_id as usize]
                            .globals
                            .get(idx)
                            .cloned()
                        {
                            if let Some(si) = self.global_names.iter().position(|n| *n == name) {
                                self.stable_globals[si] = val;
                                if si >= self.stable_defined.len() {
                                    self.stable_defined.resize_with(si + 1, || false);
                                }
                                self.stable_defined[si] = true;
                            }
                        }
                    }
                    pc += 3;
                }

                Opcode::Add => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.add(&r));
                    pc += 1;
                }
                Opcode::Subtract => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.subtract(&r));
                    pc += 1;
                }
                Opcode::Multiply => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.multiply(&r));
                    pc += 1;
                }
                Opcode::Divide => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.divide(&r));
                    pc += 1;
                }
                Opcode::Modulo => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.modulo(&r));
                    pc += 1;
                }
                Opcode::Negate => {
                    let val = self.pop();
                    self.push(val.negate());
                    pc += 1;
                }

                Opcode::Equal => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.equal(&r));
                    pc += 1;
                }
                Opcode::NotEqual => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(Value::bool(!l.equal(&r).is_truthy()));
                    pc += 1;
                }
                Opcode::StrictEqual => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(Value::bool(strict_equal(&l, &r)));
                    pc += 1;
                }
                Opcode::StrictNotEqual => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(Value::bool(!strict_equal(&l, &r)));
                    pc += 1;
                }
                Opcode::BitAnd => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.bitand(&r));
                    pc += 1;
                }
                Opcode::BitOr => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.bitor(&r));
                    pc += 1;
                }
                Opcode::BitXor => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.bitxor(&r));
                    pc += 1;
                }
                Opcode::BitNot => {
                    let v = self.pop();
                    self.push(v.bitnot());
                    pc += 1;
                }
                Opcode::Shl => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.shl(&r));
                    pc += 1;
                }
                Opcode::Shr => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.shr(&r));
                    pc += 1;
                }
                Opcode::UShr => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.ushr(&r));
                    pc += 1;
                }
                Opcode::Pow => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(l.pow(&r));
                    pc += 1;
                }
                Opcode::DeleteProp => {
                    let name = self.pop();
                    let obj = self.pop();
                    if let Some(m) = obj.as_object() {
                        if let Some(n) = name.as_str() {
                            m.borrow_mut().delete(n);
                        }
                    }
                    // JS: deleting a property (existing or not) is true.
                    self.push(Value::bool(true));
                    pc += 1;
                }
                Opcode::DeleteIndex => {
                    let idx = self.pop();
                    let obj = self.pop();
                    if let Some(a) = obj.as_array() {
                        let i = idx.to_number();
                        if i.is_finite() && i >= 0.0 {
                            let mut a = a.borrow_mut();
                            let ix = i as usize;
                            if ix < a.len() {
                                // Deleting an element writes a hole, which
                                // escapes the packed form (undefined is not
                                // representable as an int).
                                a.set(ix, Value::undefined());
                            }
                        }
                    } else if let Some(m) = obj.as_object() {
                        let key = match idx.as_str() {
                            Some(k) => k.to_string(),
                            None => format!("{}", idx),
                        };
                        m.borrow_mut().delete(&key);
                    }
                    self.push(Value::bool(true));
                    pc += 1;
                }
                // JS semantics: numeric relational comparison, except when both
                // operands are strings, which compare lexicographically.
                Opcode::Less => {
                    let r = self.pop();
                    let l = self.pop();
                    let result = if let (Some(a), Some(b)) = (l.as_str(), r.as_str()) {
                        a < b
                    } else {
                        l.to_number() < r.to_number()
                    };
                    self.push(Value::bool(result));
                    pc += 1;
                }
                Opcode::Greater => {
                    let r = self.pop();
                    let l = self.pop();
                    let result = if let (Some(a), Some(b)) = (l.as_str(), r.as_str()) {
                        a > b
                    } else {
                        l.to_number() > r.to_number()
                    };
                    self.push(Value::bool(result));
                    pc += 1;
                }
                Opcode::LessEqual => {
                    let r = self.pop();
                    let l = self.pop();
                    let result = if let (Some(a), Some(b)) = (l.as_str(), r.as_str()) {
                        a <= b
                    } else {
                        l.to_number() <= r.to_number()
                    };
                    self.push(Value::bool(result));
                    pc += 1;
                }
                Opcode::GreaterEqual => {
                    let r = self.pop();
                    let l = self.pop();
                    let result = if let (Some(a), Some(b)) = (l.as_str(), r.as_str()) {
                        a >= b
                    } else {
                        l.to_number() >= r.to_number()
                    };
                    self.push(Value::bool(result));
                    pc += 1;
                }

                // ---- Fused superinstructions ----

                Opcode::CmpLocalInt => {
                    // r{slot} cmp imm : one dispatch for `i < 1000` style.
                    // When the slot's feedback kind is INT/NUMBER, the
                    // comparison runs in the raw i64/f64 lane — no tag
                    // probes, no ToNumber (collatz's `n !== 1` on an f64 `n`
                    // is the canonical case).
                    let slot = self.bytecode[pc + 1] as usize;
                    let imm = self.read_i32(pc + 2) as i64;
                    let cmp = self.bytecode[pc + 6];
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let result = if idx < self.stack.len() {
                        match self.stack.kind_of(idx) {
                            KIND_INT => cmp_i64(Value::int_bits_raw(self.stack.at(idx).bits()), imm, cmp),
                            KIND_NUMBER => cmp_f64(f64::from_bits(self.stack.at(idx).bits()), imm as f64, cmp),
                            _ => compare_values(&self.slot_value(idx), &Value::int(imm), cmp),
                        }
                    } else {
                        compare_values(&Value::undefined(), &Value::int(imm), cmp)
                    };
                    self.push(Value::bool(result));
                    pc += 7;
                }
                Opcode::BinLocalInt => {
                    // r{slot} ar imm : one dispatch for `i * 3` style. The
                    // peephole folds a trailing Pop into bit 7 of ar (keep=0).
                    // Known INT/NUMBER slots run in the raw lane (collatz's
                    // `n % 2` on an f64 `n` skips the whole probe storm).
                    let slot = self.bytecode[pc + 1] as usize;
                    let imm = self.read_i32(pc + 2) as i64;
                    let ar = self.bytecode[pc + 6];
                    let keep = ar & 0x80 == 0;
                    let ar = ar & 0x7F;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let (result, fast) = alu_local_imm(&self.stack, idx, imm, ar);
                    let result = if fast {
                        result
                    } else {
                        let l = if idx < self.stack.len() {
                            self.slot_value(idx)
                        } else {
                            Value::undefined()
                        };
                        arith_apply(&l, &Value::int(imm), ar)
                    };
                    if keep {
                        self.push(result);
                    }
                    pc += 7;
                }
                Opcode::BinLocalLocal => {
                    // r{a} ar r{b} : one dispatch for `i + j` style. The
                    // peephole folds a trailing Pop into bit 7 of ar (keep=0).
                    let a = self.bytecode[pc + 1] as usize;
                    let b = self.bytecode[pc + 2] as usize;
                    let ar = self.bytecode[pc + 3];
                    let keep = ar & 0x80 == 0;
                    let ar = ar & 0x7F;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let (result, fast) = alu_local_local(&self.stack, base + a, base + b, ar);
                    let result = if fast {
                        result
                    } else {
                        let va = if base + a < self.stack.len() {
                            self.slot_value(base + a)
                        } else {
                            Value::undefined()
                        };
                        let vb = if base + b < self.stack.len() {
                            self.slot_value(base + b)
                        } else {
                            Value::undefined()
                        };
                        arith_apply(&va, &vb, ar)
                    };
                    if keep {
                        self.push(result);
                    }
                    pc += 4;
                }
                Opcode::BinIntLocal => {
                    // imm ar r{slot} : one dispatch for `3 * n` style
                    // int-on-left patterns (peephole: LoadInt+LoadLocal+AR).
                    // Bit 7 of ar = keep=0 (folded trailing Pop).
                    let imm = self.read_i32(pc + 1) as i64;
                    let slot = self.bytecode[pc + 5] as usize;
                    let ar = self.bytecode[pc + 6];
                    let keep = ar & 0x80 == 0;
                    let ar = ar & 0x7F;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let (result, fast) = alu_imm_local(&self.stack, idx, imm, ar);
                    let result = if fast {
                        result
                    } else {
                        let r = if idx < self.stack.len() {
                            self.slot_value(idx)
                        } else {
                            Value::undefined()
                        };
                        arith_apply(&Value::int(imm), &r, ar)
                    };
                    if keep {
                        self.push(result);
                    }
                    pc += 7;
                }
                Opcode::BinLocalLocalInt => {
                    // (r{a} ar1 r{b}) ar2 imm : one dispatch for `(i + j) % 7`
                    // style chains (peephole: BinLocalLocal+LoadInt+AR). Bit 7
                    // of ar2 = keep=0 (folded trailing Pop).
                    let a = self.bytecode[pc + 1] as usize;
                    let b = self.bytecode[pc + 2] as usize;
                    let ar1 = self.bytecode[pc + 3];
                    let imm = self.read_i32(pc + 4) as i64;
                    let ar2 = self.bytecode[pc + 8];
                    let keep = ar2 & 0x80 == 0;
                    let ar2 = ar2 & 0x7F;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let (mid, fast) = alu_local_local(&self.stack, base + a, base + b, ar1);
                    let mid = if fast {
                        mid
                    } else {
                        let va = if base + a < self.stack.len() {
                            self.slot_value(base + a)
                        } else {
                            Value::undefined()
                        };
                        let vb = if base + b < self.stack.len() {
                            self.slot_value(base + b)
                        } else {
                            Value::undefined()
                        };
                        arith_apply(&va, &vb, ar1)
                    };
                    let result = arith_apply(&mid, &Value::int(imm), ar2);
                    if keep {
                        self.push(result);
                    }
                    pc += 9;
                }
                Opcode::ArithChain => {
                    // Register-ALU chain: ONE dispatch for an int-arithmetic
                    // tree (`3 * n + 1`, `(lo + hi) % 2`, `seed = (seed *
                    // 48271) % 2147483648`, `x op= chain`). The running value
                    // stays in an i64 register, boxed once at the end (or not
                    // at all when the terminal stores straight to a local).
                    // Non-int values and bitwise/shift/pow steps fall back to
                    // the generic Value path per step, so semantics are
                    // exactly those of the plain opcode sequence.
                    //
                    // Lean decode: the step bytes are copied once (a single
                    // bounds-checked slice read) into a stack array, then
                    // decoded with unchecked indexing — a per-step
                    // `read_u32` (4 bounds-checked loads) was ~3x the cost of
                    // the arithmetic itself and made the fusion a net loss.
                    let count = self.bytecode[pc + 1] as usize;
                    let term = self.bytecode[pc + 2];
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let n = count * 5;
                    let mut ops = [0u8; 125];
                    ops[..n]
                        .copy_from_slice(&self.bytecode[pc + 3..pc + 3 + n]);
                    let mut j = 0;
                    let mut acc_i: i64 = 0;
                    let mut acc: Value = Value::undefined();
                    let mut ok = true; // acc held in acc_i
                    while j < count {
                        let h = ops[j * 5];
                        // enc_ar = arith_code + 1; 0 = init marker (a + b must
                        // not collide with the init step's ar=0).
                        let enc_ar = h & 0x1F;
                        let kind = h >> 5;
                        let u = ((ops[j * 5 + 1] as u32) << 24)
                            | ((ops[j * 5 + 2] as u32) << 16)
                            | ((ops[j * 5 + 3] as u32) << 8)
                            | (ops[j * 5 + 4] as u32);
                        let imm = if u & 0x8000_0000 != 0 { u as i32 as i64 } else { u as i64 };
                        j += 1;
                        match kind {
                            0 => {
                                // LoadLocal.
                                let idx = base + (u & 0xFF) as usize;
                                let mut v = if idx < self.stack.len() {
                                    self.stack.at(idx).clone()
                                } else {
                                    Value::undefined()
                                };
                                let deref = v.as_cell().map(|c| c.borrow().clone());
                                if let Some(val) = deref {
                                    v = val;
                                }
                                if enc_ar == 0 {
                                    // Init: acc = v.
                                    if ok {
                                        if let Some(b) = v.as_int() {
                                            acc_i = b;
                                            continue;
                                        }
                                    }
                                    acc = v;
                                    ok = false;
                                } else if ok {
                                    let ar = enc_ar - 1;
                                    if let Some(b) = v.as_int() {
                                        if let Some(res) = chain_arith_i64(acc_i, b, ar) {
                                            match res.as_int() {
                                                Some(r) => acc_i = r,
                                                None => {
                                                    acc = res;
                                                    ok = false;
                                                }
                                            }
                                            continue;
                                        }
                                    }
                                    acc = arith_apply(&Value::int(acc_i), &v, ar);
                                    ok = false;
                                } else {
                                    acc = arith_apply(&acc, &v, enc_ar - 1);
                                }
                            }
                            1 => {
                                // Const: the i64 is already in hand — no
                                // boxing round-trip through Value.
                                if enc_ar == 0 {
                                    acc_i = imm;
                                    ok = true;
                                } else if ok {
                                    let ar = enc_ar - 1;
                                    if let Some(res) = chain_arith_i64(acc_i, imm, ar) {
                                        match res.as_int() {
                                            Some(r) => acc_i = r,
                                            None => {
                                                acc = res;
                                                ok = false;
                                            }
                                        }
                                        continue;
                                    }
                                    acc = arith_apply(&Value::int(acc_i), &Value::int(imm), ar);
                                    ok = false;
                                } else {
                                    acc = arith_apply(&acc, &Value::int(imm), enc_ar - 1);
                                }
                            }
                            2 => {
                                // Save: push the current acc.
                                let v = if ok { Value::int(acc_i) } else { acc.clone() };
                                self.push(v);
                            }
                            _ => {
                                // Combine: acc = t ar acc.
                                let ar = enc_ar - 1;
                                let t = self.pop();
                                if ok {
                                    if let Some(b) = t.as_int() {
                                        if let Some(res) = chain_arith_i64(b, acc_i, ar) {
                                            match res.as_int() {
                                                Some(r) => acc_i = r,
                                                None => {
                                                    acc = res;
                                                    ok = false;
                                                }
                                            }
                                            continue;
                                        }
                                    }
                                    acc = arith_apply(&t, &Value::int(acc_i), ar);
                                    ok = false;
                                } else {
                                    acc = arith_apply(&t, &acc, ar);
                                }
                            }
                        }
                    }
                    let result = if ok { Value::int(acc_i) } else { acc };
                    if term & 0x40 != 0 {
                        self.store_slot(base + (term & 0x3F) as usize, result.clone());
                    }
                    if term & 0x80 != 0 {
                        self.push(result);
                    }
                    pc = pc + 3 + n;
                }
                Opcode::Arith2StoreLocalConst => {
                    // t = locals[slot] ar imm; store back to slot — ONE
                    // dispatch for `n = n / 2`, `j -= 1`, `steps += 1`. Bit 7
                    // of ar = keep (the assignment's value is pushed). A
                    // known INT/NUMBER slot runs the whole op in the raw
                    // lane: no as_cell probe, no as_int probe, no probe on
                    // the store.
                    let slot = self.bytecode[pc + 1] as usize;
                    let ar_byte = self.bytecode[pc + 2];
                    let keep = ar_byte & 0x80 != 0;
                    let ar = ar_byte & 0x7F;
                    let imm = self.read_i32(pc + 3) as i64;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let (result, fast) = alu_local_imm(&self.stack, idx, imm, ar);
                    let result = if fast {
                        result
                    } else {
                        let mut v = if idx < self.stack.len() {
                            self.stack.at(idx).clone()
                        } else {
                            Value::undefined()
                        };
                        let deref = v.as_cell().map(|c| c.borrow().clone());
                        if let Some(val) = deref {
                            v = val;
                        }
                        chain_step_i64(&v, imm, ar)
                    };
                    self.store_slot(idx, result.clone());
                    if keep {
                        self.push(result);
                    }
                    pc += 7;
                }
                Opcode::Arith3StoreLocalConstConst => {
                    // t = (locals[slot] ar1 imm1) ar2 imm2; store slot — ONE
                    // dispatch for `seed = (seed * 48271) % 2147483648`.
                    let slot = self.bytecode[pc + 1] as usize;
                    let ar1 = self.bytecode[pc + 2];
                    let imm1 = self.read_i32(pc + 3) as i64;
                    let ar2_byte = self.bytecode[pc + 7];
                    let keep = ar2_byte & 0x80 != 0;
                    let ar2 = ar2_byte & 0x7F;
                    let imm2 = self.read_i32(pc + 8) as i64;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let result = match alu2_local_imm_imm(&self.stack, idx, imm1, ar1, imm2, ar2) {
                        Some(r) => r,
                        None => {
                            let mut v = if idx < self.stack.len() {
                                self.stack.at(idx).clone()
                            } else {
                                Value::undefined()
                            };
                            let deref = v.as_cell().map(|c| c.borrow().clone());
                            if let Some(val) = deref {
                                v = val;
                            }
                            let mut result = chain_step_i64(&v, imm1, ar1);
                            result = chain_step_i64(&result, imm2, ar2);
                            result
                        }
                    };
                    self.store_slot(idx, result.clone());
                    if keep {
                        self.push(result);
                    }
                    pc += 12;
                }
                Opcode::Arith3StoreConstLocalConst => {
                    // t = (imm1 ar1 locals[slot]) ar2 imm2; store slot — ONE
                    // dispatch for `n = 3 * n + 1` (constant init on the
                    // left, so non-commutative ar1 still applies correctly).
                    let imm1 = self.read_i32(pc + 1) as i64;
                    let ar1 = self.bytecode[pc + 5];
                    let slot = self.bytecode[pc + 6] as usize;
                    let ar2_byte = self.bytecode[pc + 7];
                    let keep = ar2_byte & 0x80 != 0;
                    let ar2 = ar2_byte & 0x7F;
                    let imm2 = self.read_i32(pc + 8) as i64;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let result = match alu2_imm_local_imm(&self.stack, idx, imm1, ar1, imm2, ar2) {
                        Some(r) => r,
                        None => {
                            let mut v = if idx < self.stack.len() {
                                self.stack.at(idx).clone()
                            } else {
                                Value::undefined()
                            };
                            let deref = v.as_cell().map(|c| c.borrow().clone());
                            if let Some(val) = deref {
                                v = val;
                            }
                            let l = Value::int(imm1);
                            let mut result = if let Some(a) = v.as_int() {
                                chain_step_i64(&l, a, ar1)
                            } else {
                                arith_apply(&l, &v, ar1)
                            };
                            result = chain_step_i64(&result, imm2, ar2);
                            result
                        }
                    };
                    self.store_slot(idx, result.clone());
                    if keep {
                        self.push(result);
                    }
                    pc += 12;
                }
                Opcode::CmpLocalLocal => {
                    // r{a} cmp r{b} : one dispatch for `lo <= hi` style.
                    let a = self.bytecode[pc + 1] as usize;
                    let b = self.bytecode[pc + 2] as usize;
                    let cmp = self.bytecode[pc + 3];
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let ia = base + a;
                    let ib = base + b;
                    let result = if ia < self.stack.len() && ib < self.stack.len() {
                        match (self.stack.kind_of(ia), self.stack.kind_of(ib)) {
                            (KIND_INT, KIND_INT) => cmp_i64(
                                Value::int_bits_raw(self.stack.at(ia).bits()),
                                Value::int_bits_raw(self.stack.at(ib).bits()),
                                cmp,
                            ),
                            (KIND_INT, KIND_NUMBER) => cmp_f64(
                                Value::int_bits_raw(self.stack.at(ia).bits()) as f64,
                                f64::from_bits(self.stack.at(ib).bits()),
                                cmp,
                            ),
                            (KIND_NUMBER, KIND_INT) => cmp_f64(
                                f64::from_bits(self.stack.at(ia).bits()),
                                Value::int_bits_raw(self.stack.at(ib).bits()) as f64,
                                cmp,
                            ),
                            (KIND_NUMBER, KIND_NUMBER) => cmp_f64(
                                f64::from_bits(self.stack.at(ia).bits()),
                                f64::from_bits(self.stack.at(ib).bits()),
                                cmp,
                            ),
                            _ => {
                                compare_values(&self.slot_value(ia), &self.slot_value(ib), cmp)
                            }
                        }
                    } else {
                        let va = if ia < self.stack.len() { self.slot_value(ia) } else { Value::undefined() };
                        let vb = if ib < self.stack.len() { self.slot_value(ib) } else { Value::undefined() };
                        compare_values(&va, &vb, cmp)
                    };
                    self.push(Value::bool(result));
                    pc += 4;
                }
                Opcode::CmpAndLocalLocal => {
                    // r{a} cmp1 r{b} &&/|| r{c} cmp2 r{d} : one dispatch for
                    // `a < b && b < c`. The short-circuit value semantics are
                    // preserved: when the first bool fires (&&: falsy, ||:
                    // truthy) it IS the result and the second comparison is
                    // never evaluated. Bit 7 of cmp2 selects `||`.
                    let a = self.bytecode[pc + 1] as usize;
                    let b = self.bytecode[pc + 2] as usize;
                    let cmp1 = self.bytecode[pc + 3];
                    let c = self.bytecode[pc + 4] as usize;
                    let d = self.bytecode[pc + 5] as usize;
                    let cmp2 = self.bytecode[pc + 6];
                    let or = cmp2 & 0x80 != 0;
                    let cmp2 = cmp2 & 0x7F;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let va = if base + a < self.stack.len() {
                        self.slot_value(base + a)
                    } else {
                        Value::undefined()
                    };
                    let vb = if base + b < self.stack.len() {
                        self.slot_value(base + b)
                    } else {
                        Value::undefined()
                    };
                    let v1 = compare_values(&va, &vb, cmp1);
                    let result = if (or && v1) || (!or && !v1) {
                        v1
                    } else {
                        let vc = if base + c < self.stack.len() {
                            self.slot_value(base + c)
                        } else {
                            Value::undefined()
                        };
                        let vd = if base + d < self.stack.len() {
                            self.slot_value(base + d)
                        } else {
                            Value::undefined()
                        };
                        compare_values(&vc, &vd, cmp2)
                    };
                    self.push(Value::bool(result));
                    pc += 7;
                }
                Opcode::CmpAndLocalInt => {
                    // r{a} cmp1 r{b} &&/|| r{c} cmp2 imm : one dispatch for
                    // `a < b && b < 5` style mixed chains.
                    let a = self.bytecode[pc + 1] as usize;
                    let b = self.bytecode[pc + 2] as usize;
                    let cmp1 = self.bytecode[pc + 3];
                    let c = self.bytecode[pc + 4] as usize;
                    let imm = self.read_i32(pc + 5) as i64;
                    let cmp2 = self.bytecode[pc + 9];
                    let or = cmp2 & 0x80 != 0;
                    let cmp2 = cmp2 & 0x7F;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let va = if base + a < self.stack.len() {
                        self.slot_value(base + a)
                    } else {
                        Value::undefined()
                    };
                    let vb = if base + b < self.stack.len() {
                        self.slot_value(base + b)
                    } else {
                        Value::undefined()
                    };
                    let v1 = compare_values(&va, &vb, cmp1);
                    let result = if (or && v1) || (!or && !v1) {
                        v1
                    } else {
                        let vc = if base + c < self.stack.len() {
                            self.slot_value(base + c)
                        } else {
                            Value::undefined()
                        };
                        compare_values(&vc, &Value::int(imm), cmp2)
                    };
                    self.push(Value::bool(result));
                    pc += 10;
                }
                Opcode::CmpAndIntLocal => {
                    // imm cmp1 r{a} &&/|| r{c} cmp2 r{d} : one dispatch for
                    // `5 < a && b < c` style mixed chains.
                    let a = self.bytecode[pc + 1] as usize;
                    let imm = self.read_i32(pc + 2) as i64;
                    let cmp1 = self.bytecode[pc + 6];
                    let c = self.bytecode[pc + 7] as usize;
                    let d = self.bytecode[pc + 8] as usize;
                    let cmp2 = self.bytecode[pc + 9];
                    let or = cmp2 & 0x80 != 0;
                    let cmp2 = cmp2 & 0x7F;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let va = if base + a < self.stack.len() {
                        self.slot_value(base + a)
                    } else {
                        Value::undefined()
                    };
                    // CmpLocalInt semantics: r{slot} cmp imm (the slot is the
                    // LEFT operand — `<` is not symmetric).
                    let v1 = compare_values(&va, &Value::int(imm), cmp1);
                    let result = if (or && v1) || (!or && !v1) {
                        v1
                    } else {
                        let vc = if base + c < self.stack.len() {
                            self.slot_value(base + c)
                        } else {
                            Value::undefined()
                        };
                        let vd = if base + d < self.stack.len() {
                            self.slot_value(base + d)
                        } else {
                            Value::undefined()
                        };
                        compare_values(&vc, &vd, cmp2)
                    };
                    self.push(Value::bool(result));
                    pc += 10;
                }
                Opcode::CmpAndIntInt => {
                    // imm1 cmp1 r{a} &&/|| r{b} cmp2 imm2 : one dispatch for
                    // `5 < a && a < 10` style chains.
                    let a = self.bytecode[pc + 1] as usize;
                    let imm1 = self.read_i32(pc + 2) as i64;
                    let cmp1 = self.bytecode[pc + 6];
                    let b = self.bytecode[pc + 7] as usize;
                    let imm2 = self.read_i32(pc + 8) as i64;
                    let cmp2 = self.bytecode[pc + 12];
                    let or = cmp2 & 0x80 != 0;
                    let cmp2 = cmp2 & 0x7F;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let va = if base + a < self.stack.len() {
                        self.slot_value(base + a)
                    } else {
                        Value::undefined()
                    };
                    // CmpLocalInt semantics: r{slot} cmp imm on both sides.
                    let v1 = compare_values(&va, &Value::int(imm1), cmp1);
                    let result = if (or && v1) || (!or && !v1) {
                        v1
                    } else {
                        let vb = if base + b < self.stack.len() {
                            self.slot_value(base + b)
                        } else {
                            Value::undefined()
                        };
                        compare_values(&vb, &Value::int(imm2), cmp2)
                    };
                    self.push(Value::bool(result));
                    pc += 13;
                }
                Opcode::ArithStoreLocal => {
                    // [lhs, rhs] -> store (lhs ar rhs) into r{slot}, keep?
                    let slot = self.bytecode[pc + 1] as usize;
                    let ar = self.bytecode[pc + 2];
                    let keep = self.bytecode[pc + 3];
                    let r = self.pop();
                    let l = self.pop();
                    let res = arith_apply(&l, &r, ar);
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    self.store_slot(base + slot, res.clone());
                    if keep != 0 {
                        self.push(res);
                    }
                    pc += 4;
                }
                Opcode::AppendStringConst => {
                    // s = s + "x" / s += "x": one dispatch reads the local,
                    // adds the folded constant (exact `Add` semantics), stores
                    // back, and pushes the result if keep. The builder box
                    // never leaves the local slot.
                    let slot = self.bytecode[pc + 1] as usize;
                    let ci = self.read_u16(pc + 2) as usize;
                    let keep = self.bytecode[pc + 4];
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let l = self.slot_value(idx);
                    let r = self.constants[ci].clone();
                    let res = l.add(&r);
                    self.store_slot(idx, res.clone());
                    if keep != 0 {
                        self.push(res);
                    }
                    pc += 5;
                }
                Opcode::AppendStringLocal => {
                    // s = s + t / s += t: both locals read inside the opcode.
                    let slot = self.bytecode[pc + 1] as usize;
                    let src = self.bytecode[pc + 2] as usize;
                    let keep = self.bytecode[pc + 3];
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let l = self.slot_value(idx);
                    let r = self.slot_value(base + src);
                    let res = l.add(&r);
                    self.store_slot(idx, res.clone());
                    if keep != 0 {
                        self.push(res);
                    }
                    pc += 4;
                }
                Opcode::AppendStringPop => {
                    // s = s + <expr>: the lhs snapshot was pushed before the
                    // RHS evaluated (JS order); pop both, add, store.
                    let slot = self.bytecode[pc + 1] as usize;
                    let keep = self.bytecode[pc + 2];
                    let r = self.pop();
                    let l = self.pop();
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let res = l.add(&r);
                    self.store_slot(idx, res.clone());
                    if keep != 0 {
                        self.push(res);
                    }
                    pc += 3;
                }
                Opcode::IncLocal => {
                    // x++ / ++x / x-- / --x on a local: read, mutate, store,
                    // push old (postfix) or new (prefix) if keep.
                    let slot = self.bytecode[pc + 1] as usize;
                    let flags = self.bytecode[pc + 2];
                    let delta = self.bytecode[pc + 3] as i8;
                    let prefix = flags & 1 != 0;
                    let keep = flags & 2 != 0;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let ar = if delta > 0 { 0u8 } else { 1u8 };
                    let (nv, fast) = alu_local_imm(&self.stack, idx, 1, ar);
                    let (v, nv) = if fast {
                        // Old value is the raw slot word (postfix needs it
                        // before the store below overwrites it).
                        (self.slot_value(idx), nv)
                    } else {
                        let v = if idx < self.stack.len() {
                            self.slot_value(idx)
                        } else {
                            Value::undefined()
                        };
                        (v.clone(), arith_apply(&v, &Value::int(1), ar))
                    };
                    self.store_slot(idx, nv.clone());
                    if keep {
                        self.push(if prefix { nv } else { v });
                    }
                    pc += 4;
                }
                Opcode::ArithStoreUpvalue => {
                    // [lhs, rhs] -> write (lhs ar rhs) into upvalue u, keep?
                    let up = self.bytecode[pc + 1] as usize;
                    let ar = self.bytecode[pc + 2];
                    let keep = self.bytecode[pc + 3];
                    let r = self.pop();
                    let l = self.pop();
                    let res = arith_apply(&l, &r, ar);
                    let cell = self.cells_stack.last().and_then(|c| c.get(up)).cloned();
                    if let Some(cell) = cell {
                        self.note_rc_dirty(RcDirtyRef::Cell(cell.clone()));
                        *cell.borrow_mut() = res.clone();
                    }
                    if keep != 0 {
                        self.push(res);
                    }
                    pc += 4;
                }

                Opcode::And => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(Value::bool(l.is_truthy() && r.is_truthy()));
                    pc += 1;
                }
                Opcode::Or => {
                    let r = self.pop();
                    let l = self.pop();
                    self.push(Value::bool(l.is_truthy() || r.is_truthy()));
                    pc += 1;
                }
                Opcode::Not => {
                    let val = self.pop();
                    self.push(Value::bool(!val.is_truthy()));
                    pc += 1;
                }

                Opcode::Jump => {
                    let target = self.read_u32(pc + 1);
                    let t = target as usize;
                    // Hot-loop hypervisor: count taken back-edges only.
                    if t < pc {
                        let trips = {
                            let c = self.backedge_counts.entry(t).or_insert(0);
                            *c = c.saturating_add(1);
                            *c
                        };
                        if trips >= 100 {
                            if let Some(next_pc) = self.maybe_jit_loop(t, pc) {
                                pc = next_pc;
                                continue;
                            }
                        }
                        if trips == 50_000 && std::env::var("ALLOY_JIT_LOG").is_ok() {
                            eprintln!("[alloy-jit] hot loop pc={:04x} trips={}", t, trips);
                        }
                    }
                    pc = t;
                }
                Opcode::JumpIfFalse => {
                    let val = self.peek();
                    let target = self.read_u32(pc + 1);
                    if !val.is_truthy() {
                        pc = target as usize;
                    } else {
                        pc += 5;
                    }
                }
                Opcode::JumpIfFalsePop => {
                    // Consumes the condition it tests (loop conditions, if /
                    // ternary tests discard it on both paths).
                    let val = self.pop();
                    let target = self.read_u32(pc + 1);
                    if !val.is_truthy() {
                        pc = target as usize;
                    } else {
                        pc += 5;
                    }
                }
                Opcode::JumpIfTruePop => {
                    // Mirror of JumpIfFalsePop for inline `||` short-circuits
                    // in loop conditions: pops the operand and jumps to the
                    // loop body when it is truthy.
                    let val = self.pop();
                    let target = self.read_u32(pc + 1);
                    if val.is_truthy() {
                        pc = target as usize;
                    } else {
                        pc += 5;
                    }
                }
                Opcode::JumpIfTrue => {
                    let val = self.peek();
                    let target = self.read_u32(pc + 1);
                    if val.is_truthy() {
                        pc = target as usize;
                    } else {
                        pc += 5;
                    }
                }
                Opcode::JumpIfNullish => {
                    // Optional chaining: consumes the tested value and jumps
                    // to the short-circuit path when it is null or undefined
                    // (the chain discards its accumulated values and pushes
                    // undefined there).
                    let val = self.pop();
                    let target = self.read_u32(pc + 1);
                    if val.is_null() || val.is_undefined() {
                        pc = target as usize;
                    } else {
                        pc += 5;
                    }
                }

                Opcode::ToIterable => {
                    let v = self.pop();
                    if v.is_array() || v.as_str().is_some() {
                        self.push(v);
                    } else if let Some(od) = v.as_object() {
                        let c = od.borrow().container;
                        if c == 1 {
                            self.push(container_entries(&v));
                        } else if c == 2 {
                            self.push(container_values(&v));
                        } else {
                            let sym_key = format!("\0sym_{}", SYMBOL_ITERATOR);
                            let iter_method = od.borrow().get(&sym_key).cloned();
                            if let Some(im) = iter_method {
                                if im.is_function() || im.is_native() {
                                    let iter_obj = self.call_value_with_this(&im, Some(v.clone()), &[]);
                                    let arr = self.drain_iterator(&iter_obj);
                                    self.push(arr);
                                } else {
                                    self.push(v);
                                }
                            } else if od.borrow().get("next").is_some() {
                                let arr = self.drain_iterator(&v);
                                self.push(arr);
                            } else {
                                match self.throw_value(Value::string(format!(
                                    "TypeError: {} is not iterable",
                                    iterable_display(&v)
                                ))) {
                                    ThrowResult::Jump(p) => pc = p,
                                    ThrowResult::EndDispatch | ThrowResult::Abort => pc = usize::MAX,
                                }
                                continue;
                            }
                        }
                    } else {
                        match self.throw_value(Value::string(format!(
                            "TypeError: {} is not iterable",
                            iterable_display(&v)
                        ))) {
                            ThrowResult::Jump(p) => pc = p,
                            ThrowResult::EndDispatch | ThrowResult::Abort => pc = usize::MAX,
                        }
                        continue;
                    }
                    pc += 1;
                }

                Opcode::MakeRegex => {
                    // /pattern/flags — push a fresh regex value. The pattern
                    // and flags are string constants; the compiled program is
                    // cached per (pattern, flags) and shared (Arc). The lexer
                    // already validated the pattern at compile time, so this
                    // is a cache hit; hand-crafted bytecode that missed
                    // validation throws a catchable SyntaxError instead.
                    let pi = self.read_u16(pc + 1) as usize;
                    let fi = self.read_u16(pc + 3) as usize;
                    let pattern = match self.constants.get(pi) {
                        Some(v) => v.as_str().unwrap_or("").to_string(),
                        None => String::new(),
                    };
                    let flags = match self.constants.get(fi) {
                        Some(v) => v.as_str().unwrap_or("").to_string(),
                        None => String::new(),
                    };
                    let key = (pattern, flags);
                    let compiled = match self.regex_cache.get(&key) {
                        Some(c) => c.clone(),
                        None => match regex::compile_from_str(&key.0, &key.1) {
                            Ok(c) => {
                                let c = Arc::new(c);
                                self.regex_cache.insert(key, c.clone());
                                c
                            }
                            Err(e) => {
                                match self.throw_value(Value::string(format!(
                                    "SyntaxError: invalid regular expression: {e}"
                                ))) {
                                    ThrowResult::Jump(p) => pc = p,
                                    ThrowResult::EndDispatch | ThrowResult::Abort => {
                                        pc = usize::MAX
                                    }
                                }
                                continue;
                            }
                        },
                    };
                    self.push(Value::regex(compiled));
                    pc += 5;
                }

                Opcode::Call | Opcode::CallKeep0 => {
                    let argc = self.bytecode[pc + 1] as usize;
                    let callee = self.pop();
                    pc = self.dispatch_call(
                        callee,
                        argc,
                        pc + 2,
                        matches!(op, Opcode::Call),
                        None,
                        false,
                    );
                    if pc == usize::MAX {
                        break;
                    }
                }
                Opcode::CallMethod | Opcode::CallMethodKeep0 => {
                    // `o.m(args)`: the compiler emitted [receiver, func, args]
                    // — args on TOP, so pop them first, then the func. The
                    // receiver then sits at stack.len()-1 (base-1 after the
                    // args are re-pushed), exactly the frame's this slot.
                    let argc = self.bytecode[pc + 1] as usize;
                    let mut args: Vec<Value> = (0..argc).map(|_| self.pop()).collect();
                    args.reverse();
                    let callee = self.pop();
                    let this_slot = self.stack.len().saturating_sub(1);
                    for a in args {
                        self.push(a);
                    }
                    let keep = matches!(op, Opcode::CallMethod);
                    pc = self.dispatch_call(
                        callee,
                        argc,
                        pc + 2,
                        keep,
                        Some(this_slot),
                        false,
                    );
                    if pc == usize::MAX {
                        break;
                    }
                    // No receiver cleanup here: dispatch_call returns before
                    // the callee body runs. JS functions clean up in Return;
                    // natives clean up inside dispatch_call.
                }
                Opcode::CallMethodSpread | Opcode::CallMethodSpreadKeep0 => {
                    // `o.m(...args)`: same layout as CallMethod, with the
                    // spread positions expanded first.
                    let argc = self.bytecode[pc + 1] as usize;
                    let mask = self.read_u16(pc + 2);
                    let mut vals: Vec<Value> = (0..argc).map(|_| self.pop()).collect();
                    vals.reverse();
                    let vals = self.prepare_spread_values(vals, mask);
                    let callee = self.pop();
                    let this_slot = self.stack.len().saturating_sub(1);
                    let args = expand_spreads(vals, mask);
                    for a in &args {
                        self.push(a.clone());
                    }
                    let keep = matches!(op, Opcode::CallMethodSpread);
                    pc = self.dispatch_call(
                        callee,
                        args.len(),
                        pc + 4,
                        keep,
                        Some(this_slot),
                        false,
                    );
                    if pc == usize::MAX {
                        break;
                    }
                    // No receiver cleanup here: dispatch_call returns before
                    // the callee body runs. JS functions clean up in Return;
                    // natives clean up inside dispatch_call.
                }
                Opcode::New => {
                    // `new C(args)`: build the instance (proto = C.prototype),
                    // put it below the args, and call the constructor with
                    // `this` bound — the frame's ctor-return semantics keep
                    // the instance unless the constructor returns an object.
                    let argc = self.bytecode[pc + 1] as usize;
                    let callee = self.pop();
                    let mut args: Vec<Value> = Vec::with_capacity(argc);
                    for _ in 0..argc {
                        args.push(self.pop());
                    }
                    args.reverse();
                    if let Some(f) = callee.as_native() {
                        // Native constructor (Map/Set): the native builds and
                        // returns the instance itself (it captures its
                        // prototype); there is no `this` to bind.
                        let result = f(&args, self);
                        if let Some(p) = self.native_throw_jump.take() {
                            pc = p;
                            continue;
                        }
                        if self.uncaught_exception.is_some() {
                            pc = usize::MAX;
                            continue;
                        }
                        self.push(result);
                        pc += 2;
                        continue;
                    }
                    let instance = match callee.as_function() {
                        Some(f) => {
                            let proto = f
                                .props
                                .borrow()
                                .as_ref()
                                .and_then(|p| p.borrow().get("prototype").cloned())
                                .unwrap_or(Value::undefined());
                            Value::object_with_proto(proto)
                        }
                        None => {
                            // Not a constructor.
                            match self.throw_value(Value::string(format!(
                                "TypeError: {} is not a constructor",
                                callee.type_name()
                            ))) {
                                ThrowResult::Jump(p) => pc = p,
                                ThrowResult::EndDispatch | ThrowResult::Abort => pc = usize::MAX,
                            }
                            continue;
                        }
                    };
                    self.push(instance);
                    for a in args {
                        self.push(a);
                    }
                    let base_slot = self.stack.len() - argc;
                    let inst_slot = base_slot.saturating_sub(1);
                    pc = self.dispatch_call(
                        callee,
                        argc,
                        pc + 2,
                        true,
                        Some(inst_slot),
                        true,
                    );
                    if pc == usize::MAX {
                        break;
                    }
                    // No receiver cleanup here: dispatch_call returns before
                    // the ctor body runs. The Return opcode (is_ctor frames)
                    // keeps the instance unless the ctor returns an object;
                    // natives clean up inside dispatch_call.
                }
                Opcode::NewSpread => {
                    // `new C(...args)`: like New, but the argument count is
                    // dynamic (the argc byte counts argument SLOTS and the
                    // mask marks the spreads, exactly like CallSpread).
                    let argc = self.bytecode[pc + 1] as usize;
                    let mask = self.read_u16(pc + 2);
                    let callee = self.pop();
                    let mut vals: Vec<Value> = (0..argc).map(|_| self.pop()).collect();
                    vals.reverse();
                    let vals = self.prepare_spread_values(vals, mask);
                    let args = expand_spreads(vals, mask);
                    if let Some(f) = callee.as_native() {
                        let result = f(&args, self);
                        if let Some(p) = self.native_throw_jump.take() {
                            pc = p;
                            continue;
                        }
                        if self.uncaught_exception.is_some() {
                            pc = usize::MAX;
                            continue;
                        }
                        self.push(result);
                        pc += 4;
                        continue;
                    }
                    let instance = match callee.as_function() {
                        Some(f) => {
                            let proto = f
                                .props
                                .borrow()
                                .as_ref()
                                .and_then(|p| p.borrow().get("prototype").cloned())
                                .unwrap_or(Value::undefined());
                            Value::object_with_proto(proto)
                        }
                        None => {
                            match self.throw_value(Value::string(format!(
                                "TypeError: {} is not a constructor",
                                callee.type_name()
                            ))) {
                                ThrowResult::Jump(p) => pc = p,
                                ThrowResult::EndDispatch | ThrowResult::Abort => pc = usize::MAX,
                            }
                            continue;
                        }
                    };
                    self.push(instance);
                    for a in &args {
                        self.push(a.clone());
                    }
                    let base_slot = self.stack.len() - args.len();
                    let inst_slot = base_slot.saturating_sub(1);
                    pc = self.dispatch_call(
                        callee,
                        args.len(),
                        pc + 4,
                        true,
                        Some(inst_slot),
                        true,
                    );
                    if pc == usize::MAX {
                        break;
                    }
                }
                Opcode::CallSpread | Opcode::CallSpreadKeep0 => {
                    let argc = self.bytecode[pc + 1] as usize;
                    let mask = self.read_u16(pc + 2);
                    let callee = self.pop();
                    let mut vals: Vec<Value> = (0..argc).map(|_| self.pop()).collect();
                    vals.reverse();
                    let vals = self.prepare_spread_values(vals, mask);
                    let args = expand_spreads(vals, mask);
                    // Re-push the materialized arguments so the shared dispatch
                    // builds the frame from the operand stack as usual.
                    for a in &args {
                        self.push(a.clone());
                    }
                    pc = self.dispatch_call(
                        callee,
                        args.len(),
                        pc + 4,
                        matches!(op, Opcode::CallSpread),
                        None,
                        false,
                    );
                    if pc == usize::MAX {
                        break;
                    }
                }
                Opcode::Return => {
                    let val = self.pop();
                    if let Some(frame) = self.call_stack.pop() {
                        // A constructor that returns a non-object (or nothing)
                        // yields the fresh instance instead — JS semantics.
                        let val = if frame.is_ctor && !val.is_object_like() {
                            frame
                                .this_slot
                                .map(|s| self.stack.at(s).clone())
                                .unwrap_or(Value::undefined())
                        } else {
                            val
                        };
                        // An async function's return resolves its promise; the
                        // promise (not the raw value) goes back to the caller.
                        let to_caller = if let Some(ps) = frame.promise_slot {
                            let promise = self.stack.at(frame.base_slot + ps as usize).clone();
                            if let Some(p) = promise.as_promise() {
                                self.resolve_promise(p, val.clone());
                            }
                            promise
                        } else {
                            val
                        };
                        self.stack.truncate(frame.base_slot);
                        // Method/ctor receiver cleanup: the receiver sits below
                        // the args (this_slot < base_slot). Remove it so only
                        // the call result remains — JS functions are cleaned up
                        // here; natives are cleaned up inside dispatch_call.
                        if let Some(ts) = frame.this_slot {
                            self.stack.truncate(ts);
                        }
                        self.cells_stack.truncate(frame.cells_len);
                        self.handlers.truncate(frame.handlers_len);
                        // A restored continuation's caller already received the
                        // promise at suspension; end this dispatch instead of
                        // re-running the caller.
                        if let Some(gen_id) = self.active_generator {
                            if frame.generator_id.is_some() || self.call_stack.is_empty() {
                                if let Some(state_rc) = self.generators.get(&gen_id).cloned() {
                                    let mut st = state_rc.borrow_mut();
                                    st.done = true;
                                    st.yielded = false;
                                    st.return_value = to_caller.clone();
                                }
                                self.push(to_caller);
                                break;
                            }
                        }
                        if frame.resumed && self.call_stack.is_empty() {
                            break;
                        }
                        // Statement-position calls (keep=0) don't receive the
                        // result; the callee and args are already consumed.
                        if frame.keep_result {
                            self.push(to_caller);
                        }
                        if frame.return_program != self.program_id {
                            self.load_program(frame.return_program);
                        }
                        pc = frame.return_addr;
                    } else {
                        if let Some(gen_id) = self.active_generator {
                            if let Some(state_rc) = self.generators.get(&gen_id).cloned() {
                                let mut st = state_rc.borrow_mut();
                                st.done = true;
                                st.yielded = false;
                                st.return_value = val.clone();
                            }
                        }
                        self.push(val);
                        break;
                    }
                }

                Opcode::Throw => {
                    self.current_pc = pc;
                    let exc = self.pop();
                    match self.throw_value(exc) {
                        ThrowResult::Jump(p) => pc = p,
                        ThrowResult::EndDispatch => break,
                        ThrowResult::Abort => break,
                    }
                }
                Opcode::TryStart => {
                    let handler_pc = self.read_u32(pc + 1) as usize;
                    self.handlers.push(Handler {
                        stack_depth: self.stack.len(),
                        handler_pc,
                        frame_depth: self.call_stack.len(),
                        program: self.program_id,
                    });
                    pc += 5;
                }
                Opcode::TryEnd => {
                    self.handlers.pop();
                    pc += 1;
                }

                Opcode::NewPromise => {
                    let slot = self.bytecode[pc + 1] as usize;
                    let promise = self.new_promise();
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    self.record_local(idx);
                    while self.stack.len() <= idx {
                        self.stack.push(Value::undefined());
                    }                        *self.stack.at_mut(idx) = promise.clone();
                    self.stack.mark_kind(idx, KIND_OTHER);
                    if let Some(f) = self.call_stack.last_mut() {
                        f.promise_slot = Some(slot as u8);
                    }
                    pc += 2;
                }
                Opcode::Await => {
                    let val = self.pop();
                    if let Some(p) = val.as_promise() {
                        let status = p.lock().unwrap_or_else(|g| g.into_inner()).status.clone();
                        match status {
                            PromiseStatus::Fulfilled(v) => {
                                self.push(v);
                                pc += 1;
                            }
                            // Awaiting a rejected promise throws the
                            // rejection reason, like JS.
                            PromiseStatus::Rejected(v) => {
                                match self.throw_value(v) {
                                    ThrowResult::Jump(p) => pc = p,
                                    ThrowResult::EndDispatch => break,
                                    ThrowResult::Abort => break,
                                }
                            }
                            PromiseStatus::Pending => {
                                    // Find the innermost async invocation and
                                    // suspend it, returning its promise to the
                                    // caller.
                                    let boundary = match self
                                        .call_stack
                                        .iter()
                                        .rposition(|f| f.promise_slot.is_some())
                                    {
                                        Some(i) => i,
                                        None => {
                                            // Defensive: no async frame.
                                            self.push(Value::undefined());
                                            pc += 1;
                                            continue;
                                        }
                                    };
                                    let b = self.call_stack[boundary].clone();
                                    let id = self.next_cont_id;
                                    self.next_cont_id += 1;
                                    self.call_stack[boundary].resumed = true;
                                    // The saved stack starts at the boundary
                                    // frame's base, so rebase the saved frames'
                                    // slots to match (the caller's region below
                                    // is not part of this continuation).
                                    let mut frames: Vec<CallFrame> =
                                        self.call_stack[boundary..].to_vec();
                                    for f in frames.iter_mut() {
                                        f.base_slot -= b.base_slot;
                                        f.cells_len -= b.cells_len;
                                        f.handlers_len -= b.handlers_len;
                                        f.locals_end -= b.base_slot;
                                    }
                                    // The saved frames' exception handlers move
                                    // with the continuation; the caller's stay
                                    // active.
                                    let saved_handlers =
                                        self.handlers[b.handlers_len..].to_vec();
                                    self.handlers.truncate(b.handlers_len);
                                    self.continuations.insert(
                                        id,
                                        Continuation::Suspended {
                                            stack: self.stack.save_from(b.base_slot),
                                            frames,
                                            cells: self.cells_stack[b.cells_len..].to_vec(),
                                            handlers: saved_handlers,
                                            pc: pc + 1,
                                            program_id: self.program_id,
                                        },
                                    );
                                    p.lock()
                                        .unwrap_or_else(|g| g.into_inner())
                                        .continuations
                                        .push(id);
                                    // Return the async invocation's own promise
                                    // to its caller (skipped for keep=0
                                    // statement-position calls, which discard
                                    // it — the continuation still runs).
                                    let own = self.stack.at(b.base_slot + b.promise_slot.unwrap() as usize).clone();
                                    self.stack.truncate(b.base_slot);
                                    self.call_stack.truncate(boundary);
                                    self.cells_stack.truncate(b.cells_len);
                                    if b.keep_result {
                                        self.push(own);
                                    }
                                    pc = b.return_addr;
                                }
                            }
                        } else {
                            // Await on a non-promise: pass through.
                            self.push(val);
                            pc += 1;
                        }
                    }

                Opcode::MakeArray => {
                    let count = self.read_u16(pc + 1) as usize;
                    let mut elements = Vec::with_capacity(count);
                    for _ in 0..count {
                        elements.push(self.pop());
                    }
                    elements.reverse();
                    self.push(Value::array(elements));
                    pc += 3;
                }
                Opcode::MakeArraySpread => {
                    let count = self.read_u16(pc + 1) as usize;
                    let mask = self.read_u16(pc + 3);
                    let mut vals: Vec<Value> = (0..count).map(|_| self.pop()).collect();
                    vals.reverse();
                    let vals = self.prepare_spread_values(vals, mask);
                    let elements = expand_spreads(vals, mask);
                    self.push(Value::array(elements));
                    pc += 5;
                }
                Opcode::MakeRestArray => {
                    let slot = self.bytecode[pc + 1] as usize;
                    let fixed = self.bytecode[pc + 2] as usize;
                    // At function entry the stack top is exactly base + argc,
                    // so everything past the fixed params is the rest.
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let argc = self.stack.len().saturating_sub(base);
                    let mut items = Vec::new();
                    for i in (base + fixed)..(base + argc) {
                        items.push(self.stack.at(i).clone());
                    }
                    let idx = base + slot;
                    self.record_local(idx);
                    while self.stack.len() <= idx {
                        self.stack.push(Value::undefined());
                    }
                    *self.stack.at_mut(idx) = Value::array(items);
                    self.stack.mark_kind(idx, KIND_OTHER);
                    pc += 3;
                }
                Opcode::ArraySlice => {
                    let start = self.bytecode[pc + 1] as usize;
                    let obj = self.pop();
                    let items: Vec<Value> = if let Some(arr) = obj.as_array() {
                        let arr = arr.borrow();
                        arr.to_values().into_iter().skip(start).collect()
                    } else if let Some(s) = obj.as_str() {
                        s.chars()
                            .skip(start)
                            .map(|c| Value::string(c.to_string()))
                            .collect()
                    } else {
                        Vec::new()
                    };
                    self.push(Value::array(items));
                    pc += 2;
                }
                Opcode::MakeObject => {
                    let count = self.read_u16(pc + 1) as usize;
                    let mask = self.read_u16(pc + 3);
                    // Fields are pushed in source order but popped LIFO, so
                    // collect then reverse — object shapes keep JS insertion
                    // order (JSON.stringify and ordered iteration rely on it).
                    // A set mask bit marks a spread: the stack holds ONE value
                    // (the source), whose own enumerable properties are
                    // expanded in place; `{...null}`/`{...undefined}` add
                    // nothing, like JS.
                    let mut pairs: Vec<(String, Value)> = Vec::with_capacity(count + 8);
                    for i in (0..count).rev() {
                        if mask & (1 << i) != 0 {
                            let src = self.pop();
                            if let Some(mut ps) = object_spread_pairs(&src) {
                                // Fields are popped in reverse source order
                                // and the whole list is reversed below, so a
                                // spread's own entries must go in reversed
                                // order here to end up in source order.
                                pairs.extend(ps.drain(..).rev());
                            }
                        } else {
                            let val = self.pop();
                            // Keys are coerced with ToString (JS spec):
                            // computed keys may be numbers/booleans/etc.
                            let key = to_string_js(&self.pop());
                            pairs.push((key, val));
                        }
                    }
                    pairs.reverse();
                    self.push(Value::object_ordered(pairs));
                    pc += 5;
                }
                Opcode::GetProperty => {
                    let idx = self.read_u16(pc + 1);
                    let prop = self.constants[idx as usize].clone();
                    let obj = self.pop();
                    let val = self.get_prop_value(pc, &obj, &prop);
                    if let Some(p) = self.native_throw_jump.take() {
                        pc = p;
                        continue;
                    }
                    if self.uncaught_exception.is_some() {
                        break;
                    }
                    self.push(val);
                    pc += 3;
                }
                Opcode::GetPropertyCell => {
                    // Live-import binds: return the RAW property value (the
                    // cell itself) so StoreGlobal below aliases the module's
                    // own storage. Reads of the bound name then go through
                    // LoadGlobal's cell unwrap and always see the current
                    // value — ESM live-binding semantics.
                    let idx = self.read_u16(pc + 1);
                    let prop = self.constants[idx as usize].clone();
                    let obj = self.pop();
                    let val = self.get_prop_cell_value(&obj, &prop);
                    self.push(val);
                    pc += 3;
                }
                Opcode::LoadLocalGetPropConst => {
                    // `local.prop` (const prop): one dispatch instead of
                    // LoadLocal + LoadConst + GetProperty. The hottest case
                    // is `arr.length` in loop conditions; objects take the
                    // same monomorphic-IC path as GetProperty.
                    let slot = self.bytecode[pc + 1] as usize;
                    let idx = self.read_u16(pc + 2);
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let obj = if base + slot < self.stack.len() {
                        self.slot_value(base + slot)
                    } else {
                        Value::undefined()
                    };
                    let prop = self.constants[idx as usize].clone();
                    let val = self.get_prop_value(pc, &obj, &prop);
                    if let Some(p) = self.native_throw_jump.take() {
                        pc = p;
                        continue;
                    }
                    if self.uncaught_exception.is_some() {
                        break;
                    }
                    self.push(val);
                    pc += 4;
                }
                Opcode::LoadLocalLocalGetIndex => {
                    // `a[i]` with both operands locals: one dispatch instead
                    // of LoadLocal + LoadLocal + GetIndex — the packed-int
                    // array path stays in a single hot opcode.
                    let obj_slot = self.bytecode[pc + 1] as usize;
                    let idx_slot = self.bytecode[pc + 2] as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let obj = if base + obj_slot < self.stack.len() {
                        self.slot_value(base + obj_slot)
                    } else {
                        Value::undefined()
                    };
                    let idx = if base + idx_slot < self.stack.len() {
                        self.slot_value(base + idx_slot)
                    } else {
                        Value::undefined()
                    };
                    let val = self.get_index_value(&obj, &idx);
                    self.push(val);
                    pc += 3;
                }
                Opcode::CompoundPropConst => {
                    // [obj] -> (obj.p = obj.p ar imm): one dispatch for
                    // `o.a += 1`. The RHS is a compile-time constant, so the
                    // JS read-old-before-RHS order is unobservably preserved.
                    // ar bits 0-3 = arith code (0-10), bit 4 = keep result
                    // (0 in statement context).
                    let ar = self.bytecode[pc + 1];
                    let keep = ar & 16 != 0;
                    let ar = ar & 15;
                    let idx = self.read_u16(pc + 2);
                    let imm = self.read_i32(pc + 4) as i64;
                    let prop = self.constants[idx as usize].clone();
                    let obj = self.pop();
                    let old = self.get_prop_value(pc, &obj, &prop);
                    let new = arith_apply(&old, &Value::int(imm), ar);
                    self.set_prop_value(pc, &obj, &prop, new.clone());
                    if keep {
                        self.push(new);
                    }
                    pc += 8;
                }
                Opcode::PeekProperty => {
                    // [obj] -> [obj, obj.p]: read without popping, so the RHS
                    // can evaluate before the write (JS order) and the obj
                    // evaluates exactly once.
                    let idx = self.read_u16(pc + 1);
                    let prop = self.constants[idx as usize].clone();
                    let obj = self.peek();
                    let val = self.get_prop_value(pc, &obj, &prop);
                    self.push(val);
                    pc += 3;
                }
                Opcode::ArithWriteProp => {
                    // [obj, old, rhs] -> (obj.p = old ar rhs); ar bits 0-3 =
                    // arith code (0-10), bit 4 = keep result.
                    let ar = self.bytecode[pc + 1];
                    let keep = ar & 16 != 0;
                    let ar = ar & 15;
                    let idx = self.read_u16(pc + 2);
                    let prop = self.constants[idx as usize].clone();
                    let rhs = self.pop();
                    let old = self.pop();
                    let obj = self.pop();
                    let new = arith_apply(&old, &rhs, ar);
                    self.set_prop_value(pc, &obj, &prop, new.clone());
                    if keep {
                        self.push(new);
                    }
                    pc += 4;
                }
                Opcode::SetProperty => {
                    let prop = self.pop();
                    let obj = self.pop();
                    let val = self.pop();
                    self.set_prop_value(pc, &obj, &prop, val);
                    if let Some(p) = self.native_throw_jump.take() {
                        pc = p;
                        continue;
                    }
                    if self.uncaught_exception.is_some() {
                        break;
                    }
                    pc += 1;
                }
                Opcode::SetAccessor => {
                    // Class getter/setter install: pops [name, obj, fn] and
                    // stores fn as the getter (kind 1) or setter (kind 2) of
                    // obj[name]. Plain SetProperty can't do this — accessors
                    // live in ObjectData.accessors, never the shape (see
                    // get_prop/set_prop).
                    let kind = self.bytecode[pc + 1];
                    let name = self.pop();
                    let obj = self.pop();
                    let f = self.pop();
                    if let Some(m) = obj.as_object() {
                        if let Some(n) = name.as_str() {
                            let mut m = m.borrow_mut();
                            let accs = m.accessors.get_or_insert_with(Default::default);
                            let e = accs
                                .entry(n.to_string())
                                .or_insert((Value::undefined(), Value::undefined()));
                            if kind == 1 {
                                e.0 = f;
                            } else {
                                e.1 = f;
                            }
                        }
                    }
                    pc += 2;
                }
                Opcode::IncPropConst => {
                    // [obj] -> (prefix ? new : old), new = obj.p ± 1 written
                    // back (pushed only if keep; flags bit 2). Inc/dec has no
                    // RHS, so one dispatch is fully spec-correct: obj
                    // evaluated, old read, write.
                    let flags = self.bytecode[pc + 1];
                    let keep = flags & 4 != 0;
                    let idx = self.read_u16(pc + 2);
                    let prop = self.constants[idx as usize].clone();
                    let delta = if flags & 2 != 0 { -1 } else { 1 };
                    let prefix = flags & 1 != 0;
                    let obj = self.pop();
                    let old = self.get_prop_value(pc, &obj, &prop);
                    let new = arith_apply(&old, &Value::int(delta), 0);
                    self.set_prop_value(pc, &obj, &prop, new.clone());
                    if keep {
                        self.push(if prefix { new } else { old });
                    }
                    pc += 4;
                }
                Opcode::IncIndexConst => {
                    // [obj, idx] -> (prefix ? new : old), new = obj[idx] ± 1
                    // written back (pushed only if keep; flags bit 2). The obj
                    // and index each evaluate exactly once and stay on the
                    // stack.
                    let flags = self.bytecode[pc + 1];
                    let keep = flags & 4 != 0;
                    let delta = if flags & 2 != 0 { -1 } else { 1 };
                    let prefix = flags & 1 != 0;
                    let idx = self.pop();
                    let obj = self.pop();
                    let old = self.get_index_value(&obj, &idx);
                    let new = arith_apply(&old, &Value::int(delta), 0);
                    self.set_index_value(&obj, &idx, new.clone());
                    if keep {
                        self.push(if prefix { new } else { old });
                    }
                    pc += 2;
                }
                Opcode::GetIndex => {
                    let idx = self.pop();
                    let obj = self.pop();
                    let val = if obj.is_proxy() {
                        self.get_prop_value(pc, &obj, &idx)
                    } else {
                        self.get_index_value(&obj, &idx)
                    };
                    if let Some(p) = self.native_throw_jump.take() {
                        pc = p;
                        continue;
                    }
                    if self.uncaught_exception.is_some() {
                        break;
                    }
                    self.push(val);
                    pc += 1;
                }
                Opcode::CompoundIndexConst => {
                    // [obj, idx] -> (obj[idx] = obj[idx] ar imm): one dispatch
                    // for `keyed[key] += 1`. The RHS is a compile-time
                    // constant, so the read-old-before-RHS order is preserved.
                    // ar bits 0-3 = arith code (0-10), bit 4 = keep result.
                    let ar = self.bytecode[pc + 1];
                    let keep = ar & 16 != 0;
                    let ar = ar & 15;
                    let imm = self.read_i32(pc + 2) as i64;
                    let idx = self.pop();
                    let obj = self.pop();
                    let old = self.get_index_value(&obj, &idx);
                    let new = arith_apply(&old, &Value::int(imm), ar);
                    self.set_index_value(&obj, &idx, new.clone());
                    if keep {
                        self.push(new);
                    }
                    pc += 6;
                }
                Opcode::PeekIndex => {
                    // [obj, idx] -> [obj, idx, obj[idx]]: read without popping,
                    // so the RHS evaluates before the write (JS order).
                    let idx = self.peek();
                    let obj = self.stack.at(self.stack.len() - 2).clone();
                    let val = self.get_index_value(&obj, &idx);
                    self.push(val);
                    pc += 1;
                }
                Opcode::ArithWriteIndex => {
                    // [obj, idx, old, rhs] -> (obj[idx] = old ar rhs); ar bits
                    // 0-3 = arith code (0-10), bit 4 = keep result.
                    let ar = self.bytecode[pc + 1];
                    let keep = ar & 16 != 0;
                    let ar = ar & 15;
                    let rhs = self.pop();
                    let old = self.pop();
                    let idx = self.pop();
                    let obj = self.pop();
                    let new = arith_apply(&old, &rhs, ar);
                    self.set_index_value(&obj, &idx, new.clone());
                    if keep {
                        self.push(new);
                    }
                    pc += 2;
                }
                Opcode::GetKeys => {
                    let obj = self.pop();
                    let keys: Vec<Value> = match obj.as_object() {
                        Some(m) => {
                            let m = m.borrow();
                            // Deterministic order (hash maps are unordered).
                            m.keys_live()
                                .into_iter()
                                .map(|k| Value::string(k.clone()))
                                .collect()
                        }
                        None => Vec::new(),
                    };
                    self.push(Value::array(keys));
                    pc += 1;
                }
                Opcode::SetIndex => {
                    let idx = self.pop();
                    let obj = self.pop();
                    let val = self.pop();
                    if obj.is_proxy() {
                        self.set_prop_value(pc, &obj, &idx, val);
                    } else {
                        self.set_index_value(&obj, &idx, val);
                    }
                    if let Some(p) = self.native_throw_jump.take() {
                        pc = p;
                        continue;
                    }
                    if self.uncaught_exception.is_some() {
                        break;
                    }
                    pc += 1;
                }

                // ---- condition fusions (compare/jump in one dispatch) ----
                Opcode::CmpLocalIntJumpIfFalsePop => {
                    // r{slot} cmp imm, jump on falsy: `while (n !== 1)`.
                    let slot = self.bytecode[pc + 1] as usize;
                    let imm = self.read_i32(pc + 2) as i64;
                    let cmp = self.bytecode[pc + 6];
                    let target = self.read_u32(pc + 7) as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let result = if idx < self.stack.len() {
                        match self.stack.kind_of(idx) {
                            KIND_INT => cmp_i64(
                                Value::int_bits_raw(self.stack.at(idx).bits()),
                                imm,
                                cmp,
                            ),
                            KIND_NUMBER => cmp_f64(
                                f64::from_bits(self.stack.at(idx).bits()),
                                imm as f64,
                                cmp,
                            ),
                            _ => compare_values(&self.slot_value(idx), &Value::int(imm), cmp),
                        }
                    } else {
                        compare_values(&Value::undefined(), &Value::int(imm), cmp)
                    };
                    if result {
                        pc += 11;
                    } else {
                        pc = target;
                    }
                }
                Opcode::CmpLocalLocalJumpIfFalsePop => {
                    // r{a} cmp r{b}, jump on falsy: `for (j = lo; j < hi; …)`.
                    let a = self.bytecode[pc + 1] as usize;
                    let b = self.bytecode[pc + 2] as usize;
                    let cmp = self.bytecode[pc + 3];
                    let target = self.read_u32(pc + 4) as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let ia = base + a;
                    let ib = base + b;
                    let result = if ia < self.stack.len() && ib < self.stack.len() {
                        match (self.stack.kind_of(ia), self.stack.kind_of(ib)) {
                            (KIND_INT, KIND_INT) => cmp_i64(
                                Value::int_bits_raw(self.stack.at(ia).bits()),
                                Value::int_bits_raw(self.stack.at(ib).bits()),
                                cmp,
                            ),
                            (KIND_INT, KIND_NUMBER) => cmp_f64(
                                Value::int_bits_raw(self.stack.at(ia).bits()) as f64,
                                f64::from_bits(self.stack.at(ib).bits()),
                                cmp,
                            ),
                            (KIND_NUMBER, KIND_INT) => cmp_f64(
                                f64::from_bits(self.stack.at(ia).bits()),
                                Value::int_bits_raw(self.stack.at(ib).bits()) as f64,
                                cmp,
                            ),
                            (KIND_NUMBER, KIND_NUMBER) => cmp_f64(
                                f64::from_bits(self.stack.at(ia).bits()),
                                f64::from_bits(self.stack.at(ib).bits()),
                                cmp,
                            ),
                            _ => compare_values(&self.slot_value(ia), &self.slot_value(ib), cmp),
                        }
                    } else {
                        let va = if ia < self.stack.len() { self.slot_value(ia) } else { Value::undefined() };
                        let vb = if ib < self.stack.len() { self.slot_value(ib) } else { Value::undefined() };
                        compare_values(&va, &vb, cmp)
                    };
                    if result {
                        pc += 8;
                    } else {
                        if target < pc {
                            let trips = {
                                let c = self.backedge_counts.entry(target).or_insert(0);
                                *c = c.saturating_add(1);
                                *c
                            };
                            if trips >= 100 {
                                if let Some(next_pc) = self.maybe_jit_loop(target, pc) {
                                    pc = next_pc;
                                    continue;
                                }
                            }
                        }
                        pc = target;
                    }
                }
                Opcode::LoadIndexCmpLocalJumpIfFalsePop => {
                    // a[objs][idxs] cmp kslot, jump on falsy: `a[j] > key`.
                    let obj_slot = self.bytecode[pc + 1] as usize;
                    let idx_slot = self.bytecode[pc + 2] as usize;
                    let k_slot = self.bytecode[pc + 3] as usize;
                    let cmp = self.bytecode[pc + 4];
                    let target = self.read_u32(pc + 5) as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let obj = if base + obj_slot < self.stack.len() {
                        self.slot_value(base + obj_slot)
                    } else {
                        Value::undefined()
                    };
                    let idx = if base + idx_slot < self.stack.len() {
                        self.slot_value(base + idx_slot)
                    } else {
                        Value::undefined()
                    };
                    let v = self.get_index_value(&obj, &idx);
                    let k = if base + k_slot < self.stack.len() {
                        self.slot_value(base + k_slot)
                    } else {
                        Value::undefined()
                    };
                    let result = compare_values(&v, &k, cmp_semantic(
                        Opcode::from_u8(cmp).unwrap_or(Opcode::StrictEqual),
                    ));
                    if result {
                        pc += 9;
                    } else {
                        pc = target;
                    }
                }
                Opcode::ArithLocalIntCmpJumpIfFalsePop => {
                    // (r{slot} ar imm1) cmp imm2, jump on falsy:
                    // `n % 2 === 0`.
                    let slot = self.bytecode[pc + 1] as usize;
                    let imm1 = self.read_i32(pc + 2) as i64;
                    let ar = self.bytecode[pc + 6];
                    let imm2 = self.read_i32(pc + 7) as i64;
                    let cmp = self.bytecode[pc + 11];
                    let target = self.read_u32(pc + 12) as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let (res, fast) = alu_local_imm(&self.stack, idx, imm1, ar);
                    let cmp = cmp_semantic(Opcode::from_u8(cmp).unwrap_or(Opcode::StrictEqual));
                    let result = if fast {
                        compare_values(&res, &Value::int(imm2), cmp)
                    } else {
                        let l = if idx < self.stack.len() {
                            self.slot_value(idx)
                        } else {
                            Value::undefined()
                        };
                        let v = arith_apply(&l, &Value::int(imm1), ar);
                        compare_values(&v, &Value::int(imm2), cmp)
                    };
                    if result {
                        pc += 16;
                    } else {
                        pc = target;
                    }
                }

                // ---- index-write fusions (swap/shift shapes) ----
                Opcode::SetIndexLocalLocal => {
                    // arr[objs][idxs] = vslot (`arr[i] = tmp`).
                    let obj_slot = self.bytecode[pc + 1] as usize;
                    let idx_slot = self.bytecode[pc + 2] as usize;
                    let v_slot = self.bytecode[pc + 3] as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let obj = if base + obj_slot < self.stack.len() {
                        self.slot_value(base + obj_slot)
                    } else {
                        Value::undefined()
                    };
                    let idx = if base + idx_slot < self.stack.len() {
                        self.slot_value(base + idx_slot)
                    } else {
                        Value::undefined()
                    };
                    let val = if base + v_slot < self.stack.len() {
                        self.slot_value(base + v_slot)
                    } else {
                        Value::undefined()
                    };
                    self.set_index_value(&obj, &idx, val);
                    pc += 4;
                }
                Opcode::SetIndexLocalGetLocal => {
                    // arr[objs][idxs] = brr[vobjs][vidxs]
                    // (`arr[i] = arr[j]` swap write).
                    let obj_slot = self.bytecode[pc + 1] as usize;
                    let idx_slot = self.bytecode[pc + 2] as usize;
                    let v_obj_slot = self.bytecode[pc + 3] as usize;
                    let v_idx_slot = self.bytecode[pc + 4] as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let obj = if base + obj_slot < self.stack.len() {
                        self.slot_value(base + obj_slot)
                    } else {
                        Value::undefined()
                    };
                    let idx = if base + idx_slot < self.stack.len() {
                        self.slot_value(base + idx_slot)
                    } else {
                        Value::undefined()
                    };
                    let vobj = if base + v_obj_slot < self.stack.len() {
                        self.slot_value(base + v_obj_slot)
                    } else {
                        Value::undefined()
                    };
                    let vidx = if base + v_idx_slot < self.stack.len() {
                        self.slot_value(base + v_idx_slot)
                    } else {
                        Value::undefined()
                    };
                    let val = self.get_index_value(&vobj, &vidx);
                    self.set_index_value(&obj, &idx, val);
                    pc += 5;
                }
                Opcode::SetIndexLocalPlusIntLocalGetLocal => {
                    // arr[objs][idxs + imm] = brr[vobjs][vidxs]
                    // (`a[j + 1] = a[j]` shift).
                    let obj_slot = self.bytecode[pc + 1] as usize;
                    let idx_slot = self.bytecode[pc + 2] as usize;
                    let ar = self.bytecode[pc + 3];
                    let imm = self.read_i32(pc + 4) as i64;
                    let v_obj_slot = self.bytecode[pc + 8] as usize;
                    let v_idx_slot = self.bytecode[pc + 9] as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let obj = if base + obj_slot < self.stack.len() {
                        self.slot_value(base + obj_slot)
                    } else {
                        Value::undefined()
                    };
                    let widx = if base + v_idx_slot < self.stack.len() {
                        self.slot_value(base + v_idx_slot)
                    } else {
                        Value::undefined()
                    };
                    let wobj = if base + v_obj_slot < self.stack.len() {
                        self.slot_value(base + v_obj_slot)
                    } else {
                        Value::undefined()
                    };
                    let val = self.get_index_value(&wobj, &widx);
                    let (new_idx, fast) = alu_local_imm(&self.stack, base + idx_slot, imm, ar);
                    let new_idx = if fast {
                        new_idx
                    } else {
                        let l = if base + idx_slot < self.stack.len() {
                            self.slot_value(base + idx_slot)
                        } else {
                            Value::undefined()
                        };
                        arith_apply(&l, &Value::int(imm), ar)
                    };
                    self.set_index_value(&obj, &new_idx, val);
                    pc += 10;
                }

                Opcode::Pop => { self.pop(); pc += 1; }
                Opcode::Dup => {
                    let val = self.peek();
                    self.push(val);
                    pc += 1;
                }

                Opcode::TypeOf => {
                    let val = self.pop();
                    self.push(Value::string(val.type_name().to_string()));
                    pc += 1;
                }
                Opcode::Print => {
                    let val = self.pop();
                    let s = format!("{}\n", val);
                    print!("{}", s);
                    pc += 1;
                }

                Opcode::AllocShared => {
                    let size = self.read_u16(pc + 1) as usize;
                    // Allocate from the sidecar segment (never leaked) instead
                    // of a raw heap allocation that can never be freed.
                    match self.shared.bump(size) {
                        Ok(offset) => {
                            let ptr = unsafe { self.shared.raw_ptr().add(offset) };
                            self.push(Value::buffer(ptr, size));
                        }
                        Err(e) => {
                            eprintln!("alloy shared memory error: {}", e);
                            self.push(Value::undefined());
                        }
                    }
                    pc += 3;
                }
                Opcode::ReadShared => {
                    let offset = self.read_u16(pc + 1) as usize;
                    let len = self.read_u16(pc + 3) as usize;
                    if let Ok(slice) = self.shared.read(offset, len) {
                        let s = String::from_utf8_lossy(slice).to_string();
                        self.push(Value::string(s));
                    } else {
                        self.push(Value::undefined());
                    }
                    pc += 5;
                }
                Opcode::WriteShared => {
                    pc += 5;
                }

                Opcode::Send => { pc += 1; }
                Opcode::Receive => { self.push(Value::undefined()); pc += 1; }
                Opcode::Spawn => {
                    // Pop a function and push a promise that resolves with its
                    // result: the task runs as its own isolated frame on the
                    // event loop, exactly like `spawn(fn)` the native.
                    let f = self.pop();
                    let p = self.vm_spawn_fn(&f, &[]);
                    self.push(p);
                    pc += 1;
                }

                Opcode::LoadPython => {
                    let idx = self.read_u16(pc + 1) as usize;
                    let src = match self.constants.get(idx).and_then(|v| v.as_str()) {
                        Some(s) => s.to_string(),
                        None => {
                            self.push(Value::undefined());
                            pc += 3;
                            continue;
                        }
                    };
                    let m = self.python_module(&src);
                    self.push(m);
                    pc += 3;
                }

                Opcode::CaptureLocal => {
                    let slot = self.bytecode[pc + 1] as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + slot;
                    let v = if idx < self.stack.len() {
                        self.stack.at(idx).clone()
                    } else {
                        Value::undefined()
                    };
                    if v.is_cell() {
                        self.push(v);
                    } else {
                        // Wrap the current value in a cell and write it back
                        // to the slot so the enclosing function sees later
                        // mutations through the closure. The slot and the
                        // pushed capture share the same cell.
                        let cell = Value::cell(v);
                        self.record_local(idx);
                        if idx < self.stack.len() {
                            *self.stack.at_mut(idx) = cell.clone();
                        } else {
                            while self.stack.len() <= idx {
                                self.stack.push(Value::undefined());
                            }
                            *self.stack.at_mut(idx) = cell.clone();
                        }
                        // The slot now holds a cell, not the value — the
                        // int/number fast lanes must not fire on it.
                        self.stack.mark_kind(idx, KIND_OTHER);
                        self.push(cell);
                    }
                    pc += 2;
                }
                Opcode::CaptureUpvalue => {
                    let i = self.bytecode[pc + 1] as usize;
                    let v = self
                        .cells_stack
                        .last()
                        .and_then(|c| c.get(i))
                        .cloned()
                        .map(Value::cell_rc)
                        .unwrap_or(Value::undefined());
                    self.push(v);
                    pc += 2;
                }
                Opcode::WrapCell => {
                    // Arrow lexical capture: freeze the pushed `this` /
                    // `arguments` value in a fresh cell so NewClosure (which
                    // pops captures assuming they are already cells) keeps
                    // it instead of substituting undefined.
                    let v = self.pop();
                    self.push(Value::cell(v));
                    pc += 1;
                }
                Opcode::NewClosure => {
                    let ci = self.read_u16(pc + 1) as usize;
                    let count = self.bytecode[pc + 3] as usize;
                    let params = self.bytecode[pc + 4] as usize;
                    let uses_args = self.bytecode[pc + 5] as usize;
                    let ptr = match self.constants.get(ci) {
                        Some(v) => v.to_number(),
                        None => 0.0,
                    };
                    let mut cells = Vec::with_capacity(count);
                    for _ in 0..count {
                        let c = self.pop();
                        if let Some(cell) = c.as_cell_rc() {
                            cells.push(cell);
                        } else {
                            cells.push(Rc::new(RefCell::new(Value::undefined())));
                        }
                    }
                    // Captures were pushed in upvalue-index order, so pop()
                    // reversed them; restore the compiler's ordering.
                    cells.reverse();
                    self.push(Value::function(FunctionData {
                        program: self.program_id,
                        ptr: ptr as usize,
                        params: params as u8,
                        uses_args: (uses_args & 1) as u8,
                        is_generator: (uses_args & 2) != 0,
                        cells,
                        props: RefCell::new(None),
                    }));
                    pc += 6;
                }
                Opcode::LoadUpvalue => {
                    let i = self.bytecode[pc + 1] as usize;
                    let v = self
                        .cells_stack
                        .last()
                        .and_then(|c| c.get(i))
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::undefined());
                    self.push(v);
                    pc += 2;
                }
                Opcode::StoreUpvalue => {
                    let i = self.bytecode[pc + 1] as usize;
                    let val = self.pop();
                    let cell = self.cells_stack.last().and_then(|c| c.get(i)).cloned();
                    if let Some(cell) = cell {
                        self.note_rc_dirty(RcDirtyRef::Cell(cell.clone()));
                        *cell.borrow_mut() = val;
                    }
                    pc += 2;
                }
                Opcode::LoadCell => {
                    let i = self.bytecode[pc + 1] as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let v = if base + i < self.stack.len() {
                        self.stack.at(base + i).clone()
                    } else {
                        Value::undefined()
                    };
                    self.push(v);
                    pc += 2;
                }
                Opcode::StoreCell => {
                    let i = self.bytecode[pc + 1] as usize;
                    let val = self.pop();
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let idx = base + i;
                    self.record_local(idx);
                    let k = kind_of_value(&val);
                    if idx < self.stack.len() {
                        *self.stack.at_mut(idx) = val;
                    } else {
                        while self.stack.len() <= idx {
                            self.stack.push(Value::undefined());
                        }
                        *self.stack.at_mut(idx) = val;
                    }
                    self.stack.mark_kind(idx, k);
                    pc += 2;
                }
                Opcode::LoadSelf => {
                    let v = self
                        .call_stack
                        .last()
                        .map(|f| f.fn_value.clone())
                        .unwrap_or(Value::undefined());
                    self.push(v);
                    pc += 1;
                }
                Opcode::LoadThis => {
                    let v = self
                        .call_stack
                        .last()
                        .and_then(|f| f.this_slot)
                        .map(|s| self.stack.at(s).clone())
                        .unwrap_or(Value::undefined());
                    self.push(v);
                    pc += 1;
                }
                Opcode::LoadArguments => {
                    // The `arguments` object: an array snapshot of the passed
                    // args (extra args beyond the params count are included;
                    // missing params are not). An array gives `.length` and
                    // for-of/spread for free. The snapshot was taken at call
                    // entry (the body's local stores clobber the arg slots);
                    // fall back to a live stack read only for frames that
                    // never took one (legacy paths). Outside any function the
                    // compiler never emits this.
                    let v = match self.call_stack.last() {
                        Some(f) => {
                            let values: Vec<Value> = match &f.arg_values {
                                Some(v) => v.clone(),
                                None => (0..f.argc)
                                    .map(|i| self.stack.at(f.base_slot + i).clone())
                                    .collect(),
                            };
                            Value::array(values)
                        }
                        None => Value::undefined(),
                    };
                    self.push(v);
                    pc += 1;
                }
                Opcode::GetProto => {
                    let obj = self.pop();
                    let v = obj
                        .as_object()
                        .map(|od| od.borrow().proto.clone())
                        .unwrap_or(Value::undefined());
                    self.push(v);
                    pc += 1;
                }
                Opcode::SetProto => {
                    let proto = self.pop();
                    let obj = self.pop();
                    if let Some(od) = obj.as_object() {
                        od.borrow_mut().proto = proto;
                    }
                    pc += 1;
                }
                Opcode::InstanceOf => {
                    let ctor = self.pop();
                    let obj = self.pop();
                    let proto = match ctor.as_function() {
                        Some(f) => match f.props.borrow().as_ref() {
                            Some(p) => p
                                .borrow()
                                .get("prototype")
                                .cloned()
                                .unwrap_or(Value::undefined()),
                            None => Value::undefined(),
                        },
                        // Native constructors (Map/Set) carry their prototype.
                        None => ctor.as_native_proto().unwrap_or(Value::undefined()),
                    };
                    let mut found = false;
                    let mut cur = obj;
                    // Depth-limited walk: a proto chain can only be as long as
                    // the object graph, so 1024 is unreachable in practice.
                    for _ in 0..1024 {
                        match cur.as_object() {
                            Some(od) => {
                                let p = od.borrow().proto.clone();
                                if p.is_undefined() {
                                    break;
                                }
                                if p.bits() == proto.bits() {
                                    found = true;
                                    break;
                                }
                                cur = p;
                            }
                            None => break,
                        }
                    }
                    self.push(Value::bool(found));
                    pc += 1;
                }
                Opcode::In => {
                    // `key in obj` — checks the OWN properties AND the
                    // prototype chain (JS semantics: `"toString" in {}` is
                    // true). The key is coerced via ToString; anything that
                    // is not an object/function/array throws Node's
                    // TypeError (Map/Set are not property holders).
                    let obj = self.pop();
                    let key = self.pop();
                    let ks = to_string_js(&key);
                    match in_operator_probe(&obj, &ks) {
                        Some(found) => {
                            self.push(Value::bool(found));
                            pc += 1;
                        }
                        None => {
                            match self.throw_value(Value::string(format!(
                                "TypeError: Cannot use 'in' operator to search for '{}' in {}",
                                ks,
                                iterable_display(&obj)
                            ))) {
                                ThrowResult::Jump(p) => pc = p,
                                ThrowResult::EndDispatch | ThrowResult::Abort => pc = usize::MAX,
                            }
                            continue;
                        }
                    }
                }
                Opcode::CreateGenerator => {
                    let gen_id = self.next_gen_id;
                    self.next_gen_id += 1;

                    let frame = match self.call_stack.pop() {
                        Some(f) => f,
                        None => {
                            self.push(Value::undefined());
                            break;
                        }
                    };
                    let gen_stack = self.stack.save_from(frame.base_slot);
                    self.stack.truncate(frame.base_slot);
                    if let Some(ts) = frame.this_slot {
                        self.stack.truncate(ts);
                    }
                    let gen_cells = self.cells_stack.split_off(frame.cells_len);
                    let gen_handlers = self.handlers.split_off(frame.handlers_len);

                    let mut rebased_frame = frame.clone();
                    rebased_frame.generator_id = Some(gen_id);
                    rebased_frame.base_slot = 0;
                    rebased_frame.locals_end = frame.locals_end.saturating_sub(frame.base_slot);
                    rebased_frame.cells_len = 0;
                    rebased_frame.handlers_len = 0;
                    rebased_frame.this_slot = None;

                    let state = std::rc::Rc::new(std::cell::RefCell::new(crate::vm::generator::GeneratorState {
                        stack: gen_stack,
                        call_stack: vec![rebased_frame],
                        cells_stack: gen_cells,
                        handlers: gen_handlers,
                        pc: pc + 1,
                        program_id: self.program_id,
                        done: false,
                        yielded: false,
                        is_initial: true,
                        return_value: Value::undefined(),
                    }));

                    self.generators.insert(gen_id, state);

                    let gen_obj = self.create_generator_object(gen_id);

                    if frame.keep_result {
                        self.push(gen_obj);
                    }

                    if frame.return_program != self.program_id {
                        self.load_program(frame.return_program);
                    }
                    pc = frame.return_addr;
                    continue;
                }
                Opcode::Yield => {
                    let val = self.pop();
                    if let Some(gen_id) = self.active_generator {
                        if let Some(state_rc) = self.generators.get(&gen_id).cloned() {
                            let mut st = state_rc.borrow_mut();
                            st.stack = self.stack.save_from(0);
                            st.call_stack = self.call_stack.clone();
                            st.cells_stack = self.cells_stack.clone();
                            st.handlers = self.handlers.clone();
                            st.pc = pc + 1;
                            st.program_id = self.program_id;
                            st.yielded = true;
                            st.done = false;
                        }
                    }
                    self.push(val);
                    break;
                }

                Opcode::BinLocalLocalLocalArith => {
                    let dst = self.bytecode[pc + 1] as usize;
                    let src1 = self.bytecode[pc + 2] as usize;
                    let src2 = self.bytecode[pc + 3] as usize;
                    let ar = self.bytecode[pc + 4];
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let (res, fast) = alu_local_local(&self.stack, base + src1, base + src2, ar);
                    let res = if fast {
                        res
                    } else {
                        let va = if base + src1 < self.stack.len() {
                            self.slot_value(base + src1)
                        } else {
                            Value::undefined()
                        };
                        let vb = if base + src2 < self.stack.len() {
                            self.slot_value(base + src2)
                        } else {
                            Value::undefined()
                        };
                        arith_apply(&va, &vb, ar)
                    };
                    self.store_slot(base + dst, res);
                    pc += 5;
                }
                Opcode::BinLocalLocalLocalInt => {
                    let dst = self.bytecode[pc + 1] as usize;
                    let src = self.bytecode[pc + 2] as usize;
                    let ar = self.bytecode[pc + 3];
                    let imm = self.read_i32(pc + 4) as i64;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let (res, fast) = alu_local_imm(&self.stack, base + src, imm, ar);
                    let res = if fast {
                        res
                    } else {
                        let v = if base + src < self.stack.len() {
                            self.slot_value(base + src)
                        } else {
                            Value::undefined()
                        };
                        arith_apply(&v, &Value::int(imm), ar)
                    };
                    self.store_slot(base + dst, res);
                    pc += 8;
                }
                Opcode::StoreLocalLocal => {
                    let dst = self.bytecode[pc + 1] as usize;
                    let src = self.bytecode[pc + 2] as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let val = if base + src < self.stack.len() {
                        self.slot_value(base + src)
                    } else {
                        Value::undefined()
                    };
                    self.store_slot(base + dst, val);
                    pc += 3;
                }
                Opcode::CmpLocalLocalJumpIfFalse => {
                    let a = self.bytecode[pc + 1] as usize;
                    let b = self.bytecode[pc + 2] as usize;
                    let cmp = self.bytecode[pc + 3];
                    let target = self.read_u32(pc + 4) as usize;
                    let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
                    let ia = base + a;
                    let ib = base + b;
                    let result = if ia < self.stack.len() && ib < self.stack.len() {
                        match (self.stack.kind_of(ia), self.stack.kind_of(ib)) {
                            (KIND_INT, KIND_INT) => cmp_i64(
                                Value::int_bits_raw(self.stack.at(ia).bits()),
                                Value::int_bits_raw(self.stack.at(ib).bits()),
                                cmp,
                            ),
                            (KIND_INT, KIND_NUMBER) => cmp_f64(
                                Value::int_bits_raw(self.stack.at(ia).bits()) as f64,
                                f64::from_bits(self.stack.at(ib).bits()),
                                cmp,
                            ),
                            (KIND_NUMBER, KIND_INT) => cmp_f64(
                                f64::from_bits(self.stack.at(ia).bits()),
                                Value::int_bits_raw(self.stack.at(ib).bits()) as f64,
                                cmp,
                            ),
                            (KIND_NUMBER, KIND_NUMBER) => cmp_f64(
                                f64::from_bits(self.stack.at(ia).bits()),
                                f64::from_bits(self.stack.at(ib).bits()),
                                cmp,
                            ),
                            _ => compare_values(&self.slot_value(ia), &self.slot_value(ib), cmp),
                        }
                    } else {
                        let va = if ia < self.stack.len() { self.slot_value(ia) } else { Value::undefined() };
                        let vb = if ib < self.stack.len() { self.slot_value(ib) } else { Value::undefined() };
                        compare_values(&va, &vb, cmp)
                    };
                    if result {
                        pc += 8;
                    } else {
                        if target < pc {
                            let trips = {
                                let c = self.backedge_counts.entry(target).or_insert(0);
                                *c = c.saturating_add(1);
                                *c
                            };
                            if trips >= 100 {
                                if let Some(next_pc) = self.maybe_jit_loop(target, pc) {
                                    pc = next_pc;
                                    continue;
                                }
                            }
                        }
                        pc = target;
                    }
                }
            }
        }

        if HAS_BUDGET {
            self.instruction_budget = Some(budget);
        }
        self.pop()
    }

    pub(crate) fn maybe_jit_loop(&mut self, header_pc: usize, backedge_pc: usize) -> Option<usize> {
        let program = &self.programs[self.program_id as usize];
        let jit_fn = {
            let jit = self.jit.as_mut()?;
            jit.get_or_compile(self.program_id, program, header_pc, backedge_pc)?
        };

        let base = self.call_stack.last().map(|f| f.base_slot).unwrap_or(0);
        let slots_ptr = unsafe {
            self.stack.slots.as_mut_ptr().add(base) as *mut u64
        };
        let slots_len = self.stack.slots.len().saturating_sub(base) as u64;
        let max_trips = 100_000u64;

        let resume_pc = unsafe { jit_fn(slots_ptr, slots_len, max_trips) };
        if resume_pc == u64::MAX {
            if let Some(jit) = self.jit.as_mut() {
                jit.mark_uncompilable(self.program_id, header_pc);
            }
            return None;
        }

        if std::env::var("ALLOY_JIT_LOG").is_ok() {
            eprintln!(
                "[alloy-jit] executed native loop header={:04x} -> resume={:04x}",
                header_pc, resume_pc
            );
        }
        Some(resume_pc as usize)
    }

    /// Perform a call whose `argc` arguments (with the callee already popped)
    /// are on the operand stack. Returns the pc to continue at: a bytecode
    /// target for script/closure calls, or `ret_addr` for natives (their
    /// result is already pushed).
    pub(crate) fn dispatch_call(
        &mut self,
        callee: Value,
        argc: usize,
        ret_addr: usize,
        keep: bool,
        this_slot: Option<usize>,
        is_ctor: bool,
    ) -> usize {
        // Guard against runaway recursion: past the limit, calls return
        // undefined instead of growing the stack forever.
        if self.call_stack.len() >= MAX_CALL_DEPTH {
            for _ in 0..argc {
                self.pop();
            }
            if keep {
                self.push(Value::undefined());
            }
            if let Some(ts) = this_slot {
                let saved = if keep { Some(self.pop()) } else { None };
                self.stack.truncate(ts);
                if let Some(r) = saved {
                    self.push(r);
                }
            }
            return ret_addr;
        }
        // Stack-space guard: the operand stack is a fixed array, so a frame
        // that would land near the top fails gracefully (same shape as the
        // recursion guard) instead of overflowing the stack.
        let base_slot = self.stack.len() - argc;
        if base_slot + FRAME_BUDGET > STACK_SIZE {
            for _ in 0..argc {
                self.pop();
            }
            if keep {
                self.push(Value::undefined());
            }
            if let Some(ts) = this_slot {
                let saved = if keep { Some(self.pop()) } else { None };
                self.stack.truncate(ts);
                if let Some(r) = saved {
                    self.push(r);
                }
            }
            return ret_addr;
        }
        if let Some(f) = callee.as_function() {
            let base_slot = self.stack.len() - argc;
            // Call-site IC: remember last callee bits per Call pc (caller passes
            // ret_addr as the site). Hit skips re-probing `as_function` next time
            // via the leaf check below — the bits compare is one u64 cmp.
            let site = ic_slot(ret_addr);
            let cb = callee.bits();
            let ic_hit = self.call_ic[site].callee_bits == cb;
            if !ic_hit {
                self.call_ic[site] = CallIcEntry { callee_bits: cb, func_ptr: f.ptr as u64, params: f.params };
            }
            // Missing arguments read as `undefined`, never as stale stack
            // garbage from an earlier frame (`function f(x, y)` called with
            // one arg: y must be undefined). Only the missing tail is filled;
            // extra args stay put (they were pushed by the caller).
            while self.stack.len() < base_slot + f.params as usize {
                self.push(Value::undefined());
            }
            let cells_len = self.cells_stack.len();
            // Leaf fast path: 90% of hot calls (fib, ack, collatz inner) capture
            // nothing — skip the Vec push/clone entirely.
            if !f.cells.is_empty() {
                self.cells_stack.push(f.cells.clone());
            }
            // `arguments`: snapshot the passed args at entry, but only for
            // functions that reference it (the body's local stores would
            // otherwise clobber the arg slots before a lazy read).
            let arg_values = if f.uses_args != 0 {
                Some((0..argc).map(|i| self.stack.at(base_slot + i).clone()).collect())
            } else {
                None
            };
            self.call_stack.push(CallFrame {
                return_addr: ret_addr,
                return_program: self.program_id,
                base_slot,
                argc,
                arg_values,
                fn_value: callee.clone(),
                cells_len,
                promise_slot: None,
                resumed: false,
                keep_result: keep,
                handlers_len: self.handlers.len(),
                locals_end: base_slot,
                this_slot,
                is_ctor,
                generator_id: None,
            });
            // The caller pushed the args as generic temporaries, but as the
            // callee's params they are read by CmpLocalInt/BinLocalInt/…
            // fast lanes — give them their real kinds so feedback is correct
            // from the first instruction, not the first store (fib's `n` is
            // never stored).
            self.mark_param_kinds(base_slot);
            if f.program != self.program_id {
                self.load_program(f.program);
            }
            f.ptr        } else if let Some(f) = callee.as_native() {
            let mut args: Vec<Value> = (0..argc).map(|_| self.pop()).collect();
            args.reverse();
            // Method natives (Map/Set methods on the prototype) read their
            // instance from `this_value`: the receiver still sits at
            // `this_slot` (the receiver cleanup below runs after the call).
            // Save/restore so a re-entrant native call (a native invoking a
            // JS callback via `call_value`) sees its own receiver.
            let saved_this = self.native_this.take();
            self.native_this = this_slot.map(|ts| self.stack.at(ts).clone());
            self.current_pc = ret_addr;
            let result = f(&args, self);
            self.native_this = saved_this;
            // A native that threw: a handler jump means throw_value already
            // unwound the stack and pushed the exception at the handler —
            // jump there and discard the result. An uncaught top-level throw
            // (usize::MAX sentinel) aborts the dispatch loop.
            if let Some(p) = self.native_throw_jump.take() {
                return p;
            }
            if self.uncaught_exception.is_some() {
                return usize::MAX;
            }
            if keep {
                self.push(result);
            }
            // Method-call receiver cleanup: natives never run the Return
            // opcode, so the receiver must be removed here (JS functions are
            // cleaned up inside Return).
            if let Some(ts) = this_slot {
                let saved = if keep { Some(self.pop()) } else { None };
                self.stack.truncate(ts);
                if let Some(r) = saved {
                    self.push(r);
                }
            }
            ret_addr
        } else if let Some(fn_ptr) = callee.as_number() {
            // Legacy path: calling a raw number jumps to it as a program
            // counter (the pre-closure calling convention).

            let cells_len = self.cells_stack.len();
            self.call_stack.push(CallFrame {
                return_addr: ret_addr,
                return_program: self.program_id,
                base_slot,
                argc,
                arg_values: None,
                fn_value: callee.clone(),
                cells_len,
                promise_slot: None,
                resumed: false,
                keep_result: keep,
                handlers_len: self.handlers.len(),
                locals_end: base_slot,
                this_slot,
                is_ctor,
                generator_id: None,
            });
            fn_ptr as usize
        } else {
            if keep {
                self.push(Value::undefined());
            }
            if let Some(ts) = this_slot {
                let saved = if keep { Some(self.pop()) } else { None };
                self.stack.truncate(ts);
                if let Some(r) = saved {
                    self.push(r);
                }
            }
            ret_addr
        }
    }

    #[inline]
    fn read_i32(&self, offset: usize) -> i32 {
        self.read_u32(offset) as i32
    }

    /// Read a local slot value, transparently dereferencing cells (the same
    /// behavior as LoadLocal) for the fused superinstructions. When the slot
    /// feedback says the slot holds a direct int or number, the read is a raw
    /// word copy — no `as_cell` probe, no `Value::clone` payload match, no
    /// Rc traffic.
    #[inline(always)]
    fn slot_value(&self, idx: usize) -> Value {
        match self.stack.kind_of(idx) {
            KIND_INT | KIND_NUMBER => Value::from_raw_word(self.stack.at(idx).bits()),
            _ => {
                let v = self.stack.at(idx);
                match v.as_cell() {
                    Some(c) => c.borrow().clone(),
                    None => v.clone(),
                }
            }
        }
    }

    /// Mark the kinds of a freshly-pushed argument range — the callee's
    /// params. The args were pushed as generic temporaries (push invalidates
    /// to KIND_OTHER), but as params they deserve their real kinds so the
    /// load/ALU/cmp fast lanes fire from the frame's first instruction.
    pub(crate) fn mark_param_kinds(&mut self, base_slot: usize) {
        let mut i = base_slot;
        while i < self.stack.len() {
            self.stack.mark_kind(i, kind_of_value(self.stack.at(i)));
            i += 1;
        }
    }

    /// Write a local slot, growing the stack and writing through cells (the
    /// same behavior as StoreLocal) for the fused superinstructions. A slot
    /// known to hold a direct int/number is written without the `as_cell_rc`
    /// probe (it cannot be a cell).
    #[inline(always)]
    fn store_slot(&mut self, idx: usize, val: Value) {
        self.record_local(idx);
        let k = kind_of_value(&val);
        if idx < self.stack.len() && matches!(self.stack.kind_of(idx), KIND_INT | KIND_NUMBER) {
            *self.stack.at_mut(idx) = val;
            self.stack.mark_kind(idx, k);
        } else if idx < self.stack.len() {
            if let Some(c) = self.stack.at(idx).as_cell_rc() {
                self.note_rc_dirty(RcDirtyRef::Cell(c.clone()));
                *c.borrow_mut() = val;
                // The slot itself still holds the cell.
                self.stack.mark_kind(idx, KIND_OTHER);
            } else {
                *self.stack.at_mut(idx) = val;
                self.stack.mark_kind(idx, k);
            }
        } else {
            while self.stack.len() <= idx {
                self.stack.push(Value::undefined());
            }
            *self.stack.at_mut(idx) = val;
            self.stack.mark_kind(idx, k);
        }
    }

    /// 2-way polymorphic inline-cache property get. Primary hit is the old
    /// monomorphic fast path; secondary hit covers 2-shape sites without
    /// thrashing. 3+ shapes use the slow map lookup (megamorphic).
    #[inline]
    fn get_prop(
        &mut self,
        pc: usize,
        od: &RefCell<ObjectData>,
        prop: &Value,
        receiver: &Value,
    ) -> Value {
        let slot = ic_slot(pc);
        let pb = prop.bits();
        let shape_ptr = od.borrow().shape_ptr();
        if let Some(off) = self.ic[slot].probe(self.program_id, pc as u32, pb, shape_ptr) {
            let od = od.borrow();
            let off = off as usize;
            if off < od.values.len() && !od.deleted[off] {
                return unwrap_cell(od.values[off].clone());
            }
        }
        let (name, atom) = match (prop.as_str(), prop.as_atom()) {
            (Some(s), Some(a)) => (s, a),
            _ => return Value::undefined(),
        };
        // Own property first, then the prototype chain. Inherited hits are
        // NOT cached (their offset is per-ancestor, not per-receiver), so a
        // `p.dist` on a class instance always walks; the shape-IC stays for
        // the own-property fast path.
        let od = od.borrow();
        match od.shape.get_atom(atom) {
            // A deleted property reads as undefined and is not cached (it may
            // be re-set later, which clears the tombstone).
            Some(off) if !od.deleted[off as usize] => {
                let v = unwrap_cell(od.values[off as usize].clone());
                let shape = od.shape_ptr();
                drop(od);
                let fresh = IcEntry {
                    program: self.program_id,
                    pc: pc as u32,
                    shape,
                    offset: off,
                    prop: pb,
                };
                self.ic[slot].update(fresh);
                return v;
            }
            _ => {}
        }
        // Own accessor: a getter is invoked with the receiver as `this`
        // (class getters on the instance's own accessor table). An accessor
        // with no callable getter reads as undefined — it does NOT fall
        // through to the prototype chain.
        if let Some((g, _)) = od
            .accessors
            .as_ref()
            .and_then(|accs| accs.get(name))
        {
            let (g, receiver) = (g.clone(), receiver.clone());
            drop(od);
            if g.is_function() || g.is_native() {
                return self.call_value_with_this(&g, Some(receiver), &[]);
            }
            return Value::undefined();
        }
        // Chain walk with owned values (the ancestor borrow cannot outlive
        // the loop iteration).
        let mut cur = od.proto.clone();
        drop(od);
        let mut depth = 0u16;
        while depth <= 1024 {
            let Some(cd) = cur.as_object() else { break };
            let cd = cd.borrow();
            match cd.shape.get(name) {
                Some(off) if !cd.deleted[off as usize] => {
                    return unwrap_cell(cd.values[off as usize].clone());
                }
                _ => {}
            }
            // Inherited accessor (a getter on a prototype): the receiver is
            // still the original object, so `this` binds correctly.
            if let Some(accs) = cd.accessors.as_ref() {
                if let Some((g, _)) = accs.get(name) {
                    if g.is_function() || g.is_native() {
                        let g = g.clone();
                        let receiver = receiver.clone();
                        drop(cd);
                        return self.call_value_with_this(&g, Some(receiver), &[]);
                    }
                }
            }
            let next = cd.proto.clone();
            drop(cd);
            cur = next;
            depth += 1;
        }
        if name.starts_with('#') {
            self.throw_exception(Value::string(format!(
                "TypeError: Cannot read private member {} from an object whose class did not declare it",
                name
            )));
            return Value::undefined();
        }
        Value::undefined()
    }

    /// Full GetProperty semantics for `obj[prop]` (objects go through the
    /// inline cache; arrays/strings/buffers/promises have their special reads;
    /// anything else is undefined). Shared by GetProperty, PeekProperty and
    /// CompoundPropConst.
    #[inline]
    /// Read a property WITHOUT unwrapping live-import cells (the raw value:
    /// the cell itself). Only object properties can be cells (module exports
    /// objects); anything else reads as undefined.
    fn get_prop_cell_value(&self, obj: &Value, prop: &Value) -> Value {
        let (od, name) = match (obj.as_object(), prop.as_str()) {
            (Some(od), Some(s)) => (od, s),
            _ => return Value::undefined(),
        };
        let od = od.borrow();
        match od.shape.get(name) {
            Some(off) if !od.deleted[off as usize] => od.values[off as usize].clone(),
            _ => Value::undefined(),
        }
    }

    fn get_prop_value(&mut self, pc: usize, obj: &Value, prop: &Value) -> Value {
        if let Some(proxy_arc) = obj.as_proxy() {
            let (target, handler, revoked) = {
                let g = proxy_arc.lock().unwrap_or_else(|g| g.into_inner());
                (g.target.clone(), g.handler.clone(), g.revoked)
            };
            if revoked {
                self.throw_exception(Value::string(
                    "TypeError: Cannot perform 'get' on a proxy that has been revoked".to_string(),
                ));
                return Value::undefined();
            }
            let trap = if let Some(hd) = handler.as_object() {
                hd.borrow().get("get").cloned()
            } else {
                None
            };
            if let Some(t) = trap {
                if t.is_function() || t.is_native() {
                    return self.call_value_with_this(&t, Some(handler), &[target, prop.clone(), obj.clone()]);
                }
            }
            return if prop.is_symbol() || prop.is_number() || prop.is_int() {
                self.get_index_value(&target, prop)
            } else {
                self.get_prop_value(pc, &target, prop)
            };
        }
        if let Some(name) = prop.as_str() {
            if name.starts_with('#') && !obj.is_object() {
                self.throw_exception(Value::string(format!(
                    "TypeError: Cannot read private member {} from non-object",
                    name
                )));
                return Value::undefined();
            }
        }
        if let Some(od) = obj.as_object() {
            // Map/Set: only `size` is computed per read (it cannot be a
            // shared prototype native without getter support); the methods
            // live on Map.prototype/Set.prototype and resolve through the
            // normal proto-chain walk below.
            let container = od.borrow().container;
            if container != 0 {
                if let Some(v) = container_prop(obj, prop) {
                    return v;
                }
            }
            self.get_prop(pc, od, prop, obj)
        } else if let (Some(f), Some(s)) = (obj.as_function(), prop.as_str()) {
            // `f.call(thisArg, ...args)` / `f.apply(thisArg, args)`: invoke
            // the function with an explicit `this`. Synthesized per read like
            // the Promise `then` native.
            if s == "call" || s == "apply" {
                let is_apply = s == "apply";
                let callee = obj.clone();
                return Value::native(Arc::new(move |args, vm| {
                    let this_arg = args.first().cloned().unwrap_or(Value::undefined());
                    let rest: Vec<Value> = if is_apply {
                        match args.get(1) {
                            Some(a) if a.is_array() => {
                                a.as_array().map(|ad| ad.borrow().to_values()).unwrap_or_default()
                            }
                            // Node: non-array -> TypeError; the engine's
                            // non-throwing style coerces to no args.
                            _ => Vec::new(),
                        }
                    } else {
                        args.iter().skip(1).cloned().collect()
                    };
                    vm.call_value_with_this(&callee, Some(this_arg), &rest)
                }));
            }
            // Class/static properties on the function itself: `prototype`,
            // static methods. Ordinary functions have no props -> undefined.
            // `fn.length` (declared fixed-param count, like V8) falls back
            // here when no static prop shadows it — arity sniffing for
            // Express-style 4-arg error middleware depends on it.
            if let Some(v) = f.props.borrow().as_ref().and_then(|p| p.borrow().get(s).cloned()) {
                return v;
            }
            if s == "length" {
                return Value::int(f.params as i64);
            }
            Value::undefined()
        } else if let (Some(props), Some(s)) = (obj.as_native_props(), prop.as_str()) {
            // Native statics (`String.fromCharCode`) and the constructor's
            // `prototype` property.
            if let Some(p) = props.borrow().as_ref() {
                if let Some(v) = p.borrow().get(s).cloned() {
                    return v;
                }
            }
            if s == "prototype" {
                obj.as_native_proto().unwrap_or(Value::undefined())
            } else {
                Value::undefined()
            }
        } else if let (Some(arr), Some(n)) = (obj.as_array(), prop.as_number()) {
            let arr = arr.borrow();
            let i = n as usize;
            if i < arr.len() { arr.get(i) } else { Value::undefined() }
        } else if let (Some(_), Some(s)) = (obj.as_array(), prop.as_str()) {
            array_prop(obj, s)
        } else if let (Some(_), Some(s)) = (obj.as_regex(), prop.as_str()) {
            regex_prop(obj, s)
        } else if let (Some(_), Some(prop)) = (obj.as_str(), prop.as_str()) {
            string_prop(obj, prop)
        } else if (obj.is_number() || obj.is_int()) && prop.as_str().is_some() {
            number_prop(obj, prop.as_str().unwrap())
        } else if let (Some((ptr, len)), Some(s)) = (obj.as_buffer(), prop.as_str()) {
            if s == "ptr" {
                Value::number(ptr as usize as f64)
            } else if s == "length" {
                Value::int(len as i64)
            } else {
                Value::undefined()
            }
        } else if let (Some(p), Some(s)) = (obj.as_promise(), prop.as_str()) {
            if s == "then" {
                let p2 = p.clone();
                Value::native(Arc::new(move |args, vm| {
                    let cb = args.first().cloned().unwrap_or(Value::undefined());
                    let on_rejected = args.get(1).cloned();
                    vm.then(&Value::promise(p2.clone()), cb, on_rejected)
                }))
            } else {
                Value::undefined()
            }
        } else if let (Some(st), Some(s)) = (obj.as_channel(), prop.as_str()) {
            match s {
                "send" => {
                    let st2 = st.clone();
                    Value::native(Arc::new(move |args, vm| {
                        let msg = args.first().cloned().unwrap_or(Value::undefined());
                        // Incremental-GC barrier: the queue now holds a value
                        // the mark may not have seen.
                        vm.note_gc_dirty(RcDirtyRef::Channel(st2.clone()));
                        let mut guard = st2.lock().unwrap_or_else(|g| g.into_inner());
                        // Named channels are shared across VMs, so their
                        // messages travel as bytes — a Value is an arena
                        // pointer valid only on the sending thread. Anonymous
                        // channels are per-VM and pass raw values.
                        let item = if guard.named {
                            let mut bytes = Vec::new();
                            // Data crosses, code and state do not:
                            // functions/natives/promises inside the message
                            // coerce to undefined, like spawn.
                            write_spawn_value(&mut bytes, &msg, false, 0);
                            ChannelItem::Bytes(bytes)
                        } else {
                            ChannelItem::Raw(msg)
                        };
                        match guard.send_item(item) {
                            Some((waiter, ChannelItem::Bytes(bytes))) => {
                                drop(guard);
                                // Cross-thread routing: the waiter's promise
                                // belongs to the VM that parked it. If that's
                                // not this loop, hand the bytes to its owner
                                // (it decodes into its own heap and wakes);
                                // otherwise decode here and resolve locally.
                                let owner = waiter.as_promise().and_then(|p| {
                                    p.lock()
                                        .unwrap_or_else(|g| g.into_inner())
                                        .owner
                                        .clone()
                                });
                                let mine = vm.wake_handle();
                                let mine_here = match (&owner, &mine) {
                                    (Some(o), Some(m)) => Arc::ptr_eq(o, m),
                                    _ => true,
                                };
                                if mine_here {
                                    let mut pos = 0;
                                    let value = decode_spawn_value(&bytes, &mut pos);
                                    vm.resolve_promise(&waiter, value);
                                } else if let Some(w) = waiter.as_promise() {
                                    owner.as_ref().unwrap().deliver(w.clone(), bytes);
                                }
                            }
                            Some((waiter, ChannelItem::Raw(msg))) => {
                                drop(guard);
                                // Anonymous (same-VM) channel: resolve the
                                // waiter directly. Defensive: if the waiter
                                // somehow belongs to another loop, serialize
                                // and route (raw values cannot cross heaps).
                                let owner = waiter.as_promise().and_then(|p| {
                                    p.lock()
                                        .unwrap_or_else(|g| g.into_inner())
                                        .owner
                                        .clone()
                                });
                                let mine = vm.wake_handle();
                                let mine_here = match (&owner, &mine) {
                                    (Some(o), Some(m)) => Arc::ptr_eq(o, m),
                                    _ => true,
                                };
                                if mine_here {
                                    vm.resolve_promise(&waiter, msg);
                                } else {
                                    let mut bytes = Vec::new();
                                    write_spawn_value(&mut bytes, &msg, false, 0);
                                    if let Some(w) = waiter.as_promise() {
                                        owner.as_ref().unwrap().deliver(w.clone(), bytes);
                                    }
                                }
                            }
                            None => {}
                        }
                        Value::undefined()
                    }))
                }
                "recv" => {
                    let st2 = st.clone();
                    Value::native(Arc::new(move |_args, vm| {
                        let mut guard = st2.lock().unwrap_or_else(|g| g.into_inner());
                        match guard.recv_item() {
                            Some(ChannelItem::Raw(v)) => v,
                            Some(ChannelItem::Bytes(bytes)) => {
                                // Named channel: decode into this heap.
                                let mut pos = 0;
                                decode_spawn_value(&bytes, &mut pos)
                            }
                            // Empty: park on a promise the event loop resolves
                            // when the next `send` lands. The waiter is
                            // stamped with this VM's wake handle (a send from
                            // another thread routes back and wakes us) and
                            // recorded so the event loop keeps pumping until
                            // it settles.
                            None => {
                                let p = vm.new_promise();
                                vm.park_cross_waiter(&p);
                                guard.push_waiter(p.clone());
                                p
                            }
                        }
                    }))
                }
                "tryRecv" => {
                    let st2 = st.clone();
                    Value::native(Arc::new(move |_args, _vm| {
                        let mut guard = st2.lock().unwrap_or_else(|g| g.into_inner());
                        match guard.recv_item() {
                            Some(ChannelItem::Raw(v)) => v,
                            Some(ChannelItem::Bytes(bytes)) => {
                                let mut pos = 0;
                                decode_spawn_value(&bytes, &mut pos)
                            }
                            None => Value::undefined(),
                        }
                    }))
                }
                "len" => Value::int(st.lock().unwrap_or_else(|g| g.into_inner()).len() as i64),
                _ => Value::undefined(),
            }
        } else {
            Value::undefined()
        }
    }

    /// Incremental-GC write barrier for arena boxes: set the slot's dirty bit
    /// in the old generation's bitmap so the next unit boundary's scan
    /// re-traces the box. Young boxes are skipped (they are swept wholesale
    /// at the boundary anyway). One bitmap cell read-modify-write — no header
    /// traffic on the payload's cache line.
    #[inline]
    pub(crate) fn note_box_dirty(&self, box_ptr: usize) {
        self.heap.note_box_dirty(box_ptr);
    }

    /// Incremental-GC write barrier for Rc-backed structures (closure cells,
    /// promises, channels): record them so the next mark slice re-traces
    /// their (possibly new) contents. Keeps a strong ref so the structure
    /// can't dangle before the re-trace. Active only while a mark runs.
    #[inline]
    pub(crate) fn note_rc_dirty(&mut self, d: RcDirtyRef) {
        if let Some(m) = &mut self.mark {
            m.dirty_rc.push(d);
        }
    }

    /// SetProperty semantics for `obj[prop] = val`: only plain objects store
    /// (arrays/strings/etc. ignore writes), via the inline cache.
    #[inline]
    fn set_prop_value(&mut self, pc: usize, obj: &Value, prop: &Value, val: Value) {
        if let Some(proxy_arc) = obj.as_proxy() {
            let (target, handler, revoked) = {
                let g = proxy_arc.lock().unwrap_or_else(|g| g.into_inner());
                (g.target.clone(), g.handler.clone(), g.revoked)
            };
            if revoked {
                self.throw_exception(Value::string(
                    "TypeError: Cannot perform 'set' on a proxy that has been revoked".to_string(),
                ));
                return;
            }
            let trap = if let Some(hd) = handler.as_object() {
                hd.borrow().get("set").cloned()
            } else {
                None
            };
            if let Some(t) = trap {
                if t.is_function() || t.is_native() {
                    self.call_value_with_this(&t, Some(handler), &[target, prop.clone(), val, obj.clone()]);
                    return;
                }
            }
            if prop.is_symbol() || prop.is_number() || prop.is_int() {
                self.set_index_value(&target, prop, val);
            } else {
                self.set_prop_value(pc, &target, prop, val);
            }
            return;
        }
        if let Some(name) = prop.as_str() {
            if name.starts_with('#') && !obj.is_object() {
                self.throw_exception(Value::string(format!(
                    "TypeError: Cannot write private member {} to non-object",
                    name
                )));
                return;
            }
        }
        if let Some(r) = obj.as_regex() {
            // RegExp.lastIndex is the one writable regex property: sets the
            // /g /y cursor (ToLength: negatives and NaN clamp to 0).
            if prop.as_str() == Some("lastIndex") {
                let n = val.to_number();
                let len = if n.is_nan() || n.is_infinite() || n <= 0.0 {
                    0.0
                } else {
                    n.trunc()
                };
                let mut g = r.lock().unwrap_or_else(|g| g.into_inner());
                g.last_index = len as usize;
            }
        } else if let Some(od) = obj.as_object() {
            self.set_prop(pc, od, prop, val, obj);
        } else if let (Some(f), Some(s)) = (obj.as_function(), prop.as_str()) {
            // Class construction and static assignment: `C.prototype = X`,
            // `C.sm = fn`. Lazy-allocate the props map on first write via
            // the outer RefCell (the function lives behind a shared Rc).
            let mut slot = f.props.borrow_mut();
            let map = slot
                .get_or_insert_with(|| Rc::new(RefCell::new(hashbrown::HashMap::new())));
            map.borrow_mut().insert(s.to_string(), val);
        } else if let (Some(props), Some(s)) = (obj.as_native_props(), prop.as_str()) {
            // Same for natives with statics (`String.x = ...`).
            let mut slot = props.borrow_mut();
            let map = slot
                .get_or_insert_with(|| Rc::new(RefCell::new(hashbrown::HashMap::new())));
            map.borrow_mut().insert(s.to_string(), val);
        }
    }

    /// Full GetIndex semantics for `obj[idx]` (arrays index numerically,
    /// strings index by char with the O(1) ASCII fast path, objects look up
    /// the stringified key; anything else is undefined). Shared by GetIndex,
    /// PeekIndex and CompoundIndexConst.
    #[inline]
    fn get_index_value(&self, obj: &Value, idx: &Value) -> Value {
        if let Some(id) = idx.as_symbol() {
            if id == SYMBOL_ITERATOR {
                if obj.is_array() {
                    let arr_clone = obj.clone();
                    return Value::native(Arc::new(move |_args, _vm| {
                        let items: Vec<Value> = if let Some(a) = arr_clone.as_array() {
                            a.borrow().to_values()
                        } else {
                            Vec::new()
                        };
                        let cursor = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                        let c2 = cursor.clone();
                        let next_fn = Value::native(Arc::new(move |_args, _vm| {
                            let idx = c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            if idx < items.len() {
                                let val = items[idx].clone();
                                let mut obj = hashbrown::HashMap::new();
                                obj.insert("value".to_string(), val);
                                obj.insert("done".to_string(), Value::bool(false));
                                Value::object(obj)
                            } else {
                                let mut obj = hashbrown::HashMap::new();
                                obj.insert("value".to_string(), Value::undefined());
                                obj.insert("done".to_string(), Value::bool(true));
                                Value::object(obj)
                            }
                        }));
                        let iter_fn = Value::native(Arc::new(|_args, vm| {
                            vm.this_value()
                        }));
                        let mut props = hashbrown::HashMap::new();
                        props.insert("next".to_string(), next_fn);
                        props.insert(format!("\0sym_{}", SYMBOL_ITERATOR), iter_fn);
                        props.insert(
                            format!("\0sym_{}", SYMBOL_TO_STRING_TAG),
                            Value::string("Array Iterator".to_string()),
                        );
                        Value::object(props)
                    }));
                }
                if let Some(s) = obj.as_str() {
                    let str_clone = s.to_string();
                    return Value::native(Arc::new(move |_args, _vm| {
                        let chars: Vec<String> = str_clone.chars().map(|c| c.to_string()).collect();
                        let cursor = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                        let c2 = cursor.clone();
                        let next_fn = Value::native(Arc::new(move |_args, _vm| {
                            let idx = c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            if idx < chars.len() {
                                let val = Value::string(chars[idx].clone());
                                let mut obj = hashbrown::HashMap::new();
                                obj.insert("value".to_string(), val);
                                obj.insert("done".to_string(), Value::bool(false));
                                Value::object(obj)
                            } else {
                                let mut obj = hashbrown::HashMap::new();
                                obj.insert("value".to_string(), Value::undefined());
                                obj.insert("done".to_string(), Value::bool(true));
                                Value::object(obj)
                            }
                        }));
                        let iter_fn = Value::native(Arc::new(|_args, vm| {
                            vm.this_value()
                        }));
                        let mut props = hashbrown::HashMap::new();
                        props.insert("next".to_string(), next_fn);
                        props.insert(format!("\0sym_{}", SYMBOL_ITERATOR), iter_fn);
                        props.insert(
                            format!("\0sym_{}", SYMBOL_TO_STRING_TAG),
                            Value::string("String Iterator".to_string()),
                        );
                        Value::object(props)
                    }));
                }
            }
        }
        let i = idx.to_number();
        if let Some(arr) = obj.as_array() {
            if i.is_finite() && i >= 0.0 {
                let arr = arr.borrow();
                let ix = i as usize;
                if ix < arr.len() { arr.get(ix) } else { Value::undefined() }
            } else {
                Value::undefined()
            }
        } else if let Some(s) = obj.as_str() {
            if i.is_finite() && i >= 0.0 {
                let ix = i as usize;
                // O(1) fast path for ASCII: a byte < 128 is always a char
                // boundary, so direct byte indexing is exact. Multi-byte
                // strings fall back to the char walk.
                let b = s.as_bytes();
                if ix < b.len() && b[ix] < 128 {
                    Value::char_str(b[ix])
                } else {
                    match s.chars().nth(ix) {
                        Some(c) => Value::char_str_utf8(c),
                        None => Value::undefined(),
                    }
                }
            } else {
                Value::undefined()
            }
        } else if let Some(m) = obj.as_object() {
            let m = m.borrow();
            let v = match idx.as_str() {
                // Borrow the key from the index Value — no per-access alloc.
                Some(key) => m.get(key).cloned().unwrap_or(Value::undefined()),
                None => {
                    if let Some(id) = idx.as_symbol() {
                        let key = format!("\0sym_{}", id);
                        m.get(&key).cloned().unwrap_or(Value::undefined())
                    } else {
                        let key = format!("{}", idx);
                        m.get(&key).cloned().unwrap_or(Value::undefined())
                    }
                }
            };
            // `m["counter"]` reads through live-import cells like `m.counter`.
            unwrap_cell(v)
        } else {
            Value::undefined()
        }
    }

    /// Full SetIndex semantics for `obj[idx] = val`: arrays resize to fit,
    /// objects set the stringified key; everything else ignores the write.
    #[inline]
    fn set_index_value(&self, obj: &Value, idx: &Value, val: Value) {
        if let Some(arr) = obj.as_array() {
            self.note_box_dirty(arr as *const RefCell<ArrayData> as usize);
            let mut arr = arr.borrow_mut();
            let i = idx.to_number();
            if i.is_finite() && i >= 0.0 {
                let ix = i as usize;
                arr.set_extend(ix, val);
            }
        } else if let Some(m) = obj.as_object() {
            self.note_box_dirty(m as *const RefCell<ObjectData> as usize);
            let mut m = m.borrow_mut();
            match idx.as_str() {
                // Borrow the key from the index Value — no per-access alloc.
                Some(key) => { m.set(key, val); }
                None => {
                    if let Some(id) = idx.as_symbol() {
                        let key = format!("\0sym_{}", id);
                        m.set(&key, val);
                    } else {
                        let key = format!("{}", idx);
                        m.set(&key, val);
                    }
                }
            }
        }
    }

    /// Monomorphic inline-cache property set, mirroring `get_prop`: a hit
    /// writes straight to `values[offset]` without the shape lookup or the
    /// per-write key allocation; a miss transitions the shape if needed and
    /// repopulates the cache. An own or inherited accessor intercepts the
    /// write first (setters run with the receiver as `this`; a getter-only
    /// accessor blocks the write, matching sloppy-mode JS).
    #[inline]
    fn set_prop(
        &mut self,
        pc: usize,
        od: &RefCell<ObjectData>,
        prop: &Value,
        val: Value,
        receiver: &Value,
    ) {
        self.note_box_dirty(od as *const RefCell<ObjectData> as usize);
        let slot = ic_slot(pc);
        let pb = prop.bits();
        let shape_ptr = od.borrow().shape_ptr();
        if let Some(offset) = self.ic[slot].probe(self.program_id, pc as u32, pb, shape_ptr) {
            let mut od = od.borrow_mut();
            let off = offset as usize;
            if off < od.values.len() {
                od.values[off] = val;
                od.deleted[off] = false;
                return;
            }
        }
        let (name, atom) = match (prop.as_str(), prop.as_atom()) {
            (Some(s), Some(a)) => (s, a),
            _ => return,
        };
        if name.starts_with('#') {
            let exists = od.borrow().shape.get_atom(atom).is_some();
            let is_ctor = self.call_stack.last().map(|f| f.is_ctor).unwrap_or(false);
            if !exists && !is_ctor {
                self.throw_exception(Value::string(format!(
                    "TypeError: Cannot write private member {} to an object whose class did not declare it",
                    name
                )));
                return;
            }
        }
        // Own accessor first — it takes precedence over a shadowing write.
        let own_acc = od
            .borrow()
            .accessors
            .as_ref()
            .and_then(|accs| accs.get(name).cloned());
        if let Some((_g, s)) = own_acc {
            let (s, receiver) = (s, receiver.clone());
            if s.is_function() || s.is_native() {
                self.call_value_with_this(&s, Some(receiver), &[val]);
            }
            return;
        }
        // Inherited accessor: a setter runs; a getter-only accessor blocks
        // the write (sloppy-mode semantics — no throw).
        // Read proto from the od borrow; the borrow guard is already scoped
        // out here, so nothing needs releasing before the proto-chain loop.
        let proto_clone = od.borrow().proto.clone();
        let mut cur = proto_clone;
        while let Some(cd) = cur.as_object() {
            let guard = cd.borrow();
            let acc = guard.accessors.as_ref().and_then(|a| a.get(name).cloned());
            let next = guard.proto.clone();
            drop(guard);
            if let Some((_g, s)) = acc {
                let (s, receiver) = (s, receiver.clone());
                if s.is_function() || s.is_native() {
                    self.call_value_with_this(&s, Some(receiver), &[val]);
                }
                return;
            }
            cur = next;
        }
        // Re-borrow mutably for the plain property write.
        let mut od = od.borrow_mut();
        let off = od.set_atom(atom, val);
        let shape = od.shape_ptr();
        drop(od);
        let fresh = IcEntry {
            program: self.program_id,
            pc: pc as u32,
            shape,
            offset: off,
            prop: pb,
        };
        self.ic[slot].update(fresh);
    }

    #[inline]
    fn read_u16(&self, offset: usize) -> u16 {
        ((self.bytecode[offset] as u16) << 8) | (self.bytecode[offset + 1] as u16)
    }

    #[inline]
    fn read_u32(&self, offset: usize) -> u32 {
        ((self.bytecode[offset] as u32) << 24)
            | ((self.bytecode[offset + 1] as u32) << 16)
            | ((self.bytecode[offset + 2] as u32) << 8)
            | (self.bytecode[offset + 3] as u32)
    }
}
