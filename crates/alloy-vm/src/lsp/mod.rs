pub mod docs;
pub mod protocol;
pub mod server;

pub use server::{run_server_stdio, LspServer};

#[cfg(test)]
mod tests {
    use super::server::LspServer;
    use serde_json::json;

    #[test]
    fn test_lsp_initialize_and_capabilities() {
        let mut server = LspServer::new();
        let init_req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": 1234,
                "rootUri": "file:///workspace"
            }
        });

        let resps = server.handle_incoming(&init_req.to_string());
        assert_eq!(resps.len(), 1);
        let resp = &resps[0];
        assert_eq!(resp.id, Some(json!(1)));
        let result = resp.result.as_ref().unwrap();
        assert!(result.get("capabilities").is_some());
        let caps = result.get("capabilities").unwrap();
        assert_eq!(caps["textDocumentSync"], 1);
        assert_eq!(caps["hoverProvider"], true);
        assert_eq!(caps["documentSymbolProvider"], true);
    }

    #[test]
    fn test_lsp_diagnostics_valid_and_invalid() {
        let mut server = LspServer::new();

        // Open invalid document
        let open_invalid = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": "file:///test.ajs",
                    "languageId": "alloy",
                    "version": 1,
                    "text": "function test( { return 42; }" // syntax error: missing ')'
                }
            }
        });

        let resps = server.handle_incoming(&open_invalid.to_string());
        assert_eq!(resps.len(), 1);
        let notif = &resps[0];
        assert_eq!(
            notif.method.as_deref(),
            Some("textDocument/publishDiagnostics")
        );
        let params = notif.params.as_ref().unwrap();
        let diags = params["diagnostics"].as_array().unwrap();
        assert!(!diags.is_empty(), "Should report syntax error diagnostic");
        assert_eq!(diags[0]["severity"], 1);

        // Update with valid content
        let change_valid = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didChange",
            "params": {
                "textDocument": {
                    "uri": "file:///test.ajs",
                    "version": 2
                },
                "contentChanges": [
                    { "text": "function test() { return 42; }" }
                ]
            }
        });

        let resps2 = server.handle_incoming(&change_valid.to_string());
        assert_eq!(resps2.len(), 1);
        let params2 = resps2[0].params.as_ref().unwrap();
        let diags2 = params2["diagnostics"].as_array().unwrap();
        assert_eq!(diags2.len(), 0, "Valid code should produce 0 diagnostics");
    }

    #[test]
    fn test_lsp_hover_keywords_and_builtins() {
        let mut server = LspServer::new();
        server.documents.insert(
            "file:///test.ajs".to_string(),
            "const ch = channel();\nfetchSync(\"http://localhost\");".to_string(),
        );

        // Hover over 'channel' on line 0, char 12
        let hover_req = json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "textDocument/hover",
            "params": {
                "textDocument": { "uri": "file:///test.ajs" },
                "position": { "line": 0, "character": 12 }
            }
        });

        let resps = server.handle_incoming(&hover_req.to_string());
        assert_eq!(resps.len(), 1);
        let result = resps[0].result.as_ref().unwrap();
        assert!(!result.is_null());
        let val = result["contents"]["value"].as_str().unwrap();
        assert!(val.contains("channel()"));
        assert!(val.contains("Channel"));
    }

    #[test]
    fn test_lsp_completions() {
        let server = LspServer::new();
        let list = server.handle_completion();
        assert!(!list.items.is_empty());
        let labels: Vec<&str> = list.items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"async"));
        assert!(labels.contains(&"channel"));
        assert!(labels.contains(&"spawn"));
        assert!(labels.contains(&"fetchSync"));
        assert!(labels.contains(&"http"));
        assert!(labels.contains(&"memory"));
    }

    #[test]
    fn test_lsp_document_symbols() {
        let mut server = LspServer::new();
        server.documents.insert(
            "file:///test.ajs".to_string(),
            r#"
                function add(a, b) {
                    return a + b;
                }
                class Counter {
                    count = 0;
                    increment() {
                        this.count++;
                    }
                }
            "#
            .to_string(),
        );

        let syms = server.handle_document_symbols("file:///test.ajs");
        assert!(!syms.is_empty());
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"add"));
        assert!(names.contains(&"Counter"));

        let counter_sym = syms.iter().find(|s| s.name == "Counter").unwrap();
        assert_eq!(counter_sym.kind, 5); // Class
        assert!(counter_sym.children.is_some());
        let children = counter_sym.children.as_ref().unwrap();
        let child_names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
        assert!(child_names.contains(&"count"));
        assert!(child_names.contains(&"increment"));
    }
}
