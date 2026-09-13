use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use crate::ast::{MethodKind, Stmt};
use crate::compiler::lexer::Lexer;
use crate::compiler::parser::Parser;
use super::docs::get_hover_doc;
use super::protocol::*;

pub struct LspServer {
    pub documents: HashMap<String, String>,
}

impl Default for LspServer {
    fn default() -> Self {
        Self::new()
    }
}

impl LspServer {
    pub fn new() -> Self {
        Self {
            documents: HashMap::new(),
        }
    }

    pub fn parse_diagnostics(&self, text: &str) -> Vec<Diagnostic> {
        let mut lexer = Lexer::new(text);
        let ts = match lexer.tokenize() {
            Ok(ts) => ts,
            Err(err) => {
                let line = lexer.cur_line().saturating_sub(1);
                let col = lexer.cur_col().saturating_sub(1);
                return vec![Diagnostic {
                    range: Range {
                        start: Position { line, character: col },
                        end: Position { line, character: col + 1 },
                    },
                    severity: Some(1), // Error
                    source: Some("alloy".to_string()),
                    message: format!("{}", err),
                }];
            }
        };

        let mut parser = Parser::new(ts);
        if let Err(err) = parser.parse_program() {
            let (l, c) = parser.cur_loc();
            let line = l.saturating_sub(1);
            let col = c.saturating_sub(1);
            return vec![Diagnostic {
                range: Range {
                    start: Position { line, character: col },
                    end: Position { line, character: col + 1 },
                },
                severity: Some(1),
                source: Some("alloy".to_string()),
                message: format!("{}", err),
            }];
        }

        Vec::new()
    }

    pub fn get_word_at(&self, uri: &str, pos: Position) -> Option<String> {
        let text = self.documents.get(uri)?;
        let line_str = text.lines().nth(pos.line as usize)?;
        let chars: Vec<char> = line_str.chars().collect();
        let col = pos.character as usize;
        if col > chars.len() {
            return None;
        }

        let is_ident_char = |c: char| c.is_alphanumeric() || c == '_' || c == '$' || c == '#';

        let mut start = col;
        while start > 0 && is_ident_char(chars[start - 1]) {
            start -= 1;
        }
        let mut end = col;
        while end < chars.len() && is_ident_char(chars[end]) {
            end += 1;
        }

        if start < end {
            Some(chars[start..end].iter().collect())
        } else {
            None
        }
    }

    pub fn handle_hover(&self, params: TextDocumentPositionParams) -> Option<Hover> {
        let word = self.get_word_at(&params.text_document.uri, params.position)?;
        let (sig, desc) = get_hover_doc(&word)?;
        Some(Hover {
            contents: MarkupContent {
                kind: "markdown".to_string(),
                value: format!("```typescript\n{}\n```\n\n{}", sig, desc),
            },
            range: None,
        })
    }

    pub fn handle_completion(&self) -> CompletionList {
        let keywords = [
            "async", "await", "break", "case", "catch", "class", "const", "continue",
            "debugger", "default", "delete", "do", "else", "export", "extends", "finally",
            "for", "function", "if", "import", "in", "instanceof", "let", "new", "return",
            "super", "switch", "this", "throw", "try", "typeof", "var", "void", "while",
            "with", "yield",
        ];

        let builtins = [
            ("channel", 3, "channel(): Channel (cross-actor channel)"),
            ("spawn", 3, "spawn(fn, ...args): Actor (background actor)"),
            ("fetchSync", 3, "fetchSync(url, opts): Response (sync HTTP/HTTPS)"),
            ("print", 3, "print(...args): void (output to stdout)"),
            ("structuredClone", 3, "structuredClone(val): val (deep copy)"),
            ("http", 9, "http module (embedded high-speed web server)"),
            ("memory", 9, "memory module (sidecar IPC shared memory)"),
            ("fs", 9, "fs module (synchronous file operations)"),
            ("crypto", 9, "crypto module (Web Crypto API)"),
            ("URL", 7, "URL class (WHATWG standard URL parser)"),
            ("Promise", 7, "Promise class"),
            ("Array", 7, "Array class"),
            ("Object", 7, "Object class"),
            ("Math", 9, "Math namespace"),
            ("JSON", 9, "JSON namespace"),
            ("Error", 7, "Error class"),
            ("TypeError", 7, "TypeError class"),
            ("RangeError", 7, "RangeError class"),
            ("SyntaxError", 7, "SyntaxError class"),
            ("ReferenceError", 7, "ReferenceError class"),
        ];

        let mut items = Vec::new();
        for kw in keywords {
            items.push(CompletionItem {
                label: kw.to_string(),
                kind: Some(14), // Keyword
                detail: Some("keyword".to_string()),
                documentation: None,
                insert_text: None,
            });
        }
        for (name, kind, detail) in builtins {
            items.push(CompletionItem {
                label: name.to_string(),
                kind: Some(kind),
                detail: Some(detail.to_string()),
                documentation: None,
                insert_text: None,
            });
        }

        CompletionList {
            is_incomplete: false,
            items,
        }
    }

    pub fn handle_document_symbols(&self, uri: &str) -> Vec<DocumentSymbol> {
        let text = match self.documents.get(uri) {
            Some(t) => t,
            None => return Vec::new(),
        };

        let mut lexer = Lexer::new(text);
        let ts = match lexer.tokenize() {
            Ok(ts) => ts,
            Err(_) => return Vec::new(),
        };
        let mut parser = Parser::new(ts);
        let stmts = match parser.parse_program() {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let mut symbols = Vec::new();
        collect_symbols(&stmts, &mut symbols);
        symbols
    }

    pub fn handle_incoming(&mut self, msg_str: &str) -> Vec<JsonRpcMessage> {
        let msg: JsonRpcMessage = match serde_json::from_str(msg_str) {
            Ok(m) => m,
            Err(_) => return Vec::new(),
        };

        let mut responses = Vec::new();

        if let Some(ref method) = msg.method {
            match method.as_str() {
                "initialize" => {
                    let result = InitializeResult {
                        capabilities: ServerCapabilities {
                            text_document_sync: 1, // Full
                            hover_provider: true,
                            completion_provider: CompletionOptions {
                                resolve_provider: false,
                                trigger_characters: vec![".".into(), "@".into(), ":".into()],
                            },
                            document_symbol_provider: true,
                        },
                        server_info: ServerInfo {
                            name: "alloy-lsp".to_string(),
                            version: "0.1.0".to_string(),
                        },
                    };
                    responses.push(JsonRpcMessage {
                        jsonrpc: "2.0".to_string(),
                        id: msg.id,
                        method: None,
                        params: None,
                        result: Some(serde_json::to_value(result).unwrap()),
                        error: None,
                    });
                }
                "initialized" => {
                    // No-op notification
                }
                "textDocument/didOpen" => {
                    if let Some(ref params_val) = msg.params {
                        if let Ok(params) = serde_json::from_value::<DidOpenTextDocumentParams>(params_val.clone()) {
                            let uri = params.text_document.uri;
                            let text = params.text_document.text;
                            let diags = self.parse_diagnostics(&text);
                            self.documents.insert(uri.clone(), text);

                            let diag_params = PublishDiagnosticsParams {
                                uri,
                                diagnostics: diags,
                            };
                            responses.push(JsonRpcMessage {
                                jsonrpc: "2.0".to_string(),
                                id: None,
                                method: Some("textDocument/publishDiagnostics".to_string()),
                                params: Some(serde_json::to_value(diag_params).unwrap()),
                                result: None,
                                error: None,
                            });
                        }
                    }
                }
                "textDocument/didChange" => {
                    if let Some(ref params_val) = msg.params {
                        if let Ok(params) = serde_json::from_value::<DidChangeTextDocumentParams>(params_val.clone()) {
                            let uri = params.text_document.uri;
                            if let Some(change) = params.content_changes.into_iter().next() {
                                let diags = self.parse_diagnostics(&change.text);
                                self.documents.insert(uri.clone(), change.text);

                                let diag_params = PublishDiagnosticsParams {
                                    uri,
                                    diagnostics: diags,
                                };
                                responses.push(JsonRpcMessage {
                                    jsonrpc: "2.0".to_string(),
                                    id: None,
                                    method: Some("textDocument/publishDiagnostics".to_string()),
                                    params: Some(serde_json::to_value(diag_params).unwrap()),
                                    result: None,
                                    error: None,
                                });
                            }
                        }
                    }
                }
                "textDocument/didClose" => {
                    if let Some(ref params_val) = msg.params {
                        if let Ok(params) = serde_json::from_value::<DidCloseTextDocumentParams>(params_val.clone()) {
                            self.documents.remove(&params.text_document.uri);
                            let diag_params = PublishDiagnosticsParams {
                                uri: params.text_document.uri,
                                diagnostics: Vec::new(),
                            };
                            responses.push(JsonRpcMessage {
                                jsonrpc: "2.0".to_string(),
                                id: None,
                                method: Some("textDocument/publishDiagnostics".to_string()),
                                params: Some(serde_json::to_value(diag_params).unwrap()),
                                result: None,
                                error: None,
                            });
                        }
                    }
                }
                "textDocument/hover" => {
                    if let Some(ref params_val) = msg.params {
                        if let Ok(params) = serde_json::from_value::<TextDocumentPositionParams>(params_val.clone()) {
                            let hover = self.handle_hover(params);
                            responses.push(JsonRpcMessage {
                                jsonrpc: "2.0".to_string(),
                                id: msg.id,
                                method: None,
                                params: None,
                                result: Some(serde_json::to_value(hover).unwrap_or(serde_json::Value::Null)),
                                error: None,
                            });
                        }
                    }
                }
                "textDocument/completion" => {
                    let list = self.handle_completion();
                    responses.push(JsonRpcMessage {
                        jsonrpc: "2.0".to_string(),
                        id: msg.id,
                        method: None,
                        params: None,
                        result: Some(serde_json::to_value(list).unwrap()),
                        error: None,
                    });
                }
                "textDocument/documentSymbol" => {
                    if let Some(ref params_val) = msg.params {
                        if let Ok(params) = serde_json::from_value::<DocumentSymbolParams>(params_val.clone()) {
                            let syms = self.handle_document_symbols(&params.text_document.uri);
                            responses.push(JsonRpcMessage {
                                jsonrpc: "2.0".to_string(),
                                id: msg.id,
                                method: None,
                                params: None,
                                result: Some(serde_json::to_value(syms).unwrap()),
                                error: None,
                            });
                        }
                    }
                }
                "shutdown" => {
                    responses.push(JsonRpcMessage {
                        jsonrpc: "2.0".to_string(),
                        id: msg.id,
                        method: None,
                        params: None,
                        result: Some(serde_json::Value::Null),
                        error: None,
                    });
                }
                "exit" => {
                    // Nothing to return
                }
                _ => {
                    if msg.id.is_some() {
                        responses.push(JsonRpcMessage {
                            jsonrpc: "2.0".to_string(),
                            id: msg.id,
                            method: None,
                            params: None,
                            result: None,
                            error: Some(JsonRpcError {
                                code: -32601,
                                message: format!("Method not found: {}", method),
                                data: None,
                            }),
                        });
                    }
                }
            }
        }

        responses
    }
}

fn collect_symbols(stmts: &[Stmt], symbols: &mut Vec<DocumentSymbol>) {
    for stmt in stmts {
        match stmt {
            Stmt::Loc { line, col, stmt } => {
                let l = line.saturating_sub(1);
                let c = col.saturating_sub(1);
                match stmt.as_ref() {
                    Stmt::FnDecl { name, .. } => {
                        symbols.push(DocumentSymbol {
                            name: name.clone(),
                            kind: 12, // Function
                            range: Range {
                                start: Position { line: l, character: c },
                                end: Position { line: l + 1, character: 0 },
                            },
                            selection_range: Range {
                                start: Position { line: l, character: c },
                                end: Position { line: l, character: c + name.len() as u32 },
                            },
                            detail: Some(format!("function {}", name)),
                            children: None,
                        });
                    }
                    Stmt::Class { name, methods, .. } => {
                        let mut children = Vec::new();
                        for m in methods {
                            let k = match m.kind {
                                MethodKind::Field => 8, // Field
                                _ => 6, // Method
                            };
                            children.push(DocumentSymbol {
                                name: m.name.clone(),
                                kind: k,
                                range: Range {
                                    start: Position { line: l, character: c },
                                    end: Position { line: l + 1, character: 0 },
                                },
                                selection_range: Range {
                                    start: Position { line: l, character: c },
                                    end: Position { line: l, character: c + m.name.len() as u32 },
                                },
                                detail: Some(if m.is_static { format!("static {}", m.name) } else { m.name.clone() }),
                                children: None,
                            });
                        }
                        symbols.push(DocumentSymbol {
                            name: name.clone(),
                            kind: 5, // Class
                            range: Range {
                                start: Position { line: l, character: c },
                                end: Position { line: l + 1, character: 0 },
                            },
                            selection_range: Range {
                                start: Position { line: l, character: c },
                                end: Position { line: l, character: c + name.len() as u32 },
                            },
                            detail: Some(format!("class {}", name)),
                            children: if children.is_empty() { None } else { Some(children) },
                        });
                    }
                    Stmt::VarDecl { decls } => {
                        for (pat, _) in decls {
                            let mut names = Vec::new();
                            crate::ast::pat_bound_names(pat, &mut names);
                            for n in names {
                                symbols.push(DocumentSymbol {
                                    name: n.clone(),
                                    kind: 13, // Variable
                                    range: Range {
                                        start: Position { line: l, character: c },
                                        end: Position { line: l + 1, character: 0 },
                                    },
                                    selection_range: Range {
                                        start: Position { line: l, character: c },
                                        end: Position { line: l, character: c + n.len() as u32 },
                                    },
                                    detail: Some(format!("var {}", n)),
                                    children: None,
                                });
                            }
                        }
                    }
                    Stmt::Block(inner) => {
                        collect_symbols(inner, symbols);
                    }
                    _ => {}
                }
            }
            Stmt::Block(inner) => {
                collect_symbols(inner, symbols);
            }
            _ => {}
        }
    }
}

pub fn read_framed_message<R: BufRead>(reader: &mut R) -> std::io::Result<Option<String>> {
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            // End of headers
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            if let Ok(len) = rest.trim().parse::<usize>() {
                content_length = Some(len);
            }
        }
    }

    let len = match content_length {
        Some(l) => l,
        None => return Ok(None),
    };

    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    let msg = String::from_utf8_lossy(&body).to_string();
    Ok(Some(msg))
}

pub fn write_framed_message<W: Write>(writer: &mut W, msg: &JsonRpcMessage) -> std::io::Result<()> {
    let json_bytes = serde_json::to_vec(msg).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let header = format!("Content-Length: {}\r\n\r\n", json_bytes.len());
    writer.write_all(header.as_bytes())?;
    writer.write_all(&json_bytes)?;
    writer.flush()?;
    Ok(())
}

pub fn run_server_stdio() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();

    let mut server = LspServer::new();

    while let Ok(Some(msg_str)) = read_framed_message(&mut reader) {
        let responses = server.handle_incoming(&msg_str);
        for resp in responses {
            write_framed_message(&mut writer, &resp)?;
        }
    }

    Ok(())
}
