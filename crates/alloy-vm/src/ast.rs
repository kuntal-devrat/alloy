use crate::compiler::token::{TemplatePart, Token};

#[derive(Debug, Clone)]
pub enum Expr {
    Num(f64),
    Int(i64),
    Str(String),
    Bool(bool),
    Null,
    Undef,
    Ident(String),
    Bin(&'static str, Box<Expr>, Box<Expr>),
    Unary(&'static str, Box<Expr>),
    /// `delete target`: member expressions delete the property/index, a plain
    /// identifier evaluates to false, anything else evaluates and yields true.
    Delete(Box<Expr>),
    Assign { target: Box<Expr>, op: &'static str, value: Box<Expr> },
    /// A call. `optional` is true when written `f?.()` — the args are not
    /// evaluated and the whole chain yields undefined when the callee is
    /// nullish.
    Call { callee: Box<Expr>, args: Vec<Elem>, optional: bool },
    /// A property read. `optional` is true when written `o?.p`.
    Prop { obj: Box<Expr>, prop: String, optional: bool },
    /// An index read. `optional` is true when written `o?.[k]`.
    Index { obj: Box<Expr>, index: Box<Expr>, optional: bool },
    Array(Vec<Elem>),
    Object(Vec<ObjElem>),
    Template(Vec<TemplatePart>),
    Lambda { params: FnParams, body: Box<Stmt>, is_async: bool, is_generator: bool, is_arrow: bool },
    Yield { value: Option<Box<Expr>>, delegate: bool },
    Await(Box<Expr>),
    Ternary { cond: Box<Expr>, then: Box<Expr>, els: Box<Expr> },
    IncDec { target: Box<Expr>, is_inc: bool, is_prefix: bool },
    /// Grouping parens `(expr)`: a thin wrapper the emitter unwraps. It exists
    /// so `(-2) ** 2` (a parenthesized unary, legal as the left operand of
    /// `**`) is distinguishable from the bare `-2 ** 2` SyntaxError.
    Paren(Box<Expr>),
    /// The comma operator `(a, b, c)`: evaluate each element left-to-right,
    /// the expression's value is the last one. Only parsed in full-expression
    /// contexts (parens, statements, return, conds); in argument/array/object/
    /// declarator lists the comma stays a separator.
    Sequence(Vec<Expr>),
    /// `new C(args)` — allocate an instance (proto = `C.prototype`) and call
    /// the constructor with `this` bound to it.
    New { callee: Box<Expr>, args: Vec<Elem> },
    /// `super(args)` inside a derived class's constructor: call the parent
    /// constructor with the current `this`.
    SuperCall { args: Vec<Elem> },
    /// `super.m(args)` inside a method: look `m` up on the method's home
    /// object's parent prototype and call it with the current `this`.
    /// `args: None` is a bare `super.m` reference (no call).
    SuperProp { prop: String, args: Option<Vec<Elem>> },
    /// `class Name extends Parent { … }` / `class extends Parent { … }` —
    /// evaluates to the class (constructor) value.
    Class { name: Option<String>, extends: Option<Box<Expr>>, methods: Vec<MethodDef> },
    /// `/pattern/flags` — a fresh regex object per evaluation (its own
    /// `lastIndex`), like JS.
    Regex { pattern: String, flags: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    Normal,
    Getter,
    Setter,
    Field,
}

#[derive(Debug, Clone)]
pub struct MethodDef {
    pub name: String,
    pub is_static: bool,
    pub is_async: bool,
    pub is_generator: bool,
    pub kind: MethodKind,
    pub params: FnParams,
    pub body: Box<Stmt>,
    pub init: Option<Expr>,
}

/// One element of an array literal or call argument list; `spread` marks
/// `...e` and `hole` marks an elision (`[a, , b]`), which evaluates to
/// `undefined` in a literal and skips a position in an assignment target.
#[derive(Debug, Clone)]
pub struct Elem {
    pub spread: bool,
    pub hole: bool,
    pub expr: Expr,
}

/// Where a logical assignment (`&&=`, `||=`, `??=`) writes its result:
/// a named slot, or a member whose receiver (and index) were stashed in
/// fresh locals before the RHS ran.
#[derive(Clone, Copy)]
pub enum LStore {
    Local(u8),
    Upvalue(u8),
    Global(u16),
    /// (constant index of the property name, receiver temp slot)
    Prop(u16, u8),
    /// (receiver temp slot, index temp slot)
    Index(u8, u8),
}

/// One element of an object literal. `Pair` is a constant-key property;
/// `Computed` is `[expr]: value` (the key is evaluated at runtime and
/// coerced to a string); `Spread` is `...expr` (the source's own enumerable
/// properties are copied in order). Method shorthand `{ f() {} }` is parsed
/// into a `Pair` whose value is a non-arrow `Lambda`, so `this` binds via the
/// receiver like any method call.
#[derive(Debug, Clone)]
pub enum ObjElem {
    Pair(String, Expr),
    Computed(Expr, Expr),
    Spread(Expr),
    Getter(String, Expr),
    Setter(String, Expr),
}

/// One position in an array destructuring pattern: a hole skips an element, a
/// `Bind` reads one element, and the (last) `Rest` element collects the
/// remainder of the source into an array.
#[derive(Debug, Clone)]
pub enum PatElem {
    Hole,
    Bind(Pat),
    Rest(Pat),
}

/// One element of an OBJECT destructuring pattern: a constant key, a
/// computed key (`[expr]: v` — the key expression is evaluated at runtime),
/// or the (last) `Rest` element (`...rest` — the source's remaining own
/// enumerable properties).
#[derive(Debug, Clone)]
pub enum ObjPatElem {
    Key(String, Pat),
    Computed(Expr, Pat),
    Rest(Pat),
}

/// A destructuring pattern: `Bind(name)` binds a variable; `Object` matches
/// keys via `GetProperty`; `Array` matches indexes via `GetIndex`.
#[derive(Debug, Clone)]
pub enum Pat {
    Bind(String),
    Object(Vec<ObjPatElem>),
    Array(Vec<PatElem>),
}

/// One parameter: a binding pattern plus an optional default value
/// (`function f(a = 1, { b } = {}) {}` — the default is used when the
/// argument is `undefined`).
#[derive(Debug, Clone)]
pub struct ParamDef {
    pub pat: Pat,
    pub default: Option<Expr>,
}

/// A function's parameter list: one [`ParamDef`] per position (the rest
/// parameter, if any, is the last position — a plain `Bind`).
#[derive(Debug, Clone)]
pub struct FnParams {
    pub params: Vec<ParamDef>,
    pub rest: Option<usize>,
}

impl FnParams {
    /// The callee-frame local layout: one slot per parameter position
    /// (a `Bind` param IS its slot; a pattern param occupies a synthetic
    /// `\0param{i}` slot holding the raw argument), then the pattern-bound
    /// names, then — last — the rest param's slot.
    pub fn names(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (i, p) in self.params.iter().enumerate() {
            match &p.pat {
                Pat::Bind(n) => out.push(n.clone()),
                _ => out.push(format!("\u{0}param{}", i)),
            }
        }
        // Pattern params add their bound names; Bind params were already
        // pushed above — counting them again double-allocated the slot and
        // made `resolve`'s rposition hand back the duplicate (param read
        // one slot past the real argument).
        for p in &self.params {
            if matches!(p.pat, Pat::Bind(_)) {
                continue;
            }
            pat_bound_names(&p.pat, &mut out);
        }
        out
    }

    /// Fixed (non-rest) parameter count — what the VM pre-fills with
    /// undefined on a short call.
    pub fn pcount(&self) -> usize {
        self.params.len() - usize::from(self.rest.is_some())
    }

    /// The rest param's slot index (always the last local).
    pub fn rest_slot(&self) -> usize {
        self.names().len() - 1
    }
}

/// All names bound by a pattern, in order (used to lay out locals).
pub fn pat_bound_names(p: &Pat, out: &mut Vec<String>) {
    match p {
        Pat::Bind(n) => out.push(n.clone()),
        Pat::Object(elems) => {
            for el in elems {
                match el {
                    ObjPatElem::Key(_, sub) | ObjPatElem::Computed(_, sub) | ObjPatElem::Rest(sub) => {
                        pat_bound_names(sub, out)
                    }
                }
            }
        }
        Pat::Array(elems) => {
            for el in elems {
                match el {
                    PatElem::Bind(sub) | PatElem::Rest(sub) => pat_bound_names(sub, out),
                    PatElem::Hole => {}
                }
            }
        }
    }
}

/// Where pattern-bound names are stored: `Declare` creates a new
/// local/global (declaration); `Assign` stores into an existing target.
#[derive(Clone, Copy)]
pub enum PatStoreMode {
    Declare,
    Assign,
}

/// Whether `s` references `name` (`this` / `arguments`) at THIS function's
/// own level. Nested regular functions/methods have their own `this` and
/// `arguments`, so their bodies are skipped; nested ARROWS inherit this
/// function's bindings, so they are descended into (their hidden captures
/// chain to this function's).
pub fn stmt_uses_lexical(s: &Stmt, name: &str) -> bool {
    match s {
        Stmt::Loc { stmt, .. } => stmt_uses_lexical(stmt, name),
        Stmt::Expr(e) => expr_uses_lexical(e, name),
        Stmt::VarDecl { decls, .. } => decls
            .iter()
            .any(|(_, v)| v.as_ref().is_some_and(|e| expr_uses_lexical(e, name))),
        Stmt::Return(Some(e)) => expr_uses_lexical(e, name),
        Stmt::If { cond, then, els } => {
            expr_uses_lexical(cond, name)
                || stmt_uses_lexical(then, name)
                || els.as_ref().is_some_and(|e| stmt_uses_lexical(e, name))
        }
        Stmt::While { cond, body } | Stmt::DoWhile { cond, body } => {
            expr_uses_lexical(cond, name) || stmt_uses_lexical(body, name)
        }
        Stmt::For { init, cond, update, body } => {
            init.as_ref().is_some_and(|s| stmt_uses_lexical(s, name))
                || cond.as_ref().is_some_and(|e| expr_uses_lexical(e, name))
                || update.as_ref().is_some_and(|e| expr_uses_lexical(e, name))
                || stmt_uses_lexical(body, name)
        }
        Stmt::ForOf { iterable, body, .. } | Stmt::ForIn { obj: iterable, body, .. } => {
            expr_uses_lexical(iterable, name) || stmt_uses_lexical(body, name)
        }
        Stmt::Block(stmts) => stmts.iter().any(|s| stmt_uses_lexical(s, name)),
        Stmt::Labeled { body, .. } => stmt_uses_lexical(body, name),
        Stmt::Throw(e) => expr_uses_lexical(e, name),
        Stmt::Try { body, catch, finally } => {
            stmt_uses_lexical(body, name)
                || catch.as_ref().is_some_and(|(_, b)| stmt_uses_lexical(b, name))
                || finally.as_ref().is_some_and(|b| stmt_uses_lexical(b, name))
        }
        Stmt::Switch { disc, cases } => {
            expr_uses_lexical(disc, name)
                || cases.iter().any(|c| c.body.iter().any(|s| stmt_uses_lexical(s, name)))
        }
        // Own `this`/`arguments`.
        Stmt::FnDecl { .. } | Stmt::Class { .. } => false,
        _ => false,
    }
}

pub fn expr_uses_lexical(e: &Expr, name: &str) -> bool {
    match e {
        Expr::Ident(n) => n == name,
        Expr::Bin(_, l, r) => expr_uses_lexical(l, name) || expr_uses_lexical(r, name),
        Expr::Unary(_, x) | Expr::Await(x) | Expr::Delete(x) | Expr::Paren(x) => {
            expr_uses_lexical(x, name)
        }
        Expr::Yield { value, .. } => value.as_ref().is_some_and(|v| expr_uses_lexical(v, name)),
        Expr::Assign { target, value, .. } => {
            expr_uses_lexical(target, name) || expr_uses_lexical(value, name)
        }
        Expr::Call { callee, args, .. } => {
            expr_uses_lexical(callee, name) || args.iter().any(|a| expr_uses_lexical(&a.expr, name))
        }
        Expr::Prop { obj, .. } | Expr::IncDec { target: obj, .. } => expr_uses_lexical(obj, name),
        Expr::Index { obj, index, .. } => expr_uses_lexical(obj, name) || expr_uses_lexical(index, name),
        Expr::Array(elems) => elems.iter().any(|a| expr_uses_lexical(&a.expr, name)),
        Expr::Object(fields) => fields.iter().any(|f| match f {
            ObjElem::Pair(_, v) => expr_uses_lexical(v, name),
            ObjElem::Computed(k, v) => expr_uses_lexical(k, name) || expr_uses_lexical(v, name),
            ObjElem::Spread(s) => expr_uses_lexical(s, name),
            ObjElem::Getter(_, v) | ObjElem::Setter(_, v) => expr_uses_lexical(v, name),
        }),
        Expr::Template(parts) => parts.iter().any(|p| match p {
            TemplatePart::Lit(_) => false,
            // Interpolations are raw tokens; a conservative token scan
            // (a false positive only costs an extra hidden capture).
            TemplatePart::Expr(ts) => {
                ts.tokens.iter().any(|t| matches!(t, Token::Ident(n) if n == name))
            }
        }),
        Expr::Ternary { cond, then, els } => {
            expr_uses_lexical(cond, name)
                || expr_uses_lexical(then, name)
                || expr_uses_lexical(els, name)
        }
        Expr::Sequence(es) => es.iter().any(|x| expr_uses_lexical(x, name)),
        Expr::New { callee, args } => {
            expr_uses_lexical(callee, name) || args.iter().any(|a| expr_uses_lexical(&a.expr, name))
        }
        Expr::SuperCall { args } | Expr::SuperProp { args: Some(args), .. } => {
            args.iter().any(|a| expr_uses_lexical(&a.expr, name))
        }
        Expr::SuperProp { args: None, .. } => false,
        // Nested arrows inherit this function's bindings; regular functions
        // (and class bodies/methods) bind their own.
        Expr::Lambda { body, is_arrow, .. } => *is_arrow && stmt_uses_lexical(body, name),
        Expr::Class { .. } => false,
        _ => false,
    }
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Expr(Expr),
    /// `let a = 1, { b, c } = obj` — one or more declarators.
    VarDecl { decls: Vec<(Pat, Option<Expr>)> },
    FnDecl { name: String, params: FnParams, body: Box<Stmt>, is_async: bool, is_generator: bool },
    Return(Option<Expr>),
    If { cond: Expr, then: Box<Stmt>, els: Option<Box<Stmt>> },
    While { cond: Expr, body: Box<Stmt> },
    DoWhile { cond: Expr, body: Box<Stmt> },
    For { init: Option<Box<Stmt>>, cond: Option<Expr>, update: Option<Expr>, body: Box<Stmt> },
    ForOf { pat: Pat, declared: bool, iterable: Expr, body: Box<Stmt> },
    ForIn { pat: Pat, declared: bool, obj: Expr, body: Box<Stmt> },
    Import { src: String, kind: ImportKind },
    Block(Vec<Stmt>),
    Break,
    Continue,
    BreakLabel(String),
    ContinueLabel(String),
    Labeled { name: String, body: Box<Stmt> },
    Throw(Expr),
    Try {
        body: Box<Stmt>,
        catch: Option<(String, Box<Stmt>)>,
        finally: Option<Box<Stmt>>,
    },
    Switch { disc: Expr, cases: Vec<SwitchCase> },
    /// `class Name extends Parent { … }` — a class declaration. The name is
    /// registered like a function declaration; the value is the class.
    Class { name: String, extends: Option<Expr>, methods: Vec<MethodDef> },
    /// `export let a = 1, b = 2` / `export function f() {}` /
    /// `export { a, b }` / `export { a as c }` / `export default expr`.
    /// `pairs` is (public export name, source binding name) — an alias is
    /// `("c", "a")`, a plain declaration `("a", "a")`, and `export default`
    /// `("default", "\0default")`. `stmt` is the declaration to emit (Nop
    /// for the `export { ... }` forms); `default` marks the stored-value form.
    Export { pairs: Vec<(String, String)>, stmt: Box<Stmt>, default: bool },
    Loc { line: u32, col: u32, stmt: Box<Stmt> },
    Nop,
}

/// What an `import` statement binds, by source kind.
#[derive(Debug, Clone)]
pub enum ImportKind {
    /// `import { f } from 'alloy:core'` — bind each named builtin.
    Core(Vec<String>),
    /// `import { f } from './x.py' as python` — bind the alias (or first
    /// name) to the Python sidecar module object.
    Python(String),
    /// `import { f, g as h } from './x.ajs'` — (exported name, local binding)
    /// pairs, sugar for `require` + property extraction.
    ModuleNamed(Vec<(String, String)>),
    /// `import * as m from './x.ajs'` — bind the whole exports object.
    ModuleNamespace(String),
    /// `import d from './x.ajs'` — bind the module's default export.
    ModuleDefault(String),
    /// `import './x.ajs'` — run the module for its side effects only.
    ModuleSideEffect,
}

/// One `case value:` / `default:` arm of a switch: the (optional) test and
/// the statements until the next arm. A `test` of `None` is `default`.
#[derive(Debug, Clone)]
pub struct SwitchCase {
    pub test: Option<Expr>,
    pub body: Vec<Stmt>,
}

/// All identifier names bound by a destructuring pattern (`let { a, b } = o`
/// exports both `a` and `b`; `let [x, ...rest] = a` exports `x` and `rest`).
pub fn pat_names(p: &Pat) -> Vec<String> {
    match p {
        Pat::Bind(n) => vec![n.clone()],
        Pat::Object(fields) => fields.iter().flat_map(|e| match e {
            ObjPatElem::Key(_, p) | ObjPatElem::Computed(_, p) | ObjPatElem::Rest(p) => pat_names(p),
        }).collect(),
        Pat::Array(elems) => elems
            .iter()
            .flat_map(|e| match e {
                PatElem::Bind(p) | PatElem::Rest(p) => pat_names(p),
                PatElem::Hole => vec![],
            })
            .collect(),
    }
}
