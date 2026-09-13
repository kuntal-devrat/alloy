//! SourceMap V3 encoder, decoder, and resolver.
//!
//! Implements the Source Map Revision 3 Proposal standard, supporting:
//! - Base64 VLQ encoding and decoding of variable-length signed integers.
//! - JSON serialization and deserialization of standard v3 source maps.
//! - Fast position lookup: resolves generated (line, column) to original (source, line, column).
//! - Generation of SourceMaps from bytecode line tables.

const BASE64_CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn val_to_b64(val: u8) -> char {
    BASE64_CHARS[(val & 0x3F) as usize] as char
}

fn b64_to_val(c: char) -> Result<u8, String> {
    match c {
        'A'..='Z' => Ok(c as u8 - b'A'),
        'a'..='z' => Ok(c as u8 - b'a' + 26),
        '0'..='9' => Ok(c as u8 - b'0' + 52),
        '+' => Ok(62),
        '/' => Ok(63),
        _ => Err(format!("Invalid Base64 character in sourcemap: '{}'", c)),
    }
}

/// Encode a signed 64-bit integer into Base64 VLQ.
pub fn encode_vlq(val: i64) -> String {
    let mut vlq = if val < 0 {
        ((-val as u64) << 1) | 1
    } else {
        (val as u64) << 1
    };

    let mut out = String::new();
    loop {
        let mut digit = (vlq & 0x1F) as u8;
        vlq >>= 5;
        if vlq > 0 {
            digit |= 0x20;
        }
        out.push(val_to_b64(digit));
        if vlq == 0 {
            break;
        }
    }
    out
}

/// Decode a single signed integer from a Base64 VLQ character iterator.
pub fn decode_vlq(chars: &mut impl Iterator<Item = char>) -> Result<i64, String> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    let mut continuation = true;

    while continuation {
        let ch = chars.next().ok_or_else(|| "Unexpected end of VLQ sequence".to_string())?;
        let digit = b64_to_val(ch)?;
        continuation = (digit & 0x20) != 0;
        let data = (digit & 0x1F) as u64;
        result = result.checked_add(data << shift).ok_or("VLQ overflow")?;
        shift += 5;
    }

    let is_negative = (result & 1) != 0;
    let value = (result >> 1) as i64;
    if is_negative {
        Ok(-value)
    } else {
        Ok(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    pub source_file: String,
    pub line: u32,
    pub col: u32,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceMap {
    pub version: u32,
    pub file: Option<String>,
    pub sources: Vec<String>,
    pub sources_content: Option<Vec<String>>,
    pub names: Vec<String>,
    pub mappings: String,
}

impl SourceMap {
    pub fn new(
        file: Option<String>,
        sources: Vec<String>,
        names: Vec<String>,
        mappings: String,
    ) -> Self {
        Self {
            version: 3,
            file,
            sources,
            sources_content: None,
            names,
            mappings,
        }
    }

    /// Generate SourceMap V3 from bytecode line table.
    /// `line_table` contains `(pc, source_line_1_based, source_col_1_based)`.
    pub fn from_line_table(
        file: Option<String>,
        source_name: String,
        line_table: &[(usize, u32, u32)],
    ) -> Self {
        let mut mappings = String::new();
        let mut prev_src_idx: i64 = 0;
        let mut prev_src_line: i64 = 0;
        let mut prev_src_col: i64 = 0;

        for (i, &(pc, line, col)) in line_table.iter().enumerate() {
            if i > 0 {
                mappings.push(';');
            }
            // Generated column (0 for start of line)
            mappings.push_str(&encode_vlq(0));
            // Source index delta
            mappings.push_str(&encode_vlq(0 - prev_src_idx));
            prev_src_idx = 0;
            // Original line delta (0-based in sourcemap)
            let src_line_0 = (line.saturating_sub(1)) as i64;
            mappings.push_str(&encode_vlq(src_line_0 - prev_src_line));
            prev_src_line = src_line_0;
            // Original column delta (0-based in sourcemap)
            let src_col_0 = (col.saturating_sub(1)) as i64;
            mappings.push_str(&encode_vlq(src_col_0 - prev_src_col));
            prev_src_col = src_col_0;

            let _ = pc;
        }

        Self {
            version: 3,
            file,
            sources: vec![source_name],
            sources_content: None,
            names: Vec::new(),
            mappings,
        }
    }

    /// Serialize this SourceMap to standard V3 JSON.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\"version\":3");
        if let Some(ref f) = self.file {
            out.push_str(",\"file\":");
            out.push_str(&escape_json_string(f));
        }
        out.push_str(",\"sources\":[");
        for (i, s) in self.sources.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&escape_json_string(s));
        }
        out.push(']');

        if let Some(ref sc) = self.sources_content {
            out.push_str(",\"sourcesContent\":[");
            for (i, c) in sc.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&escape_json_string(c));
            }
            out.push(']');
        }

        out.push_str(",\"names\":[");
        for (i, n) in self.names.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&escape_json_string(n));
        }
        out.push(']');

        out.push_str(",\"mappings\":");
        out.push_str(&escape_json_string(&self.mappings));
        out.push('}');
        out
    }

    /// Parse a SourceMap from standard V3 JSON.
    pub fn from_json(json: &str) -> Result<Self, String> {
        let v: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| format!("Invalid JSON: {}", e))?;
        
        let version = v.get("version")
            .and_then(|x| x.as_u64())
            .ok_or_else(|| "Missing or invalid 'version'".to_string())? as u32;
        if version != 3 {
            return Err(format!("Unsupported sourcemap version: {}", version));
        }

        let file = v.get("file").and_then(|x| x.as_str()).map(|s| s.to_string());
        
        let sources = v.get("sources")
            .and_then(|x| x.as_array())
            .map(|arr| arr.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();

        let sources_content = v.get("sourcesContent")
            .and_then(|x| x.as_array())
            .map(|arr| arr.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect());

        let names = v.get("names")
            .and_then(|x| x.as_array())
            .map(|arr| arr.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();

        let mappings = v.get("mappings")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();

        Ok(Self {
            version,
            file,
            sources,
            sources_content,
            names,
            mappings,
        })
    }

    /// Look up original source coordinates for a 1-based (gen_line, gen_col) pair.
    pub fn lookup(&self, gen_line: u32, gen_col: u32) -> Option<SourceLocation> {
        if gen_line == 0 {
            return None;
        }
        let target_line_idx = (gen_line - 1) as usize;
        let lines: Vec<&str> = self.mappings.split(';').collect();
        let _line_mapping = lines.get(target_line_idx)?;

        let mut current_source_idx: i64 = 0;
        let mut current_src_line: i64 = 0;
        let mut current_src_col: i64 = 0;
        let mut current_name_idx: i64 = 0;

        // Cumulative coordinates must be decoded from the start of the file up to target line
        for (l_idx, line_str) in lines.iter().enumerate() {
            let mut line_gen_col: i64 = 0;
            let mut last_match: Option<(i64, i64, i64, Option<i64>)> = None;

            if !line_str.is_empty() {
                for seg in line_str.split(',') {
                    if seg.is_empty() {
                        continue;
                    }
                    let mut it = seg.chars();
                    let col_delta = decode_vlq(&mut it).ok()?;
                    line_gen_col += col_delta;

                    let mut seg_src_idx = None;
                    let mut seg_src_line = None;
                    let mut seg_src_col = None;
                    let mut seg_name_idx = None;

                    if let Ok(src_delta) = decode_vlq(&mut it) {
                        current_source_idx += src_delta;
                        seg_src_idx = Some(current_source_idx);

                        if let Ok(line_delta) = decode_vlq(&mut it) {
                            current_src_line += line_delta;
                            seg_src_line = Some(current_src_line);

                            if let Ok(c_delta) = decode_vlq(&mut it) {
                                current_src_col += c_delta;
                                seg_src_col = Some(current_src_col);

                                if let Ok(n_delta) = decode_vlq(&mut it) {
                                    current_name_idx += n_delta;
                                    seg_name_idx = Some(current_name_idx);
                                }
                            }
                        }
                    }

                    if l_idx == target_line_idx {
                        if let (Some(s_idx), Some(s_line), Some(s_col)) =
                            (seg_src_idx, seg_src_line, seg_src_col)
                        {
                            if line_gen_col as u32 <= gen_col.saturating_sub(1) || last_match.is_none() {
                                last_match = Some((s_idx, s_line, s_col, seg_name_idx));
                            }
                        }
                    }
                }
            }

            if l_idx == target_line_idx {
                if let Some((s_idx, s_line, s_col, n_idx)) = last_match {
                    let source_file = self.sources.get(s_idx as usize)?.clone();
                    let name = n_idx.and_then(|idx| self.names.get(idx as usize)).cloned();
                    return Some(SourceLocation {
                        source_file,
                        line: (s_line + 1) as u32,
                        col: (s_col + 1) as u32,
                        name,
                    });
                }
            }
        }

        None
    }
}

fn escape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0C' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vlq_roundtrip() {
        let values = [0, 1, -1, 15, -15, 16, -16, 31, -31, 32, -32, 100, -100, 1024, -1024, 1234567, -1234567];
        for &v in &values {
            let encoded = encode_vlq(v);
            let decoded = decode_vlq(&mut encoded.chars()).expect("decode failed");
            assert_eq!(v, decoded, "Failed for value {}", v);
        }
    }

    #[test]
    fn test_sourcemap_json_roundtrip() {
        let sm = SourceMap {
            version: 3,
            file: Some("out.js".to_string()),
            sources: vec!["foo.js".to_string(), "bar.js".to_string()],
            sources_content: Some(vec!["let x = 1;".to_string()]),
            names: vec!["x".to_string()],
            mappings: "AAAA;ACAA".to_string(),
        };

        let json = sm.to_json();
        let sm2 = SourceMap::from_json(&json).expect("from_json failed");
        assert_eq!(sm.version, sm2.version);
        assert_eq!(sm.file, sm2.file);
        assert_eq!(sm.sources, sm2.sources);
        assert_eq!(sm.sources_content, sm2.sources_content);
        assert_eq!(sm.names, sm2.names);
        assert_eq!(sm.mappings, sm2.mappings);
    }

    #[test]
    fn test_sourcemap_lookup() {
        let table = vec![
            (0, 1, 1),
            (10, 5, 3),
            (25, 10, 8),
        ];
        let sm = SourceMap::from_line_table(Some("out.js".to_string()), "app.ajs".to_string(), &table);
        
        let loc1 = sm.lookup(1, 1).expect("lookup line 1 failed");
        assert_eq!(loc1.source_file, "app.ajs");
        assert_eq!(loc1.line, 1);
        assert_eq!(loc1.col, 1);

        let loc2 = sm.lookup(2, 1).expect("lookup line 2 failed");
        assert_eq!(loc2.source_file, "app.ajs");
        assert_eq!(loc2.line, 5);
        assert_eq!(loc2.col, 3);

        let loc3 = sm.lookup(3, 1).expect("lookup line 3 failed");
        assert_eq!(loc3.source_file, "app.ajs");
        assert_eq!(loc3.line, 10);
        assert_eq!(loc3.col, 8);
    }
}
