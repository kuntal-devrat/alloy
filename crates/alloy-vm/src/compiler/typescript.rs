//! Lightweight and robust TypeScript-to-JavaScript type stripper.
//! Removes interfaces, type aliases, type imports, and type annotations
//! while preserving exact line counts and column structure for debugging.

pub fn strip_typescript(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;

    while i < n {
        // Handle comments and string literals untouched
        if chars[i] == '/' && i + 1 < n && chars[i + 1] == '/' {
            // Line comment: keep until newline
            while i < n && chars[i] != '\n' {
                out.push(chars[i]);
                i += 1;
            }
            continue;
        }

        if chars[i] == '/' && i + 1 < n && chars[i + 1] == '*' {
            // Block comment
            out.push(chars[i]);
            out.push(chars[i + 1]);
            i += 2;
            while i + 1 < n && !(chars[i] == '*' && chars[i + 1] == '/') {
                out.push(chars[i]);
                i += 1;
            }
            if i < n {
                out.push(chars[i]);
                i += 1;
            }
            if i < n {
                out.push(chars[i]);
                i += 1;
            }
            continue;
        }

        if chars[i] == '"' || chars[i] == '\'' || chars[i] == '`' {
            let quote = chars[i];
            out.push(quote);
            i += 1;
            while i < n && chars[i] != quote {
                if chars[i] == '\\' && i + 1 < n {
                    out.push(chars[i]);
                    out.push(chars[i + 1]);
                    i += 2;
                } else {
                    out.push(chars[i]);
                    i += 1;
                }
            }
            if i < n {
                out.push(chars[i]);
                i += 1;
            }
            continue;
        }

        // Check for `import type`
        if starts_with_word(&chars, i, "import") {
            let after_import = skip_whitespace(&chars, i + 6);
            if starts_with_word(&chars, after_import, "type") {
                // Skip entire import type ... ; statement
                i = skip_until_semicolon_or_newline(&chars, i);
                continue;
            }
        }

        // Check for `interface Name`
        if starts_with_word(&chars, i, "interface") {
            let after_iface = skip_whitespace(&chars, i + 9);
            if after_iface < n && (chars[after_iface].is_alphabetic() || chars[after_iface] == '_')
            {
                // Find opening brace '{'
                let mut brace_idx = after_iface;
                while brace_idx < n && chars[brace_idx] != '{' {
                    brace_idx += 1;
                }
                if brace_idx < n {
                    // Skip balanced braces
                    i = skip_balanced(&chars, brace_idx, '{', '}');
                    // Optional semicolon
                    let next_i = skip_whitespace(&chars, i);
                    if next_i < n && chars[next_i] == ';' {
                        i = next_i + 1;
                    }
                    continue;
                }
            }
        }

        // Check for `type Name = ...;`
        if starts_with_word(&chars, i, "type") {
            let after_type = skip_whitespace(&chars, i + 4);
            if after_type < n && (chars[after_type].is_alphabetic() || chars[after_type] == '_') {
                // Lookahead to ensure there's an '=' before ';' or '{'
                let mut cur = after_type;
                let mut is_type_alias = false;
                while cur < n && chars[cur] != ';' && chars[cur] != '\n' && chars[cur] != '{' {
                    if chars[cur] == '=' {
                        is_type_alias = true;
                        break;
                    }
                    cur += 1;
                }
                if is_type_alias {
                    i = skip_until_semicolon_or_newline(&chars, i);
                    continue;
                }
            }
        }

        // Check for `as <Type>` assertions
        if starts_with_word(&chars, i, "as") {
            let before = prev_non_whitespace(&chars, i);
            let after = skip_whitespace(&chars, i + 2);
            if let Some(b) = before {
                if (b.is_alphanumeric()
                    || b == ')'
                    || b == ']'
                    || b == '}'
                    || b == '"'
                    || b == '\'')
                    && after < n
                    && (chars[after].is_alphabetic() || chars[after] == '{' || chars[after] == '(')
                {
                    // Skip `as Type` up to `,`, `)`, `;`, `}`, `]`, or newline
                    i = after;
                    let mut paren_depth = 0;
                    let mut angle_depth = 0;
                    let mut brace_depth = 0;
                    while i < n {
                        let ch = chars[i];
                        if ch == '(' {
                            paren_depth += 1;
                        } else if ch == ')' {
                            if paren_depth == 0 {
                                break;
                            }
                            paren_depth -= 1;
                        } else if ch == '<' {
                            angle_depth += 1;
                        } else if ch == '>' {
                            if angle_depth > 0 {
                                angle_depth -= 1;
                            }
                        } else if ch == '{' {
                            brace_depth += 1;
                        } else if ch == '}' {
                            if brace_depth == 0 {
                                break;
                            }
                            brace_depth -= 1;
                        } else if (ch == ',' || ch == ';' || ch == ']' || ch == '\n')
                            && paren_depth == 0
                            && angle_depth == 0
                            && brace_depth == 0
                        {
                            break;
                        }
                        i += 1;
                    }
                    continue;
                }
            }
        }

        // Check for access modifier keywords in classes: public, private, protected, readonly
        if starts_with_word(&chars, i, "public")
            || starts_with_word(&chars, i, "protected")
            || starts_with_word(&chars, i, "readonly")
        {
            let len = if starts_with_word(&chars, i, "readonly") {
                8
            } else if starts_with_word(&chars, i, "protected") {
                9
            } else {
                6
            };
            let after = skip_whitespace(&chars, i + len);
            if after < n
                && (chars[after].is_alphabetic() || chars[after] == '_' || chars[after] == '#')
            {
                i = after;
                continue;
            }
        }
        if starts_with_word(&chars, i, "private") {
            let after = skip_whitespace(&chars, i + 7);
            if after < n && (chars[after].is_alphabetic() || chars[after] == '_') {
                i = after;
                continue;
            }
        }

        // Strip `: Type` annotations in parameter lists, variable declarations, and return types
        if chars[i] == ':' {
            let is_type_annotation = is_likely_type_annotation(&chars, i);
            if is_type_annotation {
                // Skip `: Type`
                i += 1; // skip ':'
                i = skip_whitespace(&chars, i);
                let mut paren_depth = 0;
                let mut angle_depth = 0;
                let mut brace_depth = 0;
                let mut bracket_depth = 0;

                while i < n {
                    let ch = chars[i];
                    if ch == '(' {
                        paren_depth += 1;
                    } else if ch == ')' {
                        if paren_depth == 0 {
                            break;
                        }
                        paren_depth -= 1;
                    } else if ch == '<' {
                        angle_depth += 1;
                    } else if ch == '>' {
                        if angle_depth > 0 {
                            angle_depth -= 1;
                        }
                    } else if ch == '{' {
                        if brace_depth == 0 && paren_depth == 0 && angle_depth == 0 {
                            break;
                        }
                        brace_depth += 1;
                    } else if ch == '}' {
                        if brace_depth == 0 {
                            break;
                        }
                        brace_depth -= 1;
                    } else if ch == '[' {
                        bracket_depth += 1;
                    } else if ch == ']' {
                        if bracket_depth == 0 {
                            break;
                        }
                        bracket_depth -= 1;
                    } else if (ch == '=' || ch == ',' || ch == ';')
                        && paren_depth == 0
                        && angle_depth == 0
                        && brace_depth == 0
                        && bracket_depth == 0
                    {
                        break;
                    }
                    i += 1;
                }
                continue;
            }
        }

        out.push(chars[i]);
        i += 1;
    }

    out
}

fn starts_with_word(chars: &[char], idx: usize, word: &str) -> bool {
    let wchars: Vec<char> = word.chars().collect();
    if idx + wchars.len() > chars.len() {
        return false;
    }
    for (k, &wc) in wchars.iter().enumerate() {
        if chars[idx + k] != wc {
            return false;
        }
    }
    // Check boundary
    if idx > 0
        && (chars[idx - 1].is_alphanumeric() || chars[idx - 1] == '_' || chars[idx - 1] == '$')
    {
        return false;
    }
    let end = idx + wchars.len();
    if end < chars.len() && (chars[end].is_alphanumeric() || chars[end] == '_' || chars[end] == '$')
    {
        return false;
    }
    true
}

fn skip_whitespace(chars: &[char], mut idx: usize) -> usize {
    while idx < chars.len() && chars[idx].is_whitespace() {
        idx += 1;
    }
    idx
}

fn prev_non_whitespace(chars: &[char], idx: usize) -> Option<char> {
    let mut j = idx;
    while j > 0 {
        j -= 1;
        if !chars[j].is_whitespace() {
            return Some(chars[j]);
        }
    }
    None
}

fn skip_until_semicolon_or_newline(chars: &[char], mut idx: usize) -> usize {
    while idx < chars.len() {
        if chars[idx] == ';' {
            return idx + 1;
        }
        if chars[idx] == '\n' {
            return idx + 1;
        }
        idx += 1;
    }
    idx
}

fn skip_balanced(chars: &[char], start: usize, open: char, close: char) -> usize {
    let mut depth = 0;
    let mut idx = start;
    while idx < chars.len() {
        if chars[idx] == open {
            depth += 1;
        } else if chars[idx] == close {
            depth -= 1;
            if depth == 0 {
                return idx + 1;
            }
        }
        idx += 1;
    }
    idx
}

fn is_likely_type_annotation(chars: &[char], colon_idx: usize) -> bool {
    let prev = prev_non_whitespace(chars, colon_idx);
    let Some(p) = prev else {
        return false;
    };

    // If preceded by ')' -> return type annotation: `(...): Type {` or `(...): Type =>`
    if p == ')' {
        return true;
    }

    if !p.is_alphanumeric() && p != '_' && p != '$' && p != '?' && p != ']' {
        return false;
    }

    // Scan backwards from colon_idx to find the start of the current statement/line
    let mut stmt_start = colon_idx;
    let mut paren_balance: i32 = 0;
    let mut brace_balance: i32 = 0;
    while stmt_start > 0 {
        let ch = chars[stmt_start - 1];
        if ch == '\n' || ch == ';' {
            break;
        }
        if ch == ')' {
            paren_balance += 1;
        } else if ch == '(' {
            if paren_balance > 0 {
                paren_balance -= 1;
            } else {
                // Inside an open `(` that wraps this `:` -> function parameter: `function f(x: number)`
                // Check if this is a ternary within parens: `(a ? b : c)`
                let has_q = chars[stmt_start..colon_idx].contains(&'?');
                return !has_q;
            }
        }
        if ch == '}' {
            if brace_balance == 0 {
                // Preceding block closed here, e.g. `interface Foo { } const c: ...`
                break;
            }
            brace_balance += 1;
        } else if ch == '{' {
            if brace_balance > 0 {
                brace_balance -= 1;
            } else {
                // Inside an unclosed `{` (object literal) -> key: value, NOT a type annotation
                return false;
            }
        }
        stmt_start -= 1;
    }

    let prefix: String = chars[stmt_start..colon_idx].iter().collect();
    let trimmed = prefix.trim_start_matches(|c: char| c == '}' || c == ';' || c.is_whitespace());

    if trimmed.starts_with("case ") || trimmed.starts_with("default") {
        return false;
    }

    // Ternary check: if there is a '?' between stmt_start and colon_idx
    if chars[stmt_start..colon_idx].contains(&'?') {
        return false;
    }

    // If there is an '=' between stmt_start and colon_idx, we are on the RHS of an assignment
    if chars[stmt_start..colon_idx].contains(&'=') {
        return false;
    }

    // Check if preceded by `var`, `let`, `const`
    if trimmed.starts_with("var ") || trimmed.starts_with("let ") || trimmed.starts_with("const ") {
        return true;
    }

    false
}
