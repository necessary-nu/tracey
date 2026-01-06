//! LSP server for tracey
//!
//! Provides IDE features for requirement traceability:
//! - Hover: show requirement text and coverage info
//! - Completions: suggest requirement IDs when typing r[...]
//! - Go-to-definition: jump from reference to spec definition
//!
//! r[impl lsp.lifecycle.stdio]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use eyre::Result;
use tokio::sync::watch;
use tower_lsp::jsonrpc::Result as LspResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use crate::serve::DashboardData;

/// Run the LSP server over stdio
/// r[impl cli.lsp]
pub async fn run(root: Option<PathBuf>, config_path: Option<PathBuf>) -> Result<()> {
    use crate::serve::build_dashboard_data;

    // Determine project root
    let project_root = match root {
        Some(r) => r,
        None => crate::find_project_root()?,
    };

    // Load config
    let config_path = config_path.unwrap_or_else(|| project_root.join(".config/tracey/config.kdl"));
    let config = crate::load_config(&config_path)?;

    // Build initial dashboard data (quiet mode - no stderr output since LSP uses stdio)
    let initial_data = build_dashboard_data(&project_root, &config, 1, true).await?;

    // Create watch channel for data
    let (_data_tx, data_rx) = watch::channel(Arc::new(initial_data));

    // TODO: Add file watching to rebuild data on changes
    // For now, LSP just uses the initial data snapshot

    // Run LSP server
    run_lsp_server(data_rx, project_root).await
}

/// Internal: run the LSP server with pre-built data
async fn run_lsp_server(
    data_rx: watch::Receiver<Arc<DashboardData>>,
    project_root: PathBuf,
) -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| Backend::new(client, data_rx, project_root));
    Server::new(stdin, stdout, socket).serve(service).await;

    Ok(())
}

struct Backend {
    #[allow(dead_code)]
    client: Client,
    /// Receiver for dashboard data updates
    data_rx: watch::Receiver<Arc<DashboardData>>,
    /// Project root for resolving paths
    project_root: PathBuf,
    /// Document content cache: uri -> content
    documents: std::sync::RwLock<HashMap<String, String>>,
}

impl Backend {
    fn new(
        client: Client,
        data_rx: watch::Receiver<Arc<DashboardData>>,
        project_root: PathBuf,
    ) -> Self {
        Self {
            client,
            data_rx,
            project_root,
            documents: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Get current dashboard data
    fn data(&self) -> Arc<DashboardData> {
        self.data_rx.borrow().clone()
    }

    /// Get all requirement IDs with their descriptions from current data
    fn get_requirements(&self) -> Vec<(String, String, String)> {
        let data = self.data();
        let mut reqs = Vec::new();

        // Collect from all specs
        for (impl_key, spec_data) in &data.forward_by_impl {
            for rule in &spec_data.rules {
                // (id, description/html, spec_name)
                reqs.push((rule.id.clone(), rule.html.clone(), impl_key.0.clone()));
            }
        }

        reqs
    }

    /// Find requirement by ID
    fn find_requirement(&self, req_id: &str) -> Option<RequirementInfo> {
        let data = self.data();

        for (impl_key, spec_data) in &data.forward_by_impl {
            for rule in &spec_data.rules {
                if rule.id == req_id {
                    return Some(RequirementInfo {
                        id: rule.id.clone(),
                        html: rule.html.clone(),
                        source_file: rule.source_file.clone().unwrap_or_default(),
                        source_line: rule.source_line,
                        source_column: rule.source_column,
                        impl_refs: rule.impl_refs.clone(),
                        verify_refs: rule.verify_refs.clone(),
                        spec_name: impl_key.0.clone(),
                    });
                }
            }
        }

        None
    }

    /// Find requirement reference at position in document
    fn find_req_at_position(&self, uri: &Url, position: Position) -> Option<(String, Range)> {
        let docs = self.documents.read().ok()?;
        let content = docs.get(uri.as_str())?;

        let lines: Vec<&str> = content.lines().collect();
        let line = lines.get(position.line as usize)?;

        // Find r[...] pattern at cursor position
        let mut in_bracket = false;
        let mut bracket_start = 0;

        for (i, c) in line.char_indices() {
            if !in_bracket {
                // Look for r[ pattern (or any single-char prefix followed by [)
                if line[i..].starts_with('[') && i > 0 {
                    let prev_char = line[..i].chars().last()?;
                    if prev_char.is_alphabetic() || prev_char.is_numeric() {
                        // Find start of prefix
                        let prefix_start = line[..i]
                            .char_indices()
                            .rev()
                            .take_while(|(_, c)| c.is_alphanumeric())
                            .last()
                            .map(|(idx, _)| idx)
                            .unwrap_or(i - 1);
                        bracket_start = prefix_start;
                        in_bracket = true;
                    }
                }
            } else if c == ']' {
                let bracket_end = i;
                // Check if position is within this range
                if position.character as usize >= bracket_start
                    && position.character as usize <= bracket_end
                {
                    // Extract the content between brackets
                    let bracket_open = line[bracket_start..].find('[')? + bracket_start;
                    let inner = &line[bracket_open + 1..bracket_end];

                    // Parse: might be "impl foo.bar" or just "foo.bar"
                    let req_id = if let Some(space_pos) = inner.find(' ') {
                        inner[space_pos + 1..].trim()
                    } else {
                        inner.trim()
                    };

                    let range = Range {
                        start: Position {
                            line: position.line,
                            character: bracket_start as u32,
                        },
                        end: Position {
                            line: position.line,
                            character: (bracket_end + 1) as u32,
                        },
                    };

                    return Some((req_id.to_string(), range));
                }
                in_bracket = false;
            }
        }

        None
    }

    /// Get raw content after prefix[ up to cursor (for completion logic)
    fn get_completion_context(&self, uri: &Url, position: Position) -> Option<String> {
        let docs = self.documents.read().ok()?;
        let content = docs.get(uri.as_str())?;

        let lines: Vec<&str> = content.lines().collect();
        let line = lines.get(position.line as usize)?;
        let col = position.character as usize;

        let before_cursor = &line[..col.min(line.len())];

        // Find the last prefix[ pattern before cursor
        // Look for patterns like r[, m[, h2[, etc.
        for (i, _) in before_cursor.char_indices().rev() {
            if before_cursor[i..].starts_with('[') && i > 0 {
                let after_bracket = &before_cursor[i + 1..];
                if !after_bracket.contains(']') {
                    return Some(after_bracket.to_string());
                }
            }
        }

        None
    }

    /// Get configured prefixes from data
    #[allow(dead_code)]
    fn get_prefixes(&self) -> Vec<(String, String)> {
        let data = self.data();
        data.config
            .specs
            .iter()
            .map(|s| (s.prefix.clone(), s.name.clone()))
            .collect()
    }
}

struct RequirementInfo {
    id: String,
    html: String,
    source_file: String,
    source_line: Option<usize>,
    source_column: Option<usize>,
    impl_refs: Vec<crate::serve::ApiCodeRef>,
    verify_refs: Vec<crate::serve::ApiCodeRef>,
    #[allow(dead_code)]
    spec_name: String,
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    // r[impl lsp.lifecycle.initialize]
    async fn initialize(&self, _: InitializeParams) -> LspResult<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                // r[impl lsp.completions.trigger]
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        "[".to_string(),
                        ".".to_string(),
                        " ".to_string(),
                    ]),
                    ..Default::default()
                }),
                // r[impl lsp.hover.req-reference]
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                // r[impl lsp.goto.ref-to-def]
                definition_provider: Some(OneOf::Left(true)),
                // r[impl lsp.highlight.full-range]
                document_highlight_provider: Some(OneOf::Left(true)),
                // r[impl lsp.impl.from-ref]
                implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
                // Sync full document content
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "tracey".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn shutdown(&self) -> LspResult<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let content = params.text_document.text;
        if let Ok(mut docs) = self.documents.write() {
            docs.insert(uri, content);
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        if let Some(change) = params.content_changes.into_iter().next()
            && let Ok(mut docs) = self.documents.write()
        {
            docs.insert(uri, change.text);
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        if let Ok(mut docs) = self.documents.write() {
            docs.remove(&uri);
        }
    }

    // r[impl lsp.completions.req-id]
    async fn completion(&self, params: CompletionParams) -> LspResult<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;

        let Some(raw) = self.get_completion_context(uri, position) else {
            return Ok(None);
        };

        let verbs = ["impl", "verify", "depends", "related"];

        // Check if user is typing a verb (no space yet)
        let is_typing_verb = verbs.iter().any(|v| v.starts_with(&raw) || raw == *v);

        // r[impl lsp.completions.verb]
        if is_typing_verb && !raw.contains(' ') {
            let items: Vec<CompletionItem> = verbs
                .iter()
                .filter(|v| v.starts_with(&raw))
                .map(|v| CompletionItem {
                    label: v.to_string(),
                    kind: Some(CompletionItemKind::KEYWORD),
                    detail: Some(format!("{} reference", v)),
                    insert_text: Some(format!("{} ", v)),
                    ..Default::default()
                })
                .collect();
            if !items.is_empty() {
                return Ok(Some(CompletionResponse::Array(items)));
            }
        }

        // Extract requirement prefix (after verb if present)
        let req_prefix = if let Some(space_pos) = raw.find(' ') {
            &raw[space_pos + 1..]
        } else if !is_typing_verb {
            raw.as_str()
        } else {
            ""
        };

        // r[impl lsp.completions.req-id-fuzzy]
        let requirements = self.get_requirements();
        let items: Vec<CompletionItem> = requirements
            .iter()
            .filter(|(id, _, _)| req_prefix.is_empty() || id.contains(req_prefix))
            .map(|(id, html, spec)| {
                // Strip HTML tags for plain text description
                let plain_text = html
                    .replace("<p>", "")
                    .replace("</p>", "")
                    .replace("<code>", "`")
                    .replace("</code>", "`");

                // r[impl lsp.completions.req-id-preview]
                CompletionItem {
                    label: id.clone(),
                    kind: Some(CompletionItemKind::REFERENCE),
                    detail: Some(format!("[{}]", spec)),
                    documentation: Some(Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: format!("**{}**\n\n{}", id, plain_text),
                    })),
                    ..Default::default()
                }
            })
            .collect();

        if !items.is_empty() {
            return Ok(Some(CompletionResponse::Array(items)));
        }

        Ok(None)
    }

    // r[impl lsp.hover.req-reference]
    async fn hover(&self, params: HoverParams) -> LspResult<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let Some((req_id, range)) = self.find_req_at_position(uri, position) else {
            return Ok(None);
        };

        if let Some(info) = self.find_requirement(&req_id) {
            // Strip HTML for markdown display
            let plain_text = info
                .html
                .replace("<p>", "")
                .replace("</p>", "\n\n")
                .replace("<code>", "`")
                .replace("</code>", "`");

            // r[impl lsp.hover.req-reference-format]
            let mut content = format!("### {}\n\n{}", info.id, plain_text.trim());

            // Add coverage info
            if !info.impl_refs.is_empty() || !info.verify_refs.is_empty() {
                content.push_str("\n\n---\n");
                if !info.impl_refs.is_empty() {
                    content.push_str(&format!(
                        "\n**Implementations:** {}\n",
                        info.impl_refs.len()
                    ));
                    for r in info.impl_refs.iter().take(3) {
                        content.push_str(&format!("- {}:{}\n", r.file, r.line));
                    }
                    if info.impl_refs.len() > 3 {
                        content.push_str(&format!("- ... and {} more\n", info.impl_refs.len() - 3));
                    }
                }
                if !info.verify_refs.is_empty() {
                    content.push_str(&format!("\n**Tests:** {}\n", info.verify_refs.len()));
                    for r in info.verify_refs.iter().take(3) {
                        content.push_str(&format!("- {}:{}\n", r.file, r.line));
                    }
                }
            }

            content.push_str(&format!("\n\n*Defined in: {}*", info.source_file));

            return Ok(Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: content,
                }),
                range: Some(range),
            }));
        }

        // Unknown requirement
        // r[impl lsp.diagnostics.broken-refs-message]
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("⚠️ **Unknown requirement:** `{}`", req_id),
            }),
            range: Some(range),
        }))
    }

    // r[impl lsp.goto.ref-to-def]
    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> LspResult<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let Some((req_id, _range)) = self.find_req_at_position(uri, position) else {
            return Ok(None);
        };

        let Some(info) = self.find_requirement(&req_id) else {
            return Ok(None);
        };

        // Construct path to spec file
        let spec_path = self.project_root.join(&info.source_file);

        let Ok(target_uri) = Url::from_file_path(&spec_path) else {
            return Ok(None);
        };

        // Use source_line/column if available (convert from 1-indexed to 0-indexed)
        let line = info
            .source_line
            .map(|l| l.saturating_sub(1) as u32)
            .unwrap_or(0);
        let character = info
            .source_column
            .map(|c| c.saturating_sub(1) as u32)
            .unwrap_or(0);

        Ok(Some(GotoDefinitionResponse::Scalar(Location {
            uri: target_uri,
            range: Range {
                start: Position { line, character },
                end: Position { line, character },
            },
        })))
    }

    // r[impl lsp.impl.from-ref]
    // r[impl lsp.impl.multiple]
    async fn goto_implementation(
        &self,
        params: GotoDefinitionParams,
    ) -> LspResult<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        let Some((req_id, _range)) = self.find_req_at_position(uri, position) else {
            return Ok(None);
        };

        let Some(info) = self.find_requirement(&req_id) else {
            return Ok(None);
        };

        if info.impl_refs.is_empty() {
            return Ok(None);
        }

        // Convert impl refs to locations
        let locations: Vec<Location> = info
            .impl_refs
            .iter()
            .filter_map(|r| {
                let path = self.project_root.join(&r.file);
                let uri = Url::from_file_path(&path).ok()?;
                let line = r.line.saturating_sub(1) as u32;
                Some(Location {
                    uri,
                    range: Range {
                        start: Position { line, character: 0 },
                        end: Position { line, character: 0 },
                    },
                })
            })
            .collect();

        if locations.is_empty() {
            return Ok(None);
        }

        // Return single location or array depending on count
        if locations.len() == 1 {
            Ok(Some(GotoDefinitionResponse::Scalar(
                locations.into_iter().next().unwrap(),
            )))
        } else {
            Ok(Some(GotoDefinitionResponse::Array(locations)))
        }
    }

    // r[impl lsp.highlight.full-range]
    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> LspResult<Option<Vec<DocumentHighlight>>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;

        // Find requirement at position and return its full range
        let Some((_req_id, range)) = self.find_req_at_position(uri, position) else {
            return Ok(None);
        };

        Ok(Some(vec![DocumentHighlight {
            range,
            kind: Some(DocumentHighlightKind::TEXT),
        }]))
    }
}
