use alloy_core::heap::{ArenaHeap, HeapGuard, PromoteMap};
use alloy_core::value::{Value, sweep_young, walk_value};
use std::collections::HashMap;
use crate::opcode::Opcode;

/// Big-endian i32 read from a bytecode buffer (the compiler's imm encoding).
#[inline]
fn rd_i32(bc: &[u8], o: usize) -> i32 {
    if o + 4 > bc.len() {
        return 0;
    }
    i32::from_be_bytes([bc[o], bc[o + 1], bc[o + 2], bc[o + 3]])
}

/// Big-endian u32 read from a bytecode buffer (jump-target encoding).
#[inline]
fn rd_u32(bc: &[u8], o: usize) -> u32 {
    if o + 4 > bc.len() {
        return 0;
    }
    u32::from_be_bytes([bc[o], bc[o + 1], bc[o + 2], bc[o + 3]])
}

/// Map an arith *opcode byte* (Add..Pow) to its fused `ar` code, mirroring the
/// compiler's `arith_code` / the VM's `arith_apply` codes: 0=+ 1=- 2=* 3=/ 4=%
/// 5=& 6=| 7=^ 8=<< 9=>> 10=>>> 11=**. Returns None for non-arith opcodes.
#[inline]
/// Symbolic name of an ArithChain step's arith code (0=+ 1=- 2=* 3=/ 4=% …).
fn ar_name(ar: u8) -> &'static str {
    match ar {
        0 => "+",
        1 => "-",
        2 => "*",
        3 => "/",
        4 => "%",
        5 => "&",
        6 => "|",
        7 => "^",
        8 => "<<",
        9 => ">>",
        10 => ">>>",
        _ => "**",
    }
}

fn arith_code_of(b: u8) -> Option<u8> {
    match Opcode::from_u8(b) {
        Some(Opcode::Add) => Some(0),
        Some(Opcode::Subtract) => Some(1),
        Some(Opcode::Multiply) => Some(2),
        Some(Opcode::Divide) => Some(3),
        Some(Opcode::Modulo) => Some(4),
        Some(Opcode::BitAnd) => Some(5),
        Some(Opcode::BitOr) => Some(6),
        Some(Opcode::BitXor) => Some(7),
        Some(Opcode::Shl) => Some(8),
        Some(Opcode::Shr) => Some(9),
        Some(Opcode::UShr) => Some(10),
        Some(Opcode::Pow) => Some(11),
        _ => None,
    }
}

/// Constant-fold one fused `ar` code at compile time, mirroring the VM's
/// `arith_apply` (which delegates to the `Value` methods). Only ever called
/// with two int operands from folded literals.
#[inline]
fn fold_arith(l: Value, r: Value, ar: u8) -> Value {
    match ar {
        0 => l.add(&r),
        1 => l.subtract(&r),
        2 => l.multiply(&r),
        3 => l.divide(&r),
        4 => l.modulo(&r),
        5 => l.bitand(&r),
        6 => l.bitor(&r),
        7 => l.bitxor(&r),
        8 => l.shl(&r),
        9 => l.shr(&r),
        10 => l.ushr(&r),
        11 => l.pow(&r),
        _ => Value::undefined(),
    }
}

/// Byte size of a CmpLocalLocal (4: op+a+b+cmp) or CmpLocalInt (7:
/// op+slot+i32+cmp) comparison — the two shapes a comparison chain can
/// mix in either position.
#[inline]
fn cmp_len(op: Opcode) -> Option<usize> {
    match op {
        Opcode::CmpLocalLocal => Some(4),
        Opcode::CmpLocalInt => Some(7),
        _ => None,
    }
}

/// The raw single-dispatch comparison opcodes (operands already on the
/// stack). The fused condition opcodes reuse their byte as the cmp operand.
#[inline]
fn is_cmp_op(op: Opcode) -> bool {
    matches!(
        op,
        Opcode::Equal
            | Opcode::NotEqual
            | Opcode::StrictEqual
            | Opcode::StrictNotEqual
            | Opcode::Less
            | Opcode::Greater
            | Opcode::LessEqual
            | Opcode::GreaterEqual
    )
}

/// Offset of the u32 jump target within an instruction, for the ops that
/// carry one. The standard jumps put it at +1; the fused condition opcodes
/// carry it after their operands.
#[inline]
fn jump_target_offset(op: Opcode) -> Option<usize> {
    match op {
        Opcode::Jump
        | Opcode::JumpIfFalse
        | Opcode::JumpIfTrue
        | Opcode::JumpIfFalsePop
        | Opcode::JumpIfTruePop
        | Opcode::JumpIfNullish
        | Opcode::TryStart => Some(1),
        Opcode::CmpLocalLocalJumpIfFalsePop => Some(4),
        Opcode::LoadIndexCmpLocalJumpIfFalsePop => Some(5),
        Opcode::CmpLocalIntJumpIfFalsePop => Some(7),
        Opcode::ArithLocalIntCmpJumpIfFalsePop => Some(12),
        _ => None,
    }
}

/// Build the fused bytes for a two-comparison short-circuit chain. `c1`/`c2`
/// are the offsets of the two Cmp instructions, `l1`/`l2` their lengths. `or`
/// selects `||` (short-circuit when the first bool is truthy) vs `&&`; it is
/// carried in bit 7 of the second cmp byte, which the VM masks off.
#[inline]
fn fuse_cmp_chain(
    src: &[u8],
    c1: usize,
    l1: usize,
    c2: usize,
    l2: usize,
    or: bool,
) -> (Opcode, Vec<u8>) {
    let flag = (or as u8) << 7;
    let (op, mut v) = match (l1, l2) {
        (4, 4) => (
            Opcode::CmpAndLocalLocal,
            vec![
                0,
                src[c1 + 1],
                src[c1 + 2],
                src[c1 + 3],
                src[c2 + 1],
                src[c2 + 2],
                src[c2 + 3] | flag,
            ],
        ),
        (4, 7) => {
            let mut v = vec![0, src[c1 + 1], src[c1 + 2], src[c1 + 3], src[c2 + 1]];
            v.extend_from_slice(&src[c2 + 2..c2 + 6]);
            v.push(src[c2 + 6] | flag);
            (Opcode::CmpAndLocalInt, v)
        }
        (7, 4) => {
            let mut v = vec![0, src[c1 + 1]];
            v.extend_from_slice(&src[c1 + 2..c1 + 6]);
            v.push(src[c1 + 6]);
            v.push(src[c2 + 1]);
            v.push(src[c2 + 2]);
            v.push(src[c2 + 3] | flag);
            (Opcode::CmpAndIntLocal, v)
        }
        (7, 7) => {
            let mut v = vec![0, src[c1 + 1]];
            v.extend_from_slice(&src[c1 + 2..c1 + 6]);
            v.push(src[c1 + 6]);
            v.push(src[c2 + 1]);
            v.extend_from_slice(&src[c2 + 2..c2 + 6]);
            v.push(src[c2 + 6] | flag);
            (Opcode::CmpAndIntInt, v)
        }
        _ => unreachable!("cmp_len guarantees 4 or 7"),
    };
    v[0] = op as u8;
    (op, v)
}

/// Counter key for a fused comparison chain, split by fused shape, `&&` vs
/// `||`, and value vs condition context.
fn cmp_chain_key(fop: Opcode, or: bool, cond: bool) -> &'static str {
    use Opcode::*;
    match (fop, or, cond) {
        (CmpAndLocalLocal, false, false) => "cmpchain_val_and_LL",
        (CmpAndLocalLocal, true, false) => "cmpchain_val_or_LL",
        (CmpAndLocalLocal, false, true) => "cmpchain_cond_and_LL",
        (CmpAndLocalLocal, true, true) => "cmpchain_cond_or_LL",
        (CmpAndLocalInt, false, false) => "cmpchain_val_and_LI",
        (CmpAndLocalInt, true, false) => "cmpchain_val_or_LI",
        (CmpAndLocalInt, false, true) => "cmpchain_cond_and_LI",
        (CmpAndLocalInt, true, true) => "cmpchain_cond_or_LI",
        (CmpAndIntLocal, false, false) => "cmpchain_val_and_IL",
        (CmpAndIntLocal, true, false) => "cmpchain_val_or_IL",
        (CmpAndIntLocal, false, true) => "cmpchain_cond_and_IL",
        (CmpAndIntLocal, true, true) => "cmpchain_cond_or_IL",
        (CmpAndIntInt, false, false) => "cmpchain_val_and_II",
        (CmpAndIntInt, true, false) => "cmpchain_val_or_II",
        (CmpAndIntInt, false, true) => "cmpchain_cond_and_II",
        (CmpAndIntInt, true, true) => "cmpchain_cond_or_II",
        _ => "cmpchain_unknown",
    }
}

/// Byte size of an instruction. This is the canonical size table for the
/// peephole walker (and the only place the pass needs one — every opcode's
/// size is fixed by its encoding).
#[inline]
/// Byte length of the instruction at `offset` in `src`. Most opcodes have a
/// fixed size; the variable-length ArithChain reads its step count from the
/// stream (`[op][count][term][step x count]`, 5 bytes per step).
pub(crate) fn op_len(src: &[u8], offset: usize) -> usize {
    let op = Opcode::from_u8(src.get(offset).copied().unwrap_or(0)).unwrap_or(Opcode::Nop);
    if op == Opcode::ArithChain {
        let count = src.get(offset + 1).copied().unwrap_or(0) as usize;
        return 3 + count * 5;
    }
    match op {
        // op + u16 (SetProperty is a 1-byte stack op: obj, name, value are
        // consumed from the operand stack, unlike GetProperty's inline index).
        Opcode::LoadConst
        | Opcode::LoadGlobal
        | Opcode::TypeOfGlobal
        | Opcode::StoreGlobal
        | Opcode::MakeArray
        | Opcode::GetProperty
        | Opcode::GetPropertyCell
        | Opcode::PeekProperty
        | Opcode::LoadPython => 3,
        // op + u32
        Opcode::LoadInt
        | Opcode::Jump
        | Opcode::JumpIfFalse
        | Opcode::JumpIfTrue
        | Opcode::JumpIfFalsePop
        | Opcode::JumpIfTruePop
        | Opcode::JumpIfNullish
        | Opcode::TryStart
        | Opcode::ReadShared
        | Opcode::WriteShared => 5,
        // op + u16 + u16 (field count + spread mask)
        Opcode::MakeObject => 5,
        // op + u16 + u16 (pattern const + flags const)
        Opcode::MakeRegex => 5,
        // op + u8
        Opcode::LoadLocal
        | Opcode::StoreLocal
        | Opcode::Call
        | Opcode::CallKeep0
        | Opcode::CaptureLocal
        | Opcode::CaptureUpvalue
        | Opcode::LoadUpvalue
        | Opcode::StoreUpvalue
        | Opcode::LoadCell
        | Opcode::StoreCell
        | Opcode::NewPromise
        | Opcode::ArraySlice
        | Opcode::IncIndexConst
        | Opcode::New
        | Opcode::CallMethod
        | Opcode::CallMethodKeep0 => 2,
        // op + u8 + u8
        Opcode::AllocShared
        | Opcode::MakeRestArray
        | Opcode::AppendStringPop
        | Opcode::LoadLocalLocalGetIndex => 3,
        // op + u8 + u8 + u8
        Opcode::BinLocalLocal
        | Opcode::CmpLocalLocal
        | Opcode::ArithStoreLocal
        | Opcode::IncLocal
        | Opcode::ArithStoreUpvalue
        | Opcode::ArithWriteProp
        | Opcode::IncPropConst
        | Opcode::AppendStringLocal
        | Opcode::LoadLocalGetPropConst => 4,
        // op + u16 + u16
        Opcode::MakeArraySpread => 5,
        // op + u8 + u16 (spread-call: argc byte then a spread mask)
        Opcode::CallSpread
        | Opcode::CallSpreadKeep0
        | Opcode::CallMethodSpread
        | Opcode::CallMethodSpreadKeep0
        | Opcode::NewSpread => 4,
        // op + u8 (getter/setter kind)
        Opcode::SetAccessor => 2,
        // op + u8 + u16 + u8
        Opcode::AppendStringConst => 5,
        // op + u8 + u8 + u8
        Opcode::CmpLocalInt
        | Opcode::BinLocalInt
        | Opcode::BinIntLocal => 7,
        // op + u8 + u32
        Opcode::CompoundIndexConst => 6,
        // op + u8 + u16 + u32
        Opcode::CompoundPropConst => 8,
        // op + u8 + u8 + u32
        Opcode::Arith2StoreLocalConst => 7,
        // op + u8 + u8 + u32 + u8 + u32
        Opcode::Arith3StoreLocalConstConst => 12,
        // op + u32 + u8 + u8 + u8 + u32
        Opcode::Arith3StoreConstLocalConst => 12,
        // op + u8 + u8 + u8 + u32 + u8
        Opcode::BinLocalLocalInt => 9,
        // comparison chains: op + 2 operand pairs + 2 cmp bytes
        Opcode::CmpAndLocalLocal => 7,
        // condition fusions: operands + cmp + u32 jump target
        Opcode::CmpLocalLocalJumpIfFalsePop => 8,
        Opcode::LoadIndexCmpLocalJumpIfFalsePop => 9,
        Opcode::CmpLocalIntJumpIfFalsePop => 11,
        Opcode::SetIndexLocalPlusIntLocalGetLocal => 10,
        Opcode::ArithLocalIntCmpJumpIfFalsePop => 16,
        // index-write fusions: 3-4 operand slots, no jump
        Opcode::SetIndexLocalLocal => 4,
        Opcode::SetIndexLocalGetLocal => 5,
        // op + u8 + u8 + u8 + u8 + i32 + u8
        Opcode::CmpAndLocalInt => 10,
        // op + u8 + i32 + u8 + u8 + u8 + u8
        Opcode::CmpAndIntLocal => 10,
        // op + u8 + i32 + u8 + u8 + i32 + u8
        Opcode::CmpAndIntInt => 13,
        // op + u16 + u8 + u8 + u8 (constant index, upvalue count, param
        // count, uses-arguments flag)
        Opcode::NewClosure => 6,
        // op + u8
        Opcode::ArithWriteIndex => 2,
        // single byte: constants, stack ops, comparisons, control flow,
        // async/exceptions, shared-memory/message-passing, everything else.
        _ => 1,
    }
}

/// A compiled program. The `heap` owns the arena memory behind every heap
/// constant (strings/arrays/objects): the compiler allocates constants into it
/// while emitting, and it stays alive as long as the program (the VM keeps
/// every loaded program in its registry). Not `Clone` — a program's constants
/// must not share arena memory; use [`Program::deep_clone`] for an
/// independent copy.
///
/// Constants are **interned** ([`add_constant`] dedupes identical strings and
/// numbers) and the program heap is **compacted** after emit / deserialize
/// ([`compact_constants`] promotes the live constants into the old
/// generation and sweeps the dead ones), so the compiler's duplicate
/// property-name and literal allocations are reclaimed instead of bloating
/// every loaded program.
#[derive(Debug)]
pub struct Program {
    pub bytecode: Vec<u8>,
    pub constants: Vec<Value>,
    pub sources: Vec<String>,
    pub globals: Vec<String>,
    /// (export name, binding name) pairs of a module (`export` declarations
    /// in a `require`d file), in declaration order. Aliases are `(public, `
    /// binding)` — `export { a as b }` is `("b", "a")`; a plain `export let
    /// x` is `("x", "x")`; `export default e` is `("default", "\0default")`.
    /// Empty for ordinary scripts.
    pub exports: Vec<(String, String)>,
    /// True when the program was compiled as a module (`compile_module`):
    /// top-level declarations are globals and `exports` may be non-empty.
    /// `require` refuses to load non-module `.ax` files (they would resolve
    /// their top-level locals against the wrong frame base).
    pub is_module: bool,
    pub heap: ArenaHeap,
    /// Content → constant index for string interning. Derived state (not
    /// serialized; rebuilt on demand): the bytecode only references indices
    /// into [`Program::constants`].
    intern: HashMap<String, u16>,
    /// Exact-bits → constant index for numeric interning (the string-only
    /// mirror of `intern`; `0` vs `-0` and int vs double encodings stay
    /// distinct because they differ in bits). Derived state, not serialized.
    intern_bits: HashMap<u64, u16>,
    /// How many times each peephole fusion fired during the last pass
    /// (measurement only; not serialized, cleared on each peephole run).
    pub peephole_counts: HashMap<&'static str, u64>,
}

impl Program {
    pub fn new() -> Self {
        Self {
            bytecode: Vec::with_capacity(1024),
            constants: Vec::with_capacity(256),
            sources: Vec::new(),
            globals: Vec::new(),
            exports: Vec::new(),
            is_module: false,
            heap: ArenaHeap::new(1 << 16),
            intern: HashMap::new(),
            intern_bits: HashMap::new(),
            peephole_counts: HashMap::new(),
        }
    }

    /// An independent copy whose constants live in their own arena (used by
    /// the `--bench` loop, which needs a fresh program per VM).
    pub fn deep_clone(&self) -> Result<Self, SerError> {
        Self::from_bytes(&self.to_bytes()?)
    }

    pub fn emit_op(&mut self, op: Opcode) {
        self.bytecode.push(op as u8);
    }

    pub fn emit_u8(&mut self, val: u8) {
        self.bytecode.push(val);
    }

    pub fn emit_u16(&mut self, val: u16) {
        self.bytecode.push((val >> 8) as u8);
        self.bytecode.push(val as u8);
    }

    pub fn emit_u32(&mut self, val: u32) {
        self.bytecode.push((val >> 24) as u8);
        self.bytecode.push((val >> 16) as u8);
        self.bytecode.push((val >> 8) as u8);
        self.bytecode.push(val as u8);
    }

    pub fn emit_i32(&mut self, val: i32) {
        self.emit_u32(val as u32);
    }

    /// Add a constant, **interning** it: identical strings (by content) and
    /// identical numbers (by exact bits — `0` and `-0` stay distinct, as do
    /// int vs double encodings) share one slot and one heap allocation. The
    /// compiler emits property names and string literals thousands of times;
    /// without this every occurrence would allocate a fresh string into the
    /// program's arena.
    pub fn add_constant(&mut self, val: Value) -> u16 {
        if let Some(s) = val.as_str() {
            if let Some(&i) = self.intern.get(s) {
                return i;
            }
            let idx = self.constants.len() as u16;
            self.intern.insert(s.to_string(), idx);
            self.constants.push(val);
            return idx;
        }
        let bits = val.bits();
        if let Some(&i) = self.intern_bits.get(&bits) {
            return i;
        }
        let idx = self.constants.len() as u16;
        self.intern_bits.insert(bits, idx);
        self.constants.push(val);
        idx
    }

    /// Reclaim the compiler's constant garbage: the emit pass allocates many
    /// transient strings into this program's arena (every failed intern, every
    /// intermediate literal). Walk the live constants (promoting them into
    /// this heap's old generation, so the box addresses stay stable for the
    /// program's lifetime) and sweep the young generation — dead constant
    /// allocations are dropped and the young arena is bulk-reset.
    pub fn compact_constants(&mut self) {
        let mut map = PromoteMap::new();
        let mut visited = std::collections::HashSet::new();
        for c in &mut self.constants {
            walk_value(&mut self.heap, &mut map, &mut visited, None, c);
        }
        sweep_young(&mut self.heap, &map);
    }

    /// Peephole-fuse the emitted bytecode: collapse the stack-op sequences the
    /// AST-level fusion misses into superinstructions, and fold constant
    /// arithmetic. Runs once at the end of compilation, before the constants
    /// are compacted, so newly folded constants are swept with everything
    /// else. Each fusion also folds an immediately-following `Pop` into a
    /// keep=0 bit on the fused opcode's ar byte, so discarded statement values
    /// leave nothing (the old `...; AR; Pop` pair becomes one dispatch).
    ///
    /// ```text
    /// LoadInt imm + LoadLocal s + AR     -> BinIntLocal        (`3 * n`)
    /// LoadLocal s + LoadInt imm + AR     -> BinLocalInt        (`(x) * 3` —
    ///   the AST fusion requires an Ident child, so parens fall through)
    /// LoadLocal a + LoadLocal b + AR     -> BinLocalLocal      (`(x) + (y)`)
    /// BinLocalLocal + LoadInt imm + AR   -> BinLocalLocalInt   (`(i + j) % 7`)
    /// LoadInt a + LoadInt b + AR         -> LoadConst          (constant fold)
    /// ```
    ///
    /// The stream shrinks, so absolute jump targets (patched at emit time) are
    /// rebased through an old→new offset map. Jump targets always point at
    /// instruction starts, which are exactly the mapped offsets.
    pub fn peephole(&mut self) {
        let src = std::mem::take(&mut self.bytecode);
        let n = src.len();
        let mut out: Vec<u8> = Vec::with_capacity(n);
        // old instruction-start offset -> new offset.
        let mut map = vec![usize::MAX; n + 1];

        // Every absolute jump target and function-body start in the ORIGINAL
        // stream. A fusion may consume whole instructions only if no control
        // flow points into the consumed middle: jumps land on instruction
        // starts patched by the compiler (e.g. a ternary's `JMP` can target
        // the SECOND instruction of a `10 - 2` sequence — the arithmetic
        // after the branch). Folding such a sequence would strand the jump
        // at an offset that no longer exists. Jumps to a pattern's FIRST
        // instruction stay valid: the fused opcode reproduces the same net
        // stack effect at that position.
        let mut targets: std::collections::HashSet<usize> = std::collections::HashSet::new();
        {
            let mut t = 0;
            while t < n {
                let op = Opcode::from_u8(src[t]).unwrap_or(Opcode::Nop);
                if matches!(
                    op,
                    Opcode::Jump
                        | Opcode::JumpIfFalse
                        | Opcode::JumpIfTrue
                        | Opcode::JumpIfFalsePop
                        | Opcode::JumpIfTruePop
                        | Opcode::JumpIfNullish
                        | Opcode::TryStart
                ) {
                    targets.insert(rd_u32(&src, t + 1) as usize);
                } else if op == Opcode::NewClosure {
                    let ci = ((src[t + 1] as usize) << 8) | src[t + 2] as usize;
                    if let Some(v) = self.constants.get(ci) {
                        if let Some(s) = v.as_number() {
                            targets.insert(s as usize);
                        }
                    }
                }
                t += op_len(&src, t);
            }
        }

        let mut i = 0;
        // The previously emitted instruction, with its constant index when it
        // was a LoadConst. Used by the chained folds — never sniff raw bytes:
        // an instruction's trailing operand bytes can resemble a LoadConst
        // (e.g. LoadLocalGetPropConst ends in `00 00`), which would make the
        // fold consume a live value it does not own.
        let mut last: Option<(Opcode, Option<u16>)> = None;
        while i < n {
            map[i] = out.len();
            let op = Opcode::from_u8(src[i]).unwrap_or(Opcode::Nop);

            // LoadInt + LoadLocal + AR -> BinIntLocal (`3 * n`).
            if op == Opcode::LoadInt
                && i + 8 <= n
                && src[i + 5] == Opcode::LoadLocal as u8
                && !targets.contains(&(i + 5))
                && !targets.contains(&(i + 7))
            {
                if let Some(ar) = arith_code_of(src[i + 7]) {
                    let imm = rd_i32(&src, i + 1);
                    let slot = src[i + 6];
                    // The trailing Pop is folded into keep=0 only when it is
                    // not itself a jump target — a ternary end-jump can land
                    // on a comma's Pop, which must keep popping.
                    let pop = i + 9 <= n
                        && src[i + 8] == Opcode::Pop as u8
                        && !targets.contains(&(i + 8));
                    out.push(Opcode::BinIntLocal as u8);
                    out.extend_from_slice(&imm.to_be_bytes());
                    out.push(slot);
                    out.push(ar | ((pop as u8) << 7));
                    last = Some((Opcode::BinIntLocal, None));
                    self.count("BinIntLocal");
                    i += 8 + pop as usize;
                    continue;
                }
            }

            // LoadLocal + LoadInt + AR -> BinLocalInt (`(x) * 3`).
            if op == Opcode::LoadLocal
                && i + 8 <= n
                && src[i + 2] == Opcode::LoadInt as u8
                && !targets.contains(&(i + 2))
                && !targets.contains(&(i + 7))
            {
                if let Some(ar) = arith_code_of(src[i + 7]) {
                    let slot = src[i + 1];
                    let imm = rd_i32(&src, i + 3);
                    let pop = i + 9 <= n
                        && src[i + 8] == Opcode::Pop as u8
                        && !targets.contains(&(i + 8));
                    out.push(Opcode::BinLocalInt as u8);
                    out.push(slot);
                    out.extend_from_slice(&imm.to_be_bytes());
                    out.push(ar | ((pop as u8) << 7));
                    last = Some((Opcode::BinLocalInt, None));
                    self.count("BinLocalInt");
                    i += 8 + pop as usize;
                    continue;
                }
            }

            // LoadLocal + LoadLocal + AR -> BinLocalLocal (`(x) + (y)`).
            // Layout: [06 a][06 b][AR] — the AR byte sits at i+4, AFTER the
            // second slot, and slot b is src[i+3] (src[i+2] is the opcode).
            if op == Opcode::LoadLocal
                && i + 5 <= n
                && src[i + 2] == Opcode::LoadLocal as u8
                && !targets.contains(&(i + 2))
                && !targets.contains(&(i + 4))
            {
                if let Some(ar) = arith_code_of(src[i + 4]) {
                    let pop = i + 6 <= n
                        && src[i + 5] == Opcode::Pop as u8
                        && !targets.contains(&(i + 5));
                    out.push(Opcode::BinLocalLocal as u8);
                    out.push(src[i + 1]);
                    out.push(src[i + 3]);
                    out.push(ar | ((pop as u8) << 7));
                    last = Some((Opcode::BinLocalLocal, None));
                    self.count("BinLocalLocal");
                    i += 5 + pop as usize;
                    continue;
                }
            }

            // BinLocalLocal + LoadInt + AR -> BinLocalLocalInt (`(i + j) % 7`).
            if op == Opcode::BinLocalLocal
                && i + 10 <= n
                && src[i + 4] == Opcode::LoadInt as u8
                && !targets.contains(&(i + 4))
                && !targets.contains(&(i + 9))
            {
                if let Some(ar2) = arith_code_of(src[i + 9]) {
                    let imm = rd_i32(&src, i + 5);
                    let pop = i + 11 <= n
                        && src[i + 10] == Opcode::Pop as u8
                        && !targets.contains(&(i + 10));
                    out.push(Opcode::BinLocalLocalInt as u8);
                    out.push(src[i + 1]);
                    out.push(src[i + 2]);
                    out.push(src[i + 3]);
                    out.extend_from_slice(&imm.to_be_bytes());
                    out.push(ar2 | ((pop as u8) << 7));
                    last = Some((Opcode::BinLocalLocalInt, None));
                    self.count("BinLocalLocalInt");
                    i += 10 + pop as usize;
                    continue;
                }
            }

            // Constant folding. Arithmetic on two compile-time constants is
            // pure and cannot throw, so folding is exactly equivalent. Shapes:
            // LoadInt+LoadInt+AR, plus chained folds (`2 * 3 + 1` collapses
            // fully) — a preceding LoadConst (tracked in `last`, never sniffed
            // from raw bytes) followed by LoadInt+AR or LoadConst+AR.
            if op == Opcode::LoadInt
                && i + 11 <= n
                && src[i + 5] == Opcode::LoadInt as u8
                && !targets.contains(&(i + 5))
                && !targets.contains(&(i + 10))
            {
                if let Some(ar) = arith_code_of(src[i + 10]) {
                    let a = rd_i32(&src, i + 1) as i64;
                    let b = rd_i32(&src, i + 6) as i64;
                    let result = fold_arith(Value::int(a), Value::int(b), ar);
                    let ci = self.add_constant(result);
                    out.push(Opcode::LoadConst as u8);
                    out.extend_from_slice(&ci.to_be_bytes());
                    last = Some((Opcode::LoadConst, Some(ci)));
                    self.count("fold_int");
                    i += 11;
                    continue;
                }
            }
            // [LoadConst][LoadInt][AR] -> fold into a single LoadConst. The
            // preceding LoadConst and the LoadInt are the AR's two operands
            // (the top of the stack), so folding them preserves depth. The
            // consumed LoadInt/AR must not be jump targets (the emitted tail
            // LoadConst may be — the fold reproduces its value at that spot).
            if op == Opcode::LoadInt
                && i + 6 <= n
                && !targets.contains(&i)
                && !targets.contains(&(i + 5))
            {
                if let Some(ar) = arith_code_of(src[i + 5]) {
                    if let Some((Opcode::LoadConst, Some(ci))) = last {
                        let ci = ci as usize;
                        if let Some(l) = self.constants.get(ci) {
                            let imm = rd_i32(&src, i + 1) as i64;
                            let result = fold_arith(l.clone(), Value::int(imm), ar);
                            let nci = self.add_constant(result);
                            out.truncate(out.len() - 3);
                            out.push(Opcode::LoadConst as u8);
                            out.extend_from_slice(&nci.to_be_bytes());
                            last = Some((Opcode::LoadConst, Some(nci)));
                            self.count("fold_const_int");
                            i += 6;
                            continue;
                        }
                    }
                }
            }
            // [LoadConst][LoadConst][AR] -> fold into a single LoadConst.
            // Layout: [00 ci1 2][00 ci2 2][AR] — ci2 is at i+4..i+5, the AR
            // byte at i+6. The AR consumes the two adjacent loads (the top of
            // the stack), so only they may be folded — never an earlier
            // emitted constant, which stays live on the stack below them
            // (e.g. a template literal's `"" + ("" + "in ")` must keep its
            // outer empty prefix).
            if op == Opcode::LoadConst
                && i + 7 <= n
                && src[i + 3] == Opcode::LoadConst as u8
                && !targets.contains(&(i + 3))
                && !targets.contains(&(i + 6))
            {
                if let Some(ar) = arith_code_of(src[i + 6]) {
                    let ci1 = ((src[i + 1] as usize) << 8) | src[i + 2] as usize;
                    let ci2 = ((src[i + 4] as usize) << 8) | src[i + 5] as usize;
                    if let (Some(l), Some(r)) = (self.constants.get(ci1), self.constants.get(ci2)) {
                        let result = fold_arith(l.clone(), r.clone(), ar);
                        let nci = self.add_constant(result);
                        out.push(Opcode::LoadConst as u8);
                        out.extend_from_slice(&nci.to_be_bytes());
                        last = Some((Opcode::LoadConst, Some(nci)));
                        self.count("fold_const_const");
                        i += 7;
                        continue;
                    }
                }
            }

            // Condition fusions: a trailing JumpIfFalsePop whose condition
            // was produced by a fused compare/arith chain collapses into one
            // opcode. The compare value is consumed by the pop either way, so
            // fusing is exact. Layouts carry the (rebased) jump target.
            //
            // [CmpLocalInt][JIF_POP] -> CmpLocalIntJumpIfFalsePop. The hot
            // loop-condition shape `while (n !== 1)` / `if (j >= 0)`.
            if op == Opcode::CmpLocalInt
                && i + 12 <= n
                && !targets.contains(&(i + 7))
                && src[i + 7] == Opcode::JumpIfFalsePop as u8
            {
                let mut v = vec![Opcode::CmpLocalIntJumpIfFalsePop as u8];
                v.push(src[i + 1]);
                v.extend_from_slice(&src[i + 2..i + 6]);
                v.push(src[i + 6]);
                v.extend_from_slice(&src[i + 8..i + 12]);
                out.extend_from_slice(&v);
                last = Some((Opcode::CmpLocalIntJumpIfFalsePop, None));
                self.count("CmpLocalIntJifPop");
                i += 12;
                continue;
            }
            // [CmpLocalLocal][JIF_POP] -> CmpLocalLocalJumpIfFalsePop. The
            // for-loop condition shape `for (j = lo; j < hi; …)`. CmpLocalLocal
            // is op + a + b + cmp (4 bytes): copy a/b/cmp, then the target.
            if op == Opcode::CmpLocalLocal
                && i + 9 <= n
                && !targets.contains(&(i + 4))
                && src[i + 4] == Opcode::JumpIfFalsePop as u8
            {
                let mut v = vec![Opcode::CmpLocalLocalJumpIfFalsePop as u8];
                v.push(src[i + 1]);
                v.push(src[i + 2]);
                v.push(src[i + 3]);
                v.extend_from_slice(&src[i + 5..i + 9]);
                out.extend_from_slice(&v);
                last = Some((Opcode::CmpLocalLocalJumpIfFalsePop, None));
                self.count("CmpLocalLocalJifPop");
                i += 9;
                continue;
            }
            // [LoadLocalLocalGetIndex][LoadLocal][Cmp][JIF_POP] ->
            // LoadIndexCmpLocalJumpIfFalsePop. The `a[j] > key` condition
            // shape: read arr[j], compare with a local, branch.
            if op == Opcode::LoadLocalLocalGetIndex
                && i + 11 <= n
                && !targets.contains(&(i + 3))
                && !targets.contains(&(i + 5))
                && !targets.contains(&(i + 6))
                && src[i + 3] == Opcode::LoadLocal as u8
                && is_cmp_op(Opcode::from_u8(src[i + 5]).unwrap_or(Opcode::Nop))
                && src[i + 6] == Opcode::JumpIfFalsePop as u8
            {
                let mut v = vec![Opcode::LoadIndexCmpLocalJumpIfFalsePop as u8];
                v.push(src[i + 1]);
                v.push(src[i + 2]);
                v.push(src[i + 4]);
                v.push(src[i + 5]);
                v.extend_from_slice(&src[i + 7..i + 11]);
                out.extend_from_slice(&v);
                last = Some((Opcode::LoadIndexCmpLocalJumpIfFalsePop, None));
                self.count("LoadIndexCmpLocalJifPop");
                i += 11;
                continue;
            }
            // [BinLocalInt][LoadInt][Cmp][JIF_POP] ->
            // ArithLocalIntCmpJumpIfFalsePop. The `(n % 2) === 0` shape:
            // arith a local against imm1, compare the result with imm2,
            // branch.
            if op == Opcode::BinLocalInt
                && i + 18 <= n
                && !targets.contains(&(i + 7))
                && !targets.contains(&(i + 12))
                && src[i + 7] == Opcode::LoadInt as u8
                && is_cmp_op(Opcode::from_u8(src[i + 12]).unwrap_or(Opcode::Nop))
                && src[i + 13] == Opcode::JumpIfFalsePop as u8
            {
                let mut v = vec![Opcode::ArithLocalIntCmpJumpIfFalsePop as u8];
                v.push(src[i + 1]);
                v.extend_from_slice(&src[i + 2..i + 6]);
                v.push(src[i + 6]);
                v.extend_from_slice(&src[i + 8..i + 12]);
                v.push(src[i + 12]);
                v.extend_from_slice(&src[i + 14..i + 18]);
                out.extend_from_slice(&v);
                last = Some((Opcode::ArithLocalIntCmpJumpIfFalsePop, None));
                self.count("ArithLocalIntCmpJifPop");
                i += 18;
                continue;
            }
            // Comparison chains: `a < b && b < c` (and `||`, any mix of
            // local/int operands) collapse the two-comparison short-circuit
            // into one dispatch.
            //
            // Value context: [Cmp][JIF|JIT -> end][Pop][Cmp] where the jump
            // targets exactly the end of the second Cmp — the `&&` value
            // semantics: a falsy `l` leaves `l`'s bool, the fall-through
            // evaluates `r`. The fused opcode pushes the first bool when the
            // short-circuit fires, else the second comparison's bool.
            //
            // Condition context: [Cmp][JIF_POP|JIT_POP -> E][Cmp][same jump
            // -> E] with both jumps equal (the compiler emits both `&&`
            // operands into the same exit). The fused opcode pushes the
            // effective bool and the retained second JumpPop does the
            // branching — its pop consumes exactly the one pushed value, so
            // the first JumpPop's pop is absorbed.
            if let Some(l1) = cmp_len(op) {
                let j1_at = i + l1;
                let j1 = Opcode::from_u8(*src.get(j1_at).unwrap_or(&0)).unwrap_or(Opcode::Nop);
                let j1_peek = matches!(j1, Opcode::JumpIfFalse | Opcode::JumpIfTrue);
                let j1_pop = matches!(j1, Opcode::JumpIfFalsePop | Opcode::JumpIfTruePop);
                if (j1_peek || j1_pop) && !targets.contains(&j1_at) {
                    let e = rd_u32(&src, j1_at + 1) as usize;
                    if j1_peek {
                        // Value context: [Cmp][JIF][Pop][Cmp], JIF -> after
                        // the second Cmp (the &&/|| chain end).
                        let pop_at = j1_at + 5;
                        let c2_at = pop_at + 1;
                        if src.get(pop_at) == Some(&(Opcode::Pop as u8))
                            && !targets.contains(&pop_at)
                            && !targets.contains(&c2_at)
                        {
                            if let Some(l2) =
                                cmp_len(Opcode::from_u8(*src.get(c2_at).unwrap_or(&0)).unwrap_or(Opcode::Nop))
                            {
                                if e == c2_at + l2 {
                                    let or = j1 == Opcode::JumpIfTrue;
                                    let (fop, fused) = fuse_cmp_chain(&src, i, l1, c2_at, l2, or);
                                    self.count(cmp_chain_key(fop, or, false));
                                    out.extend_from_slice(&fused);
                                    last = Some((fop, None));
                                    i = c2_at + l2;
                                    continue;
                                }
                            }
                        }
                    } else {
                        // Condition context: [Cmp][JIF_POP -> E][Cmp][JIF_POP
                        // -> E], both jumps the same kind and target. The
                        // retained second JumpPop keeps its (rebased) target.
                        let c2_at = j1_at + 5;
                        if !targets.contains(&c2_at) {
                            if let Some(l2) = cmp_len(
                                Opcode::from_u8(*src.get(c2_at).unwrap_or(&0)).unwrap_or(Opcode::Nop),
                            ) {
                                let j2_at = c2_at + l2;
                                let j2 = Opcode::from_u8(*src.get(j2_at).unwrap_or(&0))
                                    .unwrap_or(Opcode::Nop);
                                if j2 == j1
                                    && !targets.contains(&j2_at)
                                    && rd_u32(&src, j2_at + 1) as usize == e
                                {
                                    let or = j1 == Opcode::JumpIfTruePop;
                                    let (fop, fused) = fuse_cmp_chain(&src, i, l1, c2_at, l2, or);
                                    self.count(cmp_chain_key(fop, or, true));
                                    out.extend_from_slice(&fused);
                                    out.extend_from_slice(&src[j2_at..j2_at + 5]);
                                    last = Some((fop, None));
                                    i = j2_at + 5;
                                    continue;
                                }
                            }
                        }
                    }
                }
            }

            // Default: copy the instruction verbatim.
            let len = op_len(&src, i);
            out.extend_from_slice(&src[i..i + len]);
            last = Some((
                op,
                if op == Opcode::LoadConst {
                    Some(((src[i + 1] as u16) << 8) | src[i + 2] as u16)
                } else {
                    None
                },
            ));
            i += len;
        }
        map[n] = out.len();

        // Rebase absolute jump targets (patched at emit time) through the map.
        let mut j = 0;
        while j < out.len() {
            let op = Opcode::from_u8(out[j]).unwrap_or(Opcode::Nop);
            if let Some(t_off) = jump_target_offset(op) {
                let old_t = rd_u32(&out, j + t_off) as usize;
                if old_t < map.len() && map[old_t] != usize::MAX {
                    let nb = (map[old_t] as u32).to_be_bytes();
                    out[j + t_off] = nb[0];
                    out[j + t_off + 1] = nb[1];
                    out[j + t_off + 2] = nb[2];
                    out[j + t_off + 3] = nb[3];
                }
            }
            j += op_len(&out, j);
        }

        // Function-body pointers are absolute bytecode offsets stored as
        // number constants (the compiler's `add_constant(Value::number(start))`
        // for NewClosure); the compaction shifted every body, so remap exactly
        // the constants referenced by NewClosure instructions — never touch a
        // literal number that merely resembles an offset.
        let mut j = 0;
        while j < out.len() {
            let op = Opcode::from_u8(out[j]).unwrap_or(Opcode::Nop);
            if op == Opcode::NewClosure {
                let ci = ((out[j + 1] as usize) << 8) | out[j + 2] as usize;
                if let Some(v) = self.constants.get_mut(ci) {
                    if let Some(n) = v.as_number() {
                        let old = n as usize;
                        if old < map.len() && map[old] != usize::MAX {
                            *v = Value::number(map[old] as f64);
                        }
                    }
                }
            }
            j += op_len(&out, j);
        }

        self.bytecode = out;

        if std::env::var("ALLOY_PEEPHOLE_VALIDATE").is_ok() {
            let bc = &self.bytecode;
            let mut starts = vec![false; bc.len() + 1];
            let mut k = 0;
            while k < bc.len() {
                starts[k] = true;
                k += op_len(bc, k);
            }
            starts[bc.len()] = true;
            let mut k = 0;
            while k < bc.len() {
                let op = Opcode::from_u8(bc[k]).unwrap_or(Opcode::Nop);
                if matches!(
                    op,
                    Opcode::Jump
                        | Opcode::JumpIfFalse
                        | Opcode::JumpIfTrue
                        | Opcode::JumpIfFalsePop
                        | Opcode::JumpIfTruePop
                        | Opcode::JumpIfNullish
                        | Opcode::TryStart
                ) {
                    let t = rd_u32(bc, k + 1) as usize;
                    if !starts.get(t).copied().unwrap_or(false) {
                        eprintln!("VALIDATE: {op:?} @{k:04x} -> non-start {t:04x}");
                    }
                }
                k += op_len(bc, k);
            }
        }

        if std::env::var("ALLOY_PEEPHOLE_COUNTS").is_ok() {
            let mut v: Vec<_> = self.peephole_counts.iter().collect();
            v.sort_by_key(|(k, _)| *k);
            eprintln!(
                "PEEPHOLE_COUNTS {}",
                v.iter()
                    .map(|(k, c)| format!("{k}={c}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
    }

    /// Record one fusion firing (measurement for ALLOY_PEEPHOLE_COUNTS).
    fn count(&mut self, name: &'static str) {
        *self.peephole_counts.entry(name).or_insert(0) += 1;
    }

    pub fn read_u8(&self, offset: usize) -> u8 {
        self.bytecode[offset]
    }

    pub fn read_u16(&self, offset: usize) -> u16 {
        ((self.bytecode[offset] as u16) << 8) | (self.bytecode[offset + 1] as u16)
    }

    pub fn read_u32(&self, offset: usize) -> u32 {
        ((self.bytecode[offset] as u32) << 24)
            | ((self.bytecode[offset + 1] as u32) << 16)
            | ((self.bytecode[offset + 2] as u32) << 8)
            | (self.bytecode[offset + 3] as u32)
    }

    /// Serialize the program into the `.ax` bytecode format.
    pub fn to_bytes(&self) -> Result<Vec<u8>, SerError> {
        let mut out = Vec::with_capacity(4096);
        out.extend_from_slice(&AX_MAGIC);
        out.extend_from_slice(&AX_VERSION.to_be_bytes());
        write_str_list(&mut out, &self.globals)?;
        out.extend_from_slice(&(self.bytecode.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.bytecode);
        out.extend_from_slice(&(self.constants.len() as u32).to_be_bytes());
        for c in &self.constants {
            write_value(&mut out, c)?;
        }
        out.push(self.is_module as u8);
        let export_names: Vec<String> =
            self.exports.iter().map(|(n, _)| n.clone()).collect();
        let binding_names: Vec<String> =
            self.exports.iter().map(|(_, b)| b.clone()).collect();
        write_str_list(&mut out, &export_names)?;
        write_str_list(&mut out, &binding_names)?;
        Ok(out)
    }

    /// Deserialize a program from the `.ax` bytecode format.
    pub fn from_bytes(data: &[u8]) -> Result<Self, SerError> {
        let mut r = Reader { data, pos: 0 };
        if r.bytes(8)? != AX_MAGIC {
            return Err(SerError::InvalidFormat("bad magic".to_string()));
        }
        let version = r.u32()?;
        if version != AX_VERSION {
            return Err(SerError::InvalidFormat(format!(
                "unsupported version {}",
                version
            )));
        }
        let globals = r.str_list()?;
        let blen = r.u32()? as usize;
        let bytecode = r.bytes(blen)?.to_vec();
        let ccount = r.u32()? as usize;
        // Deserialized constants allocate into this program's heap.
        let mut program = Self::new();
        let heap_ptr: *mut ArenaHeap = &mut program.heap;
        let _g = HeapGuard::set(heap_ptr);
        for _ in 0..ccount {
            program.constants.push(r.value()?);
        }
        program.bytecode = bytecode;
        program.globals = globals;
        // Deserialization allocates constants one at a time; promote them to
        // old and reset the young arena so the program heap is compact and
        // future allocations (e.g. a later `deep_clone`) can't collide with
        // the constants.
        program.compact_constants();
        program.is_module = r.u8()? != 0;
        let export_names = r.str_list()?;
        let binding_names = r.str_list()?;
        if export_names.len() != binding_names.len() {
            return Err(SerError::InvalidFormat("export lists differ in length".to_string()));
        }
        program.exports = export_names
            .into_iter()
            .zip(binding_names)
            .collect::<Vec<_>>();
        Ok(program)
    }

    pub fn disassemble(&self) {
        let mut offset = 0;
        while offset < self.bytecode.len() {
            let op_byte = self.bytecode[offset];
            let op = Opcode::from_u8(op_byte).unwrap_or(Opcode::Nop);
            print!("{:04x}  {: <20}", offset, op.name());

            match op {
                Opcode::LoadConst => {
                    let idx = self.read_u16(offset + 1);
                    println!("  {}", self.constants[idx as usize]);
                    offset += 3;
                }
                Opcode::LoadInt => {
                    let val = self.read_u32(offset + 1) as i32;
                    println!("  {}", val);
                    offset += 5;
                }
                Opcode::LoadLocal
                | Opcode::StoreLocal
                | Opcode::CaptureLocal
                | Opcode::CaptureUpvalue
                | Opcode::LoadUpvalue
                | Opcode::StoreUpvalue
                | Opcode::LoadCell
                | Opcode::StoreCell => {
                    let slot = self.bytecode[offset + 1];
                    println!("  r{}", slot);
                    offset += 2;
                }
                Opcode::LoadGlobal
                | Opcode::StoreGlobal
                | Opcode::GetProperty
                | Opcode::MakeArray => {
                    let idx = self.read_u16(offset + 1);
                    println!("  {}", idx);
                    offset += 3;
                }
                Opcode::MakeObject => {
                    let count = self.read_u16(offset + 1);
                    let mask = self.read_u16(offset + 3);
                    println!("  fields={} spread={:04x}", count, mask);
                    offset += 5;
                }
                Opcode::Jump
                | Opcode::JumpIfFalse
                | Opcode::JumpIfFalsePop
                | Opcode::JumpIfTrue
                | Opcode::JumpIfTruePop
                | Opcode::JumpIfNullish
                | Opcode::TryStart => {
                    let target = self.read_u32(offset + 1);
                    println!("  {:08x}", target);
                    offset += 5;
                }
                Opcode::Throw => {
                    println!();
                    offset += 1;
                }
                Opcode::TryEnd => {
                    println!();
                    offset += 1;
                }
                Opcode::Call | Opcode::CallKeep0 | Opcode::New | Opcode::CallMethod | Opcode::CallMethodKeep0 => {
                    let argc = self.bytecode[offset + 1];
                    println!("  argc={}", argc);
                    offset += 2;
                }
                Opcode::CallSpread
                | Opcode::CallSpreadKeep0
                | Opcode::CallMethodSpread
                | Opcode::CallMethodSpreadKeep0 => {
                    let argc = self.bytecode[offset + 1];
                    let mask = self.read_u16(offset + 2);
                    println!("  argc={} spread={:04x}", argc, mask);
                    offset += 4;
                }
                Opcode::InstanceOf | Opcode::In => {
                    println!();
                    offset += 1;
                }
                Opcode::GetProto | Opcode::SetProto | Opcode::LoadThis => {
                    println!();
                    offset += 1;
                }
                Opcode::CmpLocalIntJumpIfFalsePop => {
                    let slot = self.bytecode[offset + 1];
                    let imm = self.read_u32(offset + 2) as i32;
                    let cmp = self.bytecode[offset + 6];
                    let target = self.read_u32(offset + 7);
                    println!("  r{} cmp{} {} -> {:08x}", slot, cmp, imm, target);
                    offset += 11;
                }
                Opcode::CmpLocalLocalJumpIfFalsePop => {
                    let a = self.bytecode[offset + 1];
                    let b = self.bytecode[offset + 2];
                    let cmp = self.bytecode[offset + 3];
                    let target = self.read_u32(offset + 4);
                    println!("  r{} cmp{} r{} -> {:08x}", a, cmp, b, target);
                    offset += 8;
                }
                Opcode::LoadIndexCmpLocalJumpIfFalsePop => {
                    let objs = self.bytecode[offset + 1];
                    let idxs = self.bytecode[offset + 2];
                    let ks = self.bytecode[offset + 3];
                    let cmp = self.bytecode[offset + 4];
                    let target = self.read_u32(offset + 5);
                    println!("  r{}[r{}] cmp{} r{} -> {:08x}", objs, idxs, cmp, ks, target);
                    offset += 9;
                }
                Opcode::ArithLocalIntCmpJumpIfFalsePop => {
                    let slot = self.bytecode[offset + 1];
                    let imm1 = self.read_u32(offset + 2) as i32;
                    let ar = self.bytecode[offset + 6];
                    let imm2 = self.read_u32(offset + 7) as i32;
                    let cmp = self.bytecode[offset + 11];
                    let target = self.read_u32(offset + 12);
                    println!("  (r{} ar{} {}) cmp{} {} -> {:08x}", slot, ar, imm1, cmp, imm2, target);
                    offset += 16;
                }
                Opcode::SetIndexLocalLocal => {
                    println!(
                        "  r{}[r{}] = r{}",
                        self.bytecode[offset + 1],
                        self.bytecode[offset + 2],
                        self.bytecode[offset + 3]
                    );
                    offset += 4;
                }
                Opcode::SetIndexLocalGetLocal => {
                    println!(
                        "  r{}[r{}] = r{}[r{}]",
                        self.bytecode[offset + 1],
                        self.bytecode[offset + 2],
                        self.bytecode[offset + 3],
                        self.bytecode[offset + 4]
                    );
                    offset += 5;
                }
                Opcode::SetIndexLocalPlusIntLocalGetLocal => {
                    println!(
                        "  r{}[r{} ar{} {}] = r{}[r{}]",
                        self.bytecode[offset + 1],
                        self.bytecode[offset + 2],
                        self.bytecode[offset + 3],
                        self.read_u32(offset + 4) as i32,
                        self.bytecode[offset + 8],
                        self.bytecode[offset + 9]
                    );
                    offset += 10;
                }
                Opcode::MakeArraySpread => {
                    let n = self.read_u16(offset + 1);
                    let mask = self.read_u16(offset + 3);
                    println!("  n={} spread={:04x}", n, mask);
                    offset += 5;
                }
                Opcode::MakeRestArray => {
                    let slot = self.bytecode[offset + 1];
                    let fixed = self.bytecode[offset + 2];
                    println!("  r{} fixed={}", slot, fixed);
                    offset += 3;
                }
                Opcode::NewSpread => {
                    let argc = self.bytecode[offset + 1];
                    let mask = self.read_u16(offset + 2);
                    println!("  argc={} spread={:04x}", argc, mask);
                    offset += 4;
                }
                Opcode::SetAccessor => {
                    let kind = self.bytecode[offset + 1];
                    println!("  {}", if kind == 1 { "getter" } else { "setter" });
                    offset += 2;
                }
                Opcode::ArraySlice => {
                    let start = self.bytecode[offset + 1];
                    println!("  start={}", start);
                    offset += 2;
                }
                Opcode::CmpLocalInt => {
                    let slot = self.bytecode[offset + 1];
                    let imm = self.read_u32(offset + 2) as i32;
                    let cmp = self.bytecode[offset + 6];
                    println!("  r{} vs {} cmp={}", slot, imm, cmp);
                    offset += 7;
                }
                Opcode::BinLocalInt => {
                    let slot = self.bytecode[offset + 1];
                    let imm = self.read_u32(offset + 2) as i32;
                    let ar = self.bytecode[offset + 6];
                    println!("  r{} op {} ar={}", slot, imm, ar);
                    offset += 7;
                }
                Opcode::BinLocalLocal => {
                    let a = self.bytecode[offset + 1];
                    let b = self.bytecode[offset + 2];
                    let ar = self.bytecode[offset + 3];
                    println!("  r{} {} r{} ar={} keep={}", a, ar & 0x7F, b, ar & 0x7F, ar & 0x80 == 0);
                    offset += 4;
                }
                Opcode::BinIntLocal => {
                    let imm = self.read_u32(offset + 1) as i32;
                    let slot = self.bytecode[offset + 5];
                    let ar = self.bytecode[offset + 6];
                    println!("  {} ar r{} ar={} keep={}", imm, slot, ar & 0x7F, ar & 0x80 == 0);
                    offset += 7;
                }
                Opcode::BinLocalLocalInt => {
                    let a = self.bytecode[offset + 1];
                    let b = self.bytecode[offset + 2];
                    let ar1 = self.bytecode[offset + 3];
                    let imm = self.read_u32(offset + 4) as i32;
                    let ar2 = self.bytecode[offset + 8];
                    println!(
                        "  (r{} {} r{}) {} {} keep={}",
                        a,
                        ar1 & 0x7F,
                        b,
                        ar2 & 0x7F,
                        imm,
                        ar2 & 0x80 == 0
                    );
                    offset += 9;
                }
                Opcode::CmpLocalLocal => {
                    let a = self.bytecode[offset + 1];
                    let b = self.bytecode[offset + 2];
                    let cmp = self.bytecode[offset + 3];
                    println!("  r{} cmp r{} cmp={}", a, b, cmp);
                    offset += 4;
                }
                Opcode::CmpAndLocalLocal => {
                    let a = self.bytecode[offset + 1];
                    let b = self.bytecode[offset + 2];
                    let cmp1 = self.bytecode[offset + 3];
                    let c = self.bytecode[offset + 4];
                    let d = self.bytecode[offset + 5];
                    let cmp2 = self.bytecode[offset + 6];
                    println!(
                        "  r{} cmp{} r{} {} r{} cmp{} r{}",
                        a,
                        cmp1,
                        b,
                        if cmp2 & 0x80 != 0 { "||" } else { "&&" },
                        c,
                        cmp2 & 0x7F,
                        d
                    );
                    offset += 7;
                }
                Opcode::CmpAndLocalInt => {
                    let a = self.bytecode[offset + 1];
                    let b = self.bytecode[offset + 2];
                    let cmp1 = self.bytecode[offset + 3];
                    let c = self.bytecode[offset + 4];
                    let imm = self.read_u32(offset + 5) as i32;
                    let cmp2 = self.bytecode[offset + 9];
                    println!(
                        "  r{} cmp{} r{} {} r{} cmp{} {}",
                        a,
                        cmp1,
                        b,
                        if cmp2 & 0x80 != 0 { "||" } else { "&&" },
                        c,
                        cmp2 & 0x7F,
                        imm
                    );
                    offset += 10;
                }
                Opcode::CmpAndIntLocal => {
                    let a = self.bytecode[offset + 1];
                    let imm = self.read_u32(offset + 2) as i32;
                    let cmp1 = self.bytecode[offset + 6];
                    let c = self.bytecode[offset + 7];
                    let d = self.bytecode[offset + 8];
                    let cmp2 = self.bytecode[offset + 9];
                    println!(
                        "  r{} cmp{} {} {} r{} cmp{} r{}",
                        a,
                        cmp1,
                        imm,
                        if cmp2 & 0x80 != 0 { "||" } else { "&&" },
                        c,
                        cmp2 & 0x7F,
                        d
                    );
                    offset += 10;
                }
                Opcode::CmpAndIntInt => {
                    let a = self.bytecode[offset + 1];
                    let imm1 = self.read_u32(offset + 2) as i32;
                    let cmp1 = self.bytecode[offset + 6];
                    let b = self.bytecode[offset + 7];
                    let imm2 = self.read_u32(offset + 8) as i32;
                    let cmp2 = self.bytecode[offset + 12];
                    println!(
                        "  r{} cmp{} {} {} r{} cmp{} {}",
                        a,
                        cmp1,
                        imm1,
                        if cmp2 & 0x80 != 0 { "||" } else { "&&" },
                        b,
                        cmp2 & 0x7F,
                        imm2
                    );
                    offset += 13;
                }
                Opcode::ArithChain => {
                    let count = self.bytecode[offset + 1] as usize;
                    let term = self.bytecode[offset + 2];
                    let mut p = offset + 3;
                    print!("  n={} term={} [", count, term);
                    for _ in 0..count {
                        let h = self.bytecode[p];
                        let enc_ar = h & 0x1F; // arith_code + 1 (0 = init)
                        let kind = h >> 5;
                        let u = self.read_u32(p + 1);
                        let imm = if u & 0x8000_0000 != 0 { u as i32 as i64 } else { u as i64 };
                        match kind {
                            0 => print!(
                                "{}r{}",
                                if enc_ar == 0 { "" } else { ar_name(enc_ar - 1) },
                                imm
                            ),
                            1 => print!(
                                "{}#{}",
                                if enc_ar == 0 { "" } else { ar_name(enc_ar - 1) },
                                imm
                            ),
                            2 => print!("save "),
                            _ => print!("combine{} ", ar_name(enc_ar - 1)),
                        }
                        p += 5;
                    }
                    println!("]");
                    offset = p;
                }
                Opcode::Arith2StoreLocalConst => {
                    let slot = self.bytecode[offset + 1];
                    let ar = self.bytecode[offset + 2] & 0x7F;
                    let keep = self.bytecode[offset + 2] & 0x80 != 0;
                    let imm = self.read_u32(offset + 3) as i32;
                    println!("  r{} ar={} imm={} keep={}", slot, ar, imm, keep);
                    offset += 7;
                }
                Opcode::Arith3StoreLocalConstConst => {
                    let slot = self.bytecode[offset + 1];
                    let ar1 = self.bytecode[offset + 2];
                    let imm1 = self.read_u32(offset + 3) as i32;
                    let ar2 = self.bytecode[offset + 7] & 0x7F;
                    let keep = self.bytecode[offset + 7] & 0x80 != 0;
                    let imm2 = self.read_u32(offset + 8) as i32;
                    println!(
                        "  r{} ar={} imm={} ar={} imm={} keep={}",
                        slot, ar1, imm1, ar2, imm2, keep
                    );
                    offset += 12;
                }
                Opcode::Arith3StoreConstLocalConst => {
                    let imm1 = self.read_u32(offset + 1) as i32;
                    let ar1 = self.bytecode[offset + 5];
                    let slot = self.bytecode[offset + 6];
                    let ar2 = self.bytecode[offset + 7] & 0x7F;
                    let keep = self.bytecode[offset + 7] & 0x80 != 0;
                    let imm2 = self.read_u32(offset + 8) as i32;
                    println!(
                        "  imm={} ar={} r{} ar={} imm={} keep={}",
                        imm1, ar1, slot, ar2, imm2, keep
                    );
                    offset += 12;
                }
                Opcode::ArithStoreLocal => {
                    let slot = self.bytecode[offset + 1];
                    let ar = self.bytecode[offset + 2];
                    let keep = self.bytecode[offset + 3];
                    println!("  r{} ar={} keep={}", slot, ar, keep);
                    offset += 4;
                }
                Opcode::IncLocal => {
                    let slot = self.bytecode[offset + 1];
                    let flags = self.bytecode[offset + 2];
                    let delta = self.bytecode[offset + 3] as i8;
                    println!("  r{} prefix={} keep={} delta={}", slot, flags & 1, (flags >> 1) & 1, delta);
                    offset += 4;
                }
                Opcode::AppendStringConst => {
                    let slot = self.bytecode[offset + 1];
                    let ci = self.read_u16(offset + 2);
                    let keep = self.bytecode[offset + 4];
                    println!("  r{} const#{} keep={}", slot, ci, keep);
                    offset += 5;
                }
                Opcode::AppendStringLocal => {
                    let slot = self.bytecode[offset + 1];
                    let src = self.bytecode[offset + 2];
                    let keep = self.bytecode[offset + 3];
                    println!("  r{} + r{} keep={}", slot, src, keep);
                    offset += 4;
                }
                Opcode::AppendStringPop => {
                    let slot = self.bytecode[offset + 1];
                    let keep = self.bytecode[offset + 2];
                    println!("  r{} keep={}", slot, keep);
                    offset += 3;
                }
                Opcode::ArithStoreUpvalue => {
                    let up = self.bytecode[offset + 1];
                    let ar = self.bytecode[offset + 2];
                    let keep = self.bytecode[offset + 3];
                    println!("  u{} ar={} keep={}", up, ar, keep);
                    offset += 4;
                }
                Opcode::CompoundPropConst => {
                    let ar = self.bytecode[offset + 1];
                    let pi = self.read_u16(offset + 2);
                    let imm = self.read_u32(offset + 4) as i32;
                    println!("  prop={} ar={} keep={} rhs={}", pi, ar & 15, (ar >> 4) & 1, imm);
                    offset += 8;
                }
                Opcode::PeekProperty => {
                    let pi = self.read_u16(offset + 1);
                    println!("  prop={}", pi);
                    offset += 3;
                }
                Opcode::ArithWriteProp => {
                    let ar = self.bytecode[offset + 1];
                    let pi = self.read_u16(offset + 2);
                    println!("  prop={} ar={} keep={}", pi, ar & 15, (ar >> 4) & 1);
                    offset += 4;
                }
                Opcode::CompoundIndexConst => {
                    let ar = self.bytecode[offset + 1];
                    let imm = self.read_u32(offset + 2) as i32;
                    println!("  ar={} keep={} rhs={}", ar & 15, (ar >> 4) & 1, imm);
                    offset += 6;
                }
                Opcode::PeekIndex => {
                    println!();
                    offset += 1;
                }
                Opcode::ArithWriteIndex => {
                    let ar = self.bytecode[offset + 1];
                    println!("  ar={} keep={}", ar & 15, (ar >> 4) & 1);
                    offset += 2;
                }
                Opcode::IncPropConst => {
                    let flags = self.bytecode[offset + 1];
                    let pi = self.read_u16(offset + 2);
                    println!("  prop={} prefix={} dec={} keep={}", pi, flags & 1, (flags >> 1) & 1, (flags >> 2) & 1);
                    offset += 4;
                }
                Opcode::IncIndexConst => {
                    let flags = self.bytecode[offset + 1];
                    println!("  prefix={} dec={} keep={}", flags & 1, (flags >> 1) & 1, (flags >> 2) & 1);
                    offset += 2;
                }
                Opcode::NewClosure => {
                    let ci = self.read_u16(offset + 1);
                    let n = self.bytecode[offset + 3];
                    let p = self.bytecode[offset + 4];
                    let ua = self.bytecode[offset + 5];
                    println!("  fn={} upvalues={} params={} uses_args={}", ci, n, p, ua);
                    offset += 6;
                }
                Opcode::NewPromise => {
                    let slot = self.bytecode[offset + 1];
                    println!("  r{}", slot);
                    offset += 2;
                }
                Opcode::Await => {
                    println!();
                    offset += 1;
                }
                Opcode::LoadPython => {
                    let idx = self.read_u16(offset + 1);
                    match self.constants.get(idx as usize) {
                        Some(c) => println!("  {}", c),
                        None => println!("  #{}", idx),
                    }
                    offset += 3;
                }
                Opcode::LoadLocalGetPropConst => {
                    println!("  local#{} prop#{}", self.bytecode[offset + 1], self.read_u16(offset + 2));
                    offset += 4;
                }
                Opcode::LoadLocalLocalGetIndex => {
                    println!(
                        "  obj#{} idx#{}",
                        self.bytecode[offset + 1],
                        self.bytecode[offset + 2]
                    );
                    offset += 3;
                }
                Opcode::AllocShared => {
                    let size = self.read_u16(offset + 1);
                    println!("  size={}", size);
                    offset += 3;
                }
                Opcode::ReadShared | Opcode::WriteShared => {
                    let off = self.read_u16(offset + 1);
                    let len = self.read_u16(offset + 3);
                    println!("  off={} len={}", off, len);
                    offset += 5;
                }
                Opcode::Print => {
                    offset += 1;
                }
                Opcode::Halt => {
                    println!();
                    offset += 1;
                }
                _ => {
                    println!();
                    offset += 1;
                }
            }
        }
    }
}

impl Default for Program {
    fn default() -> Self {
        Self::new()
    }
}

const AX_MAGIC: [u8; 8] = *b"ALLOYAX\0";
const AX_VERSION: u32 = 4;

#[derive(Debug)]
pub enum SerError {
    InvalidFormat(String),
    UnsupportedValue(&'static str),
    UnexpectedEof,
}

impl std::fmt::Display for SerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidFormat(s) => write!(f, "invalid .ax format: {}", s),
            Self::UnsupportedValue(s) => write!(f, "constant not serializable: {}", s),
            Self::UnexpectedEof => write!(f, "unexpected end of .ax data"),
        }
    }
}

impl std::error::Error for SerError {}

fn write_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    write_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

fn write_str_list(out: &mut Vec<u8>, list: &[String]) -> Result<(), SerError> {
    write_u32(out, list.len() as u32);
    for s in list {
        if s.len() > u32::MAX as usize {
            return Err(SerError::InvalidFormat("string too long".to_string()));
        }
        write_str(out, s);
    }
    Ok(())
}

fn write_value(out: &mut Vec<u8>, v: &Value) -> Result<(), SerError> {
    if v.is_undefined() {
        out.push(0);
    } else if v.is_null() {
        out.push(1);
    } else if let Some(b) = v.as_bool() {
        out.push(2);
        out.push(b as u8);
    } else if let Some(n) = v.as_number() {
        out.push(3);
        out.extend_from_slice(&n.to_bits().to_be_bytes());
    } else if let Some(i) = v.as_int() {
        out.push(4);
        out.extend_from_slice(&i.to_be_bytes());
    } else if let Some(s) = v.as_str() {
        out.push(5);
        write_str(out, s);
    } else if let Some(id) = v.as_symbol() {
        out.push(6);
        out.extend_from_slice(&id.to_be_bytes());
    } else if let Some(arr) = v.as_array() {
        out.push(7);
        let arr = arr.borrow();
        write_u32(out, arr.len() as u32);
        for e in arr.to_values() {
            write_value(out, &e)?;
        }
    } else if let Some(m) = v.as_object() {
        out.push(8);
        let m = m.borrow();
        write_u32(out, m.len() as u32);
        for (k, val) in m.iter_sorted() {
            write_str(out, k);
            write_value(out, val)?;
        }
    } else if v.is_function() {
        return Err(SerError::UnsupportedValue("function"));
    } else if v.is_native() {
        return Err(SerError::UnsupportedValue("native function"));
    } else if v.is_pointer() {
        return Err(SerError::UnsupportedValue("pointer"));
    } else if v.is_buffer() {
        return Err(SerError::UnsupportedValue("shared buffer"));
    } else if v.is_cell() {
        return Err(SerError::UnsupportedValue("cell"));
    } else if v.is_promise() {
        return Err(SerError::UnsupportedValue("promise"));
    }
    Ok(())
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], SerError> {
        if self.pos + n > self.data.len() {
            return Err(SerError::UnexpectedEof);
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u32(&mut self) -> Result<u32, SerError> {
        let b = self.bytes(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u8(&mut self) -> Result<u8, SerError> {
        Ok(self.bytes(1)?[0])
    }

    fn str(&mut self) -> Result<String, SerError> {
        let len = self.u32()? as usize;
        let b = self.bytes(len)?;
        String::from_utf8(b.to_vec()).map_err(|_| SerError::InvalidFormat("bad utf8".to_string()))
    }

    fn str_list(&mut self) -> Result<Vec<String>, SerError> {
        let count = self.u32()? as usize;
        let mut list = Vec::with_capacity(count);
        for _ in 0..count {
            list.push(self.str()?);
        }
        Ok(list)
    }

    fn value(&mut self) -> Result<Value, SerError> {
        let tag = self.bytes(1)?[0];
        match tag {
            0 => Ok(Value::undefined()),
            1 => Ok(Value::null()),
            2 => Ok(Value::bool(self.bytes(1)?[0] != 0)),
            3 => {
                let b = self.bytes(8)?;
                let bits = u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
                Ok(Value::number(f64::from_bits(bits)))
            }
            4 => {
                let b = self.bytes(8)?;
                let mut raw = [0u8; 8];
                raw.copy_from_slice(b);
                Ok(Value::int(i64::from_be_bytes(raw)))
            }
            5 => Ok(Value::string(self.str()?)),
            6 => {
                let b = self.bytes(8)?;
                let mut raw = [0u8; 8];
                raw.copy_from_slice(b);
                Ok(Value::symbol(u64::from_be_bytes(raw)))
            }
            7 => {
                let count = self.u32()? as usize;
                let mut arr = Vec::with_capacity(count);
                for _ in 0..count {
                    arr.push(self.value()?);
                }
                Ok(Value::array(arr))
            }
            8 => {
                let count = self.u32()? as usize;
                let mut m = hashbrown::HashMap::with_capacity_and_hasher(count, Default::default());
                for _ in 0..count {
                    let k = self.str()?;
                    let v = self.value()?;
                    m.insert(k, v);
                }
                Ok(Value::object(m))
            }
            t => Err(SerError::InvalidFormat(format!("bad constant tag {}", t))),
        }
    }
}

/// Decode a value from the wire format at `pos`, advancing `pos`. The
/// cross-thread spawn path uses it: a worker's serialized result lands on the
/// VM thread as raw bytes and is decoded into fresh values that allocate into
/// the VM's arena heap.
pub(crate) fn decode_value(data: &[u8], pos: &mut usize) -> Result<Value, SerError> {
    let mut r = Reader { data, pos: *pos };
    let v = r.value()?;
    *pos = r.pos;
    Ok(v)
}
