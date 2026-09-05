#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
    // Constants
    LoadConst = 0,
    LoadInt,
    LoadTrue,
    LoadFalse,
    LoadNull,
    LoadUndefined,

    // Variables
    LoadLocal,
    StoreLocal,
    LoadGlobal,
    StoreGlobal,

    // Arithmetic
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Negate,

    // Comparison
    Equal,
    NotEqual,
    StrictEqual,
    Less,
    Greater,
    LessEqual,
    GreaterEqual,

    // Logical
    And,
    Or,
    Not,

    // Control flow
    Jump,
    JumpIfFalse,
    JumpIfTrue,
    Call,
    Return,

    // Objects/Arrays
    MakeArray,
    MakeObject,
    GetProperty,
    SetProperty,

    // Stack
    Pop,
    Dup,

    // Special
    Halt,
    Nop,
    TypeOf,
    Print,

    // Shared memory
    AllocShared,
    ReadShared,
    WriteShared,

    // Message passing
    Send,
    Receive,
    Spawn,

    // Closures / upvalues
    CaptureLocal,
    CaptureUpvalue,
    NewClosure,
    LoadUpvalue,
    StoreUpvalue,
    LoadCell,
    StoreCell,
    LoadSelf,
    GetIndex,
    SetIndex,
    GetKeys,

    // Async / promises
    NewPromise,
    Await,

    // Exceptions
    Throw,
    TryStart,
    TryEnd,

    // Spread (array literals / call arguments)
    CallSpread,
    MakeArraySpread,

    // Rest parameters / rest elements
    MakeRestArray,
    ArraySlice,

    // Fused superinstructions (appended so the .ax format stays backward
    // compatible — old files never contain them). Each collapses the hottest
    // LoadLocal→arith→StoreLocal / loop-condition sequences into one dispatch.
    CmpLocalInt,
    BinLocalInt,
    BinLocalLocal,
    CmpLocalLocal,
    ArithStoreLocal,
    IncLocal,
    ArithStoreUpvalue,

    // Property-compound superinstructions: `obj.p += v` collapses the
    // store-tmp / reload / GetProperty / arith / Dup / reload / SetProperty
    // chain. CompoundPropConst covers a constant RHS (`o.a += 1`, one
    // dispatch); PeekProperty + ArithWriteProp cover a general RHS — the old
    // value is read before the RHS evaluates, matching JS compound-assignment
    // order, and the obj stays on the stack so it evaluates exactly once.
    CompoundPropConst,
    PeekProperty,
    ArithWriteProp,

    // Index-compound superinstructions, mirroring the property fusions above
    // for `a[i] += v`: the obj and index stay on the stack and evaluate
    // exactly once, the old value is read before the RHS, and the write is
    // folded into the same dispatch(es).
    CompoundIndexConst,
    PeekIndex,
    ArithWriteIndex,

    // Property inc/dec: `o.a++` / `++o.a` / `o.a--` / `--o.a` — one dispatch
    // reads obj.p, adds ±1, writes back, and pushes the old (postfix) or new
    // (prefix) value. Inc/dec has no RHS expression, so a single opcode is
    // fully spec-correct.
    IncPropConst,

    // Index inc/dec, mirroring IncPropConst for `a[i]++` / `++a[i]`: the obj
    // and index stay on the stack (each evaluating exactly once) and one
    // dispatch reads obj[idx], adds ±1, writes back, and pushes the result.
    IncIndexConst,

    // JumpIfFalsePop: like JumpIfFalse but pops the tested condition, for
    // discarded-condition contexts (loop conditions, if/ternary tests) where
    // the old `emit cond; JumpIfFalse; Pop` pair collapsed to one dispatch.
    // The condition must be pushed exactly once and is always consumed on
    // both paths.
    JumpIfFalsePop,

    // CallKeep0 / CallSpreadKeep0: statement-position calls (`f(x);`) whose
    // return value is discarded. The callee and args are consumed exactly as
    // in Call/CallSpread, but the result is never pushed back to the caller
    // (the frame records keep_result=false, so Return / the async-suspension
    // path skip it). Cuts the old `Call; Pop` pair to one dispatch.
    CallKeep0,
    CallSpreadKeep0,

    // JumpIfTruePop: mirror of JumpIfFalsePop for `while (a || b)` loop
    // conditions compiled with inline short-circuiting — pops the tested
    // value and jumps when it is truthy (to the loop body), falling through
    // to the next operand otherwise.
    JumpIfTruePop,

    // Bitwise operators: `a & b` / `a | b` / `a ^ b` (ToInt32 both operands,
    // push the Int32 result) and unary `~a` (ToInt32 then invert). These were
    // previously silently dropped by the lexer; now real JS semantics.
    BitAnd,
    BitOr,
    BitXor,
    BitNot,

    // StrictNotEqual (`!==`): the negation of StrictEqual. Previously the
    // compiler emitted loose NotEqual for `!==`; now it has its own opcode so
    // `1 !== "1"` is true and `null !== undefined` is true.
    StrictNotEqual,

    // Shift operators: `a << b` / `a >> b` (ToInt32, count = ToUint32(b) & 31,
    // signed results) and `a >>> b` (logical — result is an unsigned 32-bit
    // value, so `-1 >>> 0` is 4294967295).
    Shl,
    Shr,
    UShr,

    // Pow (`a ** b`): exponentiation. Int/int results stay int when they fit
    // (checked pow); everything else is f64 powf, so `2 ** 0.5`, `(-2) ** 3`
    // and `2 ** 100` all follow JS semantics.
    Pow,

    // DeleteProp / DeleteIndex: `delete o.p` / `delete o[i]`. The reference
    // (obj + name/index) is on the stack; the result is always true.
    DeleteProp,
    DeleteIndex,

    // String-accumulator fusions: `s = s + X` / `s += X` collapse the
    // LoadLocal→(rhs)→Add→StoreLocal chain into one dispatch — the builder
    // box stays in the local slot (a register) between appends. Each applies
    // the exact `Add` semantics (`l.add(r)`, so non-string accumulators
    // coerce identically to the general path) and optionally leaves the
    // result on the stack for the assignment's value. The lhs is always read
    // before the rhs: Const/Local read both inside the opcode (no side
    // effects between two loads); Pop consumes a lhs snapshot pushed before
    // the general rhs expression evaluated.
    AppendStringConst,
    AppendStringLocal,
    AppendStringPop,

    // Polyglot: `import { f } from './x.py' as python` — load (or lazily
    // build) the module object for the Python file named by a string
    // constant. One native per exported function; calls round-trip through
    // the sidecar process over the shared segment.
    LoadPython,

    // LoadLocalGetPropConst: `local.prop` where the property is a string
    // constant — collapses LoadLocal + LoadConst + GetProperty (3
    // dispatches) into one. The hottest case is `arr.length` in loop
    // conditions; objects get the same monomorphic-IC path as GetProperty.
    LoadLocalGetPropConst,

    // LoadLocalLocalGetIndex: `a[i]` where both the array and the index are
    // locals — collapses LoadLocal + LoadLocal + GetIndex (3 dispatches)
    // into one, keeping the packed-int array fast path.
    LoadLocalLocalGetIndex,

    // Peephole-fused int arithmetic (emitted by Program::peephole, never
    // directly from the AST):
    //
    // BinIntLocal: `imm ar r{slot}` — one dispatch for `3 * n` style
    // int-on-left patterns (LoadInt + LoadLocal + AR collapsed). Encoding:
    // i32 imm, u8 slot, u8 ar (bit 7 = discard-result flag: a trailing Pop
    // after the old AR opcode folds into keep=0, so statement-position
    // arithmetic leaves nothing).
    BinIntLocal,

    // BinLocalLocalInt: `(r{a} ar1 r{b}) ar2 imm` — one dispatch for
    // `(i + j) % 7` style chains (BinLocalLocal + LoadInt + AR collapsed).
    // Encoding: u8 a, u8 b, u8 ar1, i32 imm, u8 ar2 (bit 7 = discard flag).
    BinLocalLocalInt,

    // Comparison-chain superinstructions (emitted by Program::peephole, never
    // directly from the AST): `a < b && b < c` (or `||`, and any mix of
    // local/int operands) collapses Cmp+Jump+Pop+Cmp (value context) or
    // Cmp+JumpPop+Cmp+JumpPop (condition context) into ONE dispatch. The
    // short-circuit value semantics are preserved: when the first comparison
    // fires (&&: falsy, ||: truthy) its bool is the result and the second
    // comparison is never evaluated; otherwise the second comparison's bool
    // is the result. Bit 7 of the second cmp byte selects `||`.
    //
    // Encodings:
    //   CmpAndLocalLocal: u8 a, u8 b, u8 cmp1, u8 c, u8 d, u8 cmp2 (7 bytes)
    //   CmpAndLocalInt:   u8 a, u8 b, u8 cmp1, u8 c, i32 imm, u8 cmp2 (10)
    //   CmpAndIntLocal:   u8 a, i32 imm, u8 cmp1, u8 c, u8 d, u8 cmp2 (10)
    //   CmpAndIntInt:     u8 a, i32 imm1, u8 cmp1, u8 b, i32 imm2, u8 cmp2 (13)
    CmpAndLocalLocal,
    CmpAndLocalInt,
    CmpAndIntLocal,
    CmpAndIntInt,

    // Register-ALU chain (emitted by the compiler from int-arithmetic trees;
    // never directly from a single op). `3 * n + 1`, `(lo + hi) % 2`,
    // `seed = (seed * 48271) % 2147483648` — chains of + - * / % over locals
    // and literals — collapse N dispatches + N stack round-trips into ONE
    // dispatch that keeps the running value in an i64 register inside the VM
    // hot state, boxing only at the end (or not at all when the terminal
    // stores directly to a local).
    //
    // Encoding (variable length):
    //   [op][count u8][term u8][step x count]  where each step = [hdr u8][op i32]
    //   count = number of steps (≤ 24)
    //   term  = 0 (discard) | 0x80 (push result) | 0x40|slot (store to local
    //           slot, bits 0-5) | 0xC0|slot (store + push)
    //   hdr   = kind << 5 | ar, kinds: 0=LoadLocal (op = slot), 1=Const
    //           (op = i32 imm, u32-sign-extended: -1 -> 0xFFFFFFFF,
    //           2147483648 -> 0x80000000), 2=Save (push acc), 3=Combine
    //           (pop t; acc = t ar acc). The first LoadLocal/Const step
    //           initializes acc (its ar is ignored); every later step applies
    //           `acc = acc ar operand`. Steps with ar > 4 (bitwise/shift/pow)
    //           or non-int values fall back to the generic Value path per
    //           step, so the opcode is exactly equivalent to running the same
    //           sequence of ADD/SUB/MUL/DIV/MOD/BitAnd/... opcodes.
    ArithChain,

    // Fixed-shape register-ALU superinstructions (emitted by the compiler
    // from the assign-path chain builder; the ar bytes are the raw
    // arith_code 0..=11 with bit 7 = keep, so there is no init marker or
    // per-step decode — the handler is a straight-line load/arith/arith/
    // store, lean enough to beat the dispatches it replaces on the
    // collatz/array/loop int loops):
    //
    // Arith2StoreLocalConst:  `t = locals[slot] ar imm; store slot` (7B) —
    //   `n = n / 2`, `j -= 1`, `steps += 1`.
    // Arith3StoreLocalConstConst: `t = (locals[s] ar1 imm1) ar2 imm2;
    //   store s` (11B) — `seed = (seed * 48271) % 2147483648`.
    // Arith3StoreConstLocalConst: `t = (imm1 ar1 locals[s]) ar2 imm2;
    //   store s` (11B) — `n = 3 * n + 1` (init on the left).
    // All three keep the result in an i64 register, boxing once at the
    // end; non-int locals or bitwise/shift/pow ars fall back to the
    // generic Value path (exactly the plain-opcode semantics).
    Arith2StoreLocalConst,
    Arith3StoreLocalConstConst,
    Arith3StoreConstLocalConst,
    /// `typeof <global>` without loading the value — undeclared globals
    /// return "undefined" instead of a ReferenceError. Declared LAST with an
    /// explicit discriminant: the enum's ordinals are the bytecode encoding
    /// (`Opcode as u8`), so inserting a variant anywhere else would shift
    /// every later opcode.
    TypeOfGlobal = 113,
    /// GetProperty for live-import binds: returns the RAW property value — a
    /// cell when the module exported a live binding — so the importing global
    /// can alias the module's own storage. Ordinary GetProperty unwraps
    /// cells, which would snapshot the value.
    GetPropertyCell = 114,
    /// `this` — the receiver of the current method/new call (undefined for a
    /// plain function call).
    LoadThis = 115,
    /// `new C(args)` — allocate an object with `C.prototype` as its proto
    /// chain head and call the constructor with `this` bound to it.
    New = 116,
    /// Method call `o.m(args)`: the receiver sits one slot below the args
    /// (base-1), the callee at the top. Binds `this` to the receiver.
    CallMethod = 117,
    /// Statement-position method call: like CallMethod but no result push.
    CallMethodKeep0 = 118,
    /// `o.m(...args)`: method call with spread arguments.
    CallMethodSpread = 119,
    /// Statement-position spread method call.
    CallMethodSpreadKeep0 = 120,
    /// `super.m` — read the prototype chain head of an object (the home
    /// object's parent). Pushes the object's `proto` field, or undefined.
    GetProto = 121,
    /// `obj.proto = v` (internal): set an object's prototype chain head.
    SetProto = 122,
    /// `obj instanceof C` — walk obj's proto chain comparing against
    /// `C.prototype`.
    InstanceOf = 123,
    /// Condition fusion: `local cmp int` tested by a JumpIfFalsePop. One
    /// dispatch: read the local, compare with the immediate, jump to the
    /// target on a falsy result (the compare value is consumed).
    /// Layout: op + slot + i32 imm + cmp + u32 target.
    CmpLocalIntJumpIfFalsePop = 124,
    /// Condition fusion: `local1 cmp local2` tested by a JumpIfFalsePop.
    /// Layout: op + a + b + cmp + u32 target.
    CmpLocalLocalJumpIfFalsePop = 125,
    /// Condition fusion: `a[idx] cmp local` tested by a JumpIfFalsePop
    /// (the LoadLocalLocalGetIndex + LoadLocal + Cmp + JumpIfFalsePop
    /// chain). Layout: op + objs + idxs + kslot + cmp + u32 target.
    LoadIndexCmpLocalJumpIfFalsePop = 126,
    /// Condition fusion: `(local ar imm1) cmp imm2` tested by a
    /// JumpIfFalsePop (the BinLocalInt + LoadInt + Cmp + JumpIfFalsePop
    /// chain, e.g. `n % 2 === 0`). Layout: op + slot + i32 imm1 + ar +
    /// i32 imm2 + cmp + u32 target.
    ArithLocalIntCmpJumpIfFalsePop = 127,
    /// Index write fusion: `arr[i] = v` with all three operands locals
    /// (LoadLocal + LoadLocal + LoadLocal + SetIndex). Layout: op + objs +
    /// idxs + vslot.
    SetIndexLocalLocal = 128,
    /// Index write fusion: `arr[i] = brr[j]` (LoadLocal + LoadLocal +
    /// LoadLocal + LoadLocal + LoadLocalLocalGetIndex + SetIndex).
    /// Layout: op + objs + idxs + vobjs + vidxs.
    SetIndexLocalGetLocal = 129,
    /// Index write fusion: `arr[i + imm] = brr[j]` (LoadLocal + LoadLocal +
    /// LoadInt + BinLocalInt + LoadLocal + LoadLocal + LoadLocalLocalGetIndex
    /// + SetIndex, the `a[j + 1] = a[j]` shift shape). Layout: op + objs +
    /// idxs + ar + i32 imm + vobjs + vidxs.
    SetIndexLocalPlusIntLocalGetLocal = 130,
    /// Optional chaining: pops the tested value and jumps when it is null or
    /// undefined (the chain's short-circuit path). The compiler emits
    /// `Dup; JumpIfNullish L; Pop` before each `?.` link; L discards the
    /// chain's accumulated stack values and pushes undefined.
    JumpIfNullish = 131,
    /// Synthetic iterator: pops a value and pushes its iteration snapshot.
    /// A Map becomes its `[k, v]` entry pairs (so `for (let [k, v] of m)`
    /// and `[...m]` yield entries in insertion order), a Set becomes its
    /// elements; everything else passes through unchanged (arrays/strings
    /// are already iterable). Single byte.
    ToIterable = 132,
    /// The `arguments` object of the current call: an array snapshot of the
    /// passed arguments (extra args beyond the params count, missing params
    /// don't). `arguments.length` and iteration therefore match Node for the
    /// common cases. Single byte.
    LoadArguments = 133,
    /// `key in obj` — true when `obj` (or its prototype chain) has `key` as
    /// a property; TypeError for non-object targets, matching Node. Pops the
    /// key then the object. Single byte.
    In = 134,
    /// Wrap the top value in a fresh cell — an arrow's lexical `\0this` /
    /// `\0arguments` capture, so NewClosure (which assumes stack captures are
    /// already cells) freezes the captured value instead of dropping it for
    /// undefined. Single byte.
    WrapCell = 135,
    /// `/pattern/flags`: pops nothing; pushes a fresh regex value. Operands
    /// are the pattern and flags constant indices (u16 each) — the VM
    /// compiles the pattern once per (pattern, flags) and caches the shared
    /// program. 5 bytes.
    MakeRegex = 136,
    /// `new C(...args)`: like New but with spread arguments (arg count is
    /// dynamic, so the argc byte is a count of argument SLOTS and a mask
    /// marks which are spreads, exactly like CallSpread). Layout:
    /// op + u8 slots + u16 mask.
    NewSpread = 137,
    /// Install a class getter/setter: pops [name, obj, fn] and stores `fn`
    /// as the getter (flag 1) or setter (flag 2) of `obj[name]`, so
    /// property reads/writes invoke it with the receiver as `this`. Layout:
    /// op + u8 kind.
    SetAccessor = 138,
}

impl Opcode {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(Self::LoadConst),
            1 => Some(Self::LoadInt),
            2 => Some(Self::LoadTrue),
            3 => Some(Self::LoadFalse),
            4 => Some(Self::LoadNull),
            5 => Some(Self::LoadUndefined),
            6 => Some(Self::LoadLocal),
            7 => Some(Self::StoreLocal),
            8 => Some(Self::LoadGlobal),
            9 => Some(Self::StoreGlobal),
            10 => Some(Self::Add),
            11 => Some(Self::Subtract),
            12 => Some(Self::Multiply),
            13 => Some(Self::Divide),
            14 => Some(Self::Modulo),
            15 => Some(Self::Negate),
            16 => Some(Self::Equal),
            17 => Some(Self::NotEqual),
            18 => Some(Self::StrictEqual),
            19 => Some(Self::Less),
            20 => Some(Self::Greater),
            21 => Some(Self::LessEqual),
            22 => Some(Self::GreaterEqual),
            23 => Some(Self::And),
            24 => Some(Self::Or),
            25 => Some(Self::Not),
            26 => Some(Self::Jump),
            27 => Some(Self::JumpIfFalse),
            28 => Some(Self::JumpIfTrue),
            29 => Some(Self::Call),
            30 => Some(Self::Return),
            31 => Some(Self::MakeArray),
            32 => Some(Self::MakeObject),
            33 => Some(Self::GetProperty),
            34 => Some(Self::SetProperty),
            35 => Some(Self::Pop),
            36 => Some(Self::Dup),
            37 => Some(Self::Halt),
            38 => Some(Self::Nop),
            39 => Some(Self::TypeOf),
            40 => Some(Self::Print),
            41 => Some(Self::AllocShared),
            42 => Some(Self::ReadShared),
            43 => Some(Self::WriteShared),
            44 => Some(Self::Send),
            45 => Some(Self::Receive),
            46 => Some(Self::Spawn),
            47 => Some(Self::CaptureLocal),
            48 => Some(Self::CaptureUpvalue),
            49 => Some(Self::NewClosure),
            50 => Some(Self::LoadUpvalue),
            51 => Some(Self::StoreUpvalue),
            52 => Some(Self::LoadCell),
            53 => Some(Self::StoreCell),
            54 => Some(Self::LoadSelf),
            55 => Some(Self::GetIndex),
            56 => Some(Self::SetIndex),
            57 => Some(Self::GetKeys),
            58 => Some(Self::NewPromise),
            59 => Some(Self::Await),
            60 => Some(Self::Throw),
            61 => Some(Self::TryStart),
            62 => Some(Self::TryEnd),
            63 => Some(Self::CallSpread),
            64 => Some(Self::MakeArraySpread),
            65 => Some(Self::MakeRestArray),
            66 => Some(Self::ArraySlice),
            67 => Some(Self::CmpLocalInt),
            68 => Some(Self::BinLocalInt),
            69 => Some(Self::BinLocalLocal),
            70 => Some(Self::CmpLocalLocal),
            71 => Some(Self::ArithStoreLocal),
            72 => Some(Self::IncLocal),
            73 => Some(Self::ArithStoreUpvalue),
            74 => Some(Self::CompoundPropConst),
            75 => Some(Self::PeekProperty),
            76 => Some(Self::ArithWriteProp),
            77 => Some(Self::CompoundIndexConst),
            78 => Some(Self::PeekIndex),
            79 => Some(Self::ArithWriteIndex),
            80 => Some(Self::IncPropConst),
            81 => Some(Self::IncIndexConst),
            82 => Some(Self::JumpIfFalsePop),
            83 => Some(Self::CallKeep0),
            84 => Some(Self::CallSpreadKeep0),
            85 => Some(Self::JumpIfTruePop),
            86 => Some(Self::BitAnd),
            87 => Some(Self::BitOr),
            88 => Some(Self::BitXor),
            89 => Some(Self::BitNot),
            90 => Some(Self::StrictNotEqual),
            91 => Some(Self::Shl),
            92 => Some(Self::Shr),
            93 => Some(Self::UShr),
            94 => Some(Self::Pow),
            95 => Some(Self::DeleteProp),
            96 => Some(Self::DeleteIndex),
            97 => Some(Self::AppendStringConst),
            98 => Some(Self::AppendStringLocal),
            99 => Some(Self::AppendStringPop),
            100 => Some(Self::LoadPython),
            101 => Some(Self::LoadLocalGetPropConst),
            102 => Some(Self::LoadLocalLocalGetIndex),
            103 => Some(Self::BinIntLocal),
            104 => Some(Self::BinLocalLocalInt),
            105 => Some(Self::CmpAndLocalLocal),
            106 => Some(Self::CmpAndLocalInt),
            107 => Some(Self::CmpAndIntLocal),
            108 => Some(Self::CmpAndIntInt),
            109 => Some(Self::ArithChain),
            110 => Some(Self::Arith2StoreLocalConst),
            111 => Some(Self::Arith3StoreLocalConstConst),
            112 => Some(Self::Arith3StoreConstLocalConst),
            113 => Some(Self::TypeOfGlobal),
            114 => Some(Self::GetPropertyCell),
            115 => Some(Self::LoadThis),
            116 => Some(Self::New),
            117 => Some(Self::CallMethod),
            118 => Some(Self::CallMethodKeep0),
            119 => Some(Self::CallMethodSpread),
            120 => Some(Self::CallMethodSpreadKeep0),
            121 => Some(Self::GetProto),
            122 => Some(Self::SetProto),
            123 => Some(Self::InstanceOf),
            124 => Some(Self::CmpLocalIntJumpIfFalsePop),
            125 => Some(Self::CmpLocalLocalJumpIfFalsePop),
            126 => Some(Self::LoadIndexCmpLocalJumpIfFalsePop),
            127 => Some(Self::ArithLocalIntCmpJumpIfFalsePop),
            128 => Some(Self::SetIndexLocalLocal),
            129 => Some(Self::SetIndexLocalGetLocal),
            130 => Some(Self::SetIndexLocalPlusIntLocalGetLocal),
            131 => Some(Self::JumpIfNullish),
            132 => Some(Self::ToIterable),
            133 => Some(Self::LoadArguments),
            134 => Some(Self::In),
            135 => Some(Self::WrapCell),
            136 => Some(Self::MakeRegex),
            137 => Some(Self::NewSpread),
            138 => Some(Self::SetAccessor),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::LoadConst => "LOAD_CONST",
            Self::LoadInt => "LOAD_INT",
            Self::LoadTrue => "LOAD_TRUE",
            Self::LoadFalse => "LOAD_FALSE",
            Self::LoadNull => "LOAD_NULL",
            Self::LoadUndefined => "LOAD_UNDEFINED",
            Self::LoadLocal => "LOAD_LOCAL",
            Self::StoreLocal => "STORE_LOCAL",
            Self::LoadGlobal => "LOAD_GLOBAL",
            Self::StoreGlobal => "STORE_GLOBAL",
            Self::Add => "ADD",
            Self::Subtract => "SUB",
            Self::Multiply => "MUL",
            Self::Divide => "DIV",
            Self::Modulo => "MOD",
            Self::Negate => "NEG",
            Self::Equal => "EQ",
            Self::NotEqual => "NEQ",
            Self::StrictEqual => "SEQ",
            Self::Less => "LT",
            Self::Greater => "GT",
            Self::LessEqual => "LTE",
            Self::GreaterEqual => "GTE",
            Self::And => "AND",
            Self::Or => "OR",
            Self::Not => "NOT",
            Self::Jump => "JMP",
            Self::JumpIfFalse => "JMP_IF_FALSE",
            Self::JumpIfTrue => "JMP_IF_TRUE",
            Self::Call => "CALL",
            Self::Return => "RET",
            Self::MakeArray => "MAKE_ARRAY",
            Self::MakeObject => "MAKE_OBJECT",
            Self::GetProperty => "GET_PROP",
            Self::SetProperty => "SET_PROP",
            Self::Pop => "POP",
            Self::Dup => "DUP",
            Self::Halt => "HALT",
            Self::Nop => "NOP",
            Self::TypeOf => "TYPEOF",
            Self::TypeOfGlobal => "TYPEOF_GLOBAL",
            Self::GetPropertyCell => "GET_PROP_CELL",
            Self::LoadThis => "LOAD_THIS",
            Self::New => "NEW",
            Self::CallMethod => "CALL_METHOD",
            Self::CallMethodKeep0 => "CALL_METHOD_KEEP0",
            Self::CallMethodSpread => "CALL_METHOD_SPREAD",
            Self::CallMethodSpreadKeep0 => "CALL_METHOD_SPREAD_KEEP0",
            Self::GetProto => "GET_PROTO",
            Self::SetProto => "SET_PROTO",
            Self::InstanceOf => "INSTANCE_OF",
            Self::In => "IN",
            Self::WrapCell => "WRAP_CELL",
            Self::CmpLocalIntJumpIfFalsePop => "CMP_LOCAL_INT_JIF_POP",
            Self::CmpLocalLocalJumpIfFalsePop => "CMP_LOCAL_LOCAL_JIF_POP",
            Self::LoadIndexCmpLocalJumpIfFalsePop => "LOAD_INDEX_CMP_LOCAL_JIF_POP",
            Self::ArithLocalIntCmpJumpIfFalsePop => "ARITH_LOCAL_INT_CMP_JIF_POP",
            Self::SetIndexLocalLocal => "SET_INDEX_LOCAL_LOCAL",
            Self::SetIndexLocalGetLocal => "SET_INDEX_LOCAL_GET_LOCAL",
            Self::SetIndexLocalPlusIntLocalGetLocal => "SET_INDEX_LOCAL_PLUS_INT_LOCAL_GET_LOCAL",
            Self::Print => "PRINT",
            Self::AllocShared => "ALLOC_SHARED",
            Self::ReadShared => "READ_SHARED",
            Self::WriteShared => "WRITE_SHARED",
            Self::Send => "SEND",
            Self::Receive => "RECEIVE",
            Self::Spawn => "SPAWN",
            Self::CaptureLocal => "CAPTURE_LOCAL",
            Self::CaptureUpvalue => "CAPTURE_UPVALUE",
            Self::NewClosure => "NEW_CLOSURE",
            Self::LoadUpvalue => "LOAD_UPVALUE",
            Self::StoreUpvalue => "STORE_UPVALUE",
            Self::LoadCell => "LOAD_CELL",
            Self::StoreCell => "STORE_CELL",
            Self::LoadSelf => "LOAD_SELF",
            Self::GetIndex => "GET_INDEX",
            Self::SetIndex => "SET_INDEX",
            Self::GetKeys => "GET_KEYS",
            Self::NewPromise => "NEW_PROMISE",
            Self::Await => "AWAIT",
            Self::Throw => "THROW",
            Self::TryStart => "TRY_START",
            Self::TryEnd => "TRY_END",
            Self::CallSpread => "CALL_SPREAD",
            Self::MakeArraySpread => "MAKE_ARRAY_SPREAD",
            Self::MakeRestArray => "MAKE_REST_ARRAY",
            Self::ArraySlice => "ARRAY_SLICE",
            Self::CmpLocalInt => "CMP_LOCAL_INT",
            Self::BinLocalInt => "BIN_LOCAL_INT",
            Self::BinLocalLocal => "BIN_LOCAL_LOCAL",
            Self::CmpLocalLocal => "CMP_LOCAL_LOCAL",
            Self::ArithStoreLocal => "ARITH_STORE_LOCAL",
            Self::IncLocal => "INC_LOCAL",
            Self::ArithStoreUpvalue => "ARITH_STORE_UPVALUE",
            Self::CompoundPropConst => "COMPOUND_PROP_CONST",
            Self::PeekProperty => "PEEK_PROP",
            Self::ArithWriteProp => "ARITH_WRITE_PROP",
            Self::CompoundIndexConst => "COMPOUND_INDEX_CONST",
            Self::PeekIndex => "PEEK_INDEX",
            Self::ArithWriteIndex => "ARITH_WRITE_INDEX",
            Self::IncPropConst => "INC_PROP_CONST",
            Self::IncIndexConst => "INC_INDEX_CONST",
            Self::JumpIfFalsePop => "JMP_IF_FALSE_POP",
            Self::CallKeep0 => "CALL_KEEP0",
            Self::CallSpreadKeep0 => "CALL_SPREAD_KEEP0",
            Self::JumpIfTruePop => "JMP_IF_TRUE_POP",
            Self::BitAnd => "BIT_AND",
            Self::BitOr => "BIT_OR",
            Self::BitXor => "BIT_XOR",
            Self::BitNot => "BIT_NOT",
            Self::StrictNotEqual => "STRICT_NEQ",
            Self::Shl => "SHL",
            Self::Shr => "SHR",
            Self::UShr => "USHR",
            Self::Pow => "POW",
            Self::DeleteProp => "DELETE_PROP",
            Self::DeleteIndex => "DELETE_INDEX",
            Self::AppendStringConst => "APPEND_STRING_CONST",
            Self::AppendStringLocal => "APPEND_STRING_LOCAL",
            Self::AppendStringPop => "APPEND_STRING_POP",
            Self::LoadPython => "LOAD_PYTHON",
            Self::LoadLocalGetPropConst => "LOAD_LOCAL_GET_PROP_CONST",
            Self::LoadLocalLocalGetIndex => "LOAD_LOCAL_LOCAL_GET_INDEX",
            Self::BinIntLocal => "BIN_INT_LOCAL",
            Self::BinLocalLocalInt => "BIN_LOCAL_LOCAL_INT",
            Self::CmpAndLocalLocal => "CMP_AND_LOCAL_LOCAL",
            Self::CmpAndLocalInt => "CMP_AND_LOCAL_INT",
            Self::CmpAndIntLocal => "CMP_AND_INT_LOCAL",
            Self::CmpAndIntInt => "CMP_AND_INT_INT",
            Self::ArithChain => "ARITH_CHAIN",
            Self::Arith2StoreLocalConst => "ARITH2_STORE_LOCAL_CONST",
            Self::Arith3StoreLocalConstConst => "ARITH3_STORE_LOCAL_CONST_CONST",
            Self::Arith3StoreConstLocalConst => "ARITH3_STORE_CONST_LOCAL_CONST",
            Self::JumpIfNullish => "JMP_IF_NULLISH",
            Self::ToIterable => "TO_ITERABLE",
            Self::LoadArguments => "LOAD_ARGUMENTS",
            Self::MakeRegex => "MAKE_REGEX",
            Self::NewSpread => "NEW_SPREAD",
            Self::SetAccessor => "SET_ACCESSOR",
        }
    }
}
