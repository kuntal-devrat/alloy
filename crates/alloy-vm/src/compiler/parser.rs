use crate::ast::*;
use crate::compiler::error::CompileError;
use crate::compiler::scope::DEFAULT_EXPORT;
use crate::compiler::token::{keyword_text, starts_expression, Token, TokenStream};

const MAX_RECURSION_DEPTH: usize = 40;

pub struct Parser {
    tokens: Vec<Token>,
    lines: Vec<u32>,
    cols: Vec<u32>,
    pos: usize,
    depth: usize,
}

impl Parser {
    pub fn new(ts: TokenStream) -> Self {
        Self {
            tokens: ts.tokens,
            lines: ts.lines,
            cols: ts.cols,
            pos: 0,
            depth: 0,
        }
    }

    #[inline]
    pub fn cur_line(&self) -> u32 {
        self.lines.get(self.pos).copied().unwrap_or(1)
    }

    #[inline]
    pub fn cur_col(&self) -> u32 {
        self.cols.get(self.pos).copied().unwrap_or(1)
    }

    #[inline]
    pub fn cur_loc(&self) -> (u32, u32) {
        (self.cur_line(), self.cur_col())
    }

    pub fn peek(&self) -> &Token { self.tokens.get(self.pos).unwrap_or(&Token::Eof) }
    pub fn advance(&mut self) -> Token {
        let t = self.tokens.get(self.pos).cloned().unwrap_or(Token::Eof);
        self.pos += 1;
        t
    }

    pub fn parse_program(&mut self) -> Result<Vec<Stmt>, CompileError> {
        let mut s = Vec::new();
        while !matches!(self.peek(), Token::Eof) { s.push(self.parse_stmt()?); }
        Ok(s)
    }

    fn parse_stmt(&mut self) -> Result<Stmt, CompileError> {
        if self.depth >= MAX_RECURSION_DEPTH {
            return Err(CompileError::UnexpectedToken(
                "maximum recursion depth exceeded".to_string(),
            ));
        }
        self.depth += 1;
        let (line, col) = self.cur_loc();
        let stmt = self.parse_stmt_raw();
        self.depth -= 1;
        let stmt = stmt?;
        Ok(Stmt::Loc {
            line,
            col,
            stmt: Box::new(stmt),
        })
    }

    fn parse_stmt_raw(&mut self) -> Result<Stmt, CompileError> {
        match self.peek().clone() {
            Token::Let | Token::Const | Token::Var => self.parse_var_decl(),
            Token::Function => {
                self.advance();
                let is_gen = if matches!(self.peek(), Token::Star) {
                    self.advance();
                    true
                } else {
                    false
                };
                self.parse_fn_decl(false, is_gen)
            }
            Token::Async => {
                if matches!(self.tokens.get(self.pos + 1), Some(Token::Function)) {
                    self.advance(); // async
                    self.advance(); // function
                    let is_gen = if matches!(self.peek(), Token::Star) {
                        self.advance();
                        true
                    } else {
                        false
                    };
                    self.parse_fn_decl(true, is_gen)
                } else {
                    // `async x => ...` / `async () => ...` as an expression.
                    self.parse_expr_stmt()
                }
            }
            Token::Return => self.parse_return(),
            Token::If => self.parse_if(),
            Token::While => self.parse_while(),
            Token::Do => self.parse_do(),
            Token::For => self.parse_for(),
            Token::LBrace => self.parse_block(),
            Token::Import => {
                if matches!(self.tokens.get(self.pos + 1), Some(Token::LParen)) {
                    self.parse_expr_stmt()
                } else {
                    self.parse_import()
                }
            }
            Token::Break => {
                self.advance();
                if let Token::Ident(s) = self.peek().clone() {
                    self.advance();
                    if matches!(self.peek(), Token::Semicolon) { self.advance(); }
                    Ok(Stmt::BreakLabel(s))
                } else {
                    if matches!(self.peek(), Token::Semicolon) { self.advance(); }
                    Ok(Stmt::Break)
                }
            }
            Token::Continue => {
                self.advance();
                if let Token::Ident(s) = self.peek().clone() {
                    self.advance();
                    if matches!(self.peek(), Token::Semicolon) { self.advance(); }
                    Ok(Stmt::ContinueLabel(s))
                } else {
                    if matches!(self.peek(), Token::Semicolon) { self.advance(); }
                    Ok(Stmt::Continue)
                }
            }
            // `name: statement` â€” a labeled statement (loop labels can be
            // targeted by `break name` / `continue name`).
            Token::Ident(s) if matches!(self.tokens.get(self.pos + 1), Some(Token::Colon)) => {
                self.advance(); // ident
                self.advance(); // colon
                let body = self.parse_stmt()?;
                Ok(Stmt::Labeled { name: s, body: Box::new(body) })
            }
            Token::Throw => {
                self.advance();
                let e = self.parse_expr(0)?;
                if matches!(self.peek(), Token::Semicolon) { self.advance(); }
                Ok(Stmt::Throw(e))
            }
            Token::Try => self.parse_try(),
            Token::Switch => self.parse_switch(),
            Token::Export => self.parse_export(),
            Token::Class => self.parse_class_decl(),
            _ => self.parse_expr_stmt(),
        }
    }

    /// `export` declarations (module files loaded with `require`):
    ///   export let a = 1, b = 2;
    ///   export function f() {}
    ///   export async function g() {}
    ///   export { a, b };
    ///   export { a as c };
    ///   export default expr;
    /// Pairs are (public name, source binding); aliases differ in the two.
    /// The emitter records them in `program.exports`; the declaration itself
    /// compiles normally.
    fn parse_export(&mut self) -> Result<Stmt, CompileError> {
        self.advance(); // export
        // `export default expr` — stored under the reserved name `\0default`.
        if matches!(self.peek(), Token::Default) {
            self.advance();
            if matches!(self.peek(), Token::Function) {
                self.advance();
                let is_gen = if matches!(self.peek(), Token::Star) {
                    self.advance();
                    true
                } else {
                    false
                };
                let s = self.parse_fn_decl(false, is_gen)?;
                let name = match &s {
                    Stmt::FnDecl { name, .. } => name.clone(),
                    _ => DEFAULT_EXPORT.to_string(),
                };
                return Ok(Stmt::Export {
                    pairs: vec![("default".to_string(), name)],
                    stmt: Box::new(s),
                    default: false,
                });
            }
            if matches!(self.peek(), Token::Async)
                && matches!(self.tokens.get(self.pos + 1), Some(Token::Function))
            {
                self.advance(); // async
                self.advance(); // function
                let is_gen = if matches!(self.peek(), Token::Star) {
                    self.advance();
                    true
                } else {
                    false
                };
                let s = self.parse_fn_decl(true, is_gen)?;
                let name = match &s {
                    Stmt::FnDecl { name, .. } => name.clone(),
                    _ => DEFAULT_EXPORT.to_string(),
                };
                return Ok(Stmt::Export {
                    pairs: vec![("default".to_string(), name)],
                    stmt: Box::new(s),
                    default: false,
                });
            }
            if matches!(self.peek(), Token::Class) {
                let s = self.parse_class_decl()?;
                let name = match &s {
                    Stmt::Class { name, .. } => name.clone(),
                    _ => DEFAULT_EXPORT.to_string(),
                };
                return Ok(Stmt::Export {
                    pairs: vec![("default".to_string(), name)],
                    stmt: Box::new(s),
                    default: false,
                });
            }
            let e = self.parse_expr(0)?;
            if matches!(self.peek(), Token::Semicolon) { self.advance(); }
            return Ok(Stmt::Export {
                pairs: vec![("default".to_string(), DEFAULT_EXPORT.to_string())],
                stmt: Box::new(Stmt::Expr(e)),
                default: true,
            });
        }
        // `export { a, b as c }` — re-export existing bindings under names.
        if matches!(self.peek(), Token::LBrace) {
            self.advance();
            let mut pairs = Vec::new();
            while !matches!(self.peek(), Token::RBrace) {
                match self.advance() {
                    Token::Ident(s) => {
                        let binding = s;
                        let name = if matches!(self.peek(), Token::As) {
                            self.advance();
                            match self.advance() {
                                Token::Ident(a) => a,
                                t => {
                                    return Err(CompileError::UnexpectedToken(format!(
                                        "{:?}",
                                        t
                                    )))
                                }
                            }
                        } else {
                            binding.clone()
                        };
                        pairs.push((name, binding));
                    }
                    Token::Comma => {}
                    Token::Eof => {
                        return Err(CompileError::UnexpectedToken("RBrace".to_string()))
                    }
                    t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                }
            }
            if matches!(self.peek(), Token::RBrace) { self.advance(); }
            if matches!(self.peek(), Token::Semicolon) { self.advance(); }
            return Ok(Stmt::Export {
                pairs,
                stmt: Box::new(Stmt::Nop),
                default: false,
            });
        }
        // `export <declaration>`: let/const/var, function, async function, class.
        let (stmt, names) = match self.peek().clone() {
            Token::Let | Token::Const | Token::Var => {
                let s = self.parse_var_decl()?;
                let names = match &s {
                    Stmt::VarDecl { decls } => decls
                        .iter()
                        .flat_map(|(p, _)| pat_names(p))
                        .collect::<Vec<_>>(),
                    _ => Vec::new(),
                };
                (s, names)
            }
            Token::Function => {
                self.advance();
                let is_gen = if matches!(self.peek(), Token::Star) {
                    self.advance();
                    true
                } else {
                    false
                };
                let s = self.parse_fn_decl(false, is_gen)?;
                let names = match &s {
                    Stmt::FnDecl { name, .. } => vec![name.clone()],
                    _ => Vec::new(),
                };
                (s, names)
            }
            Token::Async
                if matches!(self.tokens.get(self.pos + 1), Some(Token::Function)) =>
            {
                self.advance(); // async
                self.advance(); // function
                let is_gen = if matches!(self.peek(), Token::Star) {
                    self.advance();
                    true
                } else {
                    false
                };
                let s = self.parse_fn_decl(true, is_gen)?;
                let names = match &s {
                    Stmt::FnDecl { name, .. } => vec![name.clone()],
                    _ => Vec::new(),
                };
                (s, names)
            }
            Token::Class => {
                let s = self.parse_class_decl()?;
                let names = match &s {
                    Stmt::Class { name, .. } => vec![name.clone()],
                    _ => Vec::new(),
                };
                (s, names)
            }
            t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
        };
        Ok(Stmt::Export {
            pairs: names.into_iter().map(|n| (n.clone(), n)).collect(),
            stmt: Box::new(stmt),
            default: false,
        })
    }

    fn parse_var_decl(&mut self) -> Result<Stmt, CompileError> {
        self.advance();
        let mut decls = Vec::new();
        loop {
            let pat = self.parse_pattern()?;
            let init = if matches!(self.peek(), Token::Assign) {
                self.advance();
                // Initializers are AssignmentExpressions: the comma between
                // declarators (`let a = 1, b = 2`) stays a separator.
                Some(self.parse_expr(1)?)
            } else { None };
            decls.push((pat, init));
            if matches!(self.peek(), Token::Comma) {
                self.advance();
                continue;
            }
            break;
        }
        if matches!(self.peek(), Token::Semicolon) { self.advance(); }
        Ok(Stmt::VarDecl { decls })
    }

    /// Parse a binding target: a plain name, or a destructuring pattern
    /// `{ a, b: c }` / `[x, , y]` with nested patterns.
    fn parse_pattern(&mut self) -> Result<Pat, CompileError> {
        match self.peek().clone() {
            Token::Ident(s) => {
                self.advance();
                Ok(Pat::Bind(s))
            }
            Token::LBrace => {
                self.advance();
                let mut fields = Vec::new();
                while !matches!(self.peek(), Token::RBrace | Token::Eof) {
                    if matches!(self.peek(), Token::DotDotDot) {
                        // `...rest` â€” must be the last element (a trailing
                        // comma after it is allowed).
                        self.advance();
                        let sub = self.parse_pattern()?;
                        if matches!(self.peek(), Token::Comma) {
                            self.advance();
                        }
                        if !matches!(self.peek(), Token::RBrace) {
                            return Err(CompileError::UnexpectedToken(
                                "rest element must be last in object pattern".to_string(),
                            ));
                        }
                        fields.push(ObjPatElem::Rest(sub));
                        break;
                    }
                    if matches!(self.peek(), Token::LBracket) {
                        // `[expr]: v` â€” computed key, evaluated at runtime.
                        self.advance();
                        let key = self.parse_expr(0)?;
                        if !matches!(self.peek(), Token::RBracket) {
                            return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                        }
                        self.advance();
                        if !matches!(self.peek(), Token::Colon) {
                            return Err(CompileError::UnexpectedToken(
                                "expected ':' after computed key".to_string(),
                            ));
                        }
                        self.advance();
                        let sub = self.parse_pattern()?;
                        fields.push(ObjPatElem::Computed(key, sub));
                        match self.peek() {
                            Token::Comma => { self.advance(); }
                            Token::RBrace => {}
                            t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                        }
                        continue;
                    }
                    // Constant keys are IdentifierNames: keywords are legal
                    // (`const { default: d } = o`).
                    let key = match self.advance() {
                        Token::StringLit(s) => s,
                        t => match keyword_text(&t) {
                            Some(s) => s,
                            None => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                        },
                    };
                    let sub = if matches!(self.peek(), Token::Colon) {
                        self.advance();
                        self.parse_pattern()?
                    } else {
                        Pat::Bind(key.clone())
                    };
                    fields.push(ObjPatElem::Key(key, sub));
                    match self.peek() {
                        Token::Comma => { self.advance(); }
                        Token::RBrace => {}
                        t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                    }
                }
                if !matches!(self.peek(), Token::RBrace) {
                    return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                }
                self.advance();
                Ok(Pat::Object(fields))
            }
            Token::LBracket => {
                self.advance();
                let mut elems = Vec::new();
                while !matches!(self.peek(), Token::RBracket | Token::Eof) {
                    if matches!(self.peek(), Token::Comma) {
                        elems.push(PatElem::Hole); // skip this element
                        self.advance();
                        continue;
                    }
                    if matches!(self.peek(), Token::DotDotDot) {
                        // `...rest` â€” must be the last element (a trailing
                        // comma after it is allowed).
                        self.advance();
                        let sub = self.parse_pattern()?;
                        if matches!(self.peek(), Token::Comma) {
                            self.advance();
                        }
                        if !matches!(self.peek(), Token::RBracket) {
                            return Err(CompileError::UnexpectedToken(
                                "rest element must be last in array pattern".to_string(),
                            ));
                        }
                        elems.push(PatElem::Rest(sub));
                        break;
                    }
                    elems.push(PatElem::Bind(self.parse_pattern()?));
                    match self.peek() {
                        Token::Comma => { self.advance(); }
                        Token::RBracket => {}
                        t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                    }
                }
                if !matches!(self.peek(), Token::RBracket) {
                    return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                }
                self.advance();
                Ok(Pat::Array(elems))
            }
            t => Err(CompileError::UnexpectedToken(format!("{:?}", t))),
        }
    }

    /// `import` statements:
    ///   import { f } from 'alloy:core';          // builtins
    ///   import { f } from './x.py' as python;    // python sidecar module
    ///   import { f, g as h } from './x.ajs';     // sugar for require
    ///   import * as m from './x.ajs';            // whole exports object
    ///   import d from './x.ajs';                 // default export
    ///   import './x.ajs';                        // side effects only
    fn parse_import(&mut self) -> Result<Stmt, CompileError> {
        self.advance(); // import
        let kind = if matches!(self.peek(), Token::LBrace) {
            self.advance();
            let mut pairs = Vec::new();
            while !matches!(self.peek(), Token::RBrace) {
                match self.advance() {
                    Token::Ident(s) => {
                        let exported = s;
                        let local = if matches!(self.peek(), Token::As) {
                            self.advance();
                            match self.advance() {
                                Token::Ident(a) => a,
                                t => {
                                    return Err(CompileError::UnexpectedToken(format!(
                                        "{:?}",
                                        t
                                    )))
                                }
                            }
                        } else {
                            exported.clone()
                        };
                        pairs.push((exported, local));
                    }
                    Token::Comma => {}
                    Token::Eof => {
                        return Err(CompileError::UnexpectedToken("RBrace".to_string()))
                    }
                    t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                }
            }
            if matches!(self.peek(), Token::RBrace) { self.advance(); }
            ImportKind::ModuleNamed(pairs)
        } else if matches!(self.peek(), Token::Star) {
            self.advance();
            if matches!(self.peek(), Token::As) { self.advance(); }
            match self.advance() {
                Token::Ident(s) => ImportKind::ModuleNamespace(s),
                t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
            }
        } else if matches!(self.peek(), Token::Ident(_)) {
            match self.advance() {
                Token::Ident(s) => ImportKind::ModuleDefault(s),
                t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
            }
        } else {
            ImportKind::ModuleSideEffect
        };
        // `import './x.ajs'` (side effects only) skips the `from` keyword;
        // every other form is `... from '<src>'`.
        let src = if matches!(self.peek(), Token::From) {
            self.advance();
            match self.advance() {
                Token::StringLit(s) => s,
                t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
            }
        } else {
            match self.advance() {
                Token::StringLit(s) => s,
                t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
            }
        };
        // `import { f } from './x.py' as python` â€” the module-object binding.
        let py_alias = if matches!(self.peek(), Token::As) {
            self.advance();
            match self.advance() {
                Token::Ident(s) => Some(s),
                t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
            }
        } else { None };
        if matches!(self.peek(), Token::Semicolon) { self.advance(); }
        let kind = if src == "alloy:core" || src == "alloy:fs" {
            // Builtins: the braced names (or a `* as m` namespace) bind
            // directly as globals seeded with the native.
            match kind {
                ImportKind::ModuleNamed(pairs) => {
                    ImportKind::Core(pairs.into_iter().map(|(_, l)| l).collect())
                }
                ImportKind::ModuleNamespace(n) => ImportKind::Core(vec![n]),
                _ => return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek()))),
            }
        } else if src.ends_with(".py") {
            ImportKind::Python(py_alias.unwrap_or_else(|| match kind {
                ImportKind::ModuleNamed(pairs) => {
                    pairs.first().map(|(_, l)| l.clone()).unwrap_or_default()
                }
                _ => String::new(),
            }))
        } else {
            kind
        };
        Ok(Stmt::Import { src, kind })
    }

    /// Parse a function declaration; the `function` keyword (and, if
    /// `is_async`, the preceding `async`, and `*` if generator) has already been consumed.
    fn parse_fn_decl(&mut self, is_async: bool, is_generator: bool) -> Result<Stmt, CompileError> {
        let name = match self.peek() {
            Token::Ident(_) => match self.advance() {
                Token::Ident(s) => s,
                _ => unreachable!(),
            },
            _ => DEFAULT_EXPORT.to_string(),
        };
        let params = self.parse_params()?;
        let body = self.parse_block()?;
        Ok(Stmt::FnDecl { name, params, body: Box::new(body), is_async, is_generator })
    }

    /// True when the parenthesized group at the current position is followed
    /// by `=>`, i.e. it is an arrow parameter list rather than a grouping.
    fn looks_like_arrow_params(&self) -> bool {
        let mut depth = 0usize;
        let mut i = self.pos;
        while let Some(t) = self.tokens.get(i) {
            match t {
                Token::LParen => depth += 1,
                Token::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        return matches!(self.tokens.get(i + 1), Some(Token::Arrow));
                    }
                }
                _ => {}
            }
            i += 1;
        }
        false
    }

    /// Arrow body: `=> expr` (implicit return) or `=> { ... }` (block).
    fn parse_arrow_body(&mut self) -> Result<Box<Stmt>, CompileError> {
        if matches!(self.peek(), Token::LBrace) {
            Ok(Box::new(self.parse_block()?))
        } else {
            // The implicit-return body is an AssignmentExpression, so a comma
            // ends it (`x => a, b` is `(x => a), b`, not `x => (a, b)`).
            let e = self.parse_expr(1)?;
            Ok(Box::new(Stmt::Return(Some(e))))
        }
    }

    fn parse_params(&mut self) -> Result<FnParams, CompileError> {
        if matches!(self.peek(), Token::LParen) { self.advance(); }
        let mut params = Vec::new();
        let mut rest = None;
        if !matches!(self.peek(), Token::RParen) {
            loop {
                if matches!(self.peek(), Token::DotDotDot) {
                    // `...rest` â€” the rest parameter must be the last one and
                    // a plain binding identifier (JS requires this).
                    self.advance();
                    let name = match self.advance() {
                        Token::Ident(s) => s,
                        t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                    };
                    if matches!(self.peek(), Token::Comma) {
                        return Err(CompileError::UnexpectedToken(
                            "rest parameter must be last".to_string(),
                        ));
                    }
                    params.push(ParamDef { pat: Pat::Bind(name), default: None });
                    rest = Some(params.len() - 1);
                    break;
                }
                let pat = self.parse_pattern()?;
                let default = if matches!(self.peek(), Token::Assign) {
                    self.advance();
                    Some(self.parse_expr(0)?)
                } else {
                    None
                };
                params.push(ParamDef { pat, default });
                if !matches!(self.peek(), Token::Comma) { break; }
                self.advance();
            }
        }
        if !matches!(self.peek(), Token::RParen) {
            return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
        }
        self.advance();
        Ok(FnParams { params, rest })
    }

    fn parse_return(&mut self) -> Result<Stmt, CompileError> {
        self.advance();
        let e = if matches!(self.peek(), Token::Semicolon) || matches!(self.peek(), Token::RBrace) {
            None
        } else { Some(self.parse_expr(0)?) };
        if matches!(self.peek(), Token::Semicolon) { self.advance(); }
        Ok(Stmt::Return(e))
    }

    fn parse_if(&mut self) -> Result<Stmt, CompileError> {
        self.advance();
        let cond = self.parse_paren()?;
        let then = self.parse_stmt()?;
        let els = if matches!(self.peek(), Token::Else) {
            self.advance();
            Some(Box::new(self.parse_stmt()?))
        } else { None };
        Ok(Stmt::If { cond, then: Box::new(then), els })
    }

    /// Parse a parenthesized expression. Used for grouping `(a + b)` and for
    /// `if`/`while`/`switch` conditions, where the parens are optional in this
    /// engine (`if x > 0 { }` parses like Node's `if (x > 0) { }`). The closer
    /// is only required when an opener was actually consumed, so a parenless
    /// condition followed by `{` still parses.
    fn parse_paren(&mut self) -> Result<Expr, CompileError> {
        let had_paren = matches!(self.peek(), Token::LParen);
        if had_paren { self.advance(); }
        let e = self.parse_expr(0)?;
        if had_paren {
            if !matches!(self.peek(), Token::RParen) {
                return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
            }
            self.advance();
        }
        Ok(e)
    }

    fn parse_while(&mut self) -> Result<Stmt, CompileError> {
        self.advance();
        let cond = self.parse_paren()?;
        let body = self.parse_stmt()?;
        Ok(Stmt::While { cond, body: Box::new(body) })
    }

    fn parse_do(&mut self) -> Result<Stmt, CompileError> {
        self.advance(); // do
        let body = self.parse_stmt()?;
        if !matches!(self.peek(), Token::While) {
            return Err(CompileError::UnexpectedToken(
                "expected 'while' after do body".to_string(),
            ));
        }
        self.advance(); // while
        let cond = self.parse_paren()?;
        if matches!(self.peek(), Token::Semicolon) { self.advance(); }
        Ok(Stmt::DoWhile { cond, body: Box::new(body) })
    }

    fn parse_for(&mut self) -> Result<Stmt, CompileError> {
        self.advance();
        if matches!(self.peek(), Token::LParen) { self.advance(); }

        // Detect `for (let x of ...)`, `for (let [a, b] of ...)`,
        // `for ({ a, b } in ...)` headers before falling back to the C-style
        // form. The pattern is parsed optimistically and rolled back if it is
        // not followed by `of`/`in`.
        let saved = self.pos;
        let mut declared = false;
        if matches!(self.peek(), Token::Let | Token::Const | Token::Var) {
            declared = true;
            self.advance();
        }
        let pat = match self.parse_pattern() {
            Ok(p) => p,
            Err(_) => {
                self.pos = saved;
                Pat::Bind(String::new())
            }
        };
        if matches!(self.peek(), Token::Of | Token::In) {
            let is_in = matches!(self.peek(), Token::In);
            self.advance();
            // The iterable is an AssignmentExpression: `for (x of a, b)` is a
            // SyntaxError (the comma is neither an operator nor a separator
            // here), so parse at min_bp 1 and let the trailing comma fail.
            let source = self.parse_expr(1)?;
            if matches!(self.peek(), Token::RParen) { self.advance(); }
            let body = self.parse_stmt()?;
            if is_in {
                return Ok(Stmt::ForIn { pat, declared, obj: source, body: Box::new(body) });
            } else {
                return Ok(Stmt::ForOf { pat, declared, iterable: source, body: Box::new(body) });
            }
        }
        self.pos = saved;

        let init = if matches!(self.peek(), Token::Semicolon) {
            self.advance(); None
        } else if matches!(self.peek(), Token::Let) || matches!(self.peek(), Token::Const) {
            Some(Box::new(self.parse_var_decl()?))
        } else {
            Some(Box::new(self.parse_expr_stmt()?))
        };
        let cond = if matches!(self.peek(), Token::Semicolon) { None }
        else { Some(self.parse_expr(0)?) };
        if matches!(self.peek(), Token::Semicolon) { self.advance(); }
        let update = if matches!(self.peek(), Token::RParen) { None }
        else { Some(self.parse_expr(0)?) };
        if matches!(self.peek(), Token::RParen) { self.advance(); }
        let body = self.parse_stmt()?;
        Ok(Stmt::For { init, cond, update, body: Box::new(body) })
    }

    fn parse_try(&mut self) -> Result<Stmt, CompileError> {
        self.advance(); // try
        let body = self.parse_block()?;
        let mut catch = None;
        let mut finally = None;
        if matches!(self.peek(), Token::Catch) {
            self.advance();
            let name = if matches!(self.peek(), Token::LParen) {
                self.advance();
                match self.advance() {
                    Token::Ident(s) => s,
                    t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                }
            } else {
                String::new()
            };
            if matches!(self.peek(), Token::RParen) { self.advance(); }
            let block = self.parse_block()?;
            catch = Some((name, Box::new(block)));
        }
        if matches!(self.peek(), Token::Finally) {
            self.advance();
            let block = self.parse_block()?;
            finally = Some(Box::new(block));
        }
        if catch.is_none() && finally.is_none() {
            return Err(CompileError::UnexpectedToken("try without catch or finally".to_string()));
        }
        Ok(Stmt::Try { body: Box::new(body), catch, finally })
    }

    fn parse_switch(&mut self) -> Result<Stmt, CompileError> {
        self.advance(); // switch
        let disc = self.parse_paren()?;
        if matches!(self.peek(), Token::LBrace) { self.advance(); }
        let mut cases = Vec::new();
        loop {
            match self.peek().clone() {
                Token::Case => {
                    self.advance();
                    let test = self.parse_expr(0)?;
                    if matches!(self.peek(), Token::Colon) { self.advance(); }
                    let mut body = Vec::new();
                    while !matches!(self.peek(), Token::Case | Token::Default | Token::RBrace | Token::Eof) {
                        body.push(self.parse_stmt()?);
                    }
                    cases.push(SwitchCase { test: Some(test), body });
                }
                Token::Default => {
                    self.advance();
                    if matches!(self.peek(), Token::Colon) { self.advance(); }
                    let mut body = Vec::new();
                    while !matches!(self.peek(), Token::Case | Token::Default | Token::RBrace | Token::Eof) {
                        body.push(self.parse_stmt()?);
                    }
                    cases.push(SwitchCase { test: None, body });
                }
                Token::RBrace => {
                    self.advance();
                    break;
                }
                Token::Eof => break,
                t => {
                    return Err(CompileError::UnexpectedToken(format!("{:?}", t)));
                }
            }
        }
        Ok(Stmt::Switch { disc, cases })
    }

    /// `class Name extends Parent { â€¦ }` â€” a declaration (name required).
    fn parse_class_decl(&mut self) -> Result<Stmt, CompileError> {
        self.advance(); // class
        let name = match self.advance() {
            Token::Ident(s) => s,
            t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
        };
        let extends = if matches!(self.peek(), Token::Extends) {
            self.advance();
            Some(self.parse_expr(1)?)
        } else {
            None
        };
        let methods = self.parse_class_body()?;
        Ok(Stmt::Class { name, extends, methods })
    }

    /// `class [Name] extends Parent { â€¦ }` â€” a class expression.
    fn parse_class_expr(&mut self) -> Result<Expr, CompileError> {
        self.advance(); // class
        let name = match self.peek() {
            Token::Ident(s) => {
                let s = s.clone();
                self.advance();
                Some(s)
            }
            _ => None,
        };
        let extends = if matches!(self.peek(), Token::Extends) {
            self.advance();
            Some(Box::new(self.parse_expr(1)?))
        } else {
            None
        };
        let methods = self.parse_class_body()?;
        Ok(Expr::Class { name, extends, methods })
    }

    /// `{ [static] [async] [*] [get|set] [#]name(params) { body } | [static] [#]name [= expr]; … }` — the class body.
    fn parse_class_body(&mut self) -> Result<Vec<MethodDef>, CompileError> {
        if matches!(self.peek(), Token::LBrace) { self.advance(); }
        let mut methods = Vec::new();
        while !matches!(self.peek(), Token::RBrace) && !matches!(self.peek(), Token::Eof) {
            if matches!(self.peek(), Token::Semicolon) {
                self.advance();
                continue;
            }
            let mut is_static = false;
            if matches!(self.peek(), Token::Static) {
                let next = self.tokens.get(self.pos + 1);
                if !matches!(next, Some(Token::LParen) | Some(Token::Assign) | Some(Token::Semicolon) | Some(Token::RBrace)) {
                    self.advance();
                    is_static = true;
                }
            }
            let mut is_generator = false;
            if matches!(self.peek(), Token::Star) {
                self.advance();
                is_generator = true;
            }
            let mut is_async = false;
            if matches!(self.peek(), Token::Async) {
                let next = self.tokens.get(self.pos + 1);
                if !matches!(next, Some(Token::LParen) | Some(Token::Assign) | Some(Token::Semicolon) | Some(Token::RBrace)) {
                    self.advance();
                    is_async = true;
                    if matches!(self.peek(), Token::Star) {
                        self.advance();
                        is_generator = true;
                    }
                }
            }

            let mut kind = MethodKind::Normal;
            let peek_tok = self.peek().clone();
            if !is_generator && !is_async {
                if let Token::Ident(ref s) = peek_tok {
                    if s == "get" || s == "set" {
                        let next = self.tokens.get(self.pos + 1);
                        if !matches!(next, Some(Token::LParen) | Some(Token::Assign) | Some(Token::Semicolon) | Some(Token::RBrace) | None) {
                            self.advance();
                            kind = if s == "get" { MethodKind::Getter } else { MethodKind::Setter };
                        }
                    }
                }
            }

            let name = match self.advance() {
                Token::Ident(s) | Token::PrivateIdent(s) | Token::StringLit(s) => s,
                t => match keyword_text(&t) {
                    Some(s) => s,
                    None => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                },
            };

            // If not followed by '(', this is a class field.
            if matches!(self.peek(), Token::Assign | Token::Semicolon) || !matches!(self.peek(), Token::LParen) {
                let init = if matches!(self.peek(), Token::Assign) {
                    self.advance(); // =
                    Some(self.parse_expr(1)?)
                } else {
                    None
                };
                if matches!(self.peek(), Token::Semicolon) { self.advance(); }
                methods.push(MethodDef {
                    name,
                    is_static,
                    is_async: false,
                    is_generator: false,
                    kind: MethodKind::Field,
                    params: FnParams { params: Vec::new(), rest: None },
                    body: Box::new(Stmt::Nop),
                    init,
                });
                continue;
            }

            if !matches!(self.peek(), Token::LParen) {
                return Err(CompileError::UnexpectedToken("expected '(' after method name".to_string()));
            }
            self.advance(); // (
            let params = self.parse_params()?;
            let body = Box::new(self.parse_block()?);
            methods.push(MethodDef {
                name,
                is_static,
                is_async,
                is_generator,
                kind,
                params,
                body,
                init: None,
            });
            if matches!(self.peek(), Token::Semicolon) { self.advance(); }
        }
        if !matches!(self.peek(), Token::RBrace) {
            return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
        }
        self.advance();
        Ok(methods)
    }

    /// `new C(args)` / `new C.m(args)`: parse the callee as a member chain
    /// (no call parens â€” those are the constructor arguments) plus optional
    /// argument list. `new C` with no parens means `new C()`.
    fn parse_new_expr(&mut self) -> Result<Expr, CompileError> {
        self.advance(); // new
        let mut callee = match self.peek().clone() {
            Token::Ident(s) => {
                self.advance();
                Expr::Ident(s)
            }
            Token::This => {
                self.advance();
                Expr::Ident("this".into())
            }
            Token::LParen => {
                // `new (factory())()` â€” parenthesized constructor expression.
                
                self.parse_paren()?
            }
            Token::LBracket | Token::Function | Token::Async => self.parse_expr(14)?,
            t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
        };
        // Member chain: `.prop` / `[idx]` (no call parens yet).
        loop {
            match self.peek() {
                Token::Dot => {
                    self.advance();
                    let prop = match self.advance() {
                        t => match keyword_text(&t) {
                            Some(s) => s,
                            None => {
                                return Err(CompileError::UnexpectedToken(format!("{:?}", t)));
                            }
                        },
                    };
                    callee = Expr::Prop { obj: Box::new(callee), prop, optional: false };
                }
                Token::LBracket => {
                    self.advance();
                    let idx = self.parse_expr(0)?;
                    if !matches!(self.peek(), Token::RBracket) {
                        return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                    }
                    self.advance();
                    callee = Expr::Index { obj: Box::new(callee), index: Box::new(idx), optional: false };
                }
                _ => break,
            }
        }
        let mut args = Vec::new();
        if matches!(self.peek(), Token::LParen) {
            self.advance();
            while !matches!(self.peek(), Token::RParen) {
                let spread = matches!(self.peek(), Token::DotDotDot);
                if spread { self.advance(); }
                args.push(Elem { spread, hole: false, expr: self.parse_expr(1)? });
                match self.peek() {
                    Token::Comma => { self.advance(); }
                    Token::RParen => {}
                    t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                }
            }
            if !matches!(self.peek(), Token::RParen) {
                return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
            }
            self.advance();
        }
        Ok(Expr::New { callee: Box::new(callee), args })
    }

    fn parse_block(&mut self) -> Result<Stmt, CompileError> {
        if matches!(self.peek(), Token::LBrace) { self.advance(); }
        let mut s = Vec::new();
        while !matches!(self.peek(), Token::RBrace) && !matches!(self.peek(), Token::Eof) {
            s.push(self.parse_stmt()?);
        }
        if !matches!(self.peek(), Token::RBrace) {
            return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
        }
        self.advance();
        Ok(Stmt::Block(s))
    }

    fn parse_expr_stmt(&mut self) -> Result<Stmt, CompileError> {
        let e = self.parse_expr(0)?;
        if matches!(self.peek(), Token::Semicolon) { self.advance(); }
        Ok(Stmt::Expr(e))
    }

    pub fn parse_expr(&mut self, min_bp: u8) -> Result<Expr, CompileError> {
        // Fresh expression boundary: parens, statement contexts, list
        // elements, and operand parses all reset the `??`/`&&`/`||` mixing
        // family. Only the binary-op RHS recursion (below) threads it.
        self.parse_expr_inner(min_bp, None)
    }

    /// `family` is the nullish/logical family already in use at this parse
    /// level: `Some("nullish")` after `??`, `Some("logical")` after
    /// `&&`/`||`. JS forbids mixing the two in one unparenthesized
    /// expression (`a ?? b || c` and `a && b ?? c` are SyntaxErrors), so an
    /// op whose family differs from the incoming one is rejected.
    fn parse_expr_inner(
        &mut self,
        min_bp: u8,
        family: Option<&'static str>,
    ) -> Result<Expr, CompileError> {
        if self.depth >= MAX_RECURSION_DEPTH {
            return Err(CompileError::UnexpectedToken(
                "maximum recursion depth exceeded".to_string(),
            ));
        }
        self.depth += 1;
        let res = self.parse_expr_inner_raw(min_bp, family);
        self.depth -= 1;
        res
    }

    fn parse_expr_inner_raw(
        &mut self,
        min_bp: u8,
        mut family: Option<&'static str>,
    ) -> Result<Expr, CompileError> {
        let mut lhs = match self.peek().clone() {
            Token::Number(n) => { self.advance(); Expr::Num(n) }
            Token::Int(i) => { self.advance(); Expr::Int(i) }
            Token::StringLit(s) => { self.advance(); Expr::Str(s) }
            Token::True => { self.advance(); Expr::Bool(true) }
            Token::False => { self.advance(); Expr::Bool(false) }
            Token::Null => { self.advance(); Expr::Null }
            Token::Undefined => { self.advance(); Expr::Undef }
            Token::Regex { pattern, flags } => {
                self.advance();
                Expr::Regex { pattern, flags }
            }
            Token::Ident(s) => { self.advance(); Expr::Ident(s) }
            Token::Minus => {
                self.advance();
                Expr::Unary("-", Box::new(self.parse_expr(15)?))
            }
            Token::Not => {
                self.advance();
                Expr::Unary("!", Box::new(self.parse_expr(15)?))
            }
            Token::PlusPlus => {
                self.advance();
                Expr::IncDec { target: Box::new(self.parse_expr(15)?), is_inc: true, is_prefix: true }
            }
            Token::MinusMinus => {
                self.advance();
                Expr::IncDec { target: Box::new(self.parse_expr(15)?), is_inc: false, is_prefix: true }
            }
            Token::Typeof => {
                self.advance();
                Expr::Unary("typeof", Box::new(self.parse_expr(15)?))
            }
            Token::Void => {
                self.advance();
                Expr::Unary("void", Box::new(self.parse_expr(15)?))
            }
            Token::Delete => {
                self.advance();
                Expr::Delete(Box::new(self.parse_expr(15)?))
            }
            Token::BitNot => {
                self.advance();
                Expr::Unary("~", Box::new(self.parse_expr(15)?))
            }
            Token::Await => {
                self.advance();
                Expr::Await(Box::new(self.parse_expr(15)?))
            }
            Token::This => {
                self.advance();
                Expr::Ident("this".into())
            }
            Token::New => {
                
                self.parse_new_expr()?
            }
            Token::Super => {
                self.advance();
                match self.peek() {
                    Token::Dot => {
                        self.advance();
                        let prop = match self.advance() {
                            t => match keyword_text(&t) {
                                Some(s) => s,
                                None => {
                                    return Err(CompileError::UnexpectedToken(format!("{:?}", t)));
                                }
                            },
                        };
                        let args = if matches!(self.peek(), Token::LParen) {
                            self.advance();
                            let mut args = Vec::new();
                            while !matches!(self.peek(), Token::RParen) {
                                let spread = matches!(self.peek(), Token::DotDotDot);
                                if spread { self.advance(); }
                                args.push(Elem { spread, hole: false, expr: self.parse_expr(1)? });
                                match self.peek() {
                                    Token::Comma => { self.advance(); }
                                    Token::RParen => {}
                                    t => {
                                        return Err(CompileError::UnexpectedToken(format!("{:?}", t)));
                                    }
                                }
                            }
                            if !matches!(self.peek(), Token::RParen) {
                                return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                            }
                            self.advance();
                            Some(args)
                        } else {
                            None
                        };
                        Expr::SuperProp { prop, args }
                    }
                    Token::LParen => {
                        self.advance();
                        let mut args = Vec::new();
                        while !matches!(self.peek(), Token::RParen) {
                            let spread = matches!(self.peek(), Token::DotDotDot);
                            if spread { self.advance(); }
                            args.push(Elem { spread, hole: false, expr: self.parse_expr(1)? });
                            match self.peek() {
                                Token::Comma => { self.advance(); }
                                Token::RParen => {}
                                t => {
                                    return Err(CompileError::UnexpectedToken(format!("{:?}", t)));
                                }
                            }
                        }
                        if !matches!(self.peek(), Token::RParen) {
                            return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                        }
                        self.advance();
                        Expr::SuperCall { args }
                    }
                    _ => {
                        return Err(CompileError::UnexpectedToken(
                            "super must be followed by '(' or '.m('".to_string(),
                        ));
                    }
                }
            }
            Token::Class => {
                
                self.parse_class_expr()?
            }
            Token::Async => {
                self.advance();
                if matches!(self.peek(), Token::Function) {
                    self.advance();
                    let is_gen = if matches!(self.peek(), Token::Star) {
                        self.advance();
                        true
                    } else {
                        false
                    };
                    if matches!(self.peek(), Token::Ident(_)) {
                        self.advance();
                    }
                    let p = self.parse_params()?;
                    let b = self.parse_block()?;
                    Expr::Lambda { params: p, body: Box::new(b), is_async: true, is_generator: is_gen, is_arrow: false }
                } else if matches!(self.peek(), Token::LParen) && self.looks_like_arrow_params() {
                    self.advance(); // (
                    let params = self.parse_params()?;
                    self.advance(); // =>
                    let body = self.parse_arrow_body()?;
                    Expr::Lambda { params, body, is_async: true, is_generator: false, is_arrow: true }
                } else if matches!(self.peek(), Token::Ident(_)) {
                    // `async x => body`
                    let name = match self.advance() {
                        Token::Ident(s) => s,
                        _ => unreachable!(),
                    };
                    if matches!(self.peek(), Token::Arrow) { self.advance(); }
                    let body = self.parse_arrow_body()?;
                    Expr::Lambda {
                        params: FnParams { params: vec![ParamDef { pat: Pat::Bind(name), default: None }], rest: None },
                        body,
                        is_async: true,
                        is_generator: false,
                        is_arrow: true,
                    }
                } else {
                    return Err(CompileError::UnexpectedToken("async without function/arrow".to_string()));
                }
            }
            Token::LParen => {
                if self.looks_like_arrow_params() {
                    self.advance(); // (
                    let params = self.parse_params()?;
                    self.advance(); // =>
                    let body = self.parse_arrow_body()?;
                    Expr::Lambda { params, body, is_async: false, is_generator: false, is_arrow: true }
                } else {
                    self.advance();
                    let e = self.parse_expr(0)?;
                    if !matches!(self.peek(), Token::RParen) {
                        return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                    }
                    self.advance();
                    Expr::Paren(Box::new(e))
                }
            }
            Token::TemplateLit(parts) => {
                self.advance();
                Expr::Template(parts)
            }
            Token::LBracket => {
                self.advance();
                let mut el = Vec::new();
                while !matches!(self.peek(), Token::RBracket) {
                    if matches!(self.peek(), Token::Comma) {
                        // Elision `[a, , b]`: an empty slot that evaluates to
                        // undefined (and maps to a hole in assignment targets).
                        el.push(Elem { spread: false, hole: true, expr: Expr::Undef });
                        self.advance();
                        continue;
                    }
                    let spread = matches!(self.peek(), Token::DotDotDot);
                    if spread { self.advance(); }
                    // Elements are AssignmentExpressions; the comma is the
                    // list separator, not the operator.
                    el.push(Elem { spread, hole: false, expr: self.parse_expr(1)? });
                    // Commas are REQUIRED between elements (`[1 2]` is a
                    // SyntaxError in JS, not `[1, 2]`).
                    match self.peek() {
                        Token::Comma => { self.advance(); }
                        Token::RBracket => {}
                        t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                    }
                }
                if !matches!(self.peek(), Token::RBracket) {
                    return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                }
                self.advance();
                Expr::Array(el)
            }
            Token::Function => {
                self.advance();
                let is_gen = if matches!(self.peek(), Token::Star) {
                    self.advance();
                    true
                } else {
                    false
                };
                if matches!(self.peek(), Token::Ident(_)) {
                    self.advance();
                }
                let p = self.parse_params()?;
                let b = self.parse_block()?;
                Expr::Lambda { params: p, body: Box::new(b), is_async: false, is_generator: is_gen, is_arrow: false }
            }
            Token::LBrace => {
                self.advance();
                let mut fields = Vec::new();
                while !matches!(self.peek(), Token::RBrace) {
                    match self.peek().clone() {
                        // `{ ...expr }`: copy the source's own enumerable
                        // properties, in order. `{...null}` is a no-op.
                        Token::DotDotDot => {
                            self.advance();
                            let e = self.parse_expr(1)?;
                            fields.push(ObjElem::Spread(e));
                        }
                        // `{ [expr]: value }`: the key is evaluated at
                        // runtime and coerced to a string.
                        Token::LBracket => {
                            self.advance();
                            let k = self.parse_expr(0)?;
                            if !matches!(self.peek(), Token::RBracket) {
                                return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                            }
                            self.advance();
                            if !matches!(self.peek(), Token::Colon) {
                                return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                            }
                            self.advance();
                            let v = self.parse_expr(1)?;
                            fields.push(ObjElem::Computed(k, v));
                        }
                        // `{ *gen() {} }` — generator method in object literal.
                        Token::Star
                            if matches!(self.tokens.get(self.pos + 1), Some(Token::Ident(_)))
                                && matches!(self.tokens.get(self.pos + 2), Some(Token::LParen)) =>
                        {
                            self.advance(); // *
                            let name = match self.advance() {
                                Token::Ident(s) => s,
                                _ => unreachable!(),
                            };
                            self.advance(); // (
                            let params = self.parse_params()?;
                            let b = self.parse_block()?;
                            fields.push(ObjElem::Pair(
                                name,
                                Expr::Lambda {
                                    params,
                                    body: Box::new(b),
                                    is_async: false,
                                    is_generator: true,
                                    is_arrow: false,
                                },
                            ));
                        }
                        // `{ async f() {} }` — async method shorthand.
                        Token::Async
                            if matches!(self.tokens.get(self.pos + 1), Some(Token::Ident(_)))
                                && matches!(self.tokens.get(self.pos + 2), Some(Token::LParen)) =>
                        {
                            self.advance(); // async
                            let name = match self.advance() {
                                Token::Ident(s) => s,
                                _ => unreachable!(),
                            };
                            let params = self.parse_params()?;
                            let b = self.parse_block()?;
                            fields.push(ObjElem::Pair(
                                name,
                                Expr::Lambda {
                                    params,
                                    body: Box::new(b),
                                    is_async: true,
                                    is_generator: false,
                                    is_arrow: false,
                                },
                            ));
                        }
                        // `{ get name() {} }` or `{ set name(v) {} }` — getters/setters.
                        Token::Ident(ref s)
                            if (s == "get" || s == "set")
                                && matches!(self.tokens.get(self.pos + 2), Some(Token::LParen))
                                && !matches!(self.tokens.get(self.pos + 1), Some(Token::Colon) | Some(Token::Comma) | Some(Token::RBrace) | Some(Token::LParen)) =>
                        {
                            let is_getter = s == "get";
                            self.advance(); // get or set
                            let name = match self.advance() {
                                Token::Ident(n) | Token::StringLit(n) => n,
                                t => match keyword_text(&t) {
                                    Some(n) => n,
                                    None => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                                },
                            };
                            self.advance(); // (
                            let params = self.parse_params()?;
                            let b = self.parse_block()?;
                            let lambda = Expr::Lambda {
                                params,
                                body: Box::new(b),
                                is_async: false,
                                is_generator: false,
                                is_arrow: false,
                            };
                            if is_getter {
                                fields.push(ObjElem::Getter(name, lambda));
                            } else {
                                fields.push(ObjElem::Setter(name, lambda));
                            }
                        }
                        _ => {
                            // Keys are IdentifierNames: any keyword is legal
                            // (`{ default: 1, if: 2 }`), plus string keys.
                            let key = match self.advance() {
                                Token::StringLit(s) => s,
                                t => match keyword_text(&t) {
                                    Some(s) => s,
                                    None => {
                                        return Err(CompileError::UnexpectedToken(format!("{:?}", t)));
                                    }
                                },
                            };
                            if matches!(self.peek(), Token::LParen) {
                                // Method shorthand `{ f(a) { ... } }`: a
                                // non-arrow lambda, so `this` binds via the
                                // receiver when called as `o.f()`.
                                self.advance();
                                let params = self.parse_params()?;
                                let b = self.parse_block()?;
                                fields.push(ObjElem::Pair(
                                    key,
                                    Expr::Lambda {
                                        params,
                                        body: Box::new(b),
                                        is_async: false,
                                        is_generator: false,
                                        is_arrow: false,
                                    },
                                ));
                            } else if matches!(self.peek(), Token::Colon) {
                                self.advance();
                                // Property values are AssignmentExpressions;
                                // the comma is the field separator — and it
                                // is REQUIRED between fields (`{a: 1 b: 2}`
                                // is a SyntaxError in JS).
                                let value = self.parse_expr(1)?;
                                fields.push(ObjElem::Pair(key, value));
                            } else {
                                // Shorthand `{ a }` — the value is the
                                // identifier itself. This is the only legal
                                // no-colon form; `{ a.b }` errors (shorthand
                                // must be an IdentifierReference in JS).
                                fields.push(ObjElem::Pair(key.clone(), Expr::Ident(key)));
                            }
                        }
                    }
                    match self.peek() {
                        Token::Comma => { self.advance(); }
                        Token::RBrace => {}
                        t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                    }
                }
                if !matches!(self.peek(), Token::RBrace) {
                    return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                }
                self.advance();
                Expr::Object(fields)
            }
            Token::Yield => {
                self.advance(); // yield
                let delegate = if matches!(self.peek(), Token::Star) {
                    self.advance(); // *
                    true
                } else {
                    false
                };
                let value = if matches!(self.peek(), Token::Semicolon | Token::RParen | Token::RBracket | Token::RBrace | Token::Comma | Token::Eof) {
                    None
                } else {
                    Some(Box::new(self.parse_expr(1)?))
                };
                Expr::Yield { value, delegate }
            }
            Token::Import => {
                self.advance(); // import
                if !matches!(self.peek(), Token::LParen) {
                    return Err(CompileError::UnexpectedToken("expected '(' after import".to_string()));
                }
                self.advance(); // (
                let arg = self.parse_expr(0)?;
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                }
                if !matches!(self.peek(), Token::RParen) {
                    return Err(CompileError::UnexpectedToken("expected ')' after import argument".to_string()));
                }
                self.advance(); // )
                Expr::Call {
                    callee: Box::new(Expr::Ident("__alloy_import".to_string())),
                    args: vec![Elem { spread: false, hole: false, expr: arg }],
                    optional: false,
                }
            }
            t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
        };

        loop {
            if matches!(self.peek(), Token::LParen) {
                self.advance();
                let mut args = Vec::new();
                while !matches!(self.peek(), Token::RParen) {
                    let spread = matches!(self.peek(), Token::DotDotDot);
                    if spread { self.advance(); }
                    // Arguments are AssignmentExpressions; the comma is the
                    // list separator, not the operator.
                    args.push(Elem { spread, hole: false, expr: self.parse_expr(1)? });
                    // Commas are REQUIRED between arguments â€” `f(a b)` is a
                    // SyntaxError in JS, and a missing comma is how `0.0
                    // toString()` used to silently become two arguments.
                    match self.peek() {
                        Token::Comma => { self.advance(); }
                        Token::RParen => {}
                        t => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                    }
                }
                if !matches!(self.peek(), Token::RParen) {
                    return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                }
                self.advance();
                lhs = Expr::Call { callee: Box::new(lhs), args, optional: false };
                continue;
            } else if matches!(self.peek(), Token::Dot) {
                self.advance();
                // Reserved words are legal property names after a dot
                // (`m.default`, `o.delete`, `o.if` â€” the IdentifierName
                // rule). Any keyword token is mapped back to its text.
                let prop = match self.advance() {
                    t => match keyword_text(&t) {
                        Some(s) => s,
                        None => return Err(CompileError::UnexpectedToken(format!("{:?}", t))),
                    },
                };
                lhs = Expr::Prop { obj: Box::new(lhs), prop, optional: false };
                continue;
            } else if matches!(self.peek(), Token::QuestionDot) {
                // Optional chaining: `o?.p`, `o?.[k]`, `f?.()`. The `?.` must
                // be followed by a property name, `[`, or `(` (the lexer
                // already reclassifies `?.5` as a ternary). Each link records
                // its optionality; the emitter short-circuits the whole
                // remaining chain when the link's receiver is nullish.
                self.advance(); // ?.
                match self.peek() {
                    Token::LBracket => {
                        self.advance();
                        let idx = self.parse_expr(0)?;
                        if !matches!(self.peek(), Token::RBracket) {
                            return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                        }
                        self.advance();
                        lhs = Expr::Index {
                            obj: Box::new(lhs),
                            index: Box::new(idx),
                            optional: true,
                        };
                    }
                    Token::LParen => {
                        self.advance();
                        let mut args = Vec::new();
                        while !matches!(self.peek(), Token::RParen) {
                            let spread = matches!(self.peek(), Token::DotDotDot);
                            if spread {
                                self.advance();
                            }
                            args.push(Elem { spread, hole: false, expr: self.parse_expr(1)? });
                            match self.peek() {
                                Token::Comma => {
                                    self.advance();
                                }
                                Token::RParen => {}
                                t => {
                                    return Err(CompileError::UnexpectedToken(format!("{:?}", t)));
                                }
                            }
                        }
                        if !matches!(self.peek(), Token::RParen) {
                            return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                        }
                        self.advance();
                        lhs = Expr::Call {
                            callee: Box::new(lhs),
                            args,
                            optional: true,
                        };
                    }
                    // `?.prop` / `?.keyword` (reserved words are legal
                    // property names after `?.`, same as after `.`).
                    t => match keyword_text(t) {
                        Some(s) => {
                            self.advance();
                            lhs = Expr::Prop {
                                obj: Box::new(lhs),
                                prop: s,
                                optional: true,
                            };
                        }
                        None => {
                            return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                        }
                    },
                }
                continue;
            } else if matches!(self.peek(), Token::LBracket) {
                self.advance();
                let idx = self.parse_expr(0)?;
                if !matches!(self.peek(), Token::RBracket) {
                    return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                }
                self.advance();
                lhs = Expr::Index { obj: Box::new(lhs), index: Box::new(idx), optional: false };
                continue;
            } else if matches!(self.peek(), Token::Arrow) && matches!(&lhs, Expr::Ident(_)) {
                // Single-parameter arrow shorthand: `x => body`.
                let name = match &lhs {
                    Expr::Ident(s) => s.clone(),
                    _ => unreachable!(),
                };
                self.advance(); // =>
                let body = self.parse_arrow_body()?;
                lhs = Expr::Lambda {
                    params: FnParams { params: vec![ParamDef { pat: Pat::Bind(name), default: None }], rest: None },
                    body,
                    is_async: false,
                    is_generator: false,
                    is_arrow: true,
                };
                continue;
            } else if matches!(self.peek(), Token::PlusPlus | Token::MinusMinus) {
                // Postfix `x++` / `x--`; binds tighter than any binary op.
                let is_inc = matches!(self.peek(), Token::PlusPlus);
                self.advance();
                lhs = Expr::IncDec { target: Box::new(lhs), is_inc, is_prefix: false };
                continue;
            }
            let (op, bp): (&str, u8) = match self.peek() {
                Token::Plus => ("+", 10),
                Token::Minus => ("-", 10),
                Token::Star => ("*", 11),
                Token::Slash => ("/", 11),
                Token::Percent => ("%", 11),
                Token::StarStar => {
                    // `**` binds tighter than `*`/`/`/`%` and is
                    // right-associative. JS restricts the LEFT operand to a
                    // non-unary expression (`-2 ** 2` is a SyntaxError; the
                    // exponent itself may be unary: `2 ** -2` is fine).
                    if matches!(lhs, Expr::Unary(..)) {
                        return Err(CompileError::UnexpectedToken(
                            "unary expression cannot be the left operand of '**'".to_string(),
                        ));
                    }
                    ("**", 12)
                }
                Token::EqEq => ("==", 7),
                Token::Neq => ("!=", 7),
                Token::EqEqEq => ("===", 7),
                Token::NeqEq => ("!==", 7),
                Token::Lt => ("<", 8),
                Token::Gt => (">", 8),
                Token::Lte => ("<=", 8),
                Token::Gte => (">=", 8),
                // `instanceof` and the `in` operator bind at relational
                // precedence (JS spec).
                Token::InstanceOf => ("instanceof", 8),
                Token::In => ("in", 8),
                // Shifts bind between relational and additive: `a << b + c`
                // is `a << (b + c)`, `a < b << c` is `a < (b << c)`.
                Token::Shl => ("<<", 9),
                Token::Shr => (">>", 9),
                Token::UShr => (">>>", 9),
                Token::And => ("&&", 3),
                Token::Or => ("||", 2),
                Token::QuestionQuestion => ("??", 2),
                // Bitwise ops bind between the logical ops and equality,
                // matching JS precedence: `||` < `&&` < `|` < `^` < `&` < eq.
                Token::BitOr => ("|", 4),
                Token::BitXor => ("^", 5),
                Token::BitAnd => ("&", 6),
                Token::Assign => ("=", 1),
                Token::PlusAssign => ("+=", 1),
                Token::MinusAssign => ("-=", 1),
                Token::StarAssign => ("*=", 1),
                Token::SlashAssign => ("/=", 1),
                Token::PercentAssign => ("%=", 1),
                Token::BitAndAssign => ("&=", 1),
                Token::BitOrAssign => ("|=", 1),
                Token::BitXorAssign => ("^=", 1),
                Token::ShlAssign => ("<<=", 1),
                Token::ShrAssign => (">>=", 1),
                Token::UShrAssign => (">>>=", 1),
                Token::StarStarAssign => ("**=", 1),
                Token::AndAssign => ("&&=", 1),
                Token::OrAssign => ("||=", 1),
                Token::NullishAssign => ("??=", 1),
                // No binary operator here. Two expressions directly adjacent
                // (no operator, no postfix) is invalid JS (`0.toString` is a
                // SyntaxError in Node because `0.` lexes as a number and the
                // identifier follows it; `a b` and `a 5` are errors too).
                // This used to silently misparse â€” the dangling identifier
                // became a second call argument or a second statement.
                // Only a SAME-LINE adjacency is an error. The engine treats a
                // newline as an implicit statement separator (tests/REPL rely
                // on `const f = x => x` \n `print(f())` with no semicolon).
                t if starts_expression(t)
                    && self.pos > 0
                    && self.lines[self.pos] == self.lines[self.pos - 1] =>
                {
                    return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
                }
                _ => break,
            };
            if bp < min_bp { break; }
            // `??` cannot be mixed with `&&`/`||` in one unparenthesized
            // expression (JS SyntaxError). The family of the incoming parse
            // level flows down through the RHS recursion, so the check fires
            // regardless of nesting order (`a ?? b && c`, `a && b ?? c`,
            // `a ?? b || c`); parens reset it via the parse_expr wrapper.
            let fam = match op {
                "??" => Some("nullish"),
                "&&" | "||" => Some("logical"),
                _ => None,
            };
            let next_family = match (family, fam) {
                (Some(a), Some(b)) if a != b => {
                    return Err(CompileError::UnexpectedToken(
                        "mix of '??' with '&&' or '||' requires parentheses".to_string(),
                    ));
                }
                (_, Some(b)) => Some(b),
                (f, None) => f,
            };
            self.advance();
            // Assignment operators and `**` are right-associative: their RHS
            // parses at the same precedence, so `a = b = c` chains into
            // `a = (b = c)` and `2 ** 3 ** 2` into `2 ** (3 ** 2)`.
            let is_assign = op == "=" || op == "+=" || op == "-=" || op == "*=" || op == "/="
                || op == "%=" || op == "&=" || op == "|=" || op == "^=" || op == "<<="
                || op == ">>=" || op == ">>>=" || op == "**=" || op == "&&=" || op == "||="
                || op == "??=";
            let right_assoc = is_assign || op == "**";
            let rhs = self.parse_expr_inner(if right_assoc { bp } else { bp + 1 }, next_family)?;
            lhs = if is_assign {
                Expr::Assign { target: Box::new(lhs), op, value: Box::new(rhs) }
            } else {
                Expr::Bin(op, Box::new(lhs), Box::new(rhs))
            };
            // The family persists across the loop's remaining iterations, so
            // a later same-precedence op sees it: `a ?? b || c` (both bp 2,
            // left-assoc) must still hit the mixing rule even though the RHS
            // recursion stopped before the `||`.
            family = next_family;
            continue;
        }

        // Ternary `cond ? then : else`. Binds looser than `||` (so it is not
        // consumed as the RHS of `||`, min_bp 3) but tighter than assignment
        // (so `x = a ? b : c` takes the whole ternary as the RHS, min_bp 2).
        // The branches are AssignmentExpressions (JS grammar), so they parse
        // at min_bp 1 â€” assignments allowed, the comma operator is not (it
        // needs parens): `a ? b : c, d` is `(a ? b : c), d`.
        if matches!(self.peek(), Token::Question) {
            if 2 < min_bp {
                return Ok(lhs);
            }
            self.advance(); // ?
            let then_branch = self.parse_expr(1)?;
            if !matches!(self.peek(), Token::Colon) {
                return Err(CompileError::UnexpectedToken(format!("{:?}", self.peek())));
            }
            self.advance(); // :
            let else_branch = self.parse_expr(1)?;
            lhs = Expr::Ternary {
                cond: Box::new(lhs),
                then: Box::new(then_branch),
                els: Box::new(else_branch),
            };
        }

        // Comma operator: the lowest-precedence expression. Only a "full
        // expression" context (min_bp == 0 â€” parens, statements, return,
        // conditions, for-init/update, template holes, switch tests) sees it;
        // separator contexts parse at min_bp >= 1 so `f(a, b)` / `[a, b]` /
        // `let x = 1, y = 2` keep their commas as separators. Each element is
        // itself a full expression minus the comma (min_bp 1, so assignments
        // and ternaries are allowed inside).
        if min_bp == 0 && matches!(self.peek(), Token::Comma) {
            let mut seq = vec![lhs];
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                seq.push(self.parse_expr(1)?);
            }
            lhs = Expr::Sequence(seq);
        }
        Ok(lhs)
    }
}

