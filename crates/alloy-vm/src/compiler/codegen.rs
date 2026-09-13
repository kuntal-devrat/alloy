use crate::ast::*;
use crate::bytecode::Program;
use crate::compiler::error::CompileError;
use crate::compiler::lexer::Lexer;
use crate::compiler::parser::Parser;
use crate::compiler::scope::*;
use crate::compiler::token::*;
use crate::opcode::Opcode;
use alloy_core::heap::{ArenaHeap, HeapGuard};
use alloy_core::value::Value;

/// The opcode implementing a compound-assignment operator, if `op` is one.
fn compound_opcode(op: &str) -> Option<Opcode> {
    match op {
        "+=" => Some(Opcode::Add),
        "-=" => Some(Opcode::Subtract),
        "*=" => Some(Opcode::Multiply),
        "/=" => Some(Opcode::Divide),
        "%=" => Some(Opcode::Modulo),
        "&=" => Some(Opcode::BitAnd),
        "|=" => Some(Opcode::BitOr),
        "^=" => Some(Opcode::BitXor),
        "<<=" => Some(Opcode::Shl),
        ">>=" => Some(Opcode::Shr),
        ">>>=" => Some(Opcode::UShr),
        "**=" => Some(Opcode::Pow),
        _ => None,
    }
}

/// Arithmetic codes for the fused superinstructions
/// (0=Add 1=Subtract 2=Multiply 3=Divide 4=Modulo 5=BitAnd 6=BitOr 7=BitXor
/// 8=Shl 9=Shr 10=UShr).
fn arith_code(op: &str) -> Option<u8> {
    match op {
        "+" => Some(0),
        "-" => Some(1),
        "*" => Some(2),
        "/" => Some(3),
        "%" => Some(4),
        "&" => Some(5),
        "|" => Some(6),
        "^" => Some(7),
        "<<" => Some(8),
        ">>" => Some(9),
        ">>>" => Some(10),
        "**" => Some(11),
        _ => None,
    }
}    /// A step in a fused ArithChain: header byte (`kind << 5 | ar`) + operand.
    /// Kinds: 0=LoadLocal (operand = slot), 1=Const (operand = i32 imm),
    /// 2=Save (push acc onto the operand stack), 3=Combine (pop t; acc = t ar
    /// acc). Chain ar codes are `arith_code + 1` (0 = init): a LoadLocal/Const
    /// step with ar=0 initializes acc, every other step applies
    /// `acc = acc ar operand`.
#[derive(Clone, Copy)]
struct ChainStep {
    hdr: u8,
    op: i64,
}

impl ChainStep {
    // Header = kind << 5 | ar (ar bits 0-4, kind bits 5-7). The kind
    // constants below are already shifted into the header position.
    const LOAD: u8 = 0;
    const CONST: u8 = 1 << 5;
    const SAVE: u8 = 2 << 5;
    const COMBINE: u8 = 3 << 5;
}

impl Compiler {
    /// A leaf usable as an ArithChain operand: a local or an int literal
    /// (paren-wrapped ok). Everything else (calls, indexes, globals,
    /// upvalues, stringsâ€¦) is not chainable â€” those fall back to the normal
    /// stack emission, so the fused opcode is never asked to handle a value
    /// whose evaluation has side effects or unknown identity.
    fn chain_leaf(&mut self, e: &Expr) -> Option<ChainStep> {
        match e {
            Expr::Paren(inner) => self.chain_leaf(inner),
            Expr::Int(v) => Some(ChainStep { hdr: ChainStep::CONST, op: *v }),
            Expr::Ident(name) => match self.resolve(name) {
                Resolved::Local(s) => Some(ChainStep { hdr: ChainStep::LOAD, op: s as i64 }),
                _ => None,
            },
            _ => None,
        }
    }

    /// Linearize an int-arithmetic tree (add/sub/mul/div/mod/bitwise/pow over
    /// locals and literals) into ArithChain steps. Left-associative spines
    /// with leaf right-operands become straight `acc = acc ar leaf` steps;
    /// a right operand that is itself a subtree becomes Save + subtree +
    /// Combine (`acc = t ar acc`), which nests to any depth via the operand
    /// stack. Returns the number of applied arithmetic ops, or None if `e` is
    /// not chainable. The compiled chain is exactly equivalent to running the
    /// same sequence of ADD/SUB/â€¦ opcodes (per-step i64 fast path with the
    /// generic Value fallback), so evaluation order and coercion semantics
    /// are preserved by construction.
    fn build_chain(&mut self, e: &Expr, steps: &mut Vec<ChainStep>) -> Option<usize> {
        match e {
            Expr::Paren(inner) => self.build_chain(inner, steps),
            Expr::Bin(op, l, r) => self.build_chain_node(op, l, r, steps),
            _ => None,
        }
    }

    /// Build steps for one binary node `l op r` (used at the chain root and
    /// recursively by [`build_chain`]). See [`build_chain`] for the
    /// Save/Combine nesting rules.
    fn build_chain_node(
        &mut self,
        op: &str,
        l: &Expr,
        r: &Expr,
        steps: &mut Vec<ChainStep>,
    ) -> Option<usize> {
        let ar = arith_code(op)?;
        let left_ops = match self.chain_leaf(l) {
            Some(s) => {
                steps.push(s);
                0
            }
            None => self.build_chain(l, steps)?,
        };
        // Chain ar codes are ar_code + 1 (0 = init marker): the compiler's
        // `+` maps to 0 in arith_code, which must not collide with the init
        // step's ar=0.
        let right_ops = match self.chain_leaf(r) {
            Some(mut s) => {
                s.hdr |= ar + 1;
                steps.push(s);
                1
            }
            None => {
                steps.push(ChainStep { hdr: ChainStep::SAVE, op: 0 });
                let sub = self.build_chain(r, steps)?;
                steps.push(ChainStep { hdr: ChainStep::COMBINE | (ar + 1), op: 0 });
                sub + 1
            }
        };
        Some(left_ops + right_ops)
    }

    fn emit_chain(&mut self, steps: &[ChainStep], term: u8) {
        self.program.emit_op(Opcode::ArithChain);
        self.program.emit_u8(steps.len() as u8);
        self.program.emit_u8(term);
        for s in steps {
            self.program.emit_u8(s.hdr);
            self.program.emit_i32(s.op as i32);
        }
    }

    /// Emit `l op r` as an ArithChain with the given terminal (`0` discard,
    /// `0x80` push, `0x40|slot` store, `0xC0|slot` store+push). Returns true
    /// when the chain fired: â‰¥ 2 ops (single ops are already fused to
    /// BinLocalInt/BinLocalLocal at 1 dispatch, so a chain would only add
    /// decode) and at least one local operand (a pure-constant tree is left
    /// to the peephole's constant fold, which precomputes it to a single
    /// LoadConst).
    fn emit_arith_chain(&mut self, op: &str, l: &Expr, r: &Expr, term: u8) -> bool {
        // Dev A/B switch: ALLOY_NO_CHAIN=1 disables the register-ALU fusion
        // (compile-time only; zero runtime cost) for measuring its effect.
        if std::env::var("ALLOY_NO_CHAIN").is_ok() {
            return false;
        }
        let mut steps = Vec::new();
        let Some(ops) = self.build_chain_node(op, l, r, &mut steps) else {
            return false;
        };
        let has_local = steps.iter().any(|s| s.hdr & 0xE0 == ChainStep::LOAD);
        if ops < 2 || !has_local || steps.len() > 48 {
            return false;
        }
        self.emit_chain(&steps, term);
        true
    }

    /// Emit `x = <chain>` / `x op= <chain>` (local target) as one ArithChain
    /// with a store terminal. Always fires when the RHS is chainable â€” even a
    /// single-op RHS beats the current LoadLocal + value + ArithStoreLocal /
    /// fused-op + StoreLocal sequences. Falls back (false) for slots â‰¥ 64
    /// (the store terminal holds 6 bits) â€” the existing path still chains the
    /// value via emit_arith_chain.
    fn try_emit_chain_assign(&mut self, s: u8, op: &str, value: &Expr, keep: bool) -> bool {
        // Dev A/B switch (see emit_arith_chain).
        if std::env::var("ALLOY_NO_CHAIN").is_ok() {
            return false;
        }
        if s >= 64 {
            return false;
        }
        let mut steps = Vec::new();
        if op == "=" {
            if self.build_chain(value, &mut steps).is_none() || steps.len() > 48 {
                return false;
            }
        } else {
            // x op= v  =>  x = x op v. A leaf RHS applies directly
            // (`acc = x op leaf`, 3 steps); a subtree needs the save/combine
            // bracket (`acc = x op [subtree]`).
            let Some(ar) = arith_code(op.trim_end_matches('=')) else {
                return false;
            };
            steps.push(ChainStep { hdr: ChainStep::LOAD, op: s as i64 });
            match self.chain_leaf(value) {
                Some(mut st) => {
                    st.hdr |= ar + 1;
                    steps.push(st);
                }
                None => {
                    steps.push(ChainStep { hdr: ChainStep::SAVE, op: 0 });
                    if self.build_chain(value, &mut steps).is_none() {
                        return false;
                    }
                    steps.push(ChainStep { hdr: ChainStep::COMBINE | (ar + 1), op: 0 });
                }
            }
            if steps.len() > 48 {
                return false;
            }
        }
        // Emit the chain as one of the fixed-shape register-ALU
        // superinstructions (lean straight-line handlers â€” the variable
        // ArithChain's per-step decode costs more than the dispatches it
        // replaces on these shapes). Non-matching chains fall back to the
        // normal emission below. The ar bytes are the raw arith_code with
        // bit 7 = keep; each step's enc_ar (arith_code + 1) is decoded back.
        let keepb = if keep { 0x80 } else { 0 };
        // [LOAD s][CONST imm ar]  ->  Arith2StoreLocalConst
        if steps.len() == 2
            && steps[0].hdr & 0xE0 == ChainStep::LOAD
            && steps[0].op as u8 == s
            && steps[1].hdr & 0xE0 == ChainStep::CONST
        {
            let ar = (steps[1].hdr & 0x1F) - 1;
            self.program.emit_op(Opcode::Arith2StoreLocalConst);
            self.program.emit_u8(s);
            self.program.emit_u8(ar | keepb);
            self.program.emit_i32(steps[1].op as i32);
            return true;
        }
        // [LOAD s][CONST i1 ar1][CONST i2 ar2]  ->  Arith3StoreLocalConstConst
        if steps.len() == 3
            && steps[0].hdr & 0xE0 == ChainStep::LOAD
            && steps[0].op as u8 == s
            && steps[1].hdr & 0xE0 == ChainStep::CONST
            && steps[2].hdr & 0xE0 == ChainStep::CONST
        {
            let ar1 = (steps[1].hdr & 0x1F) - 1;
            let ar2 = (steps[2].hdr & 0x1F) - 1;
            self.program.emit_op(Opcode::Arith3StoreLocalConstConst);
            self.program.emit_u8(s);
            self.program.emit_u8(ar1);
            self.program.emit_i32(steps[1].op as i32);
            self.program.emit_u8(ar2 | keepb);
            self.program.emit_i32(steps[2].op as i32);
            return true;
        }
        // [CONST c1][LOAD s ar1][CONST c2 ar2]  ->  Arith3StoreConstLocalConst
        if steps.len() == 3
            && steps[0].hdr & 0xE0 == ChainStep::CONST
            && steps[1].hdr & 0xE0 == ChainStep::LOAD
            && steps[1].op as u8 == s
            && steps[2].hdr & 0xE0 == ChainStep::CONST
        {
            let ar1 = (steps[1].hdr & 0x1F) - 1;
            let ar2 = (steps[2].hdr & 0x1F) - 1;
            self.program.emit_op(Opcode::Arith3StoreConstLocalConst);
            self.program.emit_i32(steps[0].op as i32);
            self.program.emit_u8(ar1);
            self.program.emit_u8(s);
            self.program.emit_u8(ar2 | keepb);
            self.program.emit_i32(steps[2].op as i32);
            return true;
        }
        false
    }
}

/// Comparison codes for the fused compare superinstructions, mirroring the
/// VM's Less/Greater/LessEqual/GreaterEqual/Equal/NotEqual/StrictEqual/
/// StrictNotEqual opcodes (7 = `!==`, the strict negation of `===`).
fn cmp_code(op: &str) -> Option<u8> {
    match op {
        "<" => Some(0),
        "<=" => Some(1),
        ">" => Some(2),
        ">=" => Some(3),
        "==" => Some(4),
        "!=" => Some(5),
        "===" => Some(6),
        "!==" => Some(7),
        _ => None,
    }
}

/// Arith code for a compound-assignment operator, if `op` is one.
fn compound_arith(op: &str) -> Option<u8> {
    match op {
        "+=" => Some(0),
        "-=" => Some(1),
        "*=" => Some(2),
        "/=" => Some(3),
        "%=" => Some(4),
        "&=" => Some(5),
        "|=" => Some(6),
        "^=" => Some(7),
        "<<=" => Some(8),
        ">>=" => Some(9),
        ">>>=" => Some(10),
        "**=" => Some(11),
        _ => None,
    }
}

pub struct Compiler {
    program: Program,
    /// Stack of function contexts; index 0 is the innermost (current) function.
    funcs: Vec<FuncCtx>,
    /// Break/continue targets for enclosing loops.
    loops: Vec<LoopCtx>,
    /// When true, top-level declarations are stored as globals (REPL mode).
    top_globals: bool,
    /// When true, `export` declarations are legal and recorded into
    /// `program.exports` (module mode, set by `compile_module`). In ordinary
    /// script mode `export` is a loud compile error â€” Node also rejects it
    /// outside ES modules.
    in_module: bool,
    /// Names declared at the top level (globals), as opposed to merely read:
    /// `delete x` on a declared binding is false, on an undeclared global is
    /// true (sloppy JS), and the globals table itself conflates the two.
    declared_globals: std::collections::HashSet<String>,
    /// Names bound by `import` statements (module files). Assigning to an
    /// import is an ESM SyntaxError â€” here it's a loud compile error, so a
    /// live-import cell can never be clobbered from the importing scope.
    imported_bindings: std::collections::HashSet<String>,
    /// Counter for compiler-generated temporary names (for-of/in desugaring).
    fresh_counter: u32,
    /// Enclosing `try` bodies being compiled, innermost last. Exiting one via
    /// `break`/`continue`/`return` runs its `finally` inline first.
    trys: Vec<TryCtx>,
    /// Slots pre-allocated for variables declared inside `finally` bodies, so
    /// re-emitting the body at exit sites stores into the same slot instead of
    /// allocating a new one each time.
    finally_locals: std::collections::HashMap<String, u8>,
    /// Enclosing labeled statements, innermost last.
    labels: Vec<LabelCtx>,
    /// Set right before emitting a loop that is directly labeled; the loop
    /// emitter consumes it so `continue label` targets the right loop even
    /// when the label stack's top belongs to an enclosing labeled statement.
    pending_label: Option<String>,
    /// Whether the expression currently being emitted must leave its value on
    /// the stack. False only for a top-level statement expression that is an
    /// assignment or inc/dec (their value is discarded): those arms then emit
    /// the fused ops with keep=0 and leave nothing, so the statement emits no
    /// trailing Pop. Sub-expressions are always emitted with this true (their
    /// values are consumed by their parent), which the keep-aware arms enforce
    /// by scoping their operand emissions.
    keep_result: bool,
    current_line: u32,
    current_col: u32,
}

impl Default for Compiler {
    fn default() -> Self {
        Self::new()
    }
}

impl Compiler {
    pub fn new() -> Self {
        Self {
            program: Program::new(),
            funcs: vec![FuncCtx {
                locals: Vec::new(),
                upvalues: Vec::new(),
                is_async: false,
                uses_arguments: false,
                is_arrow: false,
            }],
            loops: Vec::new(),
            top_globals: false,
            in_module: false,
            imported_bindings: std::collections::HashSet::new(),
            declared_globals: std::collections::HashSet::new(),
            fresh_counter: 0,
            trys: Vec::new(),
            finally_locals: std::collections::HashMap::new(),
            labels: Vec::new(),
            pending_label: None,
            keep_result: true,
            current_line: 1,
            current_col: 1,
        }
    }

    fn record_loc(&mut self, line: u32, col: u32) {
        self.current_line = line;
        self.current_col = col;
        self.program.record_location(line, col);
    }

    /// Emit `e` with its value discarded: the top-level assignment/inc/dec
    /// leaves no result (fused ops get keep=0) and the caller must not emit a
    /// trailing Pop. Restores `keep_result` afterwards.
    fn emit_discard(&mut self, e: &Expr) -> Result<(), CompileError> {
        let saved = self.keep_result;
        self.keep_result = false;
        let r = self.emit_expr(e);
        self.keep_result = saved;
        r
    }

    /// Emit an expression in statement (value-discarded) position. Assignments,
    /// inc/dec, and calls are keep-aware: they leave nothing, so no Pop.
    /// Everything else still pushes its value, so the trailing Pop stays.
    fn emit_stmt_expr(&mut self, e: &Expr) -> Result<(), CompileError> {
        // Optional chains always push their value (their short-circuit paths
        // produce undefined at the same stack position), so they cannot use
        // the keep=0 call variants â€” the statement discards with a Pop.
        if Self::is_optional_chain(e) {
            self.emit_expr(e)?;
            self.program.emit_op(Opcode::Pop);
            return Ok(());
        }
        match e {
            Expr::Assign { .. } | Expr::IncDec { .. } | Expr::Call { .. } | Expr::Sequence(..) => {
                self.emit_discard(e)
            }
            _ => {
                self.emit_expr(e)?;
                self.program.emit_op(Opcode::Pop);
                Ok(())
            }
        }
    }

    /// Emit `delete target` per JS semantics: a property/index expression
    /// deletes the member (evaluating the reference first, exactly once); a
    /// plain identifier cannot be deleted (false); any other expression
    /// evaluates for its side effects and yields true.
    fn emit_delete(&mut self, target: &Expr) -> Result<(), CompileError> {
        // `delete a?.b` is a SyntaxError in JS.
        if Self::is_optional_chain(target) {
            return Err(CompileError::UnexpectedToken(
                "invalid delete target: optional chaining cannot be deleted".to_string(),
            ));
        }
        match target {
            Expr::Paren(inner) => self.emit_delete(inner),
            Expr::Prop { obj, prop, .. } => {
                self.emit_keep(|s| s.emit_expr(obj))?;
                let ci = self.program.add_constant(Value::string(prop.clone()));
                self.program.emit_op(Opcode::LoadConst);
                self.program.emit_u16(ci);
                self.program.emit_op(Opcode::DeleteProp);
                Ok(())
            }
            Expr::Index { obj, index, .. } => {
                self.emit_keep(|s| s.emit_expr(obj))?;
                self.emit_keep(|s| s.emit_expr(index))?;
                self.program.emit_op(Opcode::DeleteIndex);
                Ok(())
            }
            Expr::Ident(name) => {
                // `delete x`: a bound identifier (local, upvalue, declared
                // top-level global, or native) can't be deleted â€” false. An
                // undeclared global reference deletes to true (sloppy JS).
                let bound = self.funcs.last().unwrap().locals.iter().any(|l| l == name)
                    || self.funcs.iter().skip(1).any(|f| f.locals.iter().any(|l| l == name))
                    || self.declared_globals.contains(name)
                    || is_native(name);
                self.program.emit_op(if bound { Opcode::LoadFalse } else { Opcode::LoadTrue });
                Ok(())
            }
            other => {
                // `delete f()`, `delete (5)`, `delete (a ? b : c)` â€¦ â€”
                // evaluate for side effects, the result is true.
                self.emit_stmt_expr(other)?;
                self.program.emit_op(Opcode::LoadTrue);
                Ok(())
            }
        }
    }

    /// Run `emit` with the surrounding keep context forced to true (the value
    /// of a sub-expression is always consumed by its parent).
    fn emit_keep(&mut self, emit: impl FnOnce(&mut Self) -> Result<(), CompileError>) -> Result<(), CompileError> {
        let saved = self.keep_result;
        self.keep_result = true;
        let r = emit(self);
        self.keep_result = saved;
        r
    }

    /// Emit `require('<src>')` as a call that KEEPS its result (the module
    /// object), for `import` statements compiled as require sugar. The
    /// require global is resolved like any identifier; LoadGlobal of a
    /// builtin seeds the native.
    fn emit_require_call(&mut self, src: &str) -> Result<(), CompileError> {
        let call = Expr::Call {
            callee: Box::new(Expr::Ident("require".to_string())),
            args: vec![Elem {
                spread: false,
                hole: false,
                expr: Expr::Str(src.to_string()),
            }],
            optional: false,
        };
        self.emit_keep(|c| c.emit_expr(&call))
    }

    /// Emit a discarded condition (`&&` / `||` or a plain expression) as
    /// jump-with-pop edges instead of materializing a boolean value, for
    /// contexts that test the condition and never need its value: loop
    /// conditions, if/ternary tests. The condition is decomposed so each
    /// operand is evaluated exactly once, in JS order, with untaken operands
    /// never evaluated and every operand's value popped by the edge that
    /// tests it:
    ///
    /// - `a && b`:   `a; JumpIfFalsePop exit; b; JumpIfFalsePop exit`
    /// - `a || b`:   `a; JumpIfTruePop body; b; JumpIfFalsePop exit`
    /// - plain `e`:  `e; JumpIfFalsePop exit`
    ///
    /// `exit` collects the falsy-edge jumps (patched to the loop exit / else
    /// path after the body) and `body` the truthy-edge jumps (patched to the
    /// body start). Fall-through means "continue evaluating the next piece of
    /// the condition"; the innermost fall-through is the body itself.
    ///
    /// Compounds nest correctly on either side: `(a || b) && c` emits
    /// `a; JumpIfTruePop [skip b]; b; JumpIfFalsePop exit; c; JumpIfFalsePop
    /// exit` (a truthy `a` still tests `c`), and `a && b || c` emits
    /// `a; JumpIfFalsePop [skip b]; b; JumpIfTruePop body; c; JumpIfFalsePop
    /// exit`.
    fn emit_cond(
        &mut self,
        e: &Expr,
        exit: &mut Vec<usize>,
        body: &mut Vec<usize>,
    ) -> Result<(), CompileError> {
        match e {
            Expr::Paren(inner) => self.emit_cond(inner, exit, body)?,
            Expr::Bin(op, l, r) if *op == "&&" => {
                self.emit_cond_false(l, exit)?;
                self.emit_cond(r, exit, body)?;
            }
            Expr::Bin(op, l, r) if *op == "||" => {
                self.emit_cond_true(l, body)?;
                self.emit_cond(r, exit, body)?;
            }
            _ => {
                self.emit_keep(|c| c.emit_expr(e))?;
                exit.push(self.emit_jump(Opcode::JumpIfFalsePop));
            }
        }
        Ok(())
    }

    /// Emit `e` so a falsy result jumps to `exit` and a truthy result falls
    /// through (continuing with the next piece of the surrounding condition).
    fn emit_cond_false(&mut self, e: &Expr, exit: &mut Vec<usize>) -> Result<(), CompileError> {
        match e {
            Expr::Bin(op, l, r) if *op == "&&" => {
                self.emit_cond_false(l, exit)?;
                self.emit_cond_false(r, exit)?;
            }
            Expr::Bin(op, l, r) if *op == "||" => {
                // A truthy `l` makes the whole `||` truthy, so it must skip
                // past `r` (a forward jump, patched after `r` is emitted).
                let mut skip = Vec::new();
                self.emit_cond_true(l, &mut skip)?;
                self.emit_cond_false(r, exit)?;
                let after = self.program.bytecode.len();
                for j in skip { self.patch_jump_to(j, after); }
            }
            _ => {
                self.emit_keep(|c| c.emit_expr(e))?;
                exit.push(self.emit_jump(Opcode::JumpIfFalsePop));
            }
        }
        Ok(())
    }

    /// Emit `e` so a truthy result jumps to `body` and a falsy result falls
    /// through (continuing with the next piece of the surrounding condition).
    fn emit_cond_true(&mut self, e: &Expr, body: &mut Vec<usize>) -> Result<(), CompileError> {
        match e {
            Expr::Bin(op, l, r) if *op == "||" => {
                self.emit_cond_true(l, body)?;
                self.emit_cond_true(r, body)?;
            }
            Expr::Bin(op, l, r) if *op == "&&" => {
                // A falsy `l` makes the whole `&&` falsy, so it must skip
                // past `r` (a forward jump, patched after `r` is emitted).
                let mut skip = Vec::new();
                self.emit_cond_false(l, &mut skip)?;
                self.emit_cond_true(r, body)?;
                let after = self.program.bytecode.len();
                for j in skip { self.patch_jump_to(j, after); }
            }
            _ => {
                self.emit_keep(|c| c.emit_expr(e))?;
                body.push(self.emit_jump(Opcode::JumpIfTruePop));
            }
        }
        Ok(())
    }

    /// Collect variable names declared directly within `stmt`'s body (not
    /// descending into nested functions, which allocate their own locals).
    fn collect_declared(stmt: &Stmt, out: &mut Vec<String>) {
        match stmt {
            Stmt::Loc { stmt, .. } => Self::collect_declared(stmt, out),
            Stmt::VarDecl { decls } => {
                for (pat, _) in decls {
                    Self::collect_pat_names(pat, out);
                }
            }
            Stmt::Block(ss) => {
                for s in ss {
                    Self::collect_declared(s, out);
                }
            }
            Stmt::If { then, els, .. } => {
                Self::collect_declared(then, out);
                if let Some(e) = els {
                    Self::collect_declared(e, out);
                }
            }
            Stmt::While { body, .. } => Self::collect_declared(body, out),
            Stmt::For { init, body, .. } => {
                if let Some(i) = init {
                    Self::collect_declared(i, out);
                }
                Self::collect_declared(body, out);
            }
            Stmt::ForOf { body, .. } | Stmt::ForIn { body, .. } => {
                Self::collect_declared(body, out)
            }
            Stmt::Try { body, catch, finally, .. } => {
                Self::collect_declared(body, out);
                if let Some((_, c)) = catch {
                    Self::collect_declared(c, out);
                }
                if let Some(f) = finally {
                    Self::collect_declared(f, out);
                }
            }
            Stmt::Switch { cases, .. } => {
                for c in cases {
                    for s in &c.body {
                        Self::collect_declared(s, out);
                    }
                }
            }
            // FnDecl / Lambda bodies and imports declare nothing here.
            _ => {}
        }
    }

    /// Collect the bound names of a destructuring pattern.
    fn collect_pat_names(pat: &Pat, out: &mut Vec<String>) {
        match pat {
            Pat::Bind(n) => out.push(n.clone()),
            Pat::Object(fields) => {
                for f in fields {
                    match f {
                        ObjPatElem::Key(_, sub) | ObjPatElem::Computed(_, sub) | ObjPatElem::Rest(sub) => {
                            Self::collect_pat_names(sub, out);
                        }
                    }
                }
            }
            Pat::Array(elems) => {
                for el in elems {
                    match el {
                        PatElem::Bind(sub) | PatElem::Rest(sub) => {
                            Self::collect_pat_names(sub, out);
                        }
                        PatElem::Hole => {}
                    }
                }
            }
        }
    }

    /// Pre-allocate slots for a `finally` body's declared variables (once), so
    /// every re-emission of the body writes the same slot. At the REPL top
    /// level declarations are globals (name-keyed), so no allocation is needed.
    fn ensure_finally_locals(&mut self, f: &Stmt) {
        if self.top_globals && self.funcs.len() == 1 {
            return;
        }
        let mut names = Vec::new();
        Self::collect_declared(f, &mut names);
        for name in names {
            if !self.finally_locals.contains_key(&name) {
                let s = self.funcs.last_mut().unwrap().locals.len() as u8;
                self.funcs.last_mut().unwrap().locals.push(name.clone());
                self.finally_locals.insert(name, s);
            }
        }
    }

    /// Emit a `finally` body (re-entrant: used for the normal path, the
    /// handler path, and every exit site).
    fn emit_finally(&mut self, f: &Stmt) -> Result<(), CompileError> {
        self.ensure_finally_locals(f);
        self.emit_stmt(f)
    }

    fn fresh_name(&mut self, base: &str) -> String {
        self.fresh_counter += 1;
        format!("${}{}", base, self.fresh_counter)
    }

    /// Allocate a hidden compiler-generated local slot (for temporaries such
    /// as the object/index of an increment target).
    fn fresh_local(&mut self) -> u8 {
        let s = self.funcs.last_mut().unwrap().locals.len() as u8;
        let name = self.fresh_name("t");
        self.funcs.last_mut().unwrap().locals.push(name);
        s
    }

    fn resolve_global(&mut self, name: &str) -> u16 {
        if let Some(i) = self.program.globals.iter().position(|g| g == name) {
            i as u16
        } else {
            let i = self.program.globals.len() as u16;
            self.program.globals.push(name.to_string());
            i
        }
    }

    fn global_store_index(&mut self, name: &str) -> Result<u16, CompileError> {
        if is_native(name) {
            return Err(CompileError::CannotShadowBuiltin(name.to_string()));
        }
        Ok(self.resolve_global(name))
    }

    /// Resolve a variable to a local slot, an upvalue index, or a global slot.
    fn resolve(&mut self, name: &str) -> Resolved {
        if let Some(s) = self.funcs.last().unwrap().locals.iter().rposition(|l| l == name) {
            return Resolved::Local(s as u8);
        }
        if let Some(u) = self.resolve_upvalue(name) {
            return Resolved::Upvalue(u);
        }
        Resolved::Global(self.resolve_global(name))
    }

    /// Find `name` in an enclosing function's locals and build (or reuse) the
    /// upvalue chain from the current function up to the owner. Returns the
    /// upvalue index in the current function's context.
    ///
    /// `funcs` is a stack with the outermost (main) context at index 0 and the
    /// innermost (current) function at the last index.
    fn resolve_upvalue(&mut self, name: &str) -> Option<u8> {
        let n = self.funcs.len();
        for owner in (0..n - 1).rev() {
            if let Some(slot) = self.funcs[owner].locals.iter().rposition(|l| l == name) {
                let mut target_idx = slot as u8;
                let mut target_is_local = true;
                for level in (owner + 1)..n {
                    let entry = if target_is_local {
                        UpvalueRef { name: name.to_string(), kind: UpvalueKind::Local { slot: target_idx } }
                    } else {
                        UpvalueRef { name: name.to_string(), kind: UpvalueKind::Upvalue { index: target_idx } }
                    };
                    let idx = match self.funcs[level].upvalues.iter().position(|u| u.name == name) {
                        Some(i) => i as u8,
                        None => {
                            self.funcs[level].upvalues.push(entry);
                            (self.funcs[level].upvalues.len() - 1) as u8
                        }
                    };
                    if level == n - 1 {
                        return Some(idx);
                    }
                    target_idx = idx;
                    target_is_local = false;
                }
            }
        }
        None
    }

    /// Where an arrow's hidden `\0this`/`\0arguments` capture comes from.
    /// Scanning the enclosing functions innermost-first: the first arrow
    /// that already holds the hidden capture provides it (a nested arrow
    /// re-captures that cell, so a chain of arrows shares one value no
    /// matter how the inner arrows are later called); the first non-arrow
    /// function binds the value itself, so the capture reads it from that
    /// frame at creation time (Lexical). With no enclosing function at all
    /// (top level), the capture is still Lexical: LoadThis/LoadArguments
    /// yield undefined, matching a top-level ESM binding.
    fn lexical_capture_kind(&self, hidden: &str) -> UpvalueKind {
        for f in self.funcs.iter().rev() {
            if f.is_arrow {
                if let Some(i) = f.upvalues.iter().position(|u| u.name == hidden) {
                    return UpvalueKind::Upvalue { index: i as u8 };
                }
                // An arrow without this hidden capture does not bind the
                // name, so it passes through lexically â€” keep scanning.
            } else {
                return UpvalueKind::Lexical;
            }
        }
        UpvalueKind::Lexical
    }

    /// Index of the current function's hidden `\0this`/`\0arguments` capture
    /// (added to the FuncCtx when the arrow was pushed). `None` only when the
    /// body never referenced the name, which the emission path guarantees
    /// cannot be reached for a reference that exists.
    fn arrow_hidden_index(&self, hidden: &str) -> Option<u8> {
        self.funcs
            .last()
            .and_then(|f| f.upvalues.iter().position(|u| u.name == hidden))
            .map(|i| i as u8)
    }

    /// `target &&= / ||= / ??= value` â€” short-circuit assignment. The target's
    /// reference (receiver, optional index) is evaluated exactly once and
    /// stashed in temps; the current value is read and tested, and only when
    /// the test fails (falsy / truthy / nullish) is the RHS evaluated and
    /// written. The expression's value is the old value on the short-circuit
    /// path, the new value otherwise â€” matching JS.
    fn emit_logical_assign(
        &mut self,
        op: &str,
        target: &Expr,
        value: &Expr,
        keep: bool,
    ) -> Result<(), CompileError> {
        let (test, nullish) = match op {
            "&&=" => (Opcode::JumpIfFalse, false),
            "||=" => (Opcode::JumpIfTrue, false),
            _ => (Opcode::JumpIfNullish, true),
        };
        let store = match target {
            Expr::Ident(name) => match self.resolve(name) {
                Resolved::Local(s) => LStore::Local(s),
                Resolved::Upvalue(i) => LStore::Upvalue(i),
                Resolved::Global(i) => LStore::Global(i),
            },
            Expr::Prop { obj, prop, .. } => {
                let t = self.fresh_local();
                self.emit_keep(|c| c.emit_expr(obj))?;
                self.program.emit_op(Opcode::StoreLocal);
                self.program.emit_u8(t);
                let pi = self.program.add_constant(Value::string(prop.clone()));
                LStore::Prop(pi, t)
            }
            Expr::Index { obj, index, .. } => {
                let to = self.fresh_local();
                let ti = self.fresh_local();
                self.emit_keep(|c| c.emit_expr(obj))?;
                self.program.emit_op(Opcode::StoreLocal);
                self.program.emit_u8(to);
                self.emit_keep(|c| c.emit_expr(index))?;
                self.program.emit_op(Opcode::StoreLocal);
                self.program.emit_u8(ti);
                LStore::Index(to, ti)
            }
            // Invalid targets (calls, literals, â€¦) fall through to the main
            // assignment arm, which reports the error.
            _ => return Ok(()),
        };
        // Read the current value (through the stash for members).
        match &store {
            LStore::Local(s) => {
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(*s);
            }
            LStore::Upvalue(i) => {
                self.program.emit_op(Opcode::LoadUpvalue);
                self.program.emit_u8(*i);
            }
            LStore::Global(i) => {
                self.program.emit_op(Opcode::LoadGlobal);
                self.program.emit_u16(*i);
            }
            // GetProperty reads its key from the inline constant â€” it does
            // NOT consume a key from the stack (only the object), so no
            // LoadConst here; that would leave a stray key on the stack.
            LStore::Prop(pi, t) => {
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(*t);
                self.program.emit_op(Opcode::GetProperty);
                self.program.emit_u16(*pi);
            }
            LStore::Index(to, ti) => {
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(*to);
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(*ti);
                self.program.emit_op(Opcode::GetIndex);
            }
        }
        self.program.emit_op(Opcode::Dup);
        let j = self.emit_jump(test);
        if nullish {
            // JumpIfNullish consumes the dup, so the non-nullish path (which
            // already holds the old value as the result) must jump over the
            // write; the nullish path replaces the old value.
            let end = self.emit_jump(Opcode::Jump);
            let pop_at = self.program.bytecode.len();
            self.program.emit_op(Opcode::Pop);
            self.emit_value_store(&store, value, keep)?;
            self.patch_jump_to(j, pop_at);
            self.patch_jump(end);
        } else {
            // JumpIfFalse/JumpIfTrue keep the old value and jump straight to
            // the end; the fall-through path writes the new value.
            self.program.emit_op(Opcode::Pop);
            self.emit_value_store(&store, value, keep)?;
            self.patch_jump(j);
        }
        Ok(())
    }

    /// Tail of a logical assignment: evaluate the RHS, keep it if the result
    /// is consumed, and store it through `store`.
    fn emit_value_store(
        &mut self,
        store: &LStore,
        value: &Expr,
        keep: bool,
    ) -> Result<(), CompileError> {
        self.emit_keep(|c| c.emit_expr(value))?;
        if keep {
            self.program.emit_op(Opcode::Dup);
        }
        match store {
            LStore::Local(s) => {
                self.program.emit_op(Opcode::StoreLocal);
                self.program.emit_u8(*s);
            }
            LStore::Upvalue(i) => {
                self.program.emit_op(Opcode::StoreUpvalue);
                self.program.emit_u8(*i);
            }
            LStore::Global(i) => {
                self.program.emit_op(Opcode::StoreGlobal);
                self.program.emit_u16(*i);
            }
            LStore::Prop(pi, t) => {
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(*t);
                self.program.emit_op(Opcode::LoadConst);
                self.program.emit_u16(*pi);
                self.program.emit_op(Opcode::SetProperty);
            }
            LStore::Index(to, ti) => {
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(*to);
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(*ti);
                self.program.emit_op(Opcode::SetIndex);
            }
        }
        Ok(())
    }

    fn emit_captures(&mut self, ctx: &FuncCtx) {
        for u in &ctx.upvalues {
            match &u.kind {
                UpvalueKind::Local { slot } => {
                    self.program.emit_op(Opcode::CaptureLocal);
                    self.program.emit_u8(*slot);
                }
                UpvalueKind::Upvalue { index } => {
                    self.program.emit_op(Opcode::CaptureUpvalue);
                    self.program.emit_u8(*index);
                }
                UpvalueKind::Lexical => {
                    // Capture the enclosing frame's `this` or `arguments` at
                    // NewClosure time; WrapCell freezes it in a cell (the
                    // cell, not the raw value, is what NewClosure pops).
                    self.program.emit_op(if u.name == "\u{0}this" {
                        Opcode::LoadThis
                    } else {
                        Opcode::LoadArguments
                    });
                    self.program.emit_op(Opcode::WrapCell);
                }
            }
        }
    }

    fn emit_jump(&mut self, op: Opcode) -> usize {
        self.program.emit_op(op);
        let p = self.program.bytecode.len();
        self.program.emit_u32(0);
        p
    }

    fn patch_jump(&mut self, off: usize) {
        self.patch_jump_to(off, self.program.bytecode.len());
    }

    fn patch_jump_to(&mut self, off: usize, target: usize) {
        let b = (target as u32).to_be_bytes();
        self.program.bytecode[off] = b[0];
        self.program.bytecode[off + 1] = b[1];
        self.program.bytecode[off + 2] = b[2];
        self.program.bytecode[off + 3] = b[3];
    }

    /// Convert an object/array *literal* that appears as the target of `=`
    /// into a destructuring pattern (`[x, y] = arr`, `({ a, b } = obj)`).
    fn expr_to_pattern(e: &Expr) -> Result<Pat, CompileError> {
        match e {
            Expr::Object(fields) => {
                let mut out = Vec::new();
                for f in fields {
                    let (key, value) = match f {
                        ObjElem::Pair(k, v) => (k, v),
                        // Loud errors: computed keys/spreads are legal in JS
                        // assignment targets but unsupported here â€” better a
                        // compile error than a silent misparse.
                        ObjElem::Computed(..) | ObjElem::Spread(..) | ObjElem::Getter(..) | ObjElem::Setter(..) => {
                            return Err(CompileError::UnexpectedToken(
                                "computed keys and spreads are not supported in destructuring patterns".to_string(),
                            ));
                        }
                    };
                    let sub = match value {
                        Expr::Ident(name) => Pat::Bind(name.clone()),
                        Expr::Object(_) | Expr::Array(_) => Self::expr_to_pattern(value)?,
                        _ => {
                            return Err(CompileError::UnexpectedToken(
                                "invalid destructuring target".to_string(),
                            ));
                        }
                    };
                    out.push(ObjPatElem::Key(key.clone(), sub));
                }
                Ok(Pat::Object(out))
            }
            Expr::Array(elems) => {
                let mut out = Vec::new();
                for el in elems {
                    if el.hole {
                        out.push(PatElem::Hole);
                        continue;
                    }
                    if el.spread {
                        // `...rest` in an assignment target â€” must be last.
                        let sub = match &el.expr {
                            Expr::Ident(name) => Pat::Bind(name.clone()),
                            Expr::Object(_) | Expr::Array(_) => Self::expr_to_pattern(&el.expr)?,
                            _ => {
                                return Err(CompileError::UnexpectedToken(
                                    "invalid destructuring target".to_string(),
                                ));
                            }
                        };
                        out.push(PatElem::Rest(sub));
                        if out.len() < elems.len() {
                            return Err(CompileError::UnexpectedToken(
                                "rest element must be last in array pattern".to_string(),
                            ));
                        }
                        continue;
                    }
                    match &el.expr {
                        Expr::Ident(name) => out.push(PatElem::Bind(Pat::Bind(name.clone()))),
                        Expr::Object(_) | Expr::Array(_) => {
                            out.push(PatElem::Bind(Self::expr_to_pattern(&el.expr)?))
                        }
                        _ => {
                            return Err(CompileError::UnexpectedToken(
                                "invalid destructuring target".to_string(),
                            ));
                        }
                    }
                }
                Ok(Pat::Array(out))
            }
            _ => Err(CompileError::UnexpectedToken(
                "invalid destructuring target".to_string(),
            )),
        }
    }

    /// Fuse the string-accumulator assignment `s = s + X` / `s += X` (the
    /// target is a local `s`) into a single dispatch: read the local, add the
    /// RHS (a folded string constant, another local, or the value pushed
    /// after a lhs snapshot â€” JS evaluation order), store back, and keep the
    /// result when the assignment's value is consumed. Returns true when the
    /// shape matched and the fused code was emitted.
    fn emit_append_assignment(
        &mut self,
        s: u8,
        op: &str,
        value: &Expr,
        name: &str,
        keep: bool,
    ) -> Result<bool, CompileError> {
        // Match the shape: `+=` takes the value as the RHS; `=` requires a
        // binary `+` whose left operand is the target itself.
        let rhs = match op {
            "+=" => Some(value),
            "=" => match value {
                Expr::Bin("+", left, right) => {
                    let mut l = left.as_ref();
                    while let Expr::Paren(inner) = l {
                        l = inner.as_ref();
                    }
                    if let Expr::Ident(ln) = l {
                        if ln == name {
                            Some(right.as_ref())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                _ => None,
            },
            _ => None,
        };
        let rhs = match rhs {
            Some(r) => r,
            None => return Ok(false),
        };
        let mut r = rhs;
        while let Expr::Paren(inner) = r {
            r = inner.as_ref();
        }
        match r {
            // String literal leaf: fold into the opcode (the accumulator
            // case â€” one dispatch for the whole append).
            Expr::Str(sl) => {
                let ci = self.program.add_constant(Value::string(sl.clone()));
                self.program.emit_op(Opcode::AppendStringConst);
                self.program.emit_u8(s);
                self.program.emit_u16(ci);
                self.program.emit_u8(keep as u8);
            }
            // Another local: read both inside the opcode.
            Expr::Ident(other) => match self.resolve(other) {
                Resolved::Local(slot) => {
                    self.program.emit_op(Opcode::AppendStringLocal);
                    self.program.emit_u8(s);
                    self.program.emit_u8(slot);
                    self.program.emit_u8(keep as u8);
                }
                _ => return Ok(false),
            },
            // General RHS: take the lhs snapshot first (the accumulator is
            // read before the RHS evaluates, per JS), then the RHS, then the
            // fused pop-store.
            _ => {
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(s);
                self.emit_keep(|c| c.emit_expr(r))?;
                self.program.emit_op(Opcode::AppendStringPop);
                self.program.emit_u8(s);
                self.program.emit_u8(keep as u8);
            }
        }
        Ok(true)
    }

    /// Store the value on top of the stack into `name`, creating a local (or
    /// REPL global) if needed â€” the declaration path.
    fn store_declared(&mut self, name: &str) -> Result<(), CompileError> {
        if is_native(name) {
            return Err(CompileError::CannotShadowBuiltin(name.to_string()));
        }
        if self.top_globals && self.funcs.len() == 1 {
            let i = self.global_store_index(name)?;
            self.program.emit_op(Opcode::StoreGlobal);
            self.program.emit_u16(i);
            self.declared_globals.insert(name.to_string());
        } else if let Some(&s) = self.finally_locals.get(name) {
            // Re-emitted finally body: reuse the pre-allocated slot.
            self.program.emit_op(Opcode::StoreLocal);
            self.program.emit_u8(s);
        } else {
            self.funcs.last_mut().unwrap().locals.push(name.to_string());
            let s = (self.funcs.last().unwrap().locals.len() - 1) as u8;
            self.program.emit_op(Opcode::StoreLocal);
            self.program.emit_u8(s);
        }
        Ok(())
    }

    /// Store the value on top of the stack into `name`, resolving to an
    /// existing local/upvalue/global â€” the assignment path.
    fn store_assign(&mut self, name: &str) -> Result<(), CompileError> {
        match self.resolve(name) {
            Resolved::Local(s) => {
                self.program.emit_op(Opcode::StoreLocal);
                self.program.emit_u8(s);
            }
            Resolved::Upvalue(u) => {
                self.program.emit_op(Opcode::StoreUpvalue);
                self.program.emit_u8(u);
            }
            Resolved::Global(g) => {
                if is_native(name) {
                    return Err(CompileError::CannotShadowBuiltin(name.to_string()));
                }
                self.program.emit_op(Opcode::StoreGlobal);
                self.program.emit_u16(g);
            }
        }
        Ok(())
    }

    /// Destructure the value stashed in `src_slot` according to `pat`, storing
    /// each bound name. Nested patterns stash their sub-value in a fresh local
    /// before recursing.
    fn emit_pattern_store(&mut self, pat: &Pat, src_slot: u8, mode: PatStoreMode) -> Result<(), CompileError> {
        match pat {
            Pat::Bind(name) => {
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(src_slot);
                match mode {
                    PatStoreMode::Declare => self.store_declared(name)?,
                    PatStoreMode::Assign => self.store_assign(name)?,
                }
            }
            Pat::Object(elems) => {
                // Computed-key VALUES are stashed as they are evaluated so the
                // (final) rest element can delete those keys from its copy
                // without re-evaluating the key expressions.
                let mut computed_slots: Vec<u8> = Vec::new();
                for el in elems {
                    match el {
                        ObjPatElem::Key(key, sub) => {
                            let ki = self.program.add_constant(Value::string(key.clone()));
                            self.program.emit_op(Opcode::LoadLocal);
                            self.program.emit_u8(src_slot);
                            self.program.emit_op(Opcode::GetProperty);
                            self.program.emit_u16(ki);
                            let tmp = self.fresh_local();
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(tmp);
                            self.emit_pattern_store(sub, tmp, mode)?;
                        }
                        ObjPatElem::Computed(key_expr, sub) => {
                            // Evaluate the key ONCE, stash it, then read the
                            // property with the dynamic key.
                            self.emit_expr(key_expr)?;
                            let key_tmp = self.fresh_local();
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(key_tmp);
                            self.program.emit_op(Opcode::LoadLocal);
                            self.program.emit_u8(src_slot);
                            self.program.emit_op(Opcode::LoadLocal);
                            self.program.emit_u8(key_tmp);
                            self.program.emit_op(Opcode::GetIndex);
                            let tmp = self.fresh_local();
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(tmp);
                            self.emit_pattern_store(sub, tmp, mode)?;
                            computed_slots.push(key_tmp);
                        }
                        ObjPatElem::Rest(sub) => {
                            // `...rest`: copy the source's own enumerable
                            // properties into a fresh object, then delete the
                            // already-destructured keys (constant AND computed)
                            // so they stay out of the rest, like JS.
                            self.program.emit_op(Opcode::LoadLocal);
                            self.program.emit_u8(src_slot);
                            self.program.emit_op(Opcode::MakeObject);
                            self.program.emit_u16(1);
                            self.program.emit_u16(1);
                            let rest_tmp = self.fresh_local();
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(rest_tmp);
                            for key in elems.iter().filter_map(|e| match e {
                                ObjPatElem::Key(k, _) => Some(k),
                                _ => None,
                            }) {
                                let ki = self.program.add_constant(Value::string(key.clone()));
                                self.program.emit_op(Opcode::LoadLocal);
                                self.program.emit_u8(rest_tmp);
                                self.program.emit_op(Opcode::LoadConst);
                                self.program.emit_u16(ki);
                                self.program.emit_op(Opcode::DeleteProp);
                                self.program.emit_op(Opcode::Pop);
                            }
                            for ks in &computed_slots {
                                self.program.emit_op(Opcode::LoadLocal);
                                self.program.emit_u8(rest_tmp);
                                self.program.emit_op(Opcode::LoadLocal);
                                self.program.emit_u8(*ks);
                                self.program.emit_op(Opcode::DeleteIndex);
                                self.program.emit_op(Opcode::Pop);
                            }
                            self.emit_pattern_store(sub, rest_tmp, mode)?;
                        }
                    }
                }
            }
            Pat::Array(elems) => {
                let mut i = 0usize;
                for el in elems {
                    match el {
                        PatElem::Hole => i += 1,
                        PatElem::Bind(sub) => {
                            self.program.emit_op(Opcode::LoadLocal);
                            self.program.emit_u8(src_slot);
                            self.program.emit_op(Opcode::LoadInt);
                            self.program.emit_u32(i as u32);
                            self.program.emit_op(Opcode::GetIndex);
                            let tmp = self.fresh_local();
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(tmp);
                            self.emit_pattern_store(sub, tmp, mode)?;
                            i += 1;
                        }
                        PatElem::Rest(sub) => {
                            // Collect the source's remaining elements from `i`
                            // onward into a fresh array, then bind it.
                            self.program.emit_op(Opcode::LoadLocal);
                            self.program.emit_u8(src_slot);
                            self.program.emit_op(Opcode::ArraySlice);
                            self.program.emit_u8(i as u8);
                            let tmp = self.fresh_local();
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(tmp);
                            self.emit_pattern_store(sub, tmp, mode)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The local slot when `e` is a plain identifier resolving to a local
    /// (used by the property/index superinstruction fusions).
    fn local_slot(&mut self, e: &Expr) -> Option<u8> {
        if let Expr::Ident(name) = e {
            if let Resolved::Local(s) = self.resolve(name) {
                return Some(s);
            }
        }
        None
    }

    /// `local + int` / `local - int` / â€¦ â€” the BinLocalInt shape (`j + 1`).
    /// Returns (slot, arith code, immediate). Only pure arithmetic on a
    /// local and a compile-time int: fusing it into an index write is
    /// unobservable (no side effects in the operands).
    fn local_plus_int(&mut self, e: &Expr) -> Option<(u8, u8, i32)> {
        if let Expr::Bin(op, l, r) = e {
            if let Expr::Int(iv) = r.as_ref() {
                if let Ok(imm) = i32::try_from(*iv) {
                    if let Some(s) = self.local_slot(l) {
                        if let Some(ar) = arith_code(op) {
                            return Some((s, ar, imm));
                        }
                    }
                }
            }
        }
        None
    }

    /// Find the index of a forced class upvalue (`\0home` â€” the parent class
    /// or the class's prototype) in the current function's capture list.
    /// Absent means `super` was used somewhere it is not legal (outside a
    /// derived-class method/constructor).
    fn find_upvalue(&mut self, name: &str) -> Result<u8, CompileError> {
        self.funcs
            .last()
            .and_then(|f| f.upvalues.iter().position(|u| u.name == name))
            .map(|i| i as u8)
            .ok_or_else(|| {
                CompileError::UnexpectedToken("super used outside a class method".to_string())
            })
    }

    /// Emit one class method as a closure â€” mirrors the Lambda emission but
    /// starts the function's upvalue list with the forced `\0home` capture
    /// (the class's prototype for instance methods, the parent class for the
    /// constructor of a derived class) so `super` can resolve it by name.
    /// The `home` slot lives in the enclosing frame, so the normal
    /// CaptureLocal machinery materializes it as a cell at NewClosure time.
    fn emit_method(&mut self, m: &MethodDef, home: Option<UpvalueKind>) -> Result<(), CompileError> {
        let j = self.emit_jump(Opcode::Jump);
        let start = self.program.bytecode.len();
        self.program.record_function_name(start, m.name.clone());
        let mut upvalues = Vec::new();
        if let Some(kind) = home {
            upvalues.push(UpvalueRef { name: "\u{0}home".to_string(), kind });
        }
        self.funcs.push(FuncCtx {
            locals: m.params.names(),
            upvalues,
            is_async: m.is_async,
            uses_arguments: false,
            is_arrow: false,
        });
        if let Some(r) = m.params.rest {
            self.program.emit_op(Opcode::MakeRestArray);
            self.program.emit_u8(r as u8);
            self.program.emit_u8(r as u8);
        }
        if m.is_async {
            self.funcs.last_mut().unwrap().locals.push("\u{0}promise".to_string());
            let ps = (self.funcs.last().unwrap().locals.len() - 1) as u8;
            self.program.emit_op(Opcode::NewPromise);
            self.program.emit_u8(ps);
        }
        if m.is_generator {
            self.program.emit_op(Opcode::CreateGenerator);
        }
        let saved_loops = std::mem::take(&mut self.loops);
        let saved_trys = std::mem::take(&mut self.trys);
        let saved_finally = std::mem::take(&mut self.finally_locals);
        let saved_labels = std::mem::take(&mut self.labels);
        let saved_pending = self.pending_label.take();
        self.emit_stmt(&m.body)?;
        self.loops = saved_loops;
        self.trys = saved_trys;
        self.finally_locals = saved_finally;
        self.labels = saved_labels;
        self.pending_label = saved_pending;
        self.program.emit_op(Opcode::LoadUndefined);
        self.program.emit_op(Opcode::Return);
        let ctx = self.funcs.pop().unwrap();
        self.patch_jump(j);
        let ci = self.program.add_constant(Value::number(start as f64));
        // Fixed params only: `names` includes the rest param (if any), but
        // the rest slot is materialized by MakeRestArray from the args beyond
        // the fixed params â€” the missing-arg fill must stop before it (a
        // filled rest slot would become a spurious array element).
        let pcount = m.params.names().len() as u8 - u8::from(m.params.rest.is_some());
        self.emit_captures(&ctx);
        self.program.emit_op(Opcode::NewClosure);
        self.program.emit_u16(ci);
        self.program.emit_u8(ctx.upvalues.len() as u8);
        self.program.emit_u8(pcount);
        self.program.emit_u8(ctx.uses_arguments as u8);
        Ok(())
    }

    /// Any member/call chain containing a `?.` link. Such a chain must be
    /// emitted atomically: the nullish check short-circuits the ENTIRE
    /// remaining chain (later properties, indices, call arguments â€” none of
    /// them evaluate when the guarded receiver is nullish).
    fn is_optional_chain(e: &Expr) -> bool {
        match e {
            Expr::Paren(inner) => Self::is_optional_chain(inner),
            Expr::Prop { obj, optional, .. } => *optional || Self::is_optional_chain(obj),
            Expr::Index { obj, optional, .. } => *optional || Self::is_optional_chain(obj),
            Expr::Call { callee, optional, .. } => *optional || Self::is_optional_chain(callee),
            _ => false,
        }
    }

    /// Emit `a?.b.c(d)` atomically. The spine is collected (base innermost,
    /// links outward), then emitted with a nullish guard per `?.` link:
    /// `Dup; JumpIfNullish L; Pop` before the link. Every guard jumps to a
    /// short-circuit path AFTER the whole chain, so when the guarded
    /// receiver is nullish the rest of the chain â€” later members AND call
    /// arguments â€” never evaluates, exactly like JS. Each skip path discards
    /// the `pushed` stack values the chain accumulated so far and pushes
    /// undefined.
    ///
    /// Method calls (`o?.m()`) keep the receiver below the callee
    /// (`Dup; GetProperty`), so `this` binds correctly on the non-nullish
    /// path. `pushed` counts the extra receiver slots such links leave.
    fn emit_optional_chain(&mut self, e: &Expr) -> Result<(), CompileError> {
        let mut links: Vec<&Expr> = Vec::new();
        let mut cur = e;
        loop {
            match cur {
                Expr::Paren(inner) => cur = inner,
                Expr::Prop { obj, .. } => {
                    links.push(cur);
                    cur = obj;
                }
                Expr::Index { obj, .. } => {
                    links.push(cur);
                    cur = obj;
                }
                Expr::Call { callee, .. } => {
                    links.push(cur);
                    cur = callee;
                }
                base => {
                    self.emit_expr(base)?;
                    break;
                }
            }
        }
        links.reverse();
        let n = links.len();
        // The chain's net stack footprint so far (1 for the value itself,
        // +1 per method-shaped member link that keeps the receiver below).
        let mut pushed = 1usize;
        let mut skips: Vec<(usize, usize)> = Vec::new();
        for (i, link) in links.iter().enumerate() {
            let optional = match link {
                Expr::Prop { optional, .. }
                | Expr::Index { optional, .. }
                | Expr::Call { optional, .. } => *optional,
                _ => unreachable!(),
            };
            if optional {
                // Test a copy, then fall through with the original: the jump
                // opcode consumes the copy, so the chain value stays on the
                // stack for the link below.
                self.program.emit_op(Opcode::Dup);
                let off = self.emit_jump(Opcode::JumpIfNullish);
                skips.push((off, pushed));
            }
            let is_member = matches!(link, Expr::Prop { .. } | Expr::Index { .. });
            let next_is_call = i + 1 < n && matches!(links[i + 1], Expr::Call { .. });
            let method_shape = is_member && next_is_call;
            match link {
                Expr::Prop { prop, .. } => {
                    let pi = self.program.add_constant(Value::string(prop.clone()));
                    if method_shape {
                        // Leave the receiver below the callee for CallMethod.
                        self.program.emit_op(Opcode::Dup);
                        self.program.emit_op(Opcode::GetProperty);
                        self.program.emit_u16(pi);
                        pushed += 1;
                    } else {
                        self.program.emit_op(Opcode::GetProperty);
                        self.program.emit_u16(pi);
                    }
                }
                Expr::Index { index, .. } => {
                    if method_shape {
                        self.program.emit_op(Opcode::Dup);
                        self.emit_expr(index)?;
                        self.program.emit_op(Opcode::GetIndex);
                        pushed += 1;
                    } else {
                        self.emit_expr(index)?;
                        self.program.emit_op(Opcode::GetIndex);
                    }
                }
                Expr::Call { args, .. } => {
                    // Optional chains always keep the call result (the
                    // short-circuit paths and the outer discard both rely on
                    // the chain leaving exactly one value), so the keep=0
                    // call variants never apply here.
                    let has_spread = args.iter().any(|a| a.spread);
                    let mask = args.iter().enumerate().fold(0u16, |m, (i, a)| {
                        if a.spread { m | (1 << i) } else { m }
                    });
                    let prev_is_member = i > 0
                        && matches!(
                            links[i - 1],
                            Expr::Prop { .. } | Expr::Index { .. }
                        );
                    if prev_is_member {
                        // Method call: the receiver sits one slot below the
                        // frame base (CallMethod reads it as `this`). The
                        // chain already left `[receiver, callee]`; the args
                        // just push on top.
                        for a in args.iter() {
                            self.emit_keep(|c| c.emit_expr(&a.expr))?;
                        }
                        if has_spread {
                            self.program.emit_op(Opcode::CallMethodSpread);
                            self.program.emit_u8(args.len() as u8);
                            self.program.emit_u16(mask);
                        } else {
                            self.program.emit_op(Opcode::CallMethod);
                            self.program.emit_u8(args.len() as u8);
                        }
                    } else {
                        // Plain call: Call pops the callee from the TOP, but
                        // the chain value sits below where the args would
                        // land. Stash it in a temp local, emit the args,
                        // then reload it on top so the layout is the plain
                        // `[args..., callee]` convention.
                        let tmp = self.fresh_local();
                        self.program.emit_op(Opcode::StoreLocal);
                        self.program.emit_u8(tmp);
                        for a in args.iter() {
                            self.emit_keep(|c| c.emit_expr(&a.expr))?;
                        }
                        self.program.emit_op(Opcode::LoadLocal);
                        self.program.emit_u8(tmp);
                        if has_spread {
                            self.program.emit_op(Opcode::CallSpread);
                            self.program.emit_u8(args.len() as u8);
                            self.program.emit_u16(mask);
                        } else {
                            self.program.emit_op(Opcode::Call);
                            self.program.emit_u8(args.len() as u8);
                        }
                    }
                    pushed = 1;
                }
                _ => unreachable!(),
            }
        }
        let end = self.emit_jump(Opcode::Jump);
        let mut skip_jumps = Vec::with_capacity(skips.len());
        for (off, pops) in skips {
            self.patch_jump(off);
            for _ in 0..pops {
                self.program.emit_op(Opcode::Pop);
            }
            self.program.emit_op(Opcode::LoadUndefined);
            // Each skip path is a complete branch: jump over the remaining
            // skip paths to the shared end. Without this, a fired guard
            // falls through into the NEXT skip's pops and over-pops the
            // stack (corrupting locals/values below the chain).
            skip_jumps.push(self.emit_jump(Opcode::Jump));
        }
        self.patch_jump(end);
        // All skip paths and the normal path land on the first instruction
        // after the skip block (patch_jump targets the current end).
        for j in skip_jumps {
            self.patch_jump(j);
        }
        Ok(())
    }

    fn emit_expr(&mut self, expr: &Expr) -> Result<(), CompileError> {
        // Optional chains always emit through the chain walker, even when the
        // top-level node itself isn't optional (`a?.b.c` is a non-optional
        // Prop whose obj spine is optional).
        if Self::is_optional_chain(expr) {
            return self.emit_optional_chain(expr);
        }
        match expr {
            Expr::Paren(inner) => self.emit_expr(inner)?,
            Expr::Sequence(exprs) => {
                // `(a, b, c)`: evaluate each element left-to-right, discard
                // all but the last. In a discard context (keep=false) even
                // the last element goes through the keep-aware stmt path so
                // nothing is left on the stack.
                let last = exprs.len() - 1;
                for (i, e) in exprs.iter().enumerate() {
                    if i == last && self.keep_result {
                        self.emit_expr(e)?;
                    } else {
                        self.emit_stmt_expr(e)?;
                    }
                }
            }
            Expr::Num(n) => {
                let i = self.program.add_constant(Value::number(*n));
                self.program.emit_op(Opcode::LoadConst);
                self.program.emit_u16(i);
            }
            Expr::Int(v) => {
                if *v >= 0 && *v <= u32::MAX as i64 {
                    self.program.emit_op(Opcode::LoadInt);
                    self.program.emit_u32(*v as u32);
                } else {
                    // Large or negative literals don't fit the 32-bit LoadInt
                    // encoding; fall back to the constant pool. JS numbers are
                    // f64, so an integer literal beyond 2^53 rounds to the
                    // nearest double (`9007199254740993` is 9007199254740992
                    // in V8).
                    let c = if v.unsigned_abs() <= (1u64 << 53) {
                        Value::int(*v)
                    } else {
                        Value::number(*v as f64)
                    };
                    let i = self.program.add_constant(c);
                    self.program.emit_op(Opcode::LoadConst);
                    self.program.emit_u16(i);
                }
            }
            Expr::Str(s) => {
                let i = self.program.add_constant(Value::string(s.clone()));
                self.program.emit_op(Opcode::LoadConst);
                self.program.emit_u16(i);
            }
            Expr::Bool(b) => {
                if *b { self.program.emit_op(Opcode::LoadTrue); }
                else { self.program.emit_op(Opcode::LoadFalse); }
            }
            Expr::Null => self.program.emit_op(Opcode::LoadNull),
            Expr::Undef => self.program.emit_op(Opcode::LoadUndefined),
            Expr::Ident(name) => {
                if name == "this" {
                    // Arrows bind `this` lexically: resolve the hidden
                    // `\0this` capture added at arrow-creation time. Regular
                    // functions read the receiver slot of the current
                    // method/new call (undefined for a plain call). Never a
                    // local.
                    if self.funcs.last().unwrap().is_arrow {
                        if let Some(i) = self.arrow_hidden_index("\u{0}this") {
                            self.program.emit_op(Opcode::LoadUpvalue);
                            self.program.emit_u8(i);
                            return Ok(());
                        }
                    }
                    self.program.emit_op(Opcode::LoadThis);
                    return Ok(());
                }
                if name == "arguments" && self.funcs.len() > 1 {
                    // The call's argument list â€” array snapshot of the passed
                    // args. A local/param/upvalue named `arguments` shadows
                    // it (JS allows `function f(arguments) {}`); at top level
                    // it falls through to the normal resolve (undefined).
                    let shadowed = self.funcs.last().unwrap().locals.iter().any(|l| l == name)
                        || self.resolve_upvalue(name).is_some();
                    if !shadowed {
                        if self.funcs.last().unwrap().is_arrow {
                            // Arrows have no own `arguments`; read the hidden
                            // `\0arguments` capture (the enclosing binding).
                            if let Some(i) = self.arrow_hidden_index("\u{0}arguments") {
                                self.program.emit_op(Opcode::LoadUpvalue);
                                self.program.emit_u8(i);
                                return Ok(());
                            }
                        }
                        // Flag the function so the VM snapshots the passed
                        // args into the frame at entry (locals overwrite the
                        // arg slots as the body runs).
                        self.funcs.last_mut().unwrap().uses_arguments = true;
                        self.program.emit_op(Opcode::LoadArguments);
                        return Ok(());
                    }
                }
                match self.resolve(name) {
                    Resolved::Local(s) => {
                        self.program.emit_op(Opcode::LoadLocal);
                        self.program.emit_u8(s);
                    }
                    Resolved::Upvalue(i) => {
                        self.program.emit_op(Opcode::LoadUpvalue);
                        self.program.emit_u8(i);
                    }
                    Resolved::Global(i) => {
                        self.program.emit_op(Opcode::LoadGlobal);
                        self.program.emit_u16(i);
                    }
                }
            }
            Expr::Unary(op, e) => {
                if *op == "void" {
                    // `void expr`: evaluate expr for its side effects, the
                    // result is undefined.
                    self.emit_stmt_expr(e)?;
                    self.program.emit_op(Opcode::LoadUndefined);
                } else if *op == "typeof" {
                    // `typeof undeclaredGlobal` is "undefined", NOT a
                    // ReferenceError. A bare global (through parens) gets the
                    // dedicated TypeOfGlobal opcode; locals/upvalues load
                    // normally (they always exist) and anything else (a
                    // property, a call) loads the value as usual.
                    let mut operand: &Expr = e;
                    while let Expr::Paren(inner) = operand {
                        operand = inner;
                    }
                    if let Expr::Ident(name) = operand {
                        if matches!(self.resolve(name), Resolved::Global(_)) {
                            let i = self.resolve_global(name);
                            self.program.emit_op(Opcode::TypeOfGlobal);
                            self.program.emit_u16(i);
                            return Ok(());
                        }
                    }
                    self.emit_expr(e)?;
                    self.program.emit_op(Opcode::TypeOf);
                } else {
                    self.emit_expr(e)?;
                    match *op {
                        "-" => self.program.emit_op(Opcode::Negate),
                        "~" => self.program.emit_op(Opcode::BitNot),
                        _ => self.program.emit_op(Opcode::Not),
                    }
                }
            }
            Expr::Delete(target) => self.emit_delete(target)?,
            Expr::Bin(op, l, r) => {
                match *op {
                    "&&" => {
                        self.emit_expr(l)?;
                        let j = self.emit_jump(Opcode::JumpIfFalse);
                        self.program.emit_op(Opcode::Pop);
                        self.emit_expr(r)?;
                        self.patch_jump(j);
                    }
                    "||" => {
                        self.emit_expr(l)?;
                        let j = self.emit_jump(Opcode::JumpIfTrue);
                        self.program.emit_op(Opcode::Pop);
                        self.emit_expr(r)?;
                        self.patch_jump(j);
                    }
                    "??" => {
                        // `a ?? b`: the nullish test consumes the dup; the
                        // non-nullish path keeps `a` (already on the stack)
                        // and jumps past, the nullish path replaces it with
                        // `b`. Never evaluates `b` when `a` is not nullish.
                        self.emit_expr(l)?;
                        self.program.emit_op(Opcode::Dup);
                        let j = self.emit_jump(Opcode::JumpIfNullish);
                        let end = self.emit_jump(Opcode::Jump);
                        let pop_at = self.program.bytecode.len();
                        self.program.emit_op(Opcode::Pop);
                        self.emit_expr(r)?;
                        // The nullish path lands on the Pop; the non-nullish
                        // path (which already holds `a`) jumps to the end.
                        self.patch_jump_to(j, pop_at);
                        self.patch_jump(end);
                    }
                    _ => {
                        // Register-ALU fusion: int-arithmetic trees over
                        // locals/literals (`3 * n + 1`, `(lo + hi) % 2`)
                        // collapse into ONE ArithChain dispatch that keeps the
                        // running value in an i64 register. Only chains with
                        // â‰¥ 2 ops fire â€” single ops are already fused below.
                        let term = if self.keep_result { 0x80 } else { 0 };
                        if self.emit_arith_chain(op, l, r, term) {
                            return Ok(());
                        }
                        // Fused superinstructions for the hottest shapes: loop
                        // conditions (`i < 1000` -> CmpLocalInt), local/int
                        // arithmetic (`i * 3` -> BinLocalInt), and local/local
                        // arithmetic + comparison (`i + j`, `lo <= hi`).
                        if let (Expr::Ident(ln), Expr::Int(iv)) = (l.as_ref(), r.as_ref()) {
                            if let Resolved::Local(s) = self.resolve(ln) {
                                if let Ok(imm) = i32::try_from(*iv) {
                                    if let Some(cmp) = cmp_code(op) {
                                        self.program.emit_op(Opcode::CmpLocalInt);
                                        self.program.emit_u8(s);
                                        self.program.emit_i32(imm);
                                        self.program.emit_u8(cmp);
                                        return Ok(());
                                    }
                                    if let Some(ar) = arith_code(op) {
                                        self.program.emit_op(Opcode::BinLocalInt);
                                        self.program.emit_u8(s);
                                        self.program.emit_i32(imm);
                                        self.program.emit_u8(ar);
                                        return Ok(());
                                    }
                                }
                            }
                        }
                        if let (Expr::Ident(ln), Expr::Ident(rn)) = (l.as_ref(), r.as_ref()) {
                            if let (Resolved::Local(a), Resolved::Local(b)) =
                                (self.resolve(ln), self.resolve(rn))
                            {
                                if let Some(ar) = arith_code(op) {
                                    self.program.emit_op(Opcode::BinLocalLocal);
                                    self.program.emit_u8(a);
                                    self.program.emit_u8(b);
                                    self.program.emit_u8(ar);
                                    return Ok(());
                                }
                                if let Some(cmp) = cmp_code(op) {
                                    self.program.emit_op(Opcode::CmpLocalLocal);
                                    self.program.emit_u8(a);
                                    self.program.emit_u8(b);
                                    self.program.emit_u8(cmp);
                                    return Ok(());
                                }
                            }
                        }
                        self.emit_expr(l)?;
                        self.emit_expr(r)?;
                        match *op {
                            "+" => self.program.emit_op(Opcode::Add),
                            "-" => self.program.emit_op(Opcode::Subtract),
                            "*" => self.program.emit_op(Opcode::Multiply),
                            "/" => self.program.emit_op(Opcode::Divide),
                            "%" => self.program.emit_op(Opcode::Modulo),
                            "==" => self.program.emit_op(Opcode::Equal),
                            "!=" => self.program.emit_op(Opcode::NotEqual),
                            "===" => self.program.emit_op(Opcode::StrictEqual),
                            "!==" => self.program.emit_op(Opcode::StrictNotEqual),
                            "<" => self.program.emit_op(Opcode::Less),
                            ">" => self.program.emit_op(Opcode::Greater),
                            "<=" => self.program.emit_op(Opcode::LessEqual),
                            ">=" => self.program.emit_op(Opcode::GreaterEqual),
                            "&" => self.program.emit_op(Opcode::BitAnd),
                            "|" => self.program.emit_op(Opcode::BitOr),
                            "^" => self.program.emit_op(Opcode::BitXor),
                            "<<" => self.program.emit_op(Opcode::Shl),
                            ">>" => self.program.emit_op(Opcode::Shr),
                            ">>>" => self.program.emit_op(Opcode::UShr),
                            "**" => self.program.emit_op(Opcode::Pow),
                            "instanceof" => self.program.emit_op(Opcode::InstanceOf),
                            "in" => self.program.emit_op(Opcode::In),
                            // A silently-dropped operator would corrupt the
                            // stack (operands left pushed). Every operator the
                            // lexer produces must map to an opcode here.
                            other => {
                                return Err(CompileError::UnexpectedToken(
                                    format!("unhandled binary operator '{}'", other),
                                ));
                            }
                        }
                    }
                }
            }
            Expr::Assign { target, op, value } => {
                // Whether this assignment's result is consumed. False only for
                // a top-level statement assignment (see emit_stmt_expr): the
                // fused ops then get keep=0 and leave nothing, so the
                // statement emits no trailing Pop. Operand sub-expressions are
                // always emitted with keep_result=true (their values feed this
                // assignment), hence the emit_keep scoping.
                // `a?.b = v`, `a?.[k] += v` are SyntaxErrors in JS: an
                // optional chain short-circuits to undefined, so it is not a
                // valid assignment reference.
                if Self::is_optional_chain(target) {
                    return Err(CompileError::UnexpectedToken(
                        "invalid assignment target: optional chaining cannot be assigned".to_string(),
                    ));
                }
                let keep = self.keep_result;
                // `(a) = v` is a valid assignment target: unwrap grouping
                // parens before matching the target shape.
                let mut target_ref = target.as_ref();
                while let Expr::Paren(inner) = target_ref { target_ref = inner.as_ref(); }
                if *op == "&&=" || *op == "||=" || *op == "??=" {
                    if let Expr::Ident(name) = target_ref {
                        if self.imported_bindings.contains(name) {
                            return Err(CompileError::AssignToImport(name.clone()));
                        }
                    }
                    return self.emit_logical_assign(op, target_ref, value, keep);
                }
                match target_ref {
                    Expr::Ident(name) => {
                        // Assigning to an `import` binding is an ESM
                        // SyntaxError (the live cell aliases the module).
                        if self.imported_bindings.contains(name) {
                            return Err(CompileError::AssignToImport(name.clone()));
                        }
                        match self.resolve(name) {
                        Resolved::Local(s) => {
                            // `x = <arith chain>` / `x op= <arith chain>`:
                            // fuse the whole read-compute-write into ONE
                            // ArithChain dispatch keeping the int in a
                            // register (store terminal, no stack round-trip).
                            if self.try_emit_chain_assign(s, op, value, keep) {
                                return Ok(());
                            }
                            // `s = s + X` / `s += X`: fuse the whole
                            // read-add-store into one dispatch, keeping the
                            // builder box in the local slot between appends.
                            if self.emit_append_assignment(s, op, value, name, keep)? {
                                return Ok(());
                            }
                            if let Some(ar) = compound_arith(op) {
                                // x op= v: read x, evaluate v (JS order),
                                // then fuse the arith + store into one opcode.
                                self.program.emit_op(Opcode::LoadLocal);
                                self.program.emit_u8(s);
                                self.emit_keep(|c| c.emit_expr(value))?;
                                self.program.emit_op(Opcode::ArithStoreLocal);
                                self.program.emit_u8(s);
                                self.program.emit_u8(ar);
                                self.program.emit_u8(keep as u8);
                            } else {
                                self.emit_keep(|c| c.emit_expr(value))?;
                                if keep {
                                    self.program.emit_op(Opcode::Dup);
                                }
                                self.program.emit_op(Opcode::StoreLocal);
                                self.program.emit_u8(s);
                            }
                        }
                        Resolved::Upvalue(i) => {
                            if let Some(ar) = compound_arith(op) {
                                self.program.emit_op(Opcode::LoadUpvalue);
                                self.program.emit_u8(i);
                                self.emit_keep(|c| c.emit_expr(value))?;
                                self.program.emit_op(Opcode::ArithStoreUpvalue);
                                self.program.emit_u8(i);
                                self.program.emit_u8(ar);
                                self.program.emit_u8(keep as u8);
                            } else {
                                self.emit_keep(|c| c.emit_expr(value))?;
                                if keep {
                                    self.program.emit_op(Opcode::Dup);
                                }
                                self.program.emit_op(Opcode::StoreUpvalue);
                                self.program.emit_u8(i);
                            }
                        }
                        Resolved::Global(i) => {
                            if is_native(name) {
                                return Err(CompileError::CannotShadowBuiltin(name.clone()));
                            }
                            if let Some(opc) = compound_opcode(op) {
                                self.program.emit_op(Opcode::LoadGlobal);
                                self.program.emit_u16(i);
                                self.emit_keep(|c| c.emit_expr(value))?;
                                self.program.emit_op(opc);
                            } else {
                                self.emit_keep(|c| c.emit_expr(value))?;
                            }
                            // The arith (or the plain RHS) leaves the value on
                            // the stack; keep=0 means StoreGlobal consumes it
                            // directly.
                            if keep {
                                self.program.emit_op(Opcode::Dup);
                            }
                            self.program.emit_op(Opcode::StoreGlobal);
                            self.program.emit_u16(i);
                        }
                        }
                    },
                    Expr::Prop { obj, prop, .. } => {
                        let pi = self.program.add_constant(Value::string(prop.clone()));
                        if let Some(ar) = compound_arith(op) {
                            // obj.p += v  =>  obj.p = obj.p + v, fused. The obj
                            // stays on the stack and evaluates exactly once; the
                            // old value is read before the RHS, matching JS
                            // compound-assignment order. A constant RHS
                            // collapses the whole read-modify-write into one
                            // CompoundPropConst; otherwise PeekProperty reads
                            // (leaving obj), the RHS evaluates, and
                            // ArithWriteProp writes the result back.
                            let keep_byte = if keep { 16 } else { 0 };
                            if let Expr::Int(iv) = value.as_ref() {
                                if let Ok(imm) = i32::try_from(*iv) {
                                    self.emit_keep(|c| c.emit_expr(obj))?;
                                    self.program.emit_op(Opcode::CompoundPropConst);
                                    self.program.emit_u8(ar | keep_byte);
                                    self.program.emit_u16(pi);
                                    self.program.emit_i32(imm);
                                    return Ok(());
                                }
                            }
                            self.emit_keep(|c| c.emit_expr(obj))?;
                            self.program.emit_op(Opcode::PeekProperty);
                            self.program.emit_u16(pi);
                            self.emit_keep(|c| c.emit_expr(value))?;
                            self.program.emit_op(Opcode::ArithWriteProp);
                            self.program.emit_u8(ar | keep_byte);
                            self.program.emit_u16(pi);
                            return Ok(());
                        } else {
                            // obj.p = v: evaluate the reference (obj) before
                            // the RHS, per JS. The obj is stashed in a fresh
                            // local so the RHS can run without disturbing it,
                            // then reloaded for the write; the result is the
                            // RHS value.
                            let tmp = self.fresh_local();
                            self.emit_keep(|c| c.emit_expr(obj))?;
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(tmp);
                            self.emit_keep(|c| c.emit_expr(value))?;
                            if keep {
                                self.program.emit_op(Opcode::Dup);
                            }
                            self.program.emit_op(Opcode::LoadLocal);
                            self.program.emit_u8(tmp);
                            self.program.emit_op(Opcode::LoadConst);
                            self.program.emit_u16(pi);
                            self.program.emit_op(Opcode::SetProperty);
                        }
                    }
                    Expr::Index { obj, index, .. } => {
                        if let Some(ar) = compound_arith(op) {
                            // a[i] += v, fused (mirrors the property fusions):
                            // the obj and index stay on the stack and evaluate
                            // exactly once, in JS order; the old value is read
                            // before the RHS. A constant RHS collapses the
                            // read-modify-write into one CompoundIndexConst;
                            // otherwise PeekIndex reads (leaving obj+idx) and
                            // ArithWriteIndex writes back.
                            let keep_byte = if keep { 16 } else { 0 };
                            if let Expr::Int(iv) = value.as_ref() {
                                if let Ok(imm) = i32::try_from(*iv) {
                                    self.emit_keep(|c| c.emit_expr(obj))?;
                                    self.emit_keep(|c| c.emit_expr(index))?;
                                    self.program.emit_op(Opcode::CompoundIndexConst);
                                    self.program.emit_u8(ar | keep_byte);
                                    self.program.emit_i32(imm);
                                    return Ok(());
                                }
                            }
                            self.emit_keep(|c| c.emit_expr(obj))?;
                            self.emit_keep(|c| c.emit_expr(index))?;
                            self.program.emit_op(Opcode::PeekIndex);
                            self.emit_keep(|c| c.emit_expr(value))?;
                            self.program.emit_op(Opcode::ArithWriteIndex);
                            self.program.emit_u8(ar | keep_byte);
                            return Ok(());
                        } else {
                            // a[i] = v: evaluate the reference (obj, then
                            // index) before the RHS, per JS. Both are stashed
                            // in fresh locals so the RHS can run without
                            // disturbing them; the result is the RHS value.
                            //
                            // Fusion (compiler-side, so operand roles are
                            // unambiguous â€” the peephole cannot tell a plain
                            // `arr[i] = v` from the stash path's trailing
                            // reloads): when every operand is a pure local
                            // read with no side effects, skipping the stash
                            // is unobservable and the whole write collapses
                            // into ONE dispatch.
                            //   a[i] = v        -> SetIndexLocalLocal
                            //   a[i] = b[j]     -> SetIndexLocalGetLocal
                            //   a[i + k] = b[j] -> SetIndexLocalPlusIntLocalGetLocal
                            let obj_s = self.local_slot(obj);
                            let idx_s = self.local_slot(index);
                            let val_s = self.local_slot(value);
                            if let (Some(os), Some(is)) = (obj_s, idx_s) {
                                if let Some(vs) = val_s {
                                    self.program.emit_op(Opcode::SetIndexLocalLocal);
                                    self.program.emit_u8(os);
                                    self.program.emit_u8(is);
                                    self.program.emit_u8(vs);
                                    if keep {
                                        self.program.emit_op(Opcode::LoadLocal);
                                        self.program.emit_u8(vs);
                                    }
                                    return Ok(());
                                }
                                if let Expr::Index { obj: vobj, index: vidx, .. } = value.as_ref() {
                                    if let (Some(vos), Some(vis)) =
                                        (self.local_slot(vobj), self.local_slot(vidx))
                                    {
                                        self.program.emit_op(Opcode::SetIndexLocalGetLocal);
                                        self.program.emit_u8(os);
                                        self.program.emit_u8(is);
                                        self.program.emit_u8(vos);
                                        self.program.emit_u8(vis);
                                        if keep {
                                            self.program.emit_op(Opcode::LoadLocalLocalGetIndex);
                                            self.program.emit_u8(vos);
                                            self.program.emit_u8(vis);
                                        }
                                        return Ok(());
                                    }
                                }
                            }
                            if let (Some(os), Some((is, ar, imm))) =
                                (obj_s, self.local_plus_int(index))
                            {
                                if let Expr::Index { obj: vobj, index: vidx, .. } = value.as_ref() {
                                    if let (Some(vos), Some(vis)) =
                                        (self.local_slot(vobj), self.local_slot(vidx))
                                    {
                                        self.program.emit_op(Opcode::SetIndexLocalPlusIntLocalGetLocal);
                                        self.program.emit_u8(os);
                                        self.program.emit_u8(is);
                                        self.program.emit_u8(ar);
                                        self.program.emit_i32(imm);
                                        self.program.emit_u8(vos);
                                        self.program.emit_u8(vis);
                                        if keep {
                                            self.program.emit_op(Opcode::LoadLocalLocalGetIndex);
                                            self.program.emit_u8(vos);
                                            self.program.emit_u8(vis);
                                        }
                                        return Ok(());
                                    }
                                }
                            }
                            let tmp_obj = self.fresh_local();
                            let tmp_idx = self.fresh_local();
                            self.emit_keep(|c| c.emit_expr(obj))?;
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(tmp_obj);
                            self.emit_keep(|c| c.emit_expr(index))?;
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(tmp_idx);
                            self.emit_keep(|c| c.emit_expr(value))?;
                            if keep {
                                self.program.emit_op(Opcode::Dup);
                            }
                            self.program.emit_op(Opcode::LoadLocal);
                            self.program.emit_u8(tmp_obj);
                            self.program.emit_op(Opcode::LoadLocal);
                            self.program.emit_u8(tmp_idx);
                            self.program.emit_op(Opcode::SetIndex);
                        }
                    }
                    // Destructuring assignment: `[x, y] = arr` and the
                    // parenthesized `({ a, b } = obj)`. The literal is
                    // converted to a pattern; the RHS is stashed once and every
                    // bound name is stored from it. The result is the RHS.
                    Expr::Object(_) | Expr::Array(_) => {
                        if *op != "=" {
                            return Err(CompileError::UnexpectedToken(
                                "invalid compound assignment target".to_string(),
                            ));
                        }
                        let pat = Self::expr_to_pattern(target)?;
                        self.emit_keep(|c| c.emit_expr(value))?;
                        let tmp = self.fresh_local();
                        self.program.emit_op(Opcode::StoreLocal);
                        self.program.emit_u8(tmp);
                        self.emit_pattern_store(&pat, tmp, PatStoreMode::Assign)?;
                        if keep {
                            self.program.emit_op(Opcode::LoadLocal);
                            self.program.emit_u8(tmp);
                        }
                    }
                    _ => {
                        return Err(CompileError::UnexpectedToken("invalid assignment target".to_string()));
                    }
                }
            }
            Expr::Call { callee, args, .. } => {
                // Whether this call's result is consumed. False only for a
                // top-level statement call (see emit_stmt_expr): the VM then
                // skips pushing the result (CallKeep0), so the statement emits
                // no trailing Pop. The callee and args still evaluate normally
                // and are always emitted with keep_result=true (their values
                // feed the call), hence the emit_keep scoping.
                let keep = self.keep_result;
                let has_spread = args.iter().any(|a| a.spread);
                let mask = args.iter().enumerate().fold(0u16, |m, (i, a)| {
                    if a.spread { m | (1 << i) } else { m }
                });
                // Method calls bind `this`: `o.m(...)` / `o[i](...)` â€” parens
                // don't strip the reference (`(o.m)()` still binds). The
                // receiver stays below the callee so the frame can read it as
                // its this slot. Note: the member lookup runs before the args
                // (a minor deviation from the spec's after-args order that is
                // unobservable except by an argument that mutates the method).
                let mut c = callee.as_ref();
                while let Expr::Paren(inner) = c {
                    c = inner.as_ref();
                }
                let method = match c {
                    Expr::Prop { obj, prop, .. } => {
                        let pi = self.program.add_constant(Value::string(prop.clone()));
                        self.emit_keep(|s| s.emit_expr(obj))?;
                        self.program.emit_op(Opcode::Dup);
                        self.program.emit_op(Opcode::GetProperty);
                        self.program.emit_u16(pi);
                        true
                    }
                    Expr::Index { obj, index, .. } => {
                        self.emit_keep(|s| s.emit_expr(obj))?;
                        self.program.emit_op(Opcode::Dup);
                        self.emit_keep(|s| s.emit_expr(index))?;
                        self.program.emit_op(Opcode::GetIndex);
                        true
                    }
                    _ => false,
                };
                if method {
                    // Stack: [receiver, callee]. Args push on top; CallMethod
                    // reads the receiver from one slot below the frame base.
                    for a in args.iter() { self.emit_keep(|c| c.emit_expr(&a.expr))?; }
                    if has_spread {
                        self.program.emit_op(if keep {
                            Opcode::CallMethodSpread
                        } else {
                            Opcode::CallMethodSpreadKeep0
                        });
                        self.program.emit_u8(args.len() as u8);
                        self.program.emit_u16(mask);
                    } else {
                        self.program.emit_op(if keep {
                            Opcode::CallMethod
                        } else {
                            Opcode::CallMethodKeep0
                        });
                        self.program.emit_u8(args.len() as u8);
                    }
                    return Ok(());
                }
                // Plain call: args first, then the callee (the top of stack
                // is the function the Call opcode pops).
                for a in args.iter() { self.emit_keep(|c| c.emit_expr(&a.expr))?; }
                self.emit_keep(|c| c.emit_expr(callee))?;
                if has_spread {
                    if keep {
                        self.program.emit_op(Opcode::CallSpread);
                    } else {
                        self.program.emit_op(Opcode::CallSpreadKeep0);
                    }
                    self.program.emit_u8(args.len() as u8);
                    self.program.emit_u16(mask);
                } else {
                    if keep {
                        self.program.emit_op(Opcode::Call);
                    } else {
                        self.program.emit_op(Opcode::CallKeep0);
                    }
                    self.program.emit_u8(args.len() as u8);
                }
            }
            Expr::New { callee, args } => {
                if args.iter().any(|a| a.spread) {
                    return Err(CompileError::UnexpectedToken(
                        "spread in `new` arguments is not supported".to_string(),
                    ));
                }
                // Plain-call convention (args first, callee on top): New
                // pops the callee, inserts the fresh instance below the args,
                // and calls the constructor with `this` bound to it.
                for a in args.iter() { self.emit_keep(|c| c.emit_expr(&a.expr))?; }
                self.emit_keep(|c| c.emit_expr(callee))?;
                self.program.emit_op(Opcode::New);
                self.program.emit_u8(args.len() as u8);
            }
            Expr::SuperCall { args } => {
                // `super(a)` in a derived class's constructor: call the
                // captured parent class with the current `this` bound.
                let u = self.find_upvalue("\u{0}home")?;
                // [this, parent, args...] â€” CallMethod layout.
                self.program.emit_op(Opcode::LoadThis);
                self.program.emit_op(Opcode::LoadUpvalue);
                self.program.emit_u8(u);
                let keep = self.keep_result;
                let has_spread = args.iter().any(|a| a.spread);
                let mask = args.iter().enumerate().fold(0u16, |m, (i, a)| {
                    if a.spread { m | (1 << i) } else { m }
                });
                for a in args.iter() { self.emit_keep(|c| c.emit_expr(&a.expr))?; }
                if has_spread {
                    self.program.emit_op(if keep {
                        Opcode::CallMethodSpread
                    } else {
                        Opcode::CallMethodSpreadKeep0
                    });
                    self.program.emit_u8(args.len() as u8);
                    self.program.emit_u16(mask);
                } else {
                    self.program.emit_op(if keep {
                        Opcode::CallMethod
                    } else {
                        Opcode::CallMethodKeep0
                    });
                    self.program.emit_u8(args.len() as u8);
                }
            }
            Expr::SuperProp { prop, args } => {
                // `super.m(args)` in a method: look `m` up on the home
                // object's parent (the class's proto chain) and call it with
                // the current `this` bound.
                let u = self.find_upvalue("\u{0}home")?;
                let pi = self.program.add_constant(Value::string(prop.clone()));
                match args {
                    Some(args) => {
                        // [this, this, home.proto, m, args...] â€” CallMethod
                        // reads the receiver from below the frame base.
                        self.program.emit_op(Opcode::LoadThis);
                        self.program.emit_op(Opcode::Dup);
                        self.program.emit_op(Opcode::LoadUpvalue);
                        self.program.emit_u8(u);
                        self.program.emit_op(Opcode::GetProto);
                        self.program.emit_op(Opcode::GetProperty);
                        self.program.emit_u16(pi);
                        let keep = self.keep_result;
                        let has_spread = args.iter().any(|a| a.spread);
                        let mask = args.iter().enumerate().fold(0u16, |m, (i, a)| {
                            if a.spread { m | (1 << i) } else { m }
                        });
                        for a in args.iter() { self.emit_keep(|c| c.emit_expr(&a.expr))?; }
                        if has_spread {
                            self.program.emit_op(if keep {
                                Opcode::CallMethodSpread
                            } else {
                                Opcode::CallMethodSpreadKeep0
                            });
                            self.program.emit_u8(args.len() as u8);
                            self.program.emit_u16(mask);
                        } else {
                            self.program.emit_op(if keep {
                                Opcode::CallMethod
                            } else {
                                Opcode::CallMethodKeep0
                            });
                            self.program.emit_u8(args.len() as u8);
                        }
                    }
                    None => {
                        // Bare `super.m` reference: the function value only
                        // (no receiver binding).
                        self.program.emit_op(Opcode::LoadUpvalue);
                        self.program.emit_u8(u);
                        self.program.emit_op(Opcode::GetProto);
                        self.program.emit_op(Opcode::GetProperty);
                        self.program.emit_u16(pi);
                    }
                }
            }
            Expr::Class { name: _cname, extends, methods } => {
                // Construction sequence. Synthetic locals in the enclosing
                // frame hold the parent class, the prototype object, and the
                // class (constructor) value; methods capture the prototype (or
                // the parent, for the derived constructor) as their `\0home`.
                let parent_slot = self.fresh_local();
                let proto_slot = self.fresh_local();
                let class_slot = self.fresh_local();
                {
                    let l = self.funcs.last_mut().unwrap();
                    l.locals[parent_slot as usize] = "\u{0}class_parent".into();
                    l.locals[proto_slot as usize] = "\u{0}class_proto".into();
                    l.locals[class_slot as usize] = "\u{0}class_fn".into();
                }
                let has_parent = extends.is_some();
                // 1. Parent value (or undefined).
                match extends {
                    Some(ext) => self.emit_keep(|c| c.emit_expr(ext))?,
                    None => self.program.emit_op(Opcode::LoadUndefined),
                }
                self.program.emit_op(Opcode::StoreLocal);
                self.program.emit_u8(parent_slot);
                // 2. The prototype object.
                self.program.emit_op(Opcode::MakeObject);
                self.program.emit_u16(0);
                self.program.emit_op(Opcode::StoreLocal);
                self.program.emit_u8(proto_slot);
                // 3. Instance methods / accessors → proto.
                for m in methods.iter().filter(|m| m.name != "constructor" && !m.is_static && m.kind != MethodKind::Field && !m.name.starts_with('#')) {
                    let home = if has_parent {
                        Some(UpvalueKind::Local { slot: proto_slot })
                    } else {
                        None
                    };
                    self.emit_method(m, home)?;
                    self.program.emit_op(Opcode::LoadLocal);
                    self.program.emit_u8(proto_slot);
                    let pi = self.program.add_constant(Value::string(m.name.clone()));
                    self.program.emit_op(Opcode::LoadConst);
                    self.program.emit_u16(pi);
                    match m.kind {
                        MethodKind::Getter => {
                            self.program.emit_op(Opcode::SetAccessor);
                            self.program.emit_u8(1);
                        }
                        MethodKind::Setter => {
                            self.program.emit_op(Opcode::SetAccessor);
                            self.program.emit_u8(2);
                        }
                        _ => {
                            self.program.emit_op(Opcode::SetProperty);
                        }
                    }
                }
                // 4. The constructor (the class value), stored on the proto
                //    as "constructor". A derived constructor captures the
                //    parent class for `super()`.
                let mut field_stmts: Vec<Stmt> = Vec::new();
                for m in methods.iter().filter(|m| !m.is_static) {
                    if m.kind == MethodKind::Field {
                        let target = Expr::Prop {
                            obj: Box::new(Expr::Ident("this".to_string())),
                            prop: m.name.clone(),
                            optional: false,
                        };
                        let value = m.init.clone().unwrap_or(Expr::Undef);
                        field_stmts.push(Stmt::Expr(Expr::Assign {
                            op: "=",
                            target: Box::new(target),
                            value: Box::new(value),
                        }));
                    } else if m.name.starts_with('#') {
                        let target = Expr::Prop {
                            obj: Box::new(Expr::Ident("this".to_string())),
                            prop: m.name.clone(),
                            optional: false,
                        };
                        let value = Expr::Lambda {
                            params: m.params.clone(),
                            body: m.body.clone(),
                            is_async: m.is_async,
                            is_generator: m.is_generator,
                            is_arrow: false,
                        };
                        field_stmts.push(Stmt::Expr(Expr::Assign {
                            op: "=",
                            target: Box::new(target),
                            value: Box::new(value),
                        }));
                    }
                }

                let ctor = methods.iter().find(|m| m.name == "constructor");
                let home = if has_parent {
                    Some(UpvalueKind::Local { slot: parent_slot })
                } else {
                    None
                };

                let mut ctor_def = match ctor {
                    Some(m) => m.clone(),
                    None => MethodDef {
                        name: "constructor".into(),
                        is_static: false,
                        is_async: false,
                        is_generator: false,
                        kind: MethodKind::Normal,
                        params: FnParams { params: Vec::new(), rest: None },
                        body: Box::new(Stmt::Block(Vec::new())),
                        init: None,
                    },
                };

                if !field_stmts.is_empty() {
                    let mut new_body = Vec::new();
                    let mut b = &*ctor_def.body;
                    while let Stmt::Loc { stmt, .. } = b {
                        b = stmt;
                    }
                    let orig_stmts = match b {
                        Stmt::Block(s) => s.clone(),
                        s => vec![s.clone()],
                    };
                    let super_idx = orig_stmts.iter().position(|s| {
                        let mut st = s;
                        while let Stmt::Loc { stmt, .. } = st {
                            st = stmt;
                        }
                        matches!(st, Stmt::Expr(Expr::SuperCall { .. }))
                    });
                    if let Some(idx) = super_idx {
                        new_body.extend(orig_stmts[..=idx].iter().cloned());
                        new_body.extend(field_stmts);
                        new_body.extend(orig_stmts[idx + 1..].iter().cloned());
                    } else {
                        new_body.extend(field_stmts);
                        new_body.extend(orig_stmts);
                    }
                    *ctor_def.body = Stmt::Block(new_body);
                }
                self.emit_method(&ctor_def, home)?;
                // The class value IS the constructor: keep a copy on the
                // stack (SetProperty consumes its operands), bind the proto's
                // `constructor` back-reference, then stash the class into its
                // slot.
                self.program.emit_op(Opcode::Dup);
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(proto_slot);
                let pi = self.program.add_constant(Value::string("constructor".into()));
                self.program.emit_op(Opcode::LoadConst);
                self.program.emit_u16(pi);
                self.program.emit_op(Opcode::SetProperty);
                self.program.emit_op(Opcode::StoreLocal);
                self.program.emit_u8(class_slot);
                // 5. C.prototype = proto. SetProperty pops [prop, obj, val]
                // with val on top, so the proto (val) is pushed first.
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(proto_slot);
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(class_slot);
                let pi = self.program.add_constant(Value::string("prototype".into()));
                self.program.emit_op(Opcode::LoadConst);
                self.program.emit_u16(pi);
                self.program.emit_op(Opcode::SetProperty);
                // 6. extends: proto.proto = parent.prototype.
                if has_parent {
                    self.program.emit_op(Opcode::LoadLocal);
                    self.program.emit_u8(proto_slot);
                    self.program.emit_op(Opcode::LoadLocal);
                    self.program.emit_u8(parent_slot);
                    let pi = self.program.add_constant(Value::string("prototype".into()));
                    self.program.emit_op(Opcode::GetProperty);
                    self.program.emit_u16(pi);
                    self.program.emit_op(Opcode::SetProto);
                }
                // 7. Static members (methods, fields, accessors) → the class function itself.
                for m in methods.iter().filter(|m| m.is_static) {
                    if m.kind == MethodKind::Field {
                        if let Some(init) = &m.init {
                            self.emit_expr(init)?;
                        } else {
                            self.program.emit_op(Opcode::LoadUndefined);
                        }
                        self.program.emit_op(Opcode::LoadLocal);
                        self.program.emit_u8(class_slot);
                        let pi = self.program.add_constant(Value::string(m.name.clone()));
                        self.program.emit_op(Opcode::LoadConst);
                        self.program.emit_u16(pi);
                        self.program.emit_op(Opcode::SetProperty);
                    } else {
                        self.emit_method(m, None)?;
                        self.program.emit_op(Opcode::LoadLocal);
                        self.program.emit_u8(class_slot);
                        let pi = self.program.add_constant(Value::string(m.name.clone()));
                        self.program.emit_op(Opcode::LoadConst);
                        self.program.emit_u16(pi);
                        match m.kind {
                            MethodKind::Getter => {
                                self.program.emit_op(Opcode::SetAccessor);
                                self.program.emit_u8(1);
                            }
                            MethodKind::Setter => {
                                self.program.emit_op(Opcode::SetAccessor);
                                self.program.emit_u8(2);
                            }
                            _ => {
                                self.program.emit_op(Opcode::SetProperty);
                            }
                        }
                    }
                }
                // 8. The class value is the expression result.
                self.program.emit_op(Opcode::LoadLocal);
                self.program.emit_u8(class_slot);
            }
            Expr::Regex { pattern, flags } => {
                // Fresh regex object per evaluation (own lastIndex). The
                // pattern and flags are string constants; the VM compiles
                // once per (pattern, flags) and caches the program.
                let pi = self.program.add_constant(Value::string(pattern.clone()));
                let fi = self.program.add_constant(Value::string(flags.clone()));
                self.program.emit_op(Opcode::MakeRegex);
                self.program.emit_u16(pi);
                self.program.emit_u16(fi);
            }
            Expr::Index { obj, index, .. } => {
                // `a[i]` with both operands locals fuses into one dispatch
                // (LoadLocal + LoadLocal + GetIndex -> LoadLocalLocalGetIndex).
                if let (Some(os), Some(is)) = (self.local_slot(obj), self.local_slot(index)) {
                    self.program.emit_op(Opcode::LoadLocalLocalGetIndex);
                    self.program.emit_u8(os);
                    self.program.emit_u8(is);
                    return Ok(());
                }
                self.emit_expr(obj)?;
                self.emit_expr(index)?;
                self.program.emit_op(Opcode::GetIndex);
            }
            Expr::Array(elems) => {
                for e in elems { self.emit_expr(&e.expr)?; }
                let has_spread = elems.iter().any(|e| e.spread);
                if has_spread {
                    let mask = elems.iter().enumerate().fold(0u16, |m, (i, e)| {
                        if e.spread { m | (1 << i) } else { m }
                    });
                    self.program.emit_op(Opcode::MakeArraySpread);
                    self.program.emit_u16(elems.len() as u16);
                    self.program.emit_u16(mask);
                } else {
                    self.program.emit_op(Opcode::MakeArray);
                    self.program.emit_u16(elems.len() as u16);
                }
            }
            Expr::Prop { obj, prop, .. } => {
                // `local.prop` with a local object and a literal property
                // fuses into one dispatch (LoadLocal + LoadConst +
                // GetProperty -> LoadLocalGetPropConst). The hottest case is
                // `arr.length` in loop conditions.
                if let Some(s) = self.local_slot(obj) {
                    let i = self.program.add_constant(Value::string(prop.clone()));
                    self.program.emit_op(Opcode::LoadLocalGetPropConst);
                    self.program.emit_u8(s);
                    self.program.emit_u16(i);
                    return Ok(());
                }
                self.emit_expr(obj)?;
                let i = self.program.add_constant(Value::string(prop.clone()));
                self.program.emit_op(Opcode::GetProperty);
                self.program.emit_u16(i);
            }
            Expr::Object(fields) => {
                let has_accessors = fields.iter().any(|f| matches!(f, ObjElem::Getter(..) | ObjElem::Setter(..)));
                if !has_accessors {
                    let mut mask = 0u16;
                    for (i, f) in fields.iter().enumerate() {
                        match f {
                            ObjElem::Pair(key, value) => {
                                let ki = self.program.add_constant(Value::string(key.clone()));
                                self.program.emit_op(Opcode::LoadConst);
                                self.program.emit_u16(ki);
                                self.emit_expr(value)?;
                            }
                            ObjElem::Computed(key, value) => {
                                self.emit_expr(key)?;
                                self.emit_expr(value)?;
                            }
                            ObjElem::Spread(src) => {
                                mask |= 1 << i;
                                self.emit_expr(src)?;
                            }
                            ObjElem::Getter(..) | ObjElem::Setter(..) => unreachable!(),
                        }
                    }
                    self.program.emit_op(Opcode::MakeObject);
                    self.program.emit_u16(fields.len() as u16);
                    self.program.emit_u16(mask);
                } else {
                    let normal_fields: Vec<&ObjElem> = fields.iter().filter(|f| !matches!(f, ObjElem::Getter(..) | ObjElem::Setter(..))).collect();
                    let mut mask = 0u16;
                    for (i, f) in normal_fields.iter().enumerate() {
                        match f {
                            ObjElem::Pair(key, value) => {
                                let ki = self.program.add_constant(Value::string(key.clone()));
                                self.program.emit_op(Opcode::LoadConst);
                                self.program.emit_u16(ki);
                                self.emit_expr(value)?;
                            }
                            ObjElem::Computed(key, value) => {
                                self.emit_expr(key)?;
                                self.emit_expr(value)?;
                            }
                            ObjElem::Spread(src) => {
                                mask |= 1 << i;
                                self.emit_expr(src)?;
                            }
                            _ => unreachable!(),
                        }
                    }
                    self.program.emit_op(Opcode::MakeObject);
                    self.program.emit_u16(normal_fields.len() as u16);
                    self.program.emit_u16(mask);
                    let obj_slot = self.fresh_local();
                    self.program.emit_op(Opcode::StoreLocal);
                    self.program.emit_u8(obj_slot);

                    for f in fields {
                        match f {
                            ObjElem::Getter(name, expr) => {
                                self.emit_expr(expr)?;
                                self.program.emit_op(Opcode::LoadLocal);
                                self.program.emit_u8(obj_slot);
                                let ki = self.program.add_constant(Value::string(name.clone()));
                                self.program.emit_op(Opcode::LoadConst);
                                self.program.emit_u16(ki);
                                self.program.emit_op(Opcode::SetAccessor);
                                self.program.emit_u8(1);
                            }
                            ObjElem::Setter(name, expr) => {
                                self.emit_expr(expr)?;
                                self.program.emit_op(Opcode::LoadLocal);
                                self.program.emit_u8(obj_slot);
                                let ki = self.program.add_constant(Value::string(name.clone()));
                                self.program.emit_op(Opcode::LoadConst);
                                self.program.emit_u16(ki);
                                self.program.emit_op(Opcode::SetAccessor);
                                self.program.emit_u8(2);
                            }
                            _ => {}
                        }
                    }
                    self.program.emit_op(Opcode::LoadLocal);
                    self.program.emit_u8(obj_slot);
                }
            }
            Expr::Template(parts) => {
                // `` `a${x}b` `` compiles to "" + "a" + x + "b", coercing
                // interpolated values to strings via Value::add.
                let empty = self.program.add_constant(Value::string(String::new()));
                self.program.emit_op(Opcode::LoadConst);
                self.program.emit_u16(empty);
                for part in parts {
                    match part {
                        TemplatePart::Lit(s) => {
                            let ci = self.program.add_constant(Value::string(s.clone()));
                            self.program.emit_op(Opcode::LoadConst);
                            self.program.emit_u16(ci);
                        }
                        TemplatePart::Expr(toks) => {
                            let mut p = Parser::new(toks.clone());
                            let e = p.parse_expr(0)?;
                            if !matches!(p.peek(), Token::Eof) {
                                return Err(CompileError::UnexpectedToken(format!("{:?}", p.peek())));
                            }
                            self.emit_expr(&e)?;
                        }
                    }
                    self.program.emit_op(Opcode::Add);
                }
            }
            Expr::Lambda { params, body, is_async, is_generator, is_arrow } => {
                if params.params.iter().any(|p| p.default.is_some()) {
                    return Err(CompileError::UnexpectedToken("default parameters are not yet supported — use explicit `if (x===undefined) x=...` inside the body".to_string()));
                }
                let j = self.emit_jump(Opcode::Jump);
                let start = self.program.bytecode.len();
                self.program.record_function_name(start, "<anonymous>".to_string());
                // Arrows bind `this`/`arguments` lexically. When the body
                // references them (directly or inside nested arrows), the
                // arrow gets hidden `\0this`/`\0arguments` upvalues: a direct
                // arrow captures the enclosing non-arrow frame's value at
                // creation (kind Lexical -> LoadThis/LoadArguments before
                // NewClosure); a nested arrow re-captures the nearest
                // enclosing arrow's cell (kind Upvalue), so every arrow in a
                // chain shares the same value regardless of how the inner
                // arrows are later called.
                let mut upvalues = Vec::new();
                if *is_arrow {
                    for (hidden, wants) in [
                        ("\u{0}this", stmt_uses_lexical(body, "this")),
                        ("\u{0}arguments", stmt_uses_lexical(body, "arguments")),
                    ] {
                        if !wants {
                            continue;
                        }
                        let kind = self.lexical_capture_kind(hidden);
                        if matches!(kind, UpvalueKind::Lexical) && hidden == "\u{0}arguments" {
                            // The LoadArguments capture runs in the enclosing
                            // function's frame; it needs the args snapshot.
                            self.funcs.last_mut().unwrap().uses_arguments = true;
                        }
                        upvalues.push(UpvalueRef { name: hidden.to_string(), kind });
                    }
                }
                self.funcs.push(FuncCtx {
                    locals: params.names().clone(),
                    upvalues,
                    is_async: *is_async,
                    uses_arguments: false,
                    is_arrow: *is_arrow,
                });
                if let Some(r) = params.rest {
                    // First instruction: materialize the rest array from the
                    // args that exceed the fixed parameters (the operand stack
                    // top is exactly `base + argc` at entry).
                    self.program.emit_op(Opcode::MakeRestArray);
                    self.program.emit_u8(r as u8);
                    self.program.emit_u8(r as u8);
                }
                if *is_async {
                    // Hidden slot (after the params) holds this invocation's
                    // promise; Return resolves it and hands it to the caller.
                    self.funcs.last_mut().unwrap().locals.push("\u{0}promise".to_string());
                    let ps = (self.funcs.last().unwrap().locals.len() - 1) as u8;
                    self.program.emit_op(Opcode::NewPromise);
                    self.program.emit_u8(ps);
                }
                if *is_generator {
                    self.program.emit_op(Opcode::CreateGenerator);
                }
                let saved_loops = std::mem::take(&mut self.loops);
                let saved_trys = std::mem::take(&mut self.trys);
                let saved_finally = std::mem::take(&mut self.finally_locals);
                let saved_labels = std::mem::take(&mut self.labels);
                let saved_pending = self.pending_label.take();
                self.emit_stmt(body)?;
                self.loops = saved_loops;
                self.trys = saved_trys;
                self.finally_locals = saved_finally;
                self.labels = saved_labels;
                self.pending_label = saved_pending;
                self.program.emit_op(Opcode::LoadUndefined);
                self.program.emit_op(Opcode::Return);
                let ctx = self.funcs.pop().unwrap();
                self.patch_jump(j);
                let ci = self.program.add_constant(Value::number(start as f64));
                // Fixed params only â€” `names` includes the rest param, but
                // the rest slot is materialized by MakeRestArray and must not
                // be pre-filled (see emit_method).
                let pcount = params.names().len() as u8 - u8::from(params.rest.is_some());
                self.emit_captures(&ctx);
                self.program.emit_op(Opcode::NewClosure);
                self.program.emit_u16(ci);
                self.program.emit_u8(ctx.upvalues.len() as u8);
                self.program.emit_u8(pcount);
                self.program.emit_u8(ctx.uses_arguments as u8);
            }
            Expr::Await(e) => {
                if !self.funcs.last().unwrap().is_async {
                    return Err(CompileError::AwaitOutsideAsync);
                }
                self.emit_expr(e)?;
                self.program.emit_op(Opcode::Await);
            }
            Expr::IncDec { target, is_inc, is_prefix } => {
                // `a?.b++` is a SyntaxError in JS (not a valid reference).
                if Self::is_optional_chain(target) {
                    return Err(CompileError::UnexpectedToken(
                        "invalid update target: optional chaining cannot be incremented".to_string(),
                    ));
                }
                let emit_one = |c: &mut Compiler| {
                    c.program.emit_op(Opcode::LoadInt);
                    c.program.emit_u32(1);
                    if *is_inc {
                        c.program.emit_op(Opcode::Add);
                    } else {
                        c.program.emit_op(Opcode::Subtract);
                    }
                };
                // Whether the inc/dec result is consumed (false only for a
                // top-level statement inc/dec; see emit_stmt_expr).
                let keep = self.keep_result;
                // `x++` evaluates to the old value; `++x` to the new one.
                // `(a)++` is valid: unwrap grouping parens first.
                let mut target_ref = target.as_ref();
                while let Expr::Paren(inner) = target_ref { target_ref = inner.as_ref(); }
                match target_ref {
                    Expr::Ident(name) => {
                        // `++imported` is also an assignment â€” same loud error.
                        if self.imported_bindings.contains(name) {
                            return Err(CompileError::AssignToImport(name.clone()));
                        }
                        match self.resolve(name) {
                        Resolved::Local(s) => {
                            // x++ / ++x / x-- / --x on a plain local: one fused
                            // opcode reads, mutates, stores, and pushes (if
                            // keep).
                            let delta = if *is_inc { 1i8 } else { -1i8 };
                            let flags = (*is_prefix as u8) | ((keep as u8) << 1);
                            self.program.emit_op(Opcode::IncLocal);
                            self.program.emit_u8(s);
                            self.program.emit_u8(flags);
                            self.program.emit_u8(delta as u8);
                        }
                        Resolved::Upvalue(u) => {
                            // keep=0: the result is discarded, so both forms
                            // collapse to load / Â±1 / store (the Add pushes the
                            // new value, StoreUpvalue consumes it).
                            if !keep {
                                self.program.emit_op(Opcode::LoadUpvalue);
                                self.program.emit_u8(u);
                                emit_one(self);
                                self.program.emit_op(Opcode::StoreUpvalue);
                                self.program.emit_u8(u);
                            } else if *is_prefix {
                                self.program.emit_op(Opcode::LoadUpvalue);
                                self.program.emit_u8(u);
                                emit_one(self);
                                self.program.emit_op(Opcode::Dup);
                                self.program.emit_op(Opcode::StoreUpvalue);
                                self.program.emit_u8(u);
                            } else {
                                self.program.emit_op(Opcode::LoadUpvalue);
                                self.program.emit_u8(u);
                                self.program.emit_op(Opcode::Dup);
                                emit_one(self);
                                self.program.emit_op(Opcode::StoreUpvalue);
                                self.program.emit_u8(u);
                            }
                        }
                        Resolved::Global(g) => {
                            if is_native(name) {
                                return Err(CompileError::CannotShadowBuiltin(name.clone()));
                            }
                            if !keep {
                                self.program.emit_op(Opcode::LoadGlobal);
                                self.program.emit_u16(g);
                                emit_one(self);
                                self.program.emit_op(Opcode::StoreGlobal);
                                self.program.emit_u16(g);
                            } else if *is_prefix {
                                self.program.emit_op(Opcode::LoadGlobal);
                                self.program.emit_u16(g);
                                emit_one(self);
                                self.program.emit_op(Opcode::Dup);
                                self.program.emit_op(Opcode::StoreGlobal);
                                self.program.emit_u16(g);
                            } else {
                                self.program.emit_op(Opcode::LoadGlobal);
                                self.program.emit_u16(g);
                                self.program.emit_op(Opcode::Dup);
                                emit_one(self);
                                self.program.emit_op(Opcode::StoreGlobal);
                                self.program.emit_u16(g);
                            }
                        }
                        }
                    },
                    Expr::Prop { obj, prop, .. } => {
                        // o.a++ / ++o.a / o.a-- / --o.a: one fused opcode reads
                        // obj.p, adds Â±1, writes it back, and pushes the old
                        // (postfix) or new (prefix) value (if keep). The obj
                        // stays on the stack and evaluates exactly once.
                        let pi = self.program.add_constant(Value::string(prop.clone()));
                        let flags = (*is_prefix as u8) | ((!*is_inc as u8) << 1) | ((keep as u8) << 2);
                        self.emit_keep(|c| c.emit_expr(obj))?;
                        self.program.emit_op(Opcode::IncPropConst);
                        self.program.emit_u8(flags);
                        self.program.emit_u16(pi);
                    }
                    Expr::Index { obj, index, .. } => {
                        // a[i]++ / ++a[i] / a[i]-- / --a[i]: one fused opcode
                        // reads obj[idx], adds Â±1, writes back, and pushes the
                        // old (postfix) or new (prefix) value (if keep). The
                        // obj and index stay on the stack, each evaluating once.
                        let flags = (*is_prefix as u8) | ((!*is_inc as u8) << 1) | ((keep as u8) << 2);
                        self.emit_keep(|c| c.emit_expr(obj))?;
                        self.emit_keep(|c| c.emit_expr(index))?;
                        self.program.emit_op(Opcode::IncIndexConst);
                        self.program.emit_u8(flags);
                    }
                    _ => {
                        return Err(CompileError::UnexpectedToken(
                            "invalid increment/decrement target".to_string(),
                        ));
                    }
                }
            }
            Expr::Ternary { cond, then, els } => {
                // cond ? then : else â€” only the taken branch evaluates, and
                // the condition is discarded, so `&&` / `||` fuse via
                // emit_cond: falsy edges jump to the else branch,
                // truthy-`||` edges jump to the then branch.
                let mut exit_jumps = Vec::new();
                let mut body_jumps = Vec::new();
                self.emit_cond(cond, &mut exit_jumps, &mut body_jumps)?;
                let then_pos = self.program.bytecode.len();
                self.emit_expr(then)?;
                let end = self.emit_jump(Opcode::Jump);
                for j in exit_jumps { self.patch_jump(j); }
                for j in body_jumps { self.patch_jump_to(j, then_pos); }
                self.emit_expr(els)?;
                self.patch_jump(end);
            }
            Expr::Yield { value, delegate } => {
                if *delegate {
                    if let Some(val_expr) = value {
                        self.emit_expr(val_expr)?;
                    } else {
                        self.program.emit_op(Opcode::LoadUndefined);
                    }
                    self.program.emit_op(Opcode::ToIterable);
                    let arr_slot = self.fresh_local();
                    let idx_slot = self.fresh_local();
                    let len_slot = self.fresh_local();
                    self.program.emit_op(Opcode::StoreLocal);
                    self.program.emit_u8(arr_slot);
                    // idx = 0
                    self.program.emit_op(Opcode::LoadInt);
                    self.program.emit_u32(0);
                    self.program.emit_op(Opcode::StoreLocal);
                    self.program.emit_u8(idx_slot);
                    // len = arr.length
                    self.program.emit_op(Opcode::LoadLocal);
                    self.program.emit_u8(arr_slot);
                    let lc = self.program.add_constant(Value::string("length".to_string()));
                    self.program.emit_op(Opcode::GetProperty);
                    self.program.emit_u16(lc);
                    self.program.emit_op(Opcode::StoreLocal);
                    self.program.emit_u8(len_slot);
                    // loop: while idx < len
                    let ls = self.program.bytecode.len();
                    self.program.emit_op(Opcode::LoadLocal);
                    self.program.emit_u8(idx_slot);
                    self.program.emit_op(Opcode::LoadLocal);
                    self.program.emit_u8(len_slot);
                    self.program.emit_op(Opcode::Less);
                    let ej = self.emit_jump(Opcode::JumpIfFalsePop);
                    // val = arr[idx]
                    self.program.emit_op(Opcode::LoadLocal);
                    self.program.emit_u8(arr_slot);
                    self.program.emit_op(Opcode::LoadLocal);
                    self.program.emit_u8(idx_slot);
                    self.program.emit_op(Opcode::GetIndex);
                    self.program.emit_op(Opcode::Yield);
                    self.program.emit_op(Opcode::Pop);
                    // idx += 1
                    self.program.emit_op(Opcode::LoadLocal);
                    self.program.emit_u8(idx_slot);
                    self.program.emit_op(Opcode::LoadInt);
                    self.program.emit_u32(1);
                    self.program.emit_op(Opcode::Add);
                    self.program.emit_op(Opcode::StoreLocal);
                    self.program.emit_u8(idx_slot);
                    let back = self.emit_jump(Opcode::Jump);
                    self.patch_jump_to(back, ls);
                    self.patch_jump(ej);
                    self.program.emit_op(Opcode::LoadUndefined);
                } else {
                    // Emit the yielded value (or undefined if `yield;`)
                    if let Some(val_expr) = value {
                        self.emit_expr(val_expr)?;
                    } else {
                        self.program.emit_op(Opcode::LoadUndefined);
                    }
                    self.program.emit_op(Opcode::Yield);
                }
            }
        }
        Ok(())
    }

    fn emit_stmt(&mut self, stmt: &Stmt) -> Result<(), CompileError> {
        match stmt {
            Stmt::Loc { line, col, stmt } => {
                self.record_loc(*line, *col);
                self.emit_stmt(stmt)?;
            }
            Stmt::Expr(e) => self.emit_stmt_expr(e)?,
            Stmt::VarDecl { decls } => {
                for (pat, init) in decls {
                    match init {
                        Some(e) => {
                            // Evaluate the initializer once, stash it, then
                            // bind every name in the pattern from it.
                            self.emit_expr(e)?;
                            let tmp = self.fresh_local();
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(tmp);
                            self.emit_pattern_store(pat, tmp, PatStoreMode::Declare)?;
                        }
                        None => match pat {
                            Pat::Bind(name) => {
                                self.program.emit_op(Opcode::LoadUndefined);
                                self.store_declared(name)?;
                            }
                            _ => {
                                return Err(CompileError::UnexpectedToken(
                                    "missing initializer in destructuring declaration".to_string(),
                                ));
                            }
                        },
                    }
                }
            }
            Stmt::FnDecl { name, params, body, is_async, is_generator } => {
                if is_native(name) {
                    return Err(CompileError::CannotShadowBuiltin(name.clone()));
                }
                if params.params.iter().any(|p| p.default.is_some()) {
                    return Err(CompileError::UnexpectedToken("default parameters are not yet supported — use explicit `if (x===undefined) x=...` inside the body".to_string()));
                }
                let j = self.emit_jump(Opcode::Jump);
                let start = self.program.bytecode.len();
                self.program.record_function_name(start, name.clone());
                self.funcs.push(FuncCtx {
                    locals: params.names().clone(),
                    upvalues: Vec::new(),
                    is_async: *is_async,
                    uses_arguments: false,
                    is_arrow: false,
                });
                if let Some(r) = params.rest {
                    self.program.emit_op(Opcode::MakeRestArray);
                    self.program.emit_u8(r as u8);
                    self.program.emit_u8(r as u8);
                }
                // Self slot for recursion: the function value is materialized
                // into this slot at call entry via LoadSelf.
                let name_slot = self.funcs.last().unwrap().locals.len() as u8;
                self.funcs.last_mut().unwrap().locals.push(name.clone());
                self.program.emit_op(Opcode::LoadSelf);
                self.program.emit_op(Opcode::StoreLocal);
                self.program.emit_u8(name_slot);
                if *is_async {
                    // Promise slot after params and the self slot.
                    self.funcs.last_mut().unwrap().locals.push("\u{0}promise".to_string());
                    let ps = (self.funcs.last().unwrap().locals.len() - 1) as u8;
                    self.program.emit_op(Opcode::NewPromise);
                    self.program.emit_u8(ps);
                }
                if *is_generator {
                    self.program.emit_op(Opcode::CreateGenerator);
                }
                let saved_loops = std::mem::take(&mut self.loops);
                let saved_trys = std::mem::take(&mut self.trys);
                let saved_finally = std::mem::take(&mut self.finally_locals);
                let saved_labels = std::mem::take(&mut self.labels);
                let saved_pending = self.pending_label.take();
                self.emit_stmt(body)?;
                self.loops = saved_loops;
                self.trys = saved_trys;
                self.finally_locals = saved_finally;
                self.labels = saved_labels;
                self.pending_label = saved_pending;
                self.program.emit_op(Opcode::LoadUndefined);
                self.program.emit_op(Opcode::Return);
                let ctx = self.funcs.pop().unwrap();
                self.patch_jump(j);
                let ci = self.program.add_constant(Value::number(start as f64));
                // Fixed params only â€” `names` includes the rest param, but
                // the rest slot is materialized by MakeRestArray and must not
                // be pre-filled (see emit_method).
                let pcount = params.names().len() as u8 - u8::from(params.rest.is_some());
                self.emit_captures(&ctx);
                self.program.emit_op(Opcode::NewClosure);
                self.program.emit_u16(ci);
                self.program.emit_u8(ctx.upvalues.len() as u8);
                self.program.emit_u8(pcount);
                self.program.emit_u8(ctx.uses_arguments as u8);
                if self.top_globals && self.funcs.len() == 1 {
                    let i = self.global_store_index(name)?;
                    self.program.emit_op(Opcode::StoreGlobal);
                    self.program.emit_u16(i);
                } else {
                    // Reuse a hoisted (pre-allocated) slot if present; else
                    // allocate a fresh one.
                    let s = match self.funcs.last().unwrap().locals.iter().rposition(|l| l == name) {
                        Some(s) => s as u8,
                        None => {
                            self.funcs.last_mut().unwrap().locals.push(name.to_string());
                            (self.funcs.last().unwrap().locals.len() - 1) as u8
                        }
                    };
                    self.program.emit_op(Opcode::StoreLocal);
                    self.program.emit_u8(s);
                }
            }
            Stmt::Class { name, extends, methods } => {
                // `class C extends B { â€¦ }` â€” build the class value in place
                // (not hoisted, matching JS TDZ semantics), then bind it to
                // the name.
                let e = Expr::Class {
                    name: Some(name.clone()),
                    extends: extends.clone().map(Box::new),
                    methods: methods.to_vec(),
                };
                self.emit_expr(&e)?;
                if self.top_globals && self.funcs.len() == 1 {
                    let i = self.global_store_index(name)?;
                    self.program.emit_op(Opcode::StoreGlobal);
                    self.program.emit_u16(i);
                } else {
                    let s = match self.funcs.last().unwrap().locals.iter().rposition(|l| l == name) {
                        Some(s) => s as u8,
                        None => {
                            self.funcs.last_mut().unwrap().locals.push(name.to_string());
                            (self.funcs.last().unwrap().locals.len() - 1) as u8
                        }
                    };
                    self.program.emit_op(Opcode::StoreLocal);
                    self.program.emit_u8(s);
                }
            }
            Stmt::Return(e) => {
                if let Some(v) = e { self.emit_expr(v)?; }
                else { self.program.emit_op(Opcode::LoadUndefined); }
                if !self.trys.is_empty() {
                    // The finally bodies' locals materialize at their slots,
                    // which could sit where the return value is on the operand
                    // stack. Move the value into a dedicated slot below the
                    // cleanup's locals first, and restore it afterwards.
                    let stash = {
                        let s = self.funcs.last_mut().unwrap().locals.len() as u8;
                        self.funcs.last_mut().unwrap().locals
                            .push("\u{0}retstash".to_string());
                        s
                    };
                    self.program.emit_op(Opcode::StoreLocal);
                    self.program.emit_u8(stash);
                    // Returning out of enclosing trys runs their finally
                    // bodies first (innermost first).
                    while let Some(ctx) = self.trys.pop() {
                        self.program.emit_op(Opcode::TryEnd);
                        if let Some(f) = ctx.finally {
                            self.emit_finally(&f)?;
                        }
                    }
                    self.program.emit_op(Opcode::LoadLocal);
                    self.program.emit_u8(stash);
                }
                self.program.emit_op(Opcode::Return);
            }
            Stmt::If { cond, then, els } => {
                // The condition is discarded and only the taken branch
                // evaluates, so `&&` / `||` fuse via emit_cond:
                // falsy edges jump to the else path, truthy-`||` edges jump to
                // the then branch (the old jump-over-pop dance for the
                // else-less form is gone entirely).
                let mut exit_jumps = Vec::new();
                let mut body_jumps = Vec::new();
                self.emit_cond(cond, &mut exit_jumps, &mut body_jumps)?;
                let then_pos = self.program.bytecode.len();
                self.emit_stmt(then)?;
                if let Some(e) = els {
                    let end = self.emit_jump(Opcode::Jump);
                    for j in exit_jumps { self.patch_jump(j); }
                    for j in body_jumps { self.patch_jump_to(j, then_pos); }
                    self.emit_stmt(e)?;
                    self.patch_jump(end);
                } else {
                    let end = self.emit_jump(Opcode::Jump);
                    for j in exit_jumps { self.patch_jump(j); }
                    for j in body_jumps { self.patch_jump_to(j, then_pos); }
                    self.patch_jump(end);
                }
            }
            Stmt::While { cond, body } => {
                let ls = self.program.bytecode.len();
                // Inline `&&` / `||` short-circuits: each operand gets its own
                // pop-jump edge (falsy -> exit, truthy-|| -> body) instead of
                // materializing the condition value and popping it.
                let mut exit_jumps = Vec::new();
                let mut body_jumps = Vec::new();
                self.emit_cond(cond, &mut exit_jumps, &mut body_jumps)?;
                let body_pos = self.program.bytecode.len();
                self.loops.push(LoopCtx {
                    break_jumps: Vec::new(),
                    continue_jumps: Vec::new(),
                    continue_target: 0,
                    trys_depth: self.trys.len(),
                    label: self.pending_label.take(),
                    is_switch: false,
                });
                self.emit_stmt(body)?;
                let mut lc = self.loops.pop().unwrap();
                self.program.emit_op(Opcode::Jump);
                self.program.emit_u32(ls as u32);
                for j in exit_jumps { self.patch_jump(j); }
                for j in body_jumps { self.patch_jump_to(j, body_pos); }
                self.finish_loop(&mut lc, ls);
            }
            Stmt::DoWhile { cond, body } => {
                // `do body while (cond)`: the body runs first, then the
                // condition is tested â€” truthy jumps back to the body,
                // falsy falls out. `continue` inside the body jumps to the
                // condition, per JS.
                let ls = self.program.bytecode.len();
                self.loops.push(LoopCtx {
                    break_jumps: Vec::new(),
                    continue_jumps: Vec::new(),
                    continue_target: 0,
                    trys_depth: self.trys.len(),
                    label: self.pending_label.take(),
                    is_switch: false,
                });
                self.emit_stmt(body)?;
                let cond_pos = self.program.bytecode.len();
                let mut exit_jumps = Vec::new();
                let mut body_jumps = Vec::new();
                self.emit_cond(cond, &mut exit_jumps, &mut body_jumps)?;
                self.program.emit_op(Opcode::Jump);
                self.program.emit_u32(ls as u32);
                let mut lc = self.loops.pop().unwrap();
                for j in exit_jumps { self.patch_jump(j); }
                for j in body_jumps { self.patch_jump_to(j, ls); }
                self.finish_loop(&mut lc, cond_pos);
            }
            Stmt::For { init, cond, update, body } => {
                if let Some(i) = init { self.emit_stmt(i)?; }
                let ls = self.program.bytecode.len();
                if let Some(c) = cond {
                    let mut exit_jumps = Vec::new();
                    let mut body_jumps = Vec::new();
                    self.emit_cond(c, &mut exit_jumps, &mut body_jumps)?;
                    let body_pos = self.program.bytecode.len();
                    self.loops.push(LoopCtx {
                        break_jumps: Vec::new(),
                        continue_jumps: Vec::new(),
                        continue_target: 0,
                        trys_depth: self.trys.len(),
                        label: self.pending_label.take(),
                        is_switch: false,
                    });
                    self.emit_stmt(body)?;
                    let mut lc = self.loops.pop().unwrap();
                    let update_pos = self.program.bytecode.len();
                    if let Some(u) = update { self.emit_stmt_expr(u)?; }
                    self.program.emit_op(Opcode::Jump);
                    self.program.emit_u32(ls as u32);
                    for j in exit_jumps { self.patch_jump(j); }
                    for j in body_jumps { self.patch_jump_to(j, body_pos); }
                    self.finish_loop(&mut lc, update_pos);
                } else {
                    self.loops.push(LoopCtx {
                        break_jumps: Vec::new(),
                        continue_jumps: Vec::new(),
                        continue_target: 0,
                        trys_depth: self.trys.len(),
                        label: self.pending_label.take(),
                        is_switch: false,
                    });
                    self.emit_stmt(body)?;
                    let mut lc = self.loops.pop().unwrap();
                    let update_pos = self.program.bytecode.len();
                    if let Some(u) = update { self.emit_stmt_expr(u)?; }
                    self.program.emit_op(Opcode::Jump);
                    self.program.emit_u32(ls as u32);
                    self.finish_loop(&mut lc, update_pos);
                }
            }
            Stmt::ForOf { pat, declared, iterable, body } => {
                self.emit_for_iter(pat, *declared, iterable, body, false)?;
            }
            Stmt::ForIn { pat, declared, obj, body } => {
                self.emit_for_iter(pat, *declared, obj, body, true)?;
            }
            Stmt::Block(stmts) => {
                // Function declarations hoist (JS semantics): pre-allocate a
                // slot for every function declared directly in this block, so
                // a function referencing a sibling declared later in the same
                // block (or mutual recursion between siblings) resolves it as
                // a local/upvalue instead of a global. The FnDecl emission
                // reuses the pre-allocated slot via rposition.
                for st in stmts.iter() {
                    let mut s = st;
                    while let Stmt::Loc { stmt, .. } = s {
                        s = stmt;
                    }
                    if let Stmt::FnDecl { name, .. } = s {
                        if !self.funcs.last().unwrap().locals.contains(name) {
                            self.funcs.last_mut().unwrap().locals.push(name.clone());
                        }
                    }
                }
                // Local slots are intentionally not reclaimed here: truncating
                // would let a later variable reuse a slot that still holds a
                // captured cell, corrupting closure state.
                for st in stmts { self.emit_stmt(st)?; }
            }
            Stmt::Break => self.emit_loop_exit(true, None)?,
            Stmt::Continue => self.emit_loop_exit(false, None)?,
            Stmt::BreakLabel(l) => self.emit_loop_exit(true, Some(l))?,
            Stmt::ContinueLabel(l) => self.emit_loop_exit(false, Some(l))?,
            Stmt::Labeled { name, body } => {
                let mut b = body.as_ref();
                while let Stmt::Loc { stmt, .. } = b {
                    b = stmt;
                }
                let is_loop = matches!(
                    b,
                    Stmt::While { .. }
                        | Stmt::For { .. }
                        | Stmt::ForOf { .. }
                        | Stmt::ForIn { .. }
                );
                self.labels.push(LabelCtx {
                    name: name.clone(),
                    is_loop,
                    trys_depth: self.trys.len(),
                    break_jumps: Vec::new(),
                    continue_jumps: Vec::new(),
                    continue_target: 0,
                });
                if is_loop {
                    self.pending_label = Some(name.clone());
                }
                self.emit_stmt(body)?;
                let lc = self.labels.pop().unwrap();
                for b in &lc.break_jumps {
                    self.patch_jump(*b);
                }
            }
            Stmt::Throw(e) => {
                self.emit_expr(e)?;
                self.program.emit_op(Opcode::Throw);
            }
            Stmt::Try { body, catch, finally } => {
                self.trys.push(TryCtx { finally: finally.clone() });
                let handler_off = self.emit_jump(Opcode::TryStart);
                self.emit_stmt(body)?;
                // Drop this try's context (an exit via break/continue/return
                // already popped it and deactivated the handler; the normal
                // path needs its own TryEnd below).
                self.trys.pop();
                self.program.emit_op(Opcode::TryEnd);
                // Normal completion: run finally, then skip the handler.
                if let Some(f) = finally.as_ref() {
                    self.emit_finally(f)?;
                }
                let end = self.emit_jump(Opcode::Jump);
                // Handler entry: the thrown value is on the stack. Catch/stash
                // slots are allocated HERE (after the try body) so that a
                // nested `catch (e)` inside the body resolves `e` to its own
                // slot: each handler binds the most recently pushed local.
                self.patch_jump_to(handler_off, self.program.bytecode.len());
                let push_slot = |compiler: &mut Compiler, name: &str| -> u8 {
                    let s = compiler.funcs.last_mut().unwrap().locals.len() as u8;
                    compiler.funcs.last_mut().unwrap().locals.push(name.to_string());
                    s
                };
                match (catch.as_ref(), finally.as_ref()) {
                    (Some((name, c)), Some(f)) => {
                        if name.is_empty() {
                            self.program.emit_op(Opcode::Pop);
                        } else {
                            let cs = push_slot(self, name);
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(cs);
                        }
                        // The catch body runs under a finally-protection
                        // handler, so its own throws (and returns/breaks via
                        // the inline emission) still trigger the finally.
                        let fb = self.emit_jump(Opcode::TryStart);
                        self.trys.push(TryCtx { finally: finally.clone() });
                        self.emit_stmt(c)?;
                        self.trys.pop();
                        self.program.emit_op(Opcode::TryEnd);
                        self.emit_finally(f)?;
                        let bdone = self.emit_jump(Opcode::Jump);
                        // The catch body threw: run the finally, then rethrow.
                        self.patch_jump_to(fb, self.program.bytecode.len());
                        let fs = push_slot(self, "\u{0}stash");
                        self.program.emit_op(Opcode::StoreLocal);
                        self.program.emit_u8(fs);
                        self.emit_finally(f)?;
                        self.program.emit_op(Opcode::LoadLocal);
                        self.program.emit_u8(fs);
                        self.program.emit_op(Opcode::Throw);
                        self.patch_jump(bdone);
                    }
                    (Some((name, c)), None) => {
                        if name.is_empty() {
                            self.program.emit_op(Opcode::Pop);
                        } else {
                            let cs = push_slot(self, name);
                            self.program.emit_op(Opcode::StoreLocal);
                            self.program.emit_u8(cs);
                        }
                        self.emit_stmt(c)?;
                    }
                    (None, Some(f)) => {
                        let fs = push_slot(self, "\u{0}stash");
                        self.program.emit_op(Opcode::StoreLocal);
                        self.program.emit_u8(fs);
                        self.emit_finally(f)?;
                        self.program.emit_op(Opcode::LoadLocal);
                        self.program.emit_u8(fs);
                        self.program.emit_op(Opcode::Throw);
                    }
                    (None, None) => unreachable!(),
                }
                self.patch_jump(end);
            }
            Stmt::Switch { disc, cases } => {
                // The discriminant rides on the operand stack through the
                // dispatch section; each test Dups it, so it survives from
                // one comparison to the next until a match discards it.
                self.emit_expr(disc)?;
                self.loops.push(LoopCtx {
                    break_jumps: Vec::new(),
                    continue_jumps: Vec::new(),
                    continue_target: 0,
                    trys_depth: self.trys.len(),
                    label: None,
                    is_switch: true,
                });
                let default_idx = cases.iter().position(|c| c.test.is_none());
                // Dispatch: on a match, drop the discriminant and jump to that
                // arm's body; on a mismatch, keep it and test the next arm.
                let mut arm_jumps = Vec::new();
                for c in cases {
                    if let Some(t) = &c.test {
                        self.program.emit_op(Opcode::Dup);
                        self.emit_expr(t)?;
                        self.program.emit_op(Opcode::StrictEqual);
                        // JumpIfFalse leaves its condition on the stack, so
                        // both paths pop it; the false path keeps the
                        // discriminant for the next test.
                        let next = self.emit_jump(Opcode::JumpIfFalse);
                        self.program.emit_op(Opcode::Pop);
                        self.program.emit_op(Opcode::Pop);
                        arm_jumps.push(self.emit_jump(Opcode::Jump));
                        self.patch_jump(next);
                        self.program.emit_op(Opcode::Pop);
                    }
                }
                // No case matched: drop the discriminant and jump to the
                // default arm, or past the switch.
                self.program.emit_op(Opcode::Pop);
                let end_or_default = match default_idx {
                    Some(_) => self.emit_jump(Opcode::Jump), // patched to default body
                    None => self.emit_jump(Opcode::Jump),    // patched to the end
                };
                // Bodies in source order; an empty arm falls through to the
                // next, giving JS fall-through between consecutive arms.
                let mut arm_offsets = Vec::new();
                for c in cases {
                    arm_offsets.push(self.program.bytecode.len());
                    for s in &c.body {
                        self.emit_stmt(s)?;
                    }
                }
                match default_idx {
                    Some(d) => self.patch_jump_to(end_or_default, arm_offsets[d]),
                    None => self.patch_jump(end_or_default),
                }
                for (i, j) in arm_jumps.iter().enumerate() {
                    self.patch_jump_to(*j, arm_offsets[i]);
                }
                let lc = self.loops.pop().unwrap();
                for b in &lc.break_jumps {
                    self.patch_jump(*b);
                }
            }
            Stmt::Import { src, kind } => match kind {
                ImportKind::Core(names) => {
                    for n in names { self.resolve_global(n); }
                }
                ImportKind::Python(bind) => {
                    // `import { f } from './x.py' as python`: bind the alias
                    // (or first name) to a module object whose properties are
                    // natives for the file's top-level functions, served by
                    // the Python sidecar over the shared segment.
                    if !bind.is_empty() {
                        let idx = self.resolve_global(bind);
                        let ci = self.program.add_constant(Value::string(src.clone()));
                        self.program.emit_op(Opcode::LoadPython);
                        self.program.emit_u16(ci);
                        self.program.emit_op(Opcode::StoreGlobal);
                        self.program.emit_u16(idx);
                    }
                }
                ImportKind::ModuleNamed(pairs) => {
                    // `import { f, g as h } from './x.ajs'`: sugar for
                    // `const m = require(src)` then bind each exported name
                    // to a property of m. Live bindings: GetPropertyCell
                    // returns the RAW cell (the module's own storage), so
                    // the bound global aliases it and LoadGlobal unwraps to
                    // the current value on every read â€” ESM semantics.
                    self.emit_require_call(src)?;
                    for (exported, local) in pairs {
                        self.imported_bindings.insert(local.clone());
                        let idx = self.resolve_global(local);
                        let ci = self.program.add_constant(Value::string(exported.clone()));
                        self.program.emit_op(Opcode::Dup);
                        self.program.emit_op(Opcode::GetPropertyCell);
                        self.program.emit_u16(ci);
                        self.program.emit_op(Opcode::StoreGlobal);
                        self.program.emit_u16(idx);
                    }
                    self.program.emit_op(Opcode::Pop);
                }
                ImportKind::ModuleNamespace(bind) => {
                    // `import * as m`: m aliases the whole exports object,
                    // whose properties are the module's cells â€” reads of
                    // `m.x` are live. Reassigning m is an ESM SyntaxError.
                    self.imported_bindings.insert(bind.clone());
                    let idx = self.resolve_global(bind);
                    self.emit_require_call(src)?;
                    self.program.emit_op(Opcode::StoreGlobal);
                    self.program.emit_u16(idx);
                }
                ImportKind::ModuleDefault(bind) => {
                    // `import d from`: the default export is a value
                    // (ESM's default import is not a live binding), so the
                    // ordinary snapshot read is correct.
                    self.imported_bindings.insert(bind.clone());
                    let idx = self.resolve_global(bind);
                    self.emit_require_call(src)?;
                    let ci = self.program.add_constant(Value::string("default".to_string()));
                    self.program.emit_op(Opcode::GetProperty);
                    self.program.emit_u16(ci);
                    self.program.emit_op(Opcode::StoreGlobal);
                    self.program.emit_u16(idx);
                }
                ImportKind::ModuleSideEffect => {
                    self.emit_require_call(src)?;
                    self.program.emit_op(Opcode::Pop);
                }
            },
            Stmt::Export { pairs, stmt, default } => {
                if !self.in_module {
                    // Node also rejects `export` outside an ES module.
                    return Err(CompileError::UnexpectedToken(
                        "'export' is only allowed in a module (a file loaded with require())"
                            .to_string(),
                    ));
                }
                // `export default expr`: evaluate the expression and store it
                // under the reserved name, then record the export.
                if *default {
                    let idx = self.resolve_global(DEFAULT_EXPORT);
                    let mut s = &**stmt;
                    while let Stmt::Loc { stmt, .. } = s {
                        s = stmt;
                    }
                    if let Stmt::Expr(e) = s {
                        self.emit_expr(e)?;
                    }
                    self.program.emit_op(Opcode::StoreGlobal);
                    self.program.emit_u16(idx);
                } else {
                    self.emit_stmt(stmt)?;
                }
                for (name, binding) in pairs {
                    if !self.program.exports.iter().any(|(n, _)| n == name) {
                        self.program.exports.push((name.clone(), binding.clone()));
                    }
                }
            }
            Stmt::Nop => {}
        }
        Ok(())
    }

    /// Emit `for (name of/iterable)` as an indexed while-loop, so break and
    /// continue inside the body behave like any other loop. `is_in` iterates
    /// object keys instead of array elements.
    fn emit_for_iter(
        &mut self,
        pat: &Pat,
        declared: bool,
        source: &Expr,
        body: &Stmt,
        is_in: bool,
    ) -> Result<(), CompileError> {
        let arr = self.fresh_name("arr");
        let idx = self.fresh_name("i");
        let len = self.fresh_name("len");
        for t in [&arr, &idx, &len] {
            self.funcs.last_mut().unwrap().locals.push(t.clone());
        }
        let arr_slot = (self.funcs.last().unwrap().locals.len() - 3) as u8;
        let idx_slot = (self.funcs.last().unwrap().locals.len() - 2) as u8;
        let len_slot = (self.funcs.last().unwrap().locals.len() - 1) as u8;

        // arr = source (or Object.keys(source) for for-in). A for-of over a
        // Map/Set first converts the container to its iteration snapshot
        // (entry pairs / elements) â€” the synthetic iterator â€” so the index
        // loop below iterates it like any array.
        self.emit_expr(source)?;
        if is_in {
            self.program.emit_op(Opcode::GetKeys);
        } else {
            self.program.emit_op(Opcode::ToIterable);
        }
        self.program.emit_op(Opcode::StoreLocal);
        self.program.emit_u8(arr_slot);

        // idx = 0
        self.program.emit_op(Opcode::LoadInt);
        self.program.emit_u32(0);
        self.program.emit_op(Opcode::StoreLocal);
        self.program.emit_u8(idx_slot);

        // len = arr.length (GetProperty reads its prop from the operand)
        self.program.emit_op(Opcode::LoadLocal);
        self.program.emit_u8(arr_slot);
        let lc = self.program.add_constant(Value::string("length".to_string()));
        self.program.emit_op(Opcode::GetProperty);
        self.program.emit_u16(lc);
        self.program.emit_op(Opcode::StoreLocal);
        self.program.emit_u8(len_slot);

        // while idx < len
        let ls = self.program.bytecode.len();
        self.program.emit_op(Opcode::LoadLocal);
        self.program.emit_u8(idx_slot);
        self.program.emit_op(Opcode::LoadLocal);
        self.program.emit_u8(len_slot);
        self.program.emit_op(Opcode::Less);
        let ej = self.emit_jump(Opcode::JumpIfFalsePop);

        // Bind the loop pattern from arr[idx]: stash the element once, then
        // destructure it (a plain name stores directly, patterns recurse).
        self.program.emit_op(Opcode::LoadLocal);
        self.program.emit_u8(arr_slot);
        self.program.emit_op(Opcode::LoadLocal);
        self.program.emit_u8(idx_slot);
        self.program.emit_op(Opcode::GetIndex);
        let tmp = self.fresh_local();
        self.program.emit_op(Opcode::StoreLocal);
        self.program.emit_u8(tmp);
        let mode = if declared { PatStoreMode::Declare } else { PatStoreMode::Assign };
        self.emit_pattern_store(pat, tmp, mode)?;

        self.loops.push(LoopCtx {
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
            continue_target: 0,
            trys_depth: self.trys.len(),
            label: self.pending_label.take(),
            is_switch: false,
        });
        self.emit_stmt(body)?;
        let mut lc = self.loops.pop().unwrap();

        // idx += 1
        let update_pos = self.program.bytecode.len();
        self.program.emit_op(Opcode::LoadLocal);
        self.program.emit_u8(idx_slot);
        self.program.emit_op(Opcode::LoadInt);
        self.program.emit_u32(1);
        self.program.emit_op(Opcode::Add);
        self.program.emit_op(Opcode::Dup);
        self.program.emit_op(Opcode::StoreLocal);
        self.program.emit_u8(idx_slot);
        self.program.emit_op(Opcode::Pop);

        self.program.emit_op(Opcode::Jump);
        self.program.emit_u32(ls as u32);
        self.patch_jump(ej);
        self.finish_loop(&mut lc, update_pos);
        Ok(())
    }

    /// Patch a loop's break/continue jumps once the loop is fully emitted.
    /// `continue_target` is the bytecode offset the loop's `continue` jumps to
    /// (the condition for `while`, the update expression for `for`/for-of).
    /// If the loop carries a label, its `continue` jumps are patched too.
    fn finish_loop(&mut self, lc: &mut LoopCtx, continue_target: usize) {
        lc.continue_target = continue_target;
        for b in &lc.break_jumps {
            self.patch_jump(*b);
        }
        for c in &lc.continue_jumps {
            self.patch_jump_to(*c, continue_target);
        }
        if let Some(l) = lc.label.take() {
            if let Some(li) = self.labels.iter().rposition(|x| x.name == l) {
                self.labels[li].continue_target = continue_target;
                let jumps = std::mem::take(&mut self.labels[li].continue_jumps);
                for c in jumps {
                    self.patch_jump_to(c, continue_target);
                }
            }
        }
    }

    /// Emit the machinery for `break` (is_break = true) or `continue` out of a
    /// loop, optionally targeting a label. Runs the finallys of trys between
    /// the exit site and the target (but never trys enclosing the target), and
    /// records the jump for patching at the target's end.
    fn emit_loop_exit(&mut self, is_break: bool, label: Option<&str>) -> Result<(), CompileError> {
        let trys_depth;
        let target: ExitTarget;
        if let Some(l) = label {
            let idx = self.labels.iter().rposition(|lc| lc.name == l)
                .ok_or_else(|| CompileError::UndefinedLabel(l.to_string()))?;
            if !is_break && !self.labels[idx].is_loop {
                return Err(CompileError::ContinueNonLoop(l.to_string()));
            }
            trys_depth = self.labels[idx].trys_depth;
            target = ExitTarget::Label(idx);
        } else if is_break {
            // `break` targets the innermost break target (loop or switch).
            if self.loops.is_empty() {
                return Err(CompileError::BreakOutsideLoop);
            }
            let idx = self.loops.len() - 1;
            trys_depth = self.loops[idx].trys_depth;
            target = ExitTarget::Loop(idx);
        } else {
            // `continue` targets the innermost *loop*; switches are break
            // targets only, so an unlabeled `continue` skips past them to the
            // enclosing loop (or errors at the top level).
            match self.loops.iter().rposition(|lc| !lc.is_switch) {
                Some(idx) => {
                    trys_depth = self.loops[idx].trys_depth;
                    target = ExitTarget::Loop(idx);
                }
                None => return Err(CompileError::BreakOutsideLoop),
            }
        }
        while self.trys.len() > trys_depth {
            let ctx = self.trys.pop().unwrap();
            self.program.emit_op(Opcode::TryEnd);
            if let Some(f) = ctx.finally {
                self.emit_finally(&f)?;
            }
        }
        let j = self.emit_jump(Opcode::Jump);
        match target {
            ExitTarget::Loop(idx) => {
                if is_break {
                    self.loops[idx].break_jumps.push(j);
                } else {
                    self.loops[idx].continue_jumps.push(j);
                }
            }
            ExitTarget::Label(i) => {
                if is_break {
                    self.labels[i].break_jumps.push(j);
                } else {
                    self.labels[i].continue_jumps.push(j);
                }
            }
        }
        Ok(())
    }

    pub fn compile_source(source: &str) -> Result<Program, CompileError> {
        Self::compile_source_with_mode(source, false, false)
    }

    /// Compile a module file (loaded by `require`): top-level declarations
    /// become globals (so exports are readable after the file finishes), and
    /// `export` declarations are legal and recorded on the program.
    pub fn compile_module(source: &str) -> Result<Program, CompileError> {
        let mut p = Self::compile_source_with_mode(source, true, true)?;
        p.is_module = true;
        Ok(p)
    }

    pub fn compile_source_with_mode(
        source: &str,
        top_level_globals: bool,
        in_module: bool,
    ) -> Result<Program, CompileError> {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.tokenize()?;
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_program()?;
        let mut c = Compiler::new();
        c.top_globals = top_level_globals;
        c.in_module = in_module;
        // Hoist top-level function declarations too, so mutual recursion
        // between them resolves each name to its local slot. (Only when
        // top-level declarations stay locals: in top-globals/REPL mode the
        // FnDecl emitter stores them as globals, so hoisting them into locals
        // would make call sites load an uninitialized slot.)
        if !top_level_globals {
            for s in &ast {
                let mut st = s;
                while let Stmt::Loc { stmt, .. } = st {
                    st = stmt;
                }
                if let Stmt::FnDecl { name, .. } = st {
                    if !c.funcs[0].locals.contains(name) {
                        c.funcs[0].locals.push(name.clone());
                    }
                }
            }
        }
        // Constants (string/array/object literals) allocate into the
        // program's own arena while emitting.
        let heap_ptr: *mut ArenaHeap = &mut c.program.heap;
        let _g = HeapGuard::set(heap_ptr);
        for s in &ast {
            c.emit_stmt(s)?;
        }
        // Every export binding must be declared somewhere in the module
        // (Node: "Export 'x' is not defined" at link time). Only checked in
        // module mode â€” ordinary scripts already reject `export` entirely.
        if c.in_module {
            for s in &ast {
                let mut st = s;
                while let Stmt::Loc { stmt, .. } = st {
                    st = stmt;
                }
                if let Stmt::Export { pairs, .. } = st {
                    for (_, binding) in pairs {
                        if binding != DEFAULT_EXPORT
                            && !c.program.globals.contains(binding)
                        {
                            return Err(CompileError::UndefinedVariable(
                                binding.clone(),
                            ));
                        }
                    }
                }
            }
        }
        c.program.emit_op(Opcode::Halt);
        // Fuse the hottest stack-op sequences the AST-level fusion misses
        // (`3 * n`, `(i + j) % 7`, constant folds), rebasing jump targets.
        c.program.peephole();
        // Reclaim the transient constant allocations (failed interns,
        // intermediate strings) and intern the surviving constants into the
        // program's own old generation.
        c.program.compact_constants();
        Ok(c.program)
    }

    /// Whether `src` looks like a complete unit of input: brackets balanced and
    /// no unterminated string, template literal, or comment. The REPL uses this
    /// to decide whether to evaluate a line now or keep reading continuation
    /// lines. A *mismatched* closing bracket counts as complete (the compiler
    /// will report it) so the REPL never buffers forever on bad input.
    pub fn input_is_complete(src: &str) -> bool {
        let chars: Vec<char> = src.chars().collect();
        let mut i = 0usize;
        let mut delims: Vec<char> = Vec::new(); // ( [ {
        enum Mode {
            Code,
            Str(char),
            Tmpl,
            /// Inside a template's `${ ... }` interpolation.
            TmplExpr,
            LineComment,
            BlockComment,
        }
        let mut mode = Mode::Code;
        let mut tmpl_expr_depth = 0usize;
        while i < chars.len() {
            let c = chars[i];
            match &mode {
                Mode::LineComment => {
                    if c == '\n' {
                        mode = Mode::Code;
                    }
                    i += 1;
                }
                Mode::BlockComment => {
                    if c == '*' && chars.get(i + 1) == Some(&'/') {
                        mode = Mode::Code;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                Mode::Str(q) => {
                    if c == '\\' {
                        i += 2;
                    } else if c == *q {
                        mode = Mode::Code;
                        i += 1;
                    } else {
                        i += 1;
                    }
                }
                Mode::Tmpl => {
                    if c == '\\' {
                        i += 2;
                    } else if c == '`' {
                        mode = Mode::Code;
                        i += 1;
                    } else if c == '$' && chars.get(i + 1) == Some(&'{') {
                        tmpl_expr_depth = 1;
                        mode = Mode::TmplExpr;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                Mode::TmplExpr => {
                    match c {
                        '{' => {
                            tmpl_expr_depth += 1;
                            i += 1;
                        }
                        '}' => {
                            tmpl_expr_depth -= 1;
                            if tmpl_expr_depth == 0 {
                                mode = Mode::Tmpl;
                            }
                            i += 1;
                        }
                        '"' | '\'' => {
                            mode = Mode::Str(c);
                            i += 1;
                        }
                        '`' => {
                            // Nested template inside an interpolation.
                            mode = Mode::Tmpl;
                            i += 1;
                        }
                        '/' => {
                            if chars.get(i + 1) == Some(&'/') {
                                mode = Mode::LineComment;
                                i += 2;
                            } else if chars.get(i + 1) == Some(&'*') {
                                mode = Mode::BlockComment;
                                i += 2;
                            } else {
                                i += 1;
                            }
                        }
                        _ => i += 1,
                    }
                }
                Mode::Code => {
                    match c {
                        '(' | '[' | '{' => {
                            delims.push(c);
                            i += 1;
                        }
                        ')' | ']' | '}' => {
                            let want = if c == ')' { '(' } else if c == ']' { '[' } else { '{' };
                            if delims.last() == Some(&want) {
                                delims.pop();
                            } else {
                                // Mismatched: complete, so the compiler can
                                // report the error instead of buffering forever.
                                return true;
                            }
                            i += 1;
                        }
                        '"' | '\'' => {
                            mode = Mode::Str(c);
                            i += 1;
                        }
                        '`' => {
                            mode = Mode::Tmpl;
                            i += 1;
                        }
                        '/' => {
                            if chars.get(i + 1) == Some(&'/') {
                                mode = Mode::LineComment;
                                i += 2;
                            } else if chars.get(i + 1) == Some(&'*') {
                                mode = Mode::BlockComment;
                                i += 2;
                            } else {
                                i += 1;
                            }
                        }
                        _ => i += 1,
                    }
                }
            }
        }
        // A line comment runs to the end of input, so it doesn't make the
        // unit incomplete; an unterminated block comment does.
        delims.is_empty() && matches!(mode, Mode::Code | Mode::LineComment)
    }
}

