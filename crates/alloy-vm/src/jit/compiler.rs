use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{types, AbiParam, InstBuilder, MachMemFlags};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module};
use hashbrown::{HashMap, HashSet};

use super::JitLoopFn;
use crate::bytecode::Program;
use crate::opcode::Opcode;

pub struct JitCompiler {
    module: JITModule,
    ctx: Context,
    fn_builder_ctx: FunctionBuilderContext,
    next_fn_id: u32,
}

impl JitCompiler {
    pub fn new() -> Result<Self, String> {
        let mut flag_builder = settings::builder();
        flag_builder
            .set("use_colocated_libcalls", "false")
            .map_err(|e| e.to_string())?;
        flag_builder
            .set("is_pic", "false")
            .map_err(|e| e.to_string())?;
        flag_builder
            .set("opt_level", "speed")
            .map_err(|e| e.to_string())?;

        let isa_builder = cranelift_native::builder()
            .map_err(|e| format!("host target not supported by cranelift: {e}"))?;
        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .map_err(|e| format!("failed to create isa: {e}"))?;

        let builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        let module = JITModule::new(builder);
        let ctx = module.make_context();
        let fn_builder_ctx = FunctionBuilderContext::new();

        Ok(Self {
            module,
            ctx,
            fn_builder_ctx,
            next_fn_id: 0,
        })
    }

    pub fn compile_loop(
        &mut self,
        program: &Program,
        header_pc: usize,
        backedge_pc: usize,
    ) -> Option<JitLoopFn> {
        let bc = &program.bytecode;
        if header_pc >= backedge_pc || backedge_pc >= bc.len() {
            if std::env::var("ALLOY_JIT_LOG").is_ok() {
                eprintln!(
                    "[compile_loop] invalid pcs: header={} backedge={} len={}",
                    header_pc,
                    backedge_pc,
                    bc.len()
                );
            }
            return None;
        }

        // Step 1: Scan and validate all instructions in the loop body.
        let mut pc = header_pc;
        let mut inst_pcs = Vec::new();
        let mut slots_used = HashSet::new();
        let mut exit_targets = HashSet::new();

        while pc <= backedge_pc {
            inst_pcs.push(pc);
            let op = match bc.get(pc).and_then(|&b| Opcode::from_u8(b)) {
                Some(op) => op,
                None => {
                    if std::env::var("ALLOY_JIT_LOG").is_ok() {
                        eprintln!("[compile_loop] unknown opcode at pc={}", pc);
                    }
                    return None;
                }
            };
            match op {
                Opcode::Nop => {
                    pc += 1;
                }
                Opcode::StoreLocalLocal => {
                    if pc + 3 > bc.len() {
                        return None;
                    }
                    let dst = bc[pc + 1] as usize;
                    let src = bc[pc + 2] as usize;
                    slots_used.insert(dst);
                    slots_used.insert(src);
                    pc += 3;
                }
                Opcode::IncLocal => {
                    if pc + 4 > bc.len() {
                        return None;
                    }
                    let flags = bc[pc + 2];
                    if flags & 2 != 0 {
                        return None;
                    } // operand stack push not supported in JIT
                    let slot = bc[pc + 1] as usize;
                    slots_used.insert(slot);
                    pc += 4;
                }
                Opcode::AppendStringLocal => {
                    if pc + 4 > bc.len() {
                        return None;
                    }
                    let keep = bc[pc + 3];
                    if keep != 0 {
                        return None;
                    }
                    let slot = bc[pc + 1] as usize;
                    let src = bc[pc + 2] as usize;
                    slots_used.insert(slot);
                    slots_used.insert(src);
                    pc += 4;
                }
                Opcode::BinLocalLocalLocalArith => {
                    if pc + 5 > bc.len() {
                        return None;
                    }
                    let dst = bc[pc + 1] as usize;
                    let src1 = bc[pc + 2] as usize;
                    let src2 = bc[pc + 3] as usize;
                    let ar = bc[pc + 4];
                    if ar > 10 {
                        return None;
                    } // Only supported basic arith/bitwise
                    slots_used.insert(dst);
                    slots_used.insert(src1);
                    slots_used.insert(src2);
                    pc += 5;
                }
                Opcode::BinLocalLocalLocalInt => {
                    if pc + 8 > bc.len() {
                        return None;
                    }
                    let dst = bc[pc + 1] as usize;
                    let src = bc[pc + 2] as usize;
                    let ar = bc[pc + 3];
                    if ar > 10 {
                        return None;
                    }
                    slots_used.insert(dst);
                    slots_used.insert(src);
                    pc += 8;
                }
                Opcode::Arith2StoreLocalConst => {
                    if pc + 7 > bc.len() {
                        return None;
                    }
                    let ar_byte = bc[pc + 2];
                    if ar_byte & 0x80 != 0 {
                        return None;
                    } // keep pushes to stack
                    let slot = bc[pc + 1] as usize;
                    let ar = ar_byte & 0x7F;
                    if ar > 10 {
                        return None;
                    }
                    slots_used.insert(slot);
                    pc += 7;
                }
                Opcode::CmpLocalIntJumpIfFalsePop => {
                    if pc + 11 > bc.len() {
                        return None;
                    }
                    let slot = bc[pc + 1] as usize;
                    let target = read_u32(bc, pc + 7) as usize;
                    slots_used.insert(slot);
                    if target < header_pc || target > backedge_pc {
                        exit_targets.insert(target);
                    }
                    pc += 11;
                }
                Opcode::CmpLocalLocalJumpIfFalsePop | Opcode::CmpLocalLocalJumpIfFalse => {
                    if pc + 8 > bc.len() {
                        return None;
                    }
                    let s1 = bc[pc + 1] as usize;
                    let s2 = bc[pc + 2] as usize;
                    let target = read_u32(bc, pc + 4) as usize;
                    slots_used.insert(s1);
                    slots_used.insert(s2);
                    if target < header_pc || target > backedge_pc {
                        exit_targets.insert(target);
                    }
                    pc += 8;
                }
                Opcode::Jump => {
                    if pc + 5 > bc.len() {
                        return None;
                    }
                    let target = read_u32(bc, pc + 1) as usize;
                    if target < header_pc || target > backedge_pc {
                        exit_targets.insert(target);
                    }
                    pc += 5;
                }
                _ => {
                    if std::env::var("ALLOY_JIT_LOG").is_ok() {
                        eprintln!("[compile_loop] unsupported opcode: {:?}", op);
                    }
                    return None;
                }
            }
        }

        if slots_used.is_empty() {
            return None;
        }

        // Step 2: Build Cranelift IR
        self.ctx.clear();
        self.ctx.func.signature.call_conv = self.module.isa().default_call_conv();
        self.ctx
            .func
            .signature
            .params
            .push(AbiParam::new(types::I64)); // slots_ptr
        self.ctx
            .func
            .signature
            .params
            .push(AbiParam::new(types::I64)); // slots_len
        self.ctx
            .func
            .signature
            .params
            .push(AbiParam::new(types::I64)); // max_trips
        self.ctx
            .func
            .signature
            .returns
            .push(AbiParam::new(types::I64)); // resume_pc

        let mut builder = FunctionBuilder::new(&mut self.ctx.func, &mut self.fn_builder_ctx);

        let entry_block = builder.create_block();
        builder.append_block_params_for_function_params(entry_block);
        builder.switch_to_block(entry_block);

        let slots_ptr = builder.block_params(entry_block)[0];
        let slots_len = builder.block_params(entry_block)[1];
        let max_trips = builder.block_params(entry_block)[2];

        // Create Cranelift Variables for each slot used
        let mut sorted_slots: Vec<usize> = slots_used.into_iter().collect();
        sorted_slots.sort();

        let mut var_map = HashMap::new();

        for &slot in &sorted_slots {
            let var = builder.declare_var(types::I64);
            var_map.insert(slot, var);
        }

        // Trip counter variable
        let trip_var = builder.declare_var(types::I64);

        // Pre-create basic blocks for all instructions and exit targets
        let mut block_map = HashMap::new();
        for &ipc in &inst_pcs {
            block_map.insert(ipc, builder.create_block());
        }

        let mut exit_block_map = HashMap::new();
        for &epc in &exit_targets {
            exit_block_map.insert(epc, builder.create_block());
        }

        let bailout_block = builder.create_block();
        let yield_block = builder.create_block();

        // Entry block:
        // 1. Verify bounds: if max slot >= slots_len, bailout
        let max_slot_needed = sorted_slots.last().copied().unwrap_or(0);
        let max_slot_val = builder.ins().iconst(types::I64, max_slot_needed as i64);
        let in_bounds = builder
            .ins()
            .icmp(IntCC::UnsignedLessThan, max_slot_val, slots_len);
        let check_types_block = builder.create_block();
        builder
            .ins()
            .brif(in_bounds, check_types_block, &[], bailout_block, &[]);

        // 2. In check_types_block, load each slot, verify it is an INT, and unbox it
        builder.switch_to_block(check_types_block);
        let tag_mask = builder
            .ins()
            .iconst(types::I64, 0xFFFF_0000_0000_0000u64 as i64);
        let tag_int = builder
            .ins()
            .iconst(types::I64, 0xFFF8_0000_0000_0000u64 as i64);

        for &slot in &sorted_slots {
            let offset = (slot * 8) as i32;
            let raw_val = builder
                .ins()
                .load(types::I64, MachMemFlags::new(), slots_ptr, offset);
            let tag = builder.ins().band(raw_val, tag_mask);
            let is_int = builder.ins().icmp(IntCC::Equal, tag, tag_int);

            let next_block = builder.create_block();
            builder
                .ins()
                .brif(is_int, next_block, &[], bailout_block, &[]);

            builder.switch_to_block(next_block);
            // Unbox integer payload: (raw << 16) >> 16
            let shl = builder.ins().ishl_imm_u(raw_val, 16);
            let unboxed = builder.ins().sshr_imm_u(shl, 16);
            builder.def_var(var_map[&slot], unboxed);
        }

        // Initialize trip count to 0 and jump to header_pc block
        let zero = builder.ins().iconst(types::I64, 0);
        builder.def_var(trip_var, zero);
        builder.ins().jump(block_map[&header_pc], &[]);

        // Translate each instruction
        for (idx, &ipc) in inst_pcs.iter().enumerate() {
            let blk = block_map[&ipc];
            builder.switch_to_block(blk);

            // If this is the header block, increment trips and check max_trips
            if ipc == header_pc {
                let cur_trips = builder.use_var(trip_var);
                let hit_limit =
                    builder
                        .ins()
                        .icmp(IntCC::SignedGreaterThanOrEqual, cur_trips, max_trips);
                let loop_cont_block = builder.create_block();
                builder
                    .ins()
                    .brif(hit_limit, yield_block, &[], loop_cont_block, &[]);

                builder.switch_to_block(loop_cont_block);
                let next_trips = builder.ins().iadd_imm_s(cur_trips, 1);
                builder.def_var(trip_var, next_trips);
            }

            let op = Opcode::from_u8(bc[ipc]).unwrap();
            match op {
                Opcode::Nop => {
                    let next_pc = inst_pcs.get(idx + 1).copied().unwrap_or(ipc + 1);
                    builder.ins().jump(block_map[&next_pc], &[]);
                }
                Opcode::StoreLocalLocal => {
                    let dst = bc[ipc + 1] as usize;
                    let src = bc[ipc + 2] as usize;
                    let val = builder.use_var(var_map[&src]);
                    builder.def_var(var_map[&dst], val);
                    let next_pc = inst_pcs.get(idx + 1).copied().unwrap_or(ipc + 3);
                    builder.ins().jump(block_map[&next_pc], &[]);
                }
                Opcode::IncLocal => {
                    let slot = bc[ipc + 1] as usize;
                    let delta = bc[ipc + 3] as i8 as i64;
                    let val = builder.use_var(var_map[&slot]);
                    let next_val = builder.ins().iadd_imm_s(val, delta);
                    builder.def_var(var_map[&slot], next_val);
                    let next_pc = inst_pcs.get(idx + 1).copied().unwrap_or(ipc + 4);
                    builder.ins().jump(block_map[&next_pc], &[]);
                }
                Opcode::AppendStringLocal => {
                    let slot = bc[ipc + 1] as usize;
                    let src = bc[ipc + 2] as usize;
                    let v1 = builder.use_var(var_map[&slot]);
                    let v2 = builder.use_var(var_map[&src]);
                    let res = builder.ins().iadd(v1, v2);
                    builder.def_var(var_map[&slot], res);
                    let next_pc = inst_pcs.get(idx + 1).copied().unwrap_or(ipc + 4);
                    builder.ins().jump(block_map[&next_pc], &[]);
                }
                Opcode::BinLocalLocalLocalArith => {
                    let dst = bc[ipc + 1] as usize;
                    let src1 = bc[ipc + 2] as usize;
                    let src2 = bc[ipc + 3] as usize;
                    let ar = bc[ipc + 4];
                    let v1 = builder.use_var(var_map[&src1]);
                    let v2 = builder.use_var(var_map[&src2]);
                    let res = emit_arith(&mut builder, ar, v1, v2);
                    builder.def_var(var_map[&dst], res);
                    let next_pc = inst_pcs.get(idx + 1).copied().unwrap_or(ipc + 5);
                    builder.ins().jump(block_map[&next_pc], &[]);
                }
                Opcode::BinLocalLocalLocalInt => {
                    let dst = bc[ipc + 1] as usize;
                    let src = bc[ipc + 2] as usize;
                    let ar = bc[ipc + 3];
                    let imm = read_i32(bc, ipc + 4) as i64;
                    let v = builder.use_var(var_map[&src]);
                    let imm_val = builder.ins().iconst(types::I64, imm);
                    let res = emit_arith(&mut builder, ar, v, imm_val);
                    builder.def_var(var_map[&dst], res);
                    let next_pc = inst_pcs.get(idx + 1).copied().unwrap_or(ipc + 8);
                    builder.ins().jump(block_map[&next_pc], &[]);
                }
                Opcode::Arith2StoreLocalConst => {
                    let slot = bc[ipc + 1] as usize;
                    let ar = bc[ipc + 2] & 0x7F;
                    let imm = read_i32(bc, ipc + 3) as i64;
                    let v = builder.use_var(var_map[&slot]);
                    let imm_val = builder.ins().iconst(types::I64, imm);
                    let res = emit_arith(&mut builder, ar, v, imm_val);
                    builder.def_var(var_map[&slot], res);
                    let next_pc = inst_pcs.get(idx + 1).copied().unwrap_or(ipc + 7);
                    builder.ins().jump(block_map[&next_pc], &[]);
                }
                Opcode::CmpLocalIntJumpIfFalsePop => {
                    let slot = bc[ipc + 1] as usize;
                    let imm = read_i32(bc, ipc + 2) as i64;
                    let cmp = bc[ipc + 6];
                    let target = read_u32(bc, ipc + 7) as usize;

                    let v = builder.use_var(var_map[&slot]);
                    let imm_val = builder.ins().iconst(types::I64, imm);
                    let cond = emit_cmp(&mut builder, cmp, v, imm_val);

                    let next_pc = inst_pcs.get(idx + 1).copied().unwrap_or(ipc + 11);
                    let then_blk = block_map[&next_pc];
                    let else_blk = exit_block_map
                        .get(&target)
                        .copied()
                        .or_else(|| block_map.get(&target).copied())
                        .unwrap_or(bailout_block);

                    builder.ins().brif(cond, then_blk, &[], else_blk, &[]);
                }
                Opcode::CmpLocalLocalJumpIfFalsePop | Opcode::CmpLocalLocalJumpIfFalse => {
                    let s1 = bc[ipc + 1] as usize;
                    let s2 = bc[ipc + 2] as usize;
                    let cmp = bc[ipc + 3];
                    let target = read_u32(bc, ipc + 4) as usize;

                    let v1 = builder.use_var(var_map[&s1]);
                    let v2 = builder.use_var(var_map[&s2]);
                    let cond = emit_cmp(&mut builder, cmp, v1, v2);

                    let next_pc = inst_pcs.get(idx + 1).copied().unwrap_or(ipc + 8);
                    let then_blk = block_map[&next_pc];
                    let else_blk = exit_block_map
                        .get(&target)
                        .copied()
                        .or_else(|| block_map.get(&target).copied())
                        .unwrap_or(bailout_block);

                    builder.ins().brif(cond, then_blk, &[], else_blk, &[]);
                }
                Opcode::Jump => {
                    let target = read_u32(bc, ipc + 1) as usize;
                    if target == header_pc {
                        builder.ins().jump(block_map[&header_pc], &[]);
                    } else if let Some(&target_blk) = block_map.get(&target) {
                        builder.ins().jump(target_blk, &[]);
                    } else if let Some(&exit_blk) = exit_block_map.get(&target) {
                        builder.ins().jump(exit_blk, &[]);
                    } else {
                        builder.ins().jump(bailout_block, &[]);
                    }
                }
                _ => unreachable!(),
            }
        }

        // Helper to write back all variables to slots_ptr
        let emit_writeback = |builder: &mut FunctionBuilder,
                              sorted_slots: &[usize],
                              var_map: &HashMap<usize, Variable>| {
            let payload_mask = builder
                .ins()
                .iconst(types::I64, 0x0000_FFFF_FFFF_FFFFu64 as i64);
            let tag_int = builder
                .ins()
                .iconst(types::I64, 0xFFF8_0000_0000_0000u64 as i64);
            for &s in sorted_slots {
                let v = builder.use_var(var_map[&s]);
                let masked = builder.ins().band(v, payload_mask);
                let tagged = builder.ins().bor(masked, tag_int);
                let offset = (s * 8) as i32;
                builder
                    .ins()
                    .store(MachMemFlags::new(), tagged, slots_ptr, offset);
            }
        };

        // Exit blocks:
        for (&epc, &eblk) in &exit_block_map {
            builder.switch_to_block(eblk);
            emit_writeback(&mut builder, &sorted_slots, &var_map);
            let ret_val = builder.ins().iconst(types::I64, epc as i64);
            builder.ins().return_(&[ret_val]);
        }

        // Yield block (returns header_pc so interpreter can take over if needed)
        builder.switch_to_block(yield_block);
        emit_writeback(&mut builder, &sorted_slots, &var_map);
        let ret_header = builder.ins().iconst(types::I64, header_pc as i64);
        builder.ins().return_(&[ret_header]);

        // Bailout block (return u64::MAX sentinel)
        builder.switch_to_block(bailout_block);
        let ret_bailout = builder.ins().iconst(types::I64, -1i64);
        builder.ins().return_(&[ret_bailout]);

        builder.seal_all_blocks();
        builder.finalize(self.module.target_config());

        // Step 3: Compile function with JITModule
        self.next_fn_id += 1;
        let fn_name = format!("alloy_jit_loop_{}_{}", header_pc, self.next_fn_id);
        let func_id =
            match self
                .module
                .declare_function(&fn_name, Linkage::Export, &self.ctx.func.signature)
            {
                Ok(id) => id,
                Err(e) => {
                    if std::env::var("ALLOY_JIT_LOG").is_ok() {
                        eprintln!("[compile_loop] declare_function failed: {}", e);
                    }
                    return None;
                }
            };

        if let Err(e) = self.module.define_function(func_id, &mut self.ctx) {
            if std::env::var("ALLOY_JIT_LOG").is_ok() {
                eprintln!("[compile_loop] define_function failed: {}", e);
            }
            return None;
        }
        self.module.clear_context(&mut self.ctx);
        if let Err(e) = self.module.finalize_definitions() {
            if std::env::var("ALLOY_JIT_LOG").is_ok() {
                eprintln!("[compile_loop] finalize_definitions failed: {}", e);
            }
            return None;
        }

        let code_ptr = self.module.get_finalized_function(func_id);
        if std::env::var("ALLOY_JIT_LOG").is_ok() {
            eprintln!(
                "[compile_loop] SUCCESS! func_id={:?} code_ptr={:p}",
                func_id, code_ptr
            );
        }
        let jit_fn: JitLoopFn = unsafe { std::mem::transmute(code_ptr) };
        Some(jit_fn)
    }
}

fn emit_arith(
    builder: &mut FunctionBuilder,
    ar: u8,
    v1: cranelift_codegen::ir::Value,
    v2: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    match ar {
        0 => builder.ins().iadd(v1, v2),
        1 => builder.ins().isub(v1, v2),
        2 => builder.ins().imul(v1, v2),
        3 => builder.ins().sdiv(v1, v2),
        4 => builder.ins().srem(v1, v2),
        5 => builder.ins().band(v1, v2),
        6 => builder.ins().bor(v1, v2),
        7 => builder.ins().bxor(v1, v2),
        8 => builder.ins().ishl(v1, v2),
        9 => builder.ins().sshr(v1, v2),
        10 => builder.ins().ushr(v1, v2),
        _ => v1,
    }
}

fn emit_cmp(
    builder: &mut FunctionBuilder,
    cmp: u8,
    v1: cranelift_codegen::ir::Value,
    v2: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    let cc = match cmp {
        0 => IntCC::SignedLessThan,
        1 => IntCC::SignedLessThanOrEqual,
        2 => IntCC::SignedGreaterThan,
        3 => IntCC::SignedGreaterThanOrEqual,
        4 | 6 => IntCC::Equal,
        5 | 7 => IntCC::NotEqual,
        _ => IntCC::Equal,
    };
    builder.ins().icmp(cc, v1, v2)
}

#[inline(always)]
fn read_u32(bc: &[u8], offset: usize) -> u32 {
    ((bc[offset] as u32) << 24)
        | ((bc[offset + 1] as u32) << 16)
        | ((bc[offset + 2] as u32) << 8)
        | (bc[offset + 3] as u32)
}

#[inline(always)]
fn read_i32(bc: &[u8], offset: usize) -> i32 {
    read_u32(bc, offset) as i32
}
