use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use dagrun_ast::{
    AnnotationKind, BodyLine, CommandSegment, Dependency, Item, KeyValue, ParseError, SourceFile,
    Span, Spanned, parse,
};
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

// semantic token types we emit
const TOKEN_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::FUNCTION,  // 0 - task names
    SemanticTokenType::VARIABLE,  // 1 - variables
    SemanticTokenType::DECORATOR, // 2 - annotations
    SemanticTokenType::COMMENT,   // 3 - comments
    SemanticTokenType::KEYWORD,   // 4 - shebangs, set, @lua/@end
    SemanticTokenType::STRING,    // 5 - string values
    SemanticTokenType::OPERATOR,  // 6 - := : =
    SemanticTokenType::PARAMETER, // 7 - annotation parameters
];

const TOKEN_MODIFIERS: &[SemanticTokenModifier] = &[
    SemanticTokenModifier::DECLARATION, // 0
    SemanticTokenModifier::DEFINITION,  // 1
    SemanticTokenModifier::READONLY,    // 2
];

pub struct Backend {
    client: Client,
    documents: Arc<RwLock<HashMap<Url, String>>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            documents: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn publish_diagnostics(&self, uri: Url, source: &str) {
        let (ast, errors) = parse(source);

        // parse errors
        let mut diagnostics: Vec<Diagnostic> =
            errors.iter().map(|e| to_diagnostic(source, e)).collect();

        // semantic diagnostics
        diagnostics.extend(check_undefined_variables(source, &ast));
        diagnostics.extend(check_undefined_tasks(source, &ast));
        diagnostics.extend(check_dependency_cycles(source, &ast));
        diagnostics.extend(check_unused_variables(source, &ast));

        // filesystem diagnostics (paths, executables)
        let working_dir = uri
            .to_file_path()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()));
        diagnostics.extend(check_filesystem(source, &ast, working_dir.as_deref()));

        self.client
            .publish_diagnostics(uri, diagnostics, None)
            .await;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: SemanticTokensLegend {
                                token_types: TOKEN_TYPES.to_vec(),
                                token_modifiers: TOKEN_MODIFIERS.to_vec(),
                            },
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: None,
                            ..Default::default()
                        },
                    ),
                ),
                definition_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                references_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                document_symbol_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![
                        "{".to_string(), // for {{var}}
                        "@".to_string(), // for annotations
                        ":".to_string(), // after task name
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        tracing::info!("dagrun-lsp initialized");
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;
        self.documents
            .write()
            .await
            .insert(uri.clone(), text.clone());
        self.publish_diagnostics(uri, &text).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        if let Some(change) = params.content_changes.into_iter().next() {
            self.documents
                .write()
                .await
                .insert(uri.clone(), change.text.clone());
            self.publish_diagnostics(uri, &change.text).await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.documents.write().await.remove(&uri);
        self.client.publish_diagnostics(uri, vec![], None).await;
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        let docs = self.documents.read().await;
        let Some(source) = docs.get(&uri) else {
            return Ok(None);
        };

        let (ast, _) = parse(source);
        let tokens = collect_semantic_tokens(source, &ast);
        let data = encode_tokens(source, tokens);

        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let docs = self.documents.read().await;
        let Some(source) = docs.get(&uri) else {
            return Ok(None);
        };

        let (ast, _) = parse(source);
        let offset = position_to_offset(source, pos);

        if let Some(def_span) = find_definition_at(source, &ast, offset) {
            let range = span_to_range(source, def_span);
            return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                uri: uri.clone(),
                range,
            })));
        }

        Ok(None)
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;

        let docs = self.documents.read().await;
        let Some(source) = docs.get(&uri) else {
            return Ok(None);
        };

        let (ast, _) = parse(source);
        let items = get_completions(source, &ast, pos);

        if items.is_empty() {
            return Ok(None);
        }

        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let docs = self.documents.read().await;
        let Some(source) = docs.get(&uri) else {
            return Ok(None);
        };

        let (ast, _) = parse(source);
        let offset = position_to_offset(source, pos);

        if let Some((content, range)) = get_hover_info(source, &ast, offset) {
            return Ok(Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: content,
                }),
                range: Some(range),
            }));
        }

        Ok(None)
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;

        let docs = self.documents.read().await;
        let Some(source) = docs.get(&uri) else {
            return Ok(None);
        };

        let (ast, _) = parse(source);
        let offset = position_to_offset(source, pos);

        if let Some(refs) =
            find_all_references(source, &ast, offset, params.context.include_declaration)
        {
            let locations: Vec<Location> = refs
                .into_iter()
                .map(|span| Location {
                    uri: uri.clone(),
                    range: span_to_range(source, span),
                })
                .collect();
            return Ok(Some(locations));
        }

        Ok(None)
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = params.text_document.uri;
        let pos = params.position;

        let docs = self.documents.read().await;
        let Some(source) = docs.get(&uri) else {
            return Ok(None);
        };

        let (ast, _) = parse(source);
        let offset = position_to_offset(source, pos);

        // check if cursor is on a renameable symbol
        if let Some((name, span)) = get_symbol_at(source, &ast, offset) {
            return Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
                range: span_to_range(source, span),
                placeholder: name,
            }));
        }

        Ok(None)
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let new_name = params.new_name;

        let docs = self.documents.read().await;
        let Some(source) = docs.get(&uri) else {
            return Ok(None);
        };

        let (ast, _) = parse(source);
        let offset = position_to_offset(source, pos);

        if let Some(refs) = find_all_references(source, &ast, offset, true) {
            let edits: Vec<TextEdit> = refs
                .into_iter()
                .map(|span| TextEdit {
                    range: span_to_range(source, span),
                    new_text: new_name.clone(),
                })
                .collect();

            let mut changes = HashMap::new();
            changes.insert(uri, edits);

            return Ok(Some(WorkspaceEdit {
                changes: Some(changes),
                ..Default::default()
            }));
        }

        Ok(None)
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;

        let docs = self.documents.read().await;
        let Some(source) = docs.get(&uri) else {
            return Ok(None);
        };

        let (ast, _) = parse(source);
        let symbols = collect_document_symbols(source, &ast);

        Ok(Some(DocumentSymbolResponse::Flat(symbols)))
    }
}

#[derive(Debug, Clone, Copy)]
struct RawToken {
    span: Span,
    token_type: u32,
    modifiers: u32,
}

fn collect_semantic_tokens(source: &str, ast: &SourceFile) -> Vec<RawToken> {
    let mut tokens = Vec::new();

    for item in &ast.items {
        match &item.node {
            Item::Variable(var) => {
                // variable name
                tokens.push(RawToken {
                    span: var.name.span,
                    token_type: 1, // VARIABLE
                    modifiers: 1,  // DECLARATION
                });
                // :=
                tokens.push(RawToken {
                    span: var.assign_span,
                    token_type: 6, // OPERATOR
                    modifiers: 0,
                });
            }

            Item::Task(task) => {
                // annotations
                for ann in &task.annotations {
                    // @ token
                    tokens.push(RawToken {
                        span: ann.node.at_span,
                        token_type: 2, // DECORATOR
                        modifiers: 0,
                    });
                    // annotation name and params
                    collect_annotation_tokens(&ann.node.kind, &mut tokens);
                }

                // task name
                tokens.push(RawToken {
                    span: task.name.span,
                    token_type: 0, // FUNCTION
                    modifiers: 1,  // DEFINITION
                });
                // task parameters
                for param in &task.parameters {
                    tokens.push(RawToken {
                        span: param.node.name.span,
                        token_type: 7, // PARAMETER
                        modifiers: 1,  // DEFINITION
                    });
                    // highlight default value if present
                    if let Some(default) = &param.node.default {
                        match &default.node {
                            dagrun_ast::ParameterDefault::Literal(_) => {
                                tokens.push(RawToken {
                                    span: default.span,
                                    token_type: 5, // STRING
                                    modifiers: 0,
                                });
                            }
                            dagrun_ast::ParameterDefault::Variable(interp) => {
                                tokens.push(RawToken {
                                    span: interp.open_span,
                                    token_type: 6, // OPERATOR
                                    modifiers: 0,
                                });
                                tokens.push(RawToken {
                                    span: interp.name.span,
                                    token_type: 1, // VARIABLE
                                    modifiers: 0,
                                });
                                if let Some(close) = interp.close_span {
                                    tokens.push(RawToken {
                                        span: close,
                                        token_type: 6, // OPERATOR
                                        modifiers: 0,
                                    });
                                }
                            }
                        }
                    }
                }
                // colon
                tokens.push(RawToken {
                    span: task.colon_span,
                    token_type: 6, // OPERATOR
                    modifiers: 0,
                });
                // dependencies
                for dep in &task.dependencies {
                    tokens.push(RawToken {
                        span: dep.span,
                        token_type: 0, // FUNCTION (reference to another task)
                        modifiers: 0,
                    });
                }
                // body
                if let Some(body) = &task.body {
                    for line in &body.lines {
                        match &line.node {
                            BodyLine::Shebang(shebang) => {
                                tokens.push(RawToken {
                                    span: shebang.prefix_span,
                                    token_type: 4, // KEYWORD
                                    modifiers: 0,
                                });
                                tokens.push(RawToken {
                                    span: shebang.interpreter.span,
                                    token_type: 5, // STRING
                                    modifiers: 0,
                                });
                            }
                            BodyLine::Command(cmd) => {
                                for seg in &cmd.segments {
                                    if let CommandSegment::Interpolation(interp) = &seg.node {
                                        tokens.push(RawToken {
                                            span: interp.open_span,
                                            token_type: 6, // OPERATOR
                                            modifiers: 0,
                                        });
                                        tokens.push(RawToken {
                                            span: interp.name.span,
                                            token_type: 1, // VARIABLE
                                            modifiers: 0,
                                        });
                                        if let Some(close) = interp.close_span {
                                            tokens.push(RawToken {
                                                span: close,
                                                token_type: 6, // OPERATOR
                                                modifiers: 0,
                                            });
                                        }
                                    }
                                }
                            }
                            BodyLine::Empty => {}
                        }
                    }
                }
            }

            Item::LuaBlock(lua) => {
                // @lua
                tokens.push(RawToken {
                    span: lua.open_span,
                    token_type: 4, // KEYWORD
                    modifiers: 0,
                });
                // @end
                if let Some(close) = lua.close_span {
                    tokens.push(RawToken {
                        span: close,
                        token_type: 4, // KEYWORD
                        modifiers: 0,
                    });
                }
            }

            Item::SetDirective(set) => {
                // set keyword
                tokens.push(RawToken {
                    span: set.set_span,
                    token_type: 4, // KEYWORD
                    modifiers: 0,
                });
                // key
                tokens.push(RawToken {
                    span: set.key.span,
                    token_type: 7, // PARAMETER
                    modifiers: 0,
                });
                // :=
                tokens.push(RawToken {
                    span: set.assign_span,
                    token_type: 6, // OPERATOR
                    modifiers: 0,
                });
                // value
                tokens.push(RawToken {
                    span: set.value.span,
                    token_type: 5, // STRING
                    modifiers: 0,
                });
            }

            Item::Comment(_) => {
                // entire comment line
                tokens.push(RawToken {
                    span: item.span,
                    token_type: 3, // COMMENT
                    modifiers: 0,
                });
            }
        }
    }

    // sort by position
    tokens.sort_by_key(|t| t.span.start);
    tokens
}

fn collect_annotation_tokens(kind: &AnnotationKind, tokens: &mut Vec<RawToken>) {
    match kind {
        AnnotationKind::Timeout(val) | AnnotationKind::Retry(val) => {
            tokens.push(RawToken {
                span: val.span,
                token_type: 5, // STRING
                modifiers: 0,
            });
        }
        AnnotationKind::PipeFrom(items) => {
            for item in items {
                tokens.push(RawToken {
                    span: item.span,
                    token_type: 0, // FUNCTION (task reference)
                    modifiers: 0,
                });
            }
        }
        AnnotationKind::Ssh(ssh) => {
            if let Some(host) = &ssh.host {
                tokens.push(RawToken {
                    span: host.span,
                    token_type: 5, // STRING
                    modifiers: 0,
                });
            }
            collect_kv_tokens(&ssh.options, tokens);
        }
        AnnotationKind::Upload(ft)
        | AnnotationKind::Download(ft)
        | AnnotationKind::K8sUpload(ft)
        | AnnotationKind::K8sDownload(ft) => {
            tokens.push(RawToken {
                span: ft.local.span,
                token_type: 5,
                modifiers: 0,
            });
            tokens.push(RawToken {
                span: ft.colon_span,
                token_type: 6,
                modifiers: 0,
            });
            tokens.push(RawToken {
                span: ft.remote.span,
                token_type: 5,
                modifiers: 0,
            });
        }
        AnnotationKind::Service(svc) | AnnotationKind::Extern(svc) => {
            collect_kv_tokens(&svc.options, tokens);
        }
        AnnotationKind::K8s(k8s) => {
            if let Some(mode) = &k8s.mode {
                tokens.push(RawToken {
                    span: mode.span,
                    token_type: 4, // KEYWORD
                    modifiers: 0,
                });
            }
            collect_kv_tokens(&k8s.options, tokens);
        }
        AnnotationKind::K8sConfigmap(cm) | AnnotationKind::K8sSecret(cm) => {
            tokens.push(RawToken {
                span: cm.name.span,
                token_type: 5,
                modifiers: 0,
            });
            tokens.push(RawToken {
                span: cm.colon_span,
                token_type: 6,
                modifiers: 0,
            });
            tokens.push(RawToken {
                span: cm.path.span,
                token_type: 5,
                modifiers: 0,
            });
        }
        AnnotationKind::K8sForward(pf) => {
            tokens.push(RawToken {
                span: pf.local_port.span,
                token_type: 5,
                modifiers: 0,
            });
            tokens.push(RawToken {
                span: pf.first_colon,
                token_type: 6,
                modifiers: 0,
            });
            tokens.push(RawToken {
                span: pf.resource.span,
                token_type: 5,
                modifiers: 0,
            });
            tokens.push(RawToken {
                span: pf.second_colon,
                token_type: 6,
                modifiers: 0,
            });
            tokens.push(RawToken {
                span: pf.remote_port.span,
                token_type: 5,
                modifiers: 0,
            });
        }
        AnnotationKind::Join => {}
        AnnotationKind::Unknown { name, rest } => {
            tokens.push(RawToken {
                span: name.span,
                token_type: 2, // DECORATOR
                modifiers: 0,
            });
            if let Some(r) = rest {
                tokens.push(RawToken {
                    span: r.span,
                    token_type: 5,
                    modifiers: 0,
                });
            }
        }
    }
}

fn collect_kv_tokens(opts: &[Spanned<dagrun_ast::KeyValue>], tokens: &mut Vec<RawToken>) {
    for kv in opts {
        tokens.push(RawToken {
            span: kv.node.key.span,
            token_type: 7, // PARAMETER
            modifiers: 0,
        });
        tokens.push(RawToken {
            span: kv.node.eq_span,
            token_type: 6, // OPERATOR
            modifiers: 0,
        });
        tokens.push(RawToken {
            span: kv.node.value.span,
            token_type: 5, // STRING
            modifiers: 0,
        });
    }
}

fn encode_tokens(source: &str, tokens: Vec<RawToken>) -> Vec<SemanticToken> {
    let mut result = Vec::with_capacity(tokens.len());
    let mut prev_line = 0u32;
    let mut prev_char = 0u32;

    for tok in tokens {
        let pos = offset_to_position(source, tok.span.start as usize);
        let length = tok.span.end.saturating_sub(tok.span.start);

        let delta_line = pos.line - prev_line;
        let delta_start = if delta_line == 0 {
            pos.character - prev_char
        } else {
            pos.character
        };

        result.push(SemanticToken {
            delta_line,
            delta_start,
            length,
            token_type: tok.token_type,
            token_modifiers_bitset: tok.modifiers,
        });

        prev_line = pos.line;
        prev_char = pos.character;
    }

    result
}

fn to_diagnostic(source: &str, error: &ParseError) -> Diagnostic {
    Diagnostic {
        range: span_to_range(source, error.span),
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some("dagrun".to_string()),
        message: error.message.clone(),
        ..Default::default()
    }
}

fn span_to_range(source: &str, span: Span) -> Range {
    Range {
        start: offset_to_position(source, span.start as usize),
        end: offset_to_position(source, span.end as usize),
    }
}

fn offset_to_position(source: &str, offset: usize) -> Position {
    let offset = offset.min(source.len());
    let before = &source[..offset];
    let line = before.chars().filter(|&c| c == '\n').count() as u32;
    let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let character = before[line_start..].chars().count() as u32;
    Position { line, character }
}

fn position_to_offset(source: &str, pos: Position) -> u32 {
    let mut offset = 0;
    for (i, line) in source.lines().enumerate() {
        if i == pos.line as usize {
            offset += line
                .chars()
                .take(pos.character as usize)
                .map(|c| c.len_utf8())
                .sum::<usize>();
            break;
        }
        offset += line.len() + 1; // +1 for newline
    }
    offset as u32
}

/// Find the parameters of the task containing the given offset
fn find_enclosing_task_params<'a>(ast: &'a SourceFile, offset: u32) -> Option<Vec<&'a str>> {
    for item in &ast.items {
        if let Item::Task(task) = &item.node {
            if item.span.contains(offset) {
                return Some(
                    task.parameters
                        .iter()
                        .map(|p| p.node.name.node.as_str())
                        .collect(),
                );
            }
        }
    }
    None
}

// ============================================================================
// Completions
// ============================================================================

fn get_completions(source: &str, ast: &SourceFile, pos: Position) -> Vec<CompletionItem> {
    let line = source.lines().nth(pos.line as usize).unwrap_or("");
    let col = pos.character as usize;
    let before_cursor = &line[..col.min(line.len())];

    // collect defined names
    let mut variables: Vec<&str> = Vec::new();
    let mut tasks: Vec<&str> = Vec::new();

    for item in &ast.items {
        match &item.node {
            Item::Variable(var) => variables.push(&var.name.node),
            Item::Task(task) => tasks.push(&task.name.node),
            _ => {}
        }
    }

    // context: inside {{...}} - complete variables and task parameters
    if let Some(open_idx) = before_cursor.rfind("{{") {
        let after_open = &before_cursor[open_idx + 2..];
        if !after_open.contains("}}") {
            let offset = position_to_offset(source, pos);
            let mut items: Vec<CompletionItem> = variables
                .iter()
                .map(|name| CompletionItem {
                    label: name.to_string(),
                    kind: Some(CompletionItemKind::VARIABLE),
                    detail: Some("variable".to_string()),
                    ..Default::default()
                })
                .collect();

            // also suggest task parameters if inside a task body
            if let Some(params) = find_enclosing_task_params(ast, offset) {
                items.extend(params.iter().map(|name| CompletionItem {
                    label: name.to_string(),
                    kind: Some(CompletionItemKind::VARIABLE),
                    detail: Some("parameter".to_string()),
                    ..Default::default()
                }));
            }

            return items;
        }
    }

    // context: after @ - complete annotation keywords or options
    let trimmed = before_cursor.trim_start();
    if trimmed.starts_with('@') {
        use dagrun_ast::docs;

        let after_at = &trimmed[1..];

        // if no space yet, complete annotation keywords
        if !after_at.contains(' ') {
            return docs::ANNOTATION_NAMES
                .iter()
                .filter_map(|name| {
                    docs::get_annotation_doc(name).map(|doc| CompletionItem {
                        label: name.to_string(),
                        kind: Some(CompletionItemKind::KEYWORD),
                        detail: Some(doc.description.to_string()),
                        ..Default::default()
                    })
                })
                .collect();
        }

        // after keyword + space, complete options for that annotation
        let keyword = after_at.split_whitespace().next().unwrap_or("");
        if let Some(doc) = docs::get_annotation_doc(keyword) {
            // collect already-used options to avoid duplicates
            let used_opts: std::collections::HashSet<&str> = after_at
                .split_whitespace()
                .skip(1)
                .filter_map(|s| s.split('=').next())
                .collect();

            let mut completions: Vec<CompletionItem> = doc
                .options
                .iter()
                .filter(|(opt, _)| {
                    let opt_name = opt.split('=').next().unwrap_or(opt);
                    !used_opts.contains(opt_name)
                })
                .map(|(opt, desc)| CompletionItem {
                    label: opt.to_string(),
                    kind: Some(CompletionItemKind::PROPERTY),
                    detail: Some(desc.to_string()),
                    insert_text: Some(opt.to_string()),
                    ..Default::default()
                })
                .collect();

            // also offer variable completions for option values
            if before_cursor.ends_with('=') || before_cursor.ends_with("={{") {
                completions.extend(variables.iter().map(|name| CompletionItem {
                    label: format!("{{{{{}}}}}", name),
                    kind: Some(CompletionItemKind::VARIABLE),
                    detail: Some("variable".to_string()),
                    ..Default::default()
                }));
            }

            return completions;
        }
    }

    // context: after task_name: - complete task dependencies
    if before_cursor.contains(':') && !before_cursor.contains(":=") {
        // likely in dependency list
        return tasks
            .iter()
            .map(|name| CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some("task".to_string()),
                ..Default::default()
            })
            .collect();
    }

    Vec::new()
}

// ============================================================================
// Go-to-definition
// ============================================================================

fn find_definition_at(source: &str, ast: &SourceFile, offset: u32) -> Option<Span> {
    // collect all definitions
    let mut var_defs: HashMap<&str, Span> = HashMap::new();
    let mut task_defs: HashMap<&str, Span> = HashMap::new();

    for item in &ast.items {
        match &item.node {
            Item::Variable(var) => {
                var_defs.insert(&var.name.node, var.name.span);
            }
            Item::Task(task) => {
                task_defs.insert(&task.name.node, task.name.span);
            }
            _ => {}
        }
    }

    // find what's at the cursor position
    for item in &ast.items {
        if let Item::Task(task) = &item.node {
            // collect task-scoped parameter definitions
            let param_defs: HashMap<&str, Span> = task
                .parameters
                .iter()
                .map(|p| (p.node.name.node.as_str(), p.node.name.span))
                .collect();

            // check dependencies
            for dep in &task.dependencies {
                if dep.span.contains(offset) {
                    let name = match &dep.node {
                        Dependency::Task(n) => n.as_str(),
                        Dependency::Service(n) => n.as_str(),
                    };
                    return task_defs.get(name).copied();
                }
            }

            // check interpolations in body
            if let Some(body) = &task.body {
                for line in &body.lines {
                    if let BodyLine::Command(cmd) = &line.node {
                        for seg in &cmd.segments {
                            if let CommandSegment::Interpolation(interp) = &seg.node {
                                if interp.name.span.contains(offset) {
                                    // check parameters first, then global variables
                                    if let Some(span) =
                                        param_defs.get(interp.name.node.as_str()).copied()
                                    {
                                        return Some(span);
                                    }
                                    return var_defs.get(interp.name.node.as_str()).copied();
                                }
                            }
                        }
                    }
                }
            }

            // check interpolations in annotation values
            for ann in &task.annotations {
                if let Some(span) = find_var_in_annotation(&ann.node.kind, offset, source) {
                    // extract the var name from the span
                    let name = span.text(source).trim();
                    return var_defs.get(name).copied();
                }
            }
        }
    }

    None
}

fn find_var_in_annotation(kind: &AnnotationKind, offset: u32, source: &str) -> Option<Span> {
    let check_value = |val: &Spanned<String>| -> Option<Span> {
        for var_ref in find_interpolations(&val.node, val.span) {
            if var_ref.span.contains(offset) {
                return Some(var_ref.span);
            }
        }
        None
    };

    let check_kv_list = |opts: &[Spanned<KeyValue>]| -> Option<Span> {
        for kv in opts {
            if let Some(span) = check_value(&kv.node.value) {
                return Some(span);
            }
        }
        None
    };

    match kind {
        AnnotationKind::Ssh(ssh) => {
            if let Some(host) = &ssh.host {
                if let Some(span) = check_value(host) {
                    return Some(span);
                }
            }
            check_kv_list(&ssh.options)
        }
        AnnotationKind::K8s(k8s) => check_kv_list(&k8s.options),
        AnnotationKind::Upload(ft)
        | AnnotationKind::Download(ft)
        | AnnotationKind::K8sUpload(ft)
        | AnnotationKind::K8sDownload(ft) => {
            check_value(&ft.local).or_else(|| check_value(&ft.remote))
        }
        AnnotationKind::Service(svc) | AnnotationKind::Extern(svc) => check_kv_list(&svc.options),
        _ => None,
    }
}

// ============================================================================
// Semantic diagnostics: undefined variable checking
// ============================================================================

fn check_undefined_variables(source: &str, ast: &SourceFile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // collect all defined variable names
    let defined: HashSet<&str> = ast
        .items
        .iter()
        .filter_map(|item| match &item.node {
            Item::Variable(var) => Some(var.name.node.as_str()),
            _ => None,
        })
        .collect();

    // check all variable references
    for item in &ast.items {
        match &item.node {
            Item::Task(task) => {
                // collect task-scoped parameters
                let params: HashSet<&str> = task
                    .parameters
                    .iter()
                    .map(|p| p.node.name.node.as_str())
                    .collect();

                // check annotations
                for ann in &task.annotations {
                    check_annotation_vars(source, &ann.node.kind, &defined, &mut diagnostics);
                }

                // check command body interpolations
                if let Some(body) = &task.body {
                    for line in &body.lines {
                        if let BodyLine::Command(cmd) = &line.node {
                            for seg in &cmd.segments {
                                if let CommandSegment::Interpolation(interp) = &seg.node {
                                    let name = &interp.name.node;
                                    // check both global variables and task parameters
                                    if !name.is_empty()
                                        && !defined.contains(name.as_str())
                                        && !params.contains(name.as_str())
                                    {
                                        diagnostics.push(Diagnostic {
                                            range: span_to_range(source, interp.name.span),
                                            severity: Some(DiagnosticSeverity::ERROR),
                                            source: Some("dagrun".to_string()),
                                            message: format!("undefined variable '{}'", name),
                                            ..Default::default()
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    diagnostics
}

fn check_annotation_vars(
    source: &str,
    kind: &AnnotationKind,
    defined: &HashSet<&str>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // helper to check a string value for {{var}} interpolations
    let check_value = |val: &Spanned<String>, diagnostics: &mut Vec<Diagnostic>| {
        for var_ref in find_interpolations(&val.node, val.span) {
            if !defined.contains(var_ref.name.as_str()) {
                diagnostics.push(Diagnostic {
                    range: span_to_range(source, var_ref.span),
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("dagrun".to_string()),
                    message: format!("undefined variable '{}'", var_ref.name),
                    ..Default::default()
                });
            }
        }
    };

    let check_kv_list = |opts: &[Spanned<KeyValue>], diagnostics: &mut Vec<Diagnostic>| {
        for kv in opts {
            check_value(&kv.node.value, diagnostics);
        }
    };

    match kind {
        AnnotationKind::Ssh(ssh) => {
            if let Some(host) = &ssh.host {
                check_value(host, diagnostics);
            }
            check_kv_list(&ssh.options, diagnostics);
        }
        AnnotationKind::K8s(k8s) => {
            check_kv_list(&k8s.options, diagnostics);
        }
        AnnotationKind::Upload(ft)
        | AnnotationKind::Download(ft)
        | AnnotationKind::K8sUpload(ft)
        | AnnotationKind::K8sDownload(ft) => {
            check_value(&ft.local, diagnostics);
            check_value(&ft.remote, diagnostics);
        }
        AnnotationKind::Service(svc) | AnnotationKind::Extern(svc) => {
            check_kv_list(&svc.options, diagnostics);
        }
        AnnotationKind::K8sConfigmap(cm) | AnnotationKind::K8sSecret(cm) => {
            check_value(&cm.name, diagnostics);
            check_value(&cm.path, diagnostics);
        }
        AnnotationKind::K8sForward(pf) => {
            check_value(&pf.local_port, diagnostics);
            check_value(&pf.resource, diagnostics);
            check_value(&pf.remote_port, diagnostics);
        }
        _ => {}
    }
}

struct VarRef {
    name: String,
    span: Span,
}

fn find_interpolations(text: &str, base_span: Span) -> Vec<VarRef> {
    let mut refs = Vec::new();
    let mut i = 0;
    let bytes = text.as_bytes();

    while i < bytes.len().saturating_sub(3) {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let start = i + 2;
            // find closing }}
            if let Some(end) = text[start..].find("}}") {
                let name = text[start..start + end].trim().to_string();
                if !name.is_empty() {
                    let span_start = base_span.start + start as u32;
                    let span_end = base_span.start + (start + end) as u32;
                    refs.push(VarRef {
                        name,
                        span: Span::new(span_start, span_end),
                    });
                }
                i = start + end + 2;
            } else {
                i += 2;
            }
        } else {
            i += 1;
        }
    }

    refs
}

// ============================================================================
// Undefined task checking
// ============================================================================

fn check_undefined_tasks(source: &str, ast: &SourceFile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // collect all defined task names
    let defined_tasks: HashSet<&str> = ast
        .items
        .iter()
        .filter_map(|item| match &item.node {
            Item::Task(task) => Some(task.name.node.as_str()),
            _ => None,
        })
        .collect();

    // check all task dependencies
    for item in &ast.items {
        if let Item::Task(task) = &item.node {
            for dep in &task.dependencies {
                let (name, is_service) = match &dep.node {
                    Dependency::Task(name) => (name.as_str(), false),
                    Dependency::Service(name) => (name.as_str(), true),
                };

                // services are checked separately, only check task deps here
                if !is_service && !defined_tasks.contains(name) {
                    diagnostics.push(Diagnostic {
                        range: span_to_range(source, dep.span),
                        severity: Some(DiagnosticSeverity::ERROR),
                        source: Some("dagrun".to_string()),
                        message: format!("undefined task '{}'", name),
                        ..Default::default()
                    });
                }
            }
        }
    }

    diagnostics
}

// ============================================================================
// Dependency cycle detection
// ============================================================================

fn check_dependency_cycles(source: &str, ast: &SourceFile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // build task name -> dependencies map, and name -> span for error reporting
    let mut deps_map: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut task_spans: HashMap<&str, Span> = HashMap::new();

    for item in &ast.items {
        if let Item::Task(task) = &item.node {
            let name = task.name.node.as_str();
            task_spans.insert(name, task.name.span);

            let deps: Vec<&str> = task
                .dependencies
                .iter()
                .filter_map(|d| match &d.node {
                    Dependency::Task(t) => Some(t.as_str()),
                    Dependency::Service(_) => None, // services don't form cycles with tasks
                })
                .collect();

            deps_map.insert(name, deps);
        }
    }

    // detect cycles using DFS
    let mut visited: HashSet<&str> = HashSet::new();
    let mut rec_stack: HashSet<&str> = HashSet::new();

    for &task in deps_map.keys() {
        if let Some(cycle) = find_cycle(task, &deps_map, &mut visited, &mut rec_stack, &mut vec![])
        {
            // report cycle on the first task in the cycle
            if let Some(&span) = task_spans.get(cycle[0]) {
                let cycle_str = cycle.join(" -> ");
                diagnostics.push(Diagnostic {
                    range: span_to_range(source, span),
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("dagrun".to_string()),
                    message: format!("dependency cycle detected: {}", cycle_str),
                    ..Default::default()
                });
            }
            // only report one cycle to avoid spam
            break;
        }
    }

    diagnostics
}

fn find_cycle<'a>(
    node: &'a str,
    deps: &HashMap<&'a str, Vec<&'a str>>,
    visited: &mut HashSet<&'a str>,
    rec_stack: &mut HashSet<&'a str>,
    path: &mut Vec<&'a str>,
) -> Option<Vec<&'a str>> {
    if rec_stack.contains(node) {
        // found cycle - extract it from path
        let cycle_start = path.iter().position(|&n| n == node).unwrap_or(0);
        let mut cycle: Vec<&str> = path[cycle_start..].to_vec();
        cycle.push(node);
        return Some(cycle);
    }

    if visited.contains(node) {
        return None;
    }

    visited.insert(node);
    rec_stack.insert(node);
    path.push(node);

    if let Some(neighbors) = deps.get(node) {
        for &neighbor in neighbors {
            if let Some(cycle) = find_cycle(neighbor, deps, visited, rec_stack, path) {
                return Some(cycle);
            }
        }
    }

    path.pop();
    rec_stack.remove(node);
    None
}

// ============================================================================
// Filesystem validation (paths, executables, interpreters)
// ============================================================================

fn check_filesystem(source: &str, ast: &SourceFile, working_dir: Option<&Path>) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut checked_paths: HashSet<String> = HashSet::new();
    let mut checked_executables: HashSet<String> = HashSet::new();

    // helper to resolve path relative to working dir
    let resolve_path = |p: &str| -> std::path::PathBuf {
        let expanded = shellexpand::tilde(p);
        let path = Path::new(expanded.as_ref());
        if path.is_absolute() {
            path.to_path_buf()
        } else if let Some(wd) = working_dir {
            wd.join(path)
        } else {
            path.to_path_buf()
        }
    };

    // shell builtins we shouldn't warn about
    let is_builtin = |name: &str| -> bool {
        matches!(
            name,
            "echo"
                | "cd"
                | "export"
                | "source"
                | "."
                | "test"
                | "["
                | "[["
                | "if"
                | "then"
                | "else"
                | "fi"
                | "for"
                | "while"
                | "do"
                | "done"
                | "case"
                | "esac"
                | "true"
                | "false"
                | "read"
                | "printf"
                | "local"
                | "return"
                | "exit"
                | "set"
                | "unset"
                | "eval"
                | "exec"
                | "trap"
                | "wait"
                | "shift"
                | "break"
                | "continue"
        )
    };

    for item in &ast.items {
        if let Item::Task(task) = &item.node {
            // check annotations for file paths
            for ann in &task.annotations {
                match &ann.node.kind {
                    // check @upload local paths
                    AnnotationKind::Upload(ft) | AnnotationKind::K8sUpload(ft) => {
                        let local_path = &ft.local.node;
                        if !local_path.contains("{{") && checked_paths.insert(local_path.clone()) {
                            let resolved = resolve_path(local_path);
                            if !resolved.exists() {
                                diagnostics.push(Diagnostic {
                                    range: span_to_range(source, ft.local.span),
                                    severity: Some(DiagnosticSeverity::WARNING),
                                    source: Some("dagrun".to_string()),
                                    message: format!("file not found: {}", local_path),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                    // check workdir in ssh/k8s annotations
                    AnnotationKind::Ssh(ssh) => {
                        for kv in &ssh.options {
                            if kv.node.key.node == "workdir" && !kv.node.value.node.contains("{{") {
                                // only check local workdirs (not remote)
                                // skip this for ssh since workdir is on remote
                            }
                        }
                    }
                    AnnotationKind::K8s(k8s) => {
                        for kv in &k8s.options {
                            if kv.node.key.node == "workdir" && !kv.node.value.node.contains("{{") {
                                // k8s workdir is in pod, skip
                            }
                        }
                    }
                    _ => {}
                }
            }

            // check shebang interpreter
            if let Some(body) = &task.body {
                for line in &body.lines {
                    if let BodyLine::Shebang(shebang) = &line.node {
                        let interp = &shebang.interpreter.node;
                        if !interp.is_empty() && checked_paths.insert(interp.clone()) {
                            let interp_path = resolve_path(interp);
                            if !interp_path.exists() {
                                diagnostics.push(Diagnostic {
                                    range: span_to_range(source, shebang.interpreter.span),
                                    severity: Some(DiagnosticSeverity::WARNING),
                                    source: Some("dagrun".to_string()),
                                    message: format!("interpreter not found: {}", interp),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }

                // check first word of commands for executables
                for line in &body.lines {
                    if let BodyLine::Command(cmd) = &line.node {
                        if let Some(first_seg) = cmd.segments.first() {
                            if let CommandSegment::Text(text) = &first_seg.node {
                                let first_word = text.split_whitespace().next().unwrap_or("");
                                if !first_word.is_empty()
                                    && !first_word.contains('/')
                                    && !first_word.contains("{{")
                                    && !first_word.starts_with('$')
                                    && !first_word.starts_with('-')
                                {
                                    if checked_executables.insert(first_word.to_string())
                                        && !is_builtin(first_word)
                                        && which::which(first_word).is_err()
                                    {
                                        diagnostics.push(Diagnostic {
                                            range: span_to_range(source, first_seg.span),
                                            severity: Some(DiagnosticSeverity::WARNING),
                                            source: Some("dagrun".to_string()),
                                            message: format!(
                                                "command not found in PATH: {}",
                                                first_word
                                            ),
                                            ..Default::default()
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    diagnostics
}

// ============================================================================
// Unused variable detection
// ============================================================================

fn check_unused_variables(source: &str, ast: &SourceFile) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // collect all defined variables with their spans
    let mut defined_vars: HashMap<&str, Span> = HashMap::new();
    for item in &ast.items {
        if let Item::Variable(var) = &item.node {
            defined_vars.insert(&var.name.node, var.name.span);
        }
    }

    // collect all used variable names
    let mut used_vars: HashSet<&str> = HashSet::new();

    for item in &ast.items {
        if let Item::Task(task) = &item.node {
            // check variables in annotations
            for ann in &task.annotations {
                collect_used_vars_in_annotation(&ann.node.kind, &mut used_vars);
            }

            // check variables in body
            if let Some(body) = &task.body {
                for line in &body.lines {
                    if let BodyLine::Command(cmd) = &line.node {
                        for seg in &cmd.segments {
                            if let CommandSegment::Interpolation(interp) = &seg.node {
                                used_vars.insert(&interp.name.node);
                            }
                        }
                    }
                }
            }
        }
    }

    // report unused variables
    for (name, span) in &defined_vars {
        if !used_vars.contains(*name) {
            diagnostics.push(Diagnostic {
                range: span_to_range(source, *span),
                severity: Some(DiagnosticSeverity::WARNING),
                source: Some("dagrun".to_string()),
                message: format!("unused variable '{}'", name),
                ..Default::default()
            });
        }
    }

    diagnostics
}

fn collect_used_vars_in_annotation<'a>(kind: &'a AnnotationKind, used: &mut HashSet<&'a str>) {
    match kind {
        AnnotationKind::Ssh(ssh) => {
            if let Some(host) = &ssh.host {
                if let Some(var) = extract_var(&host.node) {
                    used.insert(var);
                }
            }
            for kv in &ssh.options {
                if let Some(var) = extract_var(&kv.node.value.node) {
                    used.insert(var);
                }
            }
        }
        AnnotationKind::K8s(k8s) => {
            for kv in &k8s.options {
                if let Some(var) = extract_var(&kv.node.value.node) {
                    used.insert(var);
                }
            }
        }
        AnnotationKind::Upload(ft) | AnnotationKind::K8sUpload(ft) => {
            if let Some(var) = extract_var(&ft.local.node) {
                used.insert(var);
            }
            if let Some(var) = extract_var(&ft.remote.node) {
                used.insert(var);
            }
        }
        AnnotationKind::Download(ft) | AnnotationKind::K8sDownload(ft) => {
            if let Some(var) = extract_var(&ft.local.node) {
                used.insert(var);
            }
            if let Some(var) = extract_var(&ft.remote.node) {
                used.insert(var);
            }
        }
        AnnotationKind::Timeout(t) => {
            if let Some(var) = extract_var(&t.node) {
                used.insert(var);
            }
        }
        AnnotationKind::Service(s) | AnnotationKind::Extern(s) => {
            for kv in &s.options {
                if let Some(var) = extract_var(&kv.node.value.node) {
                    used.insert(var);
                }
            }
        }
        AnnotationKind::Unknown { rest, .. } => {
            if let Some(rest) = rest {
                if let Some(var) = extract_var(&rest.node) {
                    used.insert(var);
                }
            }
        }
        _ => {}
    }
}

fn extract_var(s: &str) -> Option<&str> {
    if let Some(start) = s.find("{{") {
        if let Some(end) = s[start..].find("}}") {
            return Some(s[start + 2..start + end].trim());
        }
    }
    None
}

// ============================================================================
// Hover documentation
// ============================================================================

fn get_hover_info(source: &str, ast: &SourceFile, offset: u32) -> Option<(String, Range)> {
    for item in &ast.items {
        if let Item::Task(task) = &item.node {
            for ann in &task.annotations {
                if span_contains(ann.span, offset) {
                    return get_annotation_hover(&ann.node.kind, ann.span, source);
                }
            }

            // hover over task name definition
            if span_contains(task.name.span, offset) {
                let deps: Vec<&str> = task
                    .dependencies
                    .iter()
                    .map(|d| match &d.node {
                        Dependency::Task(t) => t.as_str(),
                        Dependency::Service(s) => s.as_str(),
                    })
                    .collect();

                let doc = if deps.is_empty() {
                    format!("**Task:** `{}`\n\nNo dependencies.", task.name.node)
                } else {
                    format!(
                        "**Task:** `{}`\n\n**Dependencies:** {}",
                        task.name.node,
                        deps.join(", ")
                    )
                };
                return Some((doc, span_to_range(source, task.name.span)));
            }

            // hover over dependencies
            for dep in &task.dependencies {
                if span_contains(dep.span, offset) {
                    let (name, is_service) = match &dep.node {
                        Dependency::Task(t) => (t.as_str(), false),
                        Dependency::Service(s) => (s.as_str(), true),
                    };
                    let kind = if is_service { "Service" } else { "Task" };
                    let doc = format!("**{}:** `{}`", kind, name);
                    return Some((doc, span_to_range(source, dep.span)));
                }
            }

            // hover over variables in body
            if let Some(body) = &task.body {
                for line in &body.lines {
                    if let BodyLine::Command(cmd) = &line.node {
                        for seg in &cmd.segments {
                            if let CommandSegment::Interpolation(interp) = &seg.node {
                                if span_contains(seg.span, offset) {
                                    let def = find_variable_def(ast, &interp.name.node);
                                    let doc = if let Some(value) = def {
                                        format!(
                                            "**Variable:** `{}`\n\n**Value:** `{}`",
                                            interp.name.node,
                                            format_var_value(value)
                                        )
                                    } else {
                                        format!("**Variable:** `{}` (undefined)", interp.name.node)
                                    };
                                    return Some((doc, span_to_range(source, seg.span)));
                                }
                            }
                        }
                    }
                }
            }
        }

        // hover over variable definitions
        if let Item::Variable(var) = &item.node {
            if span_contains(var.name.span, offset) {
                let doc = format!(
                    "**Variable:** `{}`\n\n**Value:** `{}`",
                    var.name.node,
                    format_var_value(&var.value.node)
                );
                return Some((doc, span_to_range(source, var.name.span)));
            }
        }
    }

    None
}

fn get_annotation_hover(
    kind: &AnnotationKind,
    span: Span,
    source: &str,
) -> Option<(String, Range)> {
    use dagrun_ast::docs;

    let doc = match kind {
        AnnotationKind::Ssh(_) => docs::SSH.to_markdown(),
        AnnotationKind::K8s(_) => docs::K8S.to_markdown(),
        AnnotationKind::Upload(_) => docs::UPLOAD.to_markdown(),
        AnnotationKind::Download(_) => docs::DOWNLOAD.to_markdown(),
        AnnotationKind::K8sUpload(_) => docs::K8S_UPLOAD.to_markdown(),
        AnnotationKind::K8sDownload(_) => docs::K8S_DOWNLOAD.to_markdown(),
        AnnotationKind::K8sConfigmap(_) => docs::K8S_CONFIGMAP.to_markdown(),
        AnnotationKind::K8sSecret(_) => docs::K8S_SECRET.to_markdown(),
        AnnotationKind::K8sForward(_) => docs::K8S_FORWARD.to_markdown(),
        AnnotationKind::Timeout(_) => docs::TIMEOUT.to_markdown(),
        AnnotationKind::Retry(_) => docs::RETRY.to_markdown(),
        AnnotationKind::Service(_) => docs::SERVICE.to_markdown(),
        AnnotationKind::Extern(_) => docs::EXTERN.to_markdown(),
        AnnotationKind::PipeFrom(_) => docs::PIPE_FROM.to_markdown(),
        AnnotationKind::Join => docs::JOIN.to_markdown(),
        AnnotationKind::Unknown { name, .. } => {
            format!("**Unknown annotation:** `@{}`", name.node)
        }
    };
    Some((doc, span_to_range(source, span)))
}

fn find_variable_def<'a>(ast: &'a SourceFile, name: &str) -> Option<&'a dagrun_ast::VariableValue> {
    for item in &ast.items {
        if let Item::Variable(var) = &item.node {
            if var.name.node == name {
                return Some(&var.value.node);
            }
        }
    }
    None
}

fn format_var_value(value: &dagrun_ast::VariableValue) -> String {
    match value {
        dagrun_ast::VariableValue::Static(s) => s.clone(),
        dagrun_ast::VariableValue::Shell(sh) => format!("`{}`", sh.command.node),
    }
}

fn span_contains(span: Span, offset: u32) -> bool {
    offset >= span.start && offset < span.end
}

// ============================================================================
// References and Rename
// ============================================================================

/// find what symbol is at offset, return (name, span)
fn get_symbol_at(source: &str, ast: &SourceFile, offset: u32) -> Option<(String, Span)> {
    for item in &ast.items {
        match &item.node {
            Item::Variable(var) => {
                if span_contains(var.name.span, offset) {
                    return Some((var.name.node.clone(), var.name.span));
                }
            }
            Item::Task(task) => {
                // task name definition
                if span_contains(task.name.span, offset) {
                    return Some((task.name.node.clone(), task.name.span));
                }
                // task dependencies
                for dep in &task.dependencies {
                    if span_contains(dep.span, offset) {
                        let name = match &dep.node {
                            Dependency::Task(t) => t.clone(),
                            Dependency::Service(s) => s.clone(),
                        };
                        return Some((name, dep.span));
                    }
                }
                // variables in body
                if let Some(body) = &task.body {
                    for line in &body.lines {
                        if let BodyLine::Command(cmd) = &line.node {
                            for seg in &cmd.segments {
                                if let CommandSegment::Interpolation(interp) = &seg.node {
                                    if span_contains(seg.span, offset) {
                                        return Some((interp.name.node.clone(), interp.name.span));
                                    }
                                }
                            }
                        }
                    }
                }
                // variables in annotations
                for ann in &task.annotations {
                    if let Some((name, span)) = find_var_name_in_annotation(&ann.node.kind, offset)
                    {
                        return Some((name, span));
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn find_var_name_in_annotation(kind: &AnnotationKind, offset: u32) -> Option<(String, Span)> {
    // helper to check string for {{var}} at offset
    let check_spanned = |s: &Spanned<String>| -> Option<(String, Span)> {
        if span_contains(s.span, offset) && s.node.contains("{{") {
            // crude but works: if we're in the span and there's a var, extract it
            if let Some(start) = s.node.find("{{") {
                if let Some(end) = s.node[start..].find("}}") {
                    let var_name = s.node[start + 2..start + end].trim().to_string();
                    return Some((var_name, s.span));
                }
            }
        }
        None
    };

    match kind {
        AnnotationKind::Ssh(ssh) => {
            if let Some(host) = &ssh.host {
                if let Some(r) = check_spanned(host) {
                    return Some(r);
                }
            }
            for kv in &ssh.options {
                if let Some(r) = check_spanned(&kv.node.value) {
                    return Some(r);
                }
            }
        }
        AnnotationKind::K8s(k8s) => {
            for kv in &k8s.options {
                if let Some(r) = check_spanned(&kv.node.value) {
                    return Some(r);
                }
            }
        }
        AnnotationKind::Upload(ft)
        | AnnotationKind::Download(ft)
        | AnnotationKind::K8sUpload(ft)
        | AnnotationKind::K8sDownload(ft) => {
            if let Some(r) = check_spanned(&ft.local) {
                return Some(r);
            }
            if let Some(r) = check_spanned(&ft.remote) {
                return Some(r);
            }
        }
        _ => {}
    }
    None
}

/// find all references to the symbol at offset
fn find_all_references(
    source: &str,
    ast: &SourceFile,
    offset: u32,
    include_decl: bool,
) -> Option<Vec<Span>> {
    let (name, _) = get_symbol_at(source, ast, offset)?;

    // determine if it's a variable or task
    let is_variable = ast
        .items
        .iter()
        .any(|item| matches!(&item.node, Item::Variable(v) if v.name.node == name));
    let is_task = ast
        .items
        .iter()
        .any(|item| matches!(&item.node, Item::Task(t) if t.name.node == name));

    let mut refs = Vec::new();

    for item in &ast.items {
        match &item.node {
            Item::Variable(var) if is_variable && var.name.node == name => {
                if include_decl {
                    refs.push(var.name.span);
                }
            }
            Item::Task(task) => {
                // task definition
                if is_task && task.name.node == name && include_decl {
                    refs.push(task.name.span);
                }
                // task dependencies
                if is_task {
                    for dep in &task.dependencies {
                        let dep_name = match &dep.node {
                            Dependency::Task(t) => t,
                            Dependency::Service(s) => s,
                        };
                        if dep_name == &name {
                            refs.push(dep.span);
                        }
                    }
                }
                // variable usages in body
                if is_variable {
                    if let Some(body) = &task.body {
                        for line in &body.lines {
                            if let BodyLine::Command(cmd) = &line.node {
                                for seg in &cmd.segments {
                                    if let CommandSegment::Interpolation(interp) = &seg.node {
                                        if interp.name.node == name {
                                            refs.push(interp.name.span);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // variable usages in annotations
                    for ann in &task.annotations {
                        refs.extend(find_var_refs_in_annotation(&ann.node.kind, &name));
                    }
                }
            }
            _ => {}
        }
    }

    if refs.is_empty() { None } else { Some(refs) }
}

fn find_var_refs_in_annotation(kind: &AnnotationKind, var_name: &str) -> Vec<Span> {
    let mut refs = Vec::new();

    let check_spanned = |s: &Spanned<String>| -> Option<Span> {
        if s.node.contains(&format!("{{{{{}}}}}", var_name)) {
            Some(s.span)
        } else {
            None
        }
    };

    match kind {
        AnnotationKind::Ssh(ssh) => {
            if let Some(host) = &ssh.host {
                if let Some(span) = check_spanned(host) {
                    refs.push(span);
                }
            }
            for kv in &ssh.options {
                if let Some(span) = check_spanned(&kv.node.value) {
                    refs.push(span);
                }
            }
        }
        AnnotationKind::K8s(k8s) => {
            for kv in &k8s.options {
                if let Some(span) = check_spanned(&kv.node.value) {
                    refs.push(span);
                }
            }
        }
        AnnotationKind::Upload(ft)
        | AnnotationKind::Download(ft)
        | AnnotationKind::K8sUpload(ft)
        | AnnotationKind::K8sDownload(ft) => {
            if let Some(span) = check_spanned(&ft.local) {
                refs.push(span);
            }
            if let Some(span) = check_spanned(&ft.remote) {
                refs.push(span);
            }
        }
        _ => {}
    }

    refs
}

// ============================================================================
// Document Symbols
// ============================================================================

fn collect_document_symbols(source: &str, ast: &SourceFile) -> Vec<SymbolInformation> {
    let mut symbols = Vec::new();

    for item in &ast.items {
        match &item.node {
            Item::Variable(var) => {
                #[allow(deprecated)]
                symbols.push(SymbolInformation {
                    name: var.name.node.clone(),
                    kind: SymbolKind::VARIABLE,
                    tags: None,
                    deprecated: None,
                    location: Location {
                        uri: Url::parse("file:///").unwrap(), // placeholder, will be ignored
                        range: span_to_range(source, var.name.span),
                    },
                    container_name: None,
                });
            }
            Item::Task(task) => {
                #[allow(deprecated)]
                symbols.push(SymbolInformation {
                    name: task.name.node.clone(),
                    kind: SymbolKind::FUNCTION,
                    tags: None,
                    deprecated: None,
                    location: Location {
                        uri: Url::parse("file:///").unwrap(),
                        range: span_to_range(source, item.span),
                    },
                    container_name: None,
                });
            }
            _ => {}
        }
    }

    symbols
}
