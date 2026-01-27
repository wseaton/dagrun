use crate::ast::{
    Annotation, AnnotationKind, BodyLine, CommandLine, CommandSegment, Comment,
    ConfigMountAnnotation, ContextBlock, Dependency, FileTransferAnnotation, Interpolation, Item,
    K8sAnnotation, KeyValue, LuaBlock, Parameter, ParameterDefault, PortForwardAnnotation,
    ServiceAnnotation, SetDirective, Shebang, ShellExpansion, SourceFile, SshAnnotation, TaskBody,
    TaskDecl, VariableDecl, VariableValue,
};
use crate::error::{ParseError, ParseErrorKind};
use crate::lexer::{Lexer, Token, TokenKind};
use crate::{Span, Spanned};

/// Parse a dagrun source file into an AST
pub fn parse(source: &str) -> (SourceFile, Vec<ParseError>) {
    let tokens = Lexer::new(source).tokenize();
    let mut parser = Parser::new(tokens, source);
    let file = parser.parse_file();
    (file, parser.errors)
}

struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    source: &'a str,
    errors: Vec<ParseError>,
}

impl<'a> Parser<'a> {
    fn new(tokens: Vec<Token>, source: &'a str) -> Self {
        Self {
            tokens,
            pos: 0,
            source,
            errors: Vec::new(),
        }
    }

    fn parse_file(&mut self) -> SourceFile {
        let mut items = Vec::new();
        let mut pending_annotations: Vec<Spanned<Annotation>> = Vec::new();

        while !self.at_end() {
            self.skip_empty_lines();
            if self.at_end() {
                break;
            }

            // try to parse an item
            match self.parse_item(&mut pending_annotations) {
                Ok(Some(item)) => items.push(item),
                Ok(None) => {}
                Err(e) => {
                    self.errors.push(e);
                    self.recover_to_next_line();
                }
            }
        }

        // report orphaned annotations
        for ann in pending_annotations {
            self.errors.push(ParseError::new(
                ParseErrorKind::OrphanedAnnotation,
                ann.span,
                "annotation not followed by task",
            ));
        }

        SourceFile { items }
    }

    fn parse_item(
        &mut self,
        pending_annotations: &mut Vec<Spanned<Annotation>>,
    ) -> Result<Option<Spanned<Item>>, ParseError> {
        self.skip_whitespace();

        let tok = self.peek();

        match &tok.kind {
            // comment line (not annotation - those start with @)
            TokenKind::Hash => {
                let start = tok.span;
                self.advance();

                // regular comment
                let text = self.consume_to_newline();
                let end_pos = start.start + 1 + text.len() as u32;
                let span = start.merge(Span::new(end_pos, end_pos));
                let comment = Comment {
                    text: format!("#{}", text),
                    is_doc: text.starts_with('#'),
                };
                Ok(Some(Spanned::new(Item::Comment(comment), span)))
            }

            // identifier could be: task header, variable, set directive, or lua marker
            TokenKind::Identifier(name) => {
                let name_clone = name.clone();
                let name_span = tok.span;

                // check for @lua marker
                if name_clone == "lua" && self.pos > 0 {
                    // handled via annotation path
                }

                // check for set directive
                if name_clone == "set" {
                    return self.parse_set_directive(name_span, pending_annotations);
                }

                self.advance();
                self.skip_whitespace();

                // variable assignment: name :=
                if self.check(TokenKind::ColonEquals) {
                    if !pending_annotations.is_empty() {
                        // annotations before variable - report error but continue
                        for ann in pending_annotations.drain(..) {
                            self.errors.push(ParseError::new(
                                ParseErrorKind::OrphanedAnnotation,
                                ann.span,
                                "annotation before variable assignment",
                            ));
                        }
                    }
                    return self.parse_variable(name_clone, name_span);
                }

                // task header: name: or name param1 param2:
                if self.check(TokenKind::Colon) || self.check(TokenKind::Identifier(String::new()))
                {
                    let annotations = std::mem::take(pending_annotations);
                    return self.parse_task(name_clone, name_span, annotations);
                }

                // unknown construct
                Err(ParseError::new(
                    ParseErrorKind::Expected,
                    name_span,
                    "expected ':=' or ':'",
                ))
            }

            TokenKind::At => {
                // annotation or @lua/@context block at line start
                let at_span = tok.span;
                self.advance();
                if let TokenKind::Identifier(name) = &self.peek().kind {
                    if name == "lua" {
                        return self.parse_lua_block(at_span, pending_annotations);
                    }
                    if name == "context" {
                        return self.parse_context_block(at_span, pending_annotations);
                    }
                    // regular annotation
                    let ann = self.parse_annotation(at_span)?;
                    pending_annotations.push(ann);
                    self.skip_to_newline();
                    return Ok(None);
                }
                Err(ParseError::new(
                    ParseErrorKind::InvalidAnnotation,
                    at_span,
                    "expected annotation name after '@'",
                ))
            }

            TokenKind::Newline | TokenKind::Eof => {
                self.advance();
                Ok(None)
            }

            _ => {
                let span = tok.span;
                let kind = tok.kind.clone();
                self.advance();
                Err(ParseError::new(
                    ParseErrorKind::UnexpectedToken,
                    span,
                    format!("unexpected token: {:?}", kind),
                ))
            }
        }
    }

    fn parse_annotation(&mut self, at_span: Span) -> Result<Spanned<Annotation>, ParseError> {
        self.skip_whitespace();

        let name_tok = self.advance();
        let name = match &name_tok.kind {
            TokenKind::Identifier(s) => s.clone(),
            _ => {
                return Err(ParseError::new(
                    ParseErrorKind::InvalidAnnotation,
                    name_tok.span,
                    "expected annotation name",
                ));
            }
        };
        let name_span = name_tok.span;

        self.skip_whitespace();
        let kind = self.parse_annotation_kind(&name, name_span)?;

        let end_span = self.prev_span();
        let full_span = at_span.merge(end_span);

        Ok(Spanned::new(Annotation { at_span, kind }, full_span))
    }

    fn parse_annotation_kind(
        &mut self,
        name: &str,
        name_span: Span,
    ) -> Result<AnnotationKind, ParseError> {
        match name {
            "timeout" => {
                let value = self.parse_rest_of_line_trimmed();
                Ok(AnnotationKind::Timeout(value))
            }
            "retry" => {
                let value = self.parse_rest_of_line_trimmed();
                Ok(AnnotationKind::Retry(value))
            }
            "join" => Ok(AnnotationKind::Join),
            "pipe_from" => {
                let items = self.parse_comma_separated_identifiers();
                Ok(AnnotationKind::PipeFrom(items))
            }
            "ssh" => {
                let ssh = self.parse_ssh_annotation()?;
                Ok(AnnotationKind::Ssh(ssh))
            }
            "upload" => {
                let ft = self.parse_file_transfer()?;
                Ok(AnnotationKind::Upload(ft))
            }
            "download" => {
                let ft = self.parse_file_transfer()?;
                Ok(AnnotationKind::Download(ft))
            }
            "service" => {
                let opts = self.parse_key_value_options();
                Ok(AnnotationKind::Service(ServiceAnnotation { options: opts }))
            }
            "extern" => {
                let opts = self.parse_key_value_options();
                Ok(AnnotationKind::Extern(ServiceAnnotation { options: opts }))
            }
            "k8s" => {
                let k8s = self.parse_k8s_annotation()?;
                Ok(AnnotationKind::K8s(k8s))
            }
            "k8s-configmap" => {
                let cm = self.parse_config_mount()?;
                Ok(AnnotationKind::K8sConfigmap(cm))
            }
            "k8s-secret" => {
                let cm = self.parse_config_mount()?;
                Ok(AnnotationKind::K8sSecret(cm))
            }
            "k8s-upload" => {
                let ft = self.parse_file_transfer()?;
                Ok(AnnotationKind::K8sUpload(ft))
            }
            "k8s-download" => {
                let ft = self.parse_file_transfer()?;
                Ok(AnnotationKind::K8sDownload(ft))
            }
            "k8s-forward" => {
                let pf = self.parse_port_forward()?;
                Ok(AnnotationKind::K8sForward(pf))
            }
            "use" => {
                let context_name = self.parse_rest_of_line_trimmed();
                Ok(AnnotationKind::Use(context_name))
            }
            _ => {
                let rest = if self.at_line_end() {
                    None
                } else {
                    Some(self.parse_rest_of_line_trimmed())
                };
                Ok(AnnotationKind::Unknown {
                    name: Spanned::new(name.to_string(), name_span),
                    rest,
                })
            }
        }
    }

    fn parse_ssh_annotation(&mut self) -> Result<SshAnnotation, ParseError> {
        let options = self.parse_key_value_options();
        Ok(SshAnnotation { options })
    }

    fn parse_file_transfer(&mut self) -> Result<FileTransferAnnotation, ParseError> {
        self.skip_whitespace();
        let local = self.parse_path_segment()?;
        let colon_span = self.expect(TokenKind::Colon)?.span;
        let remote = self.parse_path_segment()?;

        Ok(FileTransferAnnotation {
            local,
            colon_span,
            remote,
        })
    }

    fn parse_path_segment(&mut self) -> Result<Spanned<String>, ParseError> {
        let mut text = String::new();
        let start_span = self.peek().span;

        while !self.at_line_end() {
            let tok = self.peek();
            match &tok.kind {
                TokenKind::Colon | TokenKind::Whitespace | TokenKind::Newline | TokenKind::Eof => {
                    break;
                }
                TokenKind::Identifier(s) | TokenKind::Text(s) => {
                    text.push_str(s);
                    self.advance();
                }
                _ => {
                    text.push_str(tok.text(self.source));
                    self.advance();
                }
            }
        }

        if text.is_empty() {
            return Err(ParseError::new(
                ParseErrorKind::InvalidFileTransfer,
                start_span,
                "expected path",
            ));
        }

        let end_span = self.prev_span();
        Ok(Spanned::new(text, start_span.merge(end_span)))
    }

    fn parse_k8s_annotation(&mut self) -> Result<K8sAnnotation, ParseError> {
        self.skip_whitespace();

        let mut mode = None;

        // first token might be mode keyword
        if !self.at_line_end() {
            let tok = self.peek();
            if let TokenKind::Identifier(name) = &tok.kind
                && matches!(name.as_str(), "job" | "exec" | "apply")
            {
                mode = Some(Spanned::new(name.clone(), tok.span));
                self.advance();
            }
        }

        // rest are key=value
        let options = self.parse_key_value_options();

        Ok(K8sAnnotation { mode, options })
    }

    fn parse_config_mount(&mut self) -> Result<ConfigMountAnnotation, ParseError> {
        self.skip_whitespace();
        let name = self.parse_identifier()?;
        let colon_span = self.expect(TokenKind::Colon)?.span;
        let path = self.parse_path_segment()?;

        Ok(ConfigMountAnnotation {
            name,
            colon_span,
            path,
        })
    }

    fn parse_port_forward(&mut self) -> Result<PortForwardAnnotation, ParseError> {
        self.skip_whitespace();
        let local_port = self.parse_path_segment()?;
        let first_colon = self.expect(TokenKind::Colon)?.span;
        let resource = self.parse_path_segment()?;
        let second_colon = self.expect(TokenKind::Colon)?.span;
        let remote_port = self.parse_path_segment()?;

        Ok(PortForwardAnnotation {
            local_port,
            first_colon,
            resource,
            second_colon,
            remote_port,
        })
    }

    fn parse_variable(
        &mut self,
        name: String,
        name_span: Span,
    ) -> Result<Option<Spanned<Item>>, ParseError> {
        let assign_span = self.expect(TokenKind::ColonEquals)?.span;
        self.skip_whitespace();

        let value_start = self.peek().span;

        // check for shell expansion
        let value = if self.check(TokenKind::Backtick) {
            let open_span = self.advance().span;
            let mut cmd_text = String::new();

            // consume until closing backtick or newline
            let mut close_span = None;
            while !self.at_end() {
                let tok = self.peek();
                match tok.kind {
                    TokenKind::Backtick => {
                        close_span = Some(tok.span);
                        self.advance();
                        break;
                    }
                    TokenKind::Newline | TokenKind::Eof => break,
                    _ => {
                        cmd_text.push_str(tok.text(self.source));
                        self.advance();
                    }
                }
            }

            if close_span.is_none() {
                self.errors.push(ParseError::new(
                    ParseErrorKind::UnclosedDelimiter,
                    open_span,
                    "unclosed backtick",
                ));
            }

            let cmd_span = Span::new(
                open_span.end,
                close_span.map_or(self.pos as u32, |s| s.start),
            );
            Spanned::new(
                VariableValue::Shell(ShellExpansion {
                    open_span,
                    command: Spanned::new(cmd_text, cmd_span),
                    close_span,
                }),
                value_start.merge(self.prev_span()),
            )
        } else {
            // static value - rest of line
            let text = self.consume_to_newline();
            let trimmed = text.trim().to_string();
            Spanned::new(
                VariableValue::Static(trimmed),
                value_start.merge(self.prev_span()),
            )
        };

        let span = name_span.merge(value.span);
        Ok(Some(Spanned::new(
            Item::Variable(VariableDecl {
                name: Spanned::new(name, name_span),
                assign_span,
                value,
            }),
            span,
        )))
    }

    fn parse_task(
        &mut self,
        name: String,
        name_span: Span,
        annotations: Vec<Spanned<Annotation>>,
    ) -> Result<Option<Spanned<Item>>, ParseError> {
        // parse parameters before colon
        let parameters = self.parse_task_parameters()?;

        let colon_span = self.expect(TokenKind::Colon)?.span;
        self.skip_whitespace();

        // parse dependencies
        let mut dependencies = Vec::new();
        while !self.at_line_end() {
            self.skip_whitespace();
            if self.at_line_end() {
                break;
            }

            let dep = self.parse_dependency()?;
            dependencies.push(dep);

            self.skip_whitespace();
            if self.check(TokenKind::Comma) {
                self.advance();
            }
        }

        self.skip_to_newline();
        self.advance(); // consume newline

        // parse body (indented lines)
        let body = self.parse_task_body()?;

        let end_span = body.as_ref().map(|b| b.span).unwrap_or(colon_span);
        let full_span = annotations
            .first()
            .map(|a| a.span)
            .unwrap_or(name_span)
            .merge(end_span);

        Ok(Some(Spanned::new(
            Item::Task(TaskDecl {
                annotations,
                name: Spanned::new(name, name_span),
                parameters,
                colon_span,
                dependencies,
                body,
            }),
            full_span,
        )))
    }

    /// Parse task parameters: `param1 param2="default"`
    fn parse_task_parameters(&mut self) -> Result<Vec<Spanned<Parameter>>, ParseError> {
        let mut params = Vec::new();

        while !self.at_line_end() && !self.check(TokenKind::Colon) {
            self.skip_whitespace();
            if self.at_line_end() || self.check(TokenKind::Colon) {
                break;
            }

            let param = self.parse_parameter()?;
            params.push(param);
        }

        Ok(params)
    }

    /// Parse a single parameter: `name` or `name="default"` or `name={{var}}`
    fn parse_parameter(&mut self) -> Result<Spanned<Parameter>, ParseError> {
        let name = self.parse_identifier()?;
        let start_span = name.span;

        // check for default value (=)
        let default = if self.check(TokenKind::Equals) {
            self.advance(); // consume =
            Some(self.parse_parameter_default()?)
        } else {
            None
        };

        let end_span = default.as_ref().map(|d| d.span).unwrap_or(name.span);

        Ok(Spanned::new(
            Parameter { name, default },
            start_span.merge(end_span),
        ))
    }

    /// Parse parameter default value: quoted string or {{variable}}
    fn parse_parameter_default(&mut self) -> Result<Spanned<ParameterDefault>, ParseError> {
        let start_span = self.peek().span;

        // check for {{variable}} reference
        if self.check(TokenKind::OpenBrace) {
            let open1 = self.advance().span;
            if self.check(TokenKind::OpenBrace) {
                let open2 = self.advance().span;
                self.skip_whitespace();
                let var_name = self.parse_identifier()?;
                self.skip_whitespace();

                let mut close_span = None;
                if self.check(TokenKind::CloseBrace) {
                    let close1 = self.advance().span;
                    if self.check(TokenKind::CloseBrace) {
                        close_span = Some(close1.merge(self.advance().span));
                    }
                }

                let interp = Interpolation {
                    open_span: open1.merge(open2),
                    name: var_name,
                    close_span,
                };
                let span = start_span.merge(close_span.unwrap_or(start_span));
                return Ok(Spanned::new(ParameterDefault::Variable(interp), span));
            }
            // single brace - error
            return Err(ParseError::new(
                ParseErrorKind::Expected,
                start_span,
                "expected '{{' for variable reference",
            ));
        }

        // quoted string default
        if self.check(TokenKind::Quote) {
            self.advance(); // opening quote
            let mut value = String::new();
            while !self.at_end() {
                let tok = self.peek();
                match &tok.kind {
                    TokenKind::Quote => {
                        let end = tok.span;
                        self.advance();
                        return Ok(Spanned::new(
                            ParameterDefault::Literal(value),
                            start_span.merge(end),
                        ));
                    }
                    TokenKind::Newline | TokenKind::Eof => break,
                    _ => {
                        value.push_str(tok.text(self.source));
                        self.advance();
                    }
                }
            }
            return Err(ParseError::new(
                ParseErrorKind::UnclosedDelimiter,
                start_span,
                "unclosed quote in parameter default",
            ));
        }

        // unquoted literal - consume until whitespace/colon
        let mut value = String::new();
        let mut end_span = start_span;
        while !self.at_line_end() {
            let tok = self.peek();
            match &tok.kind {
                TokenKind::Whitespace | TokenKind::Colon | TokenKind::Newline | TokenKind::Eof => {
                    break;
                }
                _ => {
                    value.push_str(tok.text(self.source));
                    end_span = tok.span;
                    self.advance();
                }
            }
        }

        if value.is_empty() {
            return Err(ParseError::new(
                ParseErrorKind::Expected,
                start_span,
                "expected parameter default value",
            ));
        }

        Ok(Spanned::new(
            ParameterDefault::Literal(value),
            start_span.merge(end_span),
        ))
    }

    fn parse_dependency(&mut self) -> Result<Spanned<Dependency>, ParseError> {
        let tok = self.advance();
        let name = match &tok.kind {
            TokenKind::Identifier(s) => s.clone(),
            _ => {
                return Err(ParseError::new(
                    ParseErrorKind::Expected,
                    tok.span,
                    "expected dependency name",
                ));
            }
        };
        let start_span = tok.span;

        // check for service: prefix
        if self.check(TokenKind::Colon) {
            self.advance();
            let service_name_tok = self.advance();
            let service_name = match &service_name_tok.kind {
                TokenKind::Identifier(s) => s.clone(),
                _ => {
                    return Err(ParseError::new(
                        ParseErrorKind::Expected,
                        service_name_tok.span,
                        "expected service name",
                    ));
                }
            };
            let span = start_span.merge(service_name_tok.span);
            return Ok(Spanned::new(Dependency::Service(service_name), span));
        }

        Ok(Spanned::new(Dependency::Task(name), start_span))
    }

    fn parse_task_body(&mut self) -> Result<Option<TaskBody>, ParseError> {
        let mut lines = Vec::new();
        let start_span = self.peek().span;

        while !self.at_end() {
            // blank lines: only keep if followed by more indented content
            if self.check(TokenKind::Newline) {
                // peek ahead to see if there's more body content
                let saved_pos = self.pos;
                let span = self.advance().span;

                // skip any additional blank lines
                while self.check(TokenKind::Newline) {
                    self.advance();
                }

                // if next is indent, body continues - record blank line(s)
                if self.check(TokenKind::Indent) {
                    lines.push(Spanned::new(BodyLine::Empty, span));
                    continue;
                } else {
                    // body ends here, rewind
                    self.pos = saved_pos;
                    break;
                }
            }

            // check for indent (actual body content)
            if !self.check(TokenKind::Indent) {
                break;
            }

            let indent_span = self.advance().span;
            let line = self.parse_body_line(indent_span)?;
            lines.push(line);

            // consume newline if present
            if self.check(TokenKind::Newline) {
                self.advance();
            }
        }

        if lines.is_empty() {
            return Ok(None);
        }

        let end_span = lines.last().map(|l| l.span).unwrap_or(start_span);
        Ok(Some(TaskBody {
            span: start_span.merge(end_span),
            lines,
        }))
    }

    fn parse_body_line(&mut self, indent_span: Span) -> Result<Spanned<BodyLine>, ParseError> {
        // check for shebang
        if self.check(TokenKind::Shebang) {
            let shebang_span = self.advance().span;
            let rest = self.consume_to_newline();
            let parts: Vec<&str> = rest.split_whitespace().collect();

            let interpreter = parts.first().map(|s| s.to_string()).unwrap_or_default();
            let interp_end = shebang_span.end + interpreter.len() as u32;

            let args: Vec<Spanned<String>> = parts
                .iter()
                .skip(1)
                .map(|s| Spanned::new(s.to_string(), Span::default()))
                .collect();

            let span = indent_span.merge(self.prev_span());
            return Ok(Spanned::new(
                BodyLine::Shebang(Shebang {
                    prefix_span: shebang_span,
                    interpreter: Spanned::new(interpreter, Span::new(shebang_span.end, interp_end)),
                    args,
                }),
                span,
            ));
        }

        // regular command line
        let segments = self.parse_command_segments();
        let span = if segments.is_empty() {
            indent_span
        } else {
            indent_span.merge(segments.last().unwrap().span)
        };

        if segments.is_empty() {
            return Ok(Spanned::new(BodyLine::Empty, span));
        }

        Ok(Spanned::new(
            BodyLine::Command(CommandLine { segments }),
            span,
        ))
    }

    fn parse_command_segments(&mut self) -> Vec<Spanned<CommandSegment>> {
        let mut segments = Vec::new();
        let mut current_text = String::new();
        let mut text_start: Option<Span> = None;

        while !self.at_line_end() {
            let tok = self.peek();

            // check for interpolation {{
            if tok.kind == TokenKind::OpenBrace {
                let open1 = tok.span;
                self.advance();
                if self.check(TokenKind::OpenBrace) {
                    // flush current text
                    if !current_text.is_empty() {
                        let span = text_start.unwrap().merge(Span::point(open1.start));
                        segments.push(Spanned::new(
                            CommandSegment::Text(std::mem::take(&mut current_text)),
                            span,
                        ));
                        text_start = None;
                    }

                    let open2 = self.advance().span;
                    let open_span = open1.merge(open2);

                    // parse variable name
                    self.skip_whitespace();
                    let name = self.parse_identifier().ok();
                    self.skip_whitespace();

                    // look for }}
                    let mut close_span = None;
                    if self.check(TokenKind::CloseBrace) {
                        let close1 = self.advance().span;
                        if self.check(TokenKind::CloseBrace) {
                            let close2 = self.advance().span;
                            close_span = Some(close1.merge(close2));
                        }
                    }

                    let interp = Interpolation {
                        open_span,
                        name: name.unwrap_or_else(|| Spanned::new(String::new(), open_span)),
                        close_span,
                    };

                    let span = open_span.merge(close_span.unwrap_or(open_span));
                    segments.push(Spanned::new(CommandSegment::Interpolation(interp), span));
                    continue;
                } else {
                    // single brace, treat as text
                    if text_start.is_none() {
                        text_start = Some(open1);
                    }
                    current_text.push('{');
                    continue;
                }
            }

            // regular content
            if text_start.is_none() {
                text_start = Some(tok.span);
            }
            current_text.push_str(tok.text(self.source));
            self.advance();
        }

        // flush remaining text
        if !current_text.is_empty() {
            let span = text_start.unwrap().merge(self.prev_span());
            segments.push(Spanned::new(CommandSegment::Text(current_text), span));
        }

        segments
    }

    fn parse_lua_block(
        &mut self,
        at_span: Span,
        pending_annotations: &mut Vec<Spanned<Annotation>>,
    ) -> Result<Option<Spanned<Item>>, ParseError> {
        if !pending_annotations.is_empty() {
            for ann in pending_annotations.drain(..) {
                self.errors.push(ParseError::new(
                    ParseErrorKind::OrphanedAnnotation,
                    ann.span,
                    "annotation before lua block",
                ));
            }
        }

        // consume "lua"
        self.advance();
        self.skip_to_newline();
        self.advance(); // newline

        let content_start = self.pos as u32;
        let mut content = String::new();
        let mut close_span = None;

        while !self.at_end() {
            // check for @end at line start
            self.skip_whitespace();
            if self.check(TokenKind::At) {
                let at_span = self.advance().span;
                let next = self.peek();
                if let TokenKind::Identifier(name) = &next.kind
                    && name == "end"
                {
                    let end_span = self.advance().span;
                    close_span = Some(at_span.merge(end_span));
                    break;
                }
                content.push('@');
            }

            // consume line
            let line = self.consume_to_newline();
            content.push_str(&line);
            content.push('\n');
            if !self.at_end() && self.check(TokenKind::Newline) {
                self.advance();
            }
        }

        if close_span.is_none() {
            self.errors.push(ParseError::new(
                ParseErrorKind::UnclosedLuaBlock,
                at_span,
                "lua block missing @end",
            ));
        }

        let content_span = Span::new(content_start, self.pos as u32);
        let full_span = at_span.merge(close_span.unwrap_or(content_span));

        Ok(Some(Spanned::new(
            Item::LuaBlock(LuaBlock {
                open_span: at_span,
                content: Spanned::new(content, content_span),
                close_span,
            }),
            full_span,
        )))
    }

    fn parse_context_block(
        &mut self,
        at_span: Span,
        pending_annotations: &mut Vec<Spanned<Annotation>>,
    ) -> Result<Option<Spanned<Item>>, ParseError> {
        if !pending_annotations.is_empty() {
            for ann in pending_annotations.drain(..) {
                self.errors.push(ParseError::new(
                    ParseErrorKind::OrphanedAnnotation,
                    ann.span,
                    "annotation before context block",
                ));
            }
        }

        // consume "context"
        self.advance();
        self.skip_whitespace();

        // parse context name
        let name = self.parse_identifier()?;

        self.skip_to_newline();
        if !self.at_end() && self.check(TokenKind::Newline) {
            self.advance();
        }

        let mut annotations = Vec::new();
        let mut close_span = None;

        while !self.at_end() {
            self.skip_whitespace();

            // check for @end or another annotation
            if self.check(TokenKind::At) {
                let ann_at_span = self.advance().span;
                if let TokenKind::Identifier(ann_name) = &self.peek().kind {
                    if ann_name == "end" {
                        let end_span = self.advance().span;
                        close_span = Some(ann_at_span.merge(end_span));
                        break;
                    }
                    // parse annotation within context
                    let ann = self.parse_annotation(ann_at_span)?;
                    annotations.push(ann);
                    self.skip_to_newline();
                    if !self.at_end() && self.check(TokenKind::Newline) {
                        self.advance();
                    }
                    continue;
                }
            }

            // skip empty lines
            if self.check(TokenKind::Newline) {
                self.advance();
                continue;
            }

            // unexpected content
            break;
        }

        if close_span.is_none() {
            self.errors.push(ParseError::new(
                ParseErrorKind::UnclosedLuaBlock,
                at_span,
                "context block missing @end",
            ));
        }

        let full_span = at_span.merge(close_span.unwrap_or(name.span));

        Ok(Some(Spanned::new(
            Item::ContextBlock(ContextBlock {
                open_span: at_span,
                name,
                annotations,
                close_span,
            }),
            full_span,
        )))
    }

    fn parse_set_directive(
        &mut self,
        set_span: Span,
        pending_annotations: &mut Vec<Spanned<Annotation>>,
    ) -> Result<Option<Spanned<Item>>, ParseError> {
        if !pending_annotations.is_empty() {
            for ann in pending_annotations.drain(..) {
                self.errors.push(ParseError::new(
                    ParseErrorKind::OrphanedAnnotation,
                    ann.span,
                    "annotation before set directive",
                ));
            }
        }

        self.advance(); // consume "set"
        self.skip_whitespace();

        let key = self.parse_identifier()?;
        self.skip_whitespace();
        let assign_span = self.expect(TokenKind::ColonEquals)?.span;
        self.skip_whitespace();

        let value = self.parse_rest_of_line_trimmed();
        let span = set_span.merge(value.span);

        Ok(Some(Spanned::new(
            Item::SetDirective(SetDirective {
                set_span,
                key,
                assign_span,
                value,
            }),
            span,
        )))
    }

    // ========================================================================
    // Helpers
    // ========================================================================

    fn peek(&self) -> &Token {
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn advance(&mut self) -> &Token {
        let tok = &self.tokens[self.pos];
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        tok
    }

    fn prev_span(&self) -> Span {
        if self.pos > 0 {
            self.tokens[self.pos - 1].span
        } else {
            Span::default()
        }
    }

    fn at_end(&self) -> bool {
        self.peek().kind == TokenKind::Eof
    }

    fn check(&self, kind: TokenKind) -> bool {
        std::mem::discriminant(&self.peek().kind) == std::mem::discriminant(&kind)
    }

    fn expect(&mut self, kind: TokenKind) -> Result<&Token, ParseError> {
        if self.check(kind.clone()) {
            Ok(self.advance())
        } else {
            Err(ParseError::new(
                ParseErrorKind::Expected,
                self.peek().span,
                format!("expected {:?}", kind),
            ))
        }
    }

    fn skip_whitespace(&mut self) {
        while self.check(TokenKind::Whitespace) {
            self.advance();
        }
    }

    fn skip_empty_lines(&mut self) {
        while matches!(self.peek().kind, TokenKind::Newline | TokenKind::Whitespace) {
            self.advance();
        }
    }

    fn at_line_end(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Newline | TokenKind::Eof)
    }

    fn skip_to_newline(&mut self) {
        while !self.at_line_end() {
            self.advance();
        }
    }

    fn consume_to_newline(&mut self) -> String {
        let mut s = String::new();
        while !self.at_line_end() {
            s.push_str(self.peek().text(self.source));
            self.advance();
        }
        s
    }

    fn recover_to_next_line(&mut self) {
        self.skip_to_newline();
        if !self.at_end() {
            self.advance();
        }
    }

    fn parse_identifier(&mut self) -> Result<Spanned<String>, ParseError> {
        let tok = self.advance();
        match &tok.kind {
            TokenKind::Identifier(s) => Ok(Spanned::new(s.clone(), tok.span)),
            _ => Err(ParseError::new(
                ParseErrorKind::Expected,
                tok.span,
                "expected identifier",
            )),
        }
    }

    fn parse_rest_of_line_trimmed(&mut self) -> Spanned<String> {
        let start = self.peek().span;
        let text = self.consume_to_newline();
        let trimmed = text.trim().to_string();
        Spanned::new(trimmed, start.merge(self.prev_span()))
    }

    fn parse_comma_separated_identifiers(&mut self) -> Vec<Spanned<String>> {
        let mut items = Vec::new();

        while !self.at_line_end() {
            self.skip_whitespace();
            if self.at_line_end() {
                break;
            }

            if let Ok(ident) = self.parse_identifier() {
                items.push(ident);
            } else {
                self.advance();
            }

            self.skip_whitespace();
            if self.check(TokenKind::Comma) {
                self.advance();
            }
        }

        items
    }

    fn try_parse_key_value(&mut self) -> Option<Spanned<KeyValue>> {
        let start_pos = self.pos;
        let key_tok = self.peek().clone();

        let key = match &key_tok.kind {
            TokenKind::Identifier(s) => s.clone(),
            _ => return None,
        };
        self.advance();
        self.skip_whitespace();

        if !self.check(TokenKind::Equals) {
            // rewind
            self.pos = start_pos;
            return None;
        }

        let eq_span = self.advance().span;
        self.skip_whitespace();

        // consume value - may span multiple tokens (e.g., tcp:127.0.0.1:8080)
        // or be a quoted string (e.g., "test -f /tmp/file")
        let val_start_span = self.peek().span;
        let mut value = String::new();
        let mut val_end_span = val_start_span;

        // check for quoted string
        let first_tok = self.peek();
        if let TokenKind::Quote = first_tok.kind {
            self.advance(); // consume opening quote
            // consume until closing quote
            while !self.at_end() {
                let tok = self.peek();
                match &tok.kind {
                    TokenKind::Quote => {
                        val_end_span = tok.span;
                        self.advance(); // consume closing quote
                        break;
                    }
                    TokenKind::Newline | TokenKind::Eof => break,
                    _ => {
                        value.push_str(tok.text(self.source));
                        val_end_span = tok.span;
                        self.advance();
                    }
                }
            }
        } else {
            // unquoted value - consume until whitespace
            while !self.at_line_end() {
                let tok = self.peek();
                match &tok.kind {
                    TokenKind::Whitespace | TokenKind::Newline | TokenKind::Eof => break,
                    TokenKind::Identifier(s) | TokenKind::Text(s) => {
                        value.push_str(s);
                        val_end_span = tok.span;
                        self.advance();
                    }
                    TokenKind::Colon => {
                        value.push(':');
                        val_end_span = tok.span;
                        self.advance();
                    }
                    TokenKind::Quote => {
                        // embedded quoted string (e.g., cmd:"test -f /tmp/file")
                        self.advance(); // consume opening quote
                        while !self.at_end() {
                            let inner_tok = self.peek();
                            match &inner_tok.kind {
                                TokenKind::Quote => {
                                    val_end_span = inner_tok.span;
                                    self.advance(); // consume closing quote
                                    break;
                                }
                                TokenKind::Newline | TokenKind::Eof => break,
                                _ => {
                                    value.push_str(inner_tok.text(self.source));
                                    val_end_span = inner_tok.span;
                                    self.advance();
                                }
                            }
                        }
                    }
                    _ => {
                        // include other tokens in the value
                        value.push_str(tok.text(self.source));
                        val_end_span = tok.span;
                        self.advance();
                    }
                }
            }
        }

        if value.is_empty() {
            self.pos = start_pos;
            return None;
        }

        let val_span = val_start_span.merge(val_end_span);
        let span = key_tok.span.merge(val_end_span);
        Some(Spanned::new(
            KeyValue {
                key: Spanned::new(key, key_tok.span),
                eq_span,
                value: Spanned::new(value, val_span),
            },
            span,
        ))
    }

    fn parse_key_value_options(&mut self) -> Vec<Spanned<KeyValue>> {
        let mut options = Vec::new();

        while !self.at_line_end() {
            self.skip_whitespace();
            if self.at_line_end() {
                break;
            }

            if let Some(kv) = self.try_parse_key_value() {
                options.push(kv);
            } else {
                // skip unknown token
                self.advance();
            }
        }

        options
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_task() {
        let (file, errors) = parse("build:\n\tcargo build");
        assert!(errors.is_empty(), "errors: {:?}", errors);
        assert_eq!(file.items.len(), 1);

        if let Item::Task(task) = &file.items[0].node {
            assert_eq!(task.name.node, "build");
            assert!(task.body.is_some());
        } else {
            panic!("expected task");
        }
    }

    #[test]
    fn parse_task_with_deps() {
        let (file, errors) = parse("test: build\n\tcargo test");
        assert!(errors.is_empty());

        if let Item::Task(task) = &file.items[0].node {
            assert_eq!(task.dependencies.len(), 1);
            if let Dependency::Task(name) = &task.dependencies[0].node {
                assert_eq!(name, "build");
            }
        }
    }

    #[test]
    fn parse_variable() {
        let (file, errors) = parse("version := 1.0.0");
        assert!(errors.is_empty());

        if let Item::Variable(var) = &file.items[0].node {
            assert_eq!(var.name.node, "version");
            if let VariableValue::Static(v) = &var.value.node {
                assert_eq!(v, "1.0.0");
            }
        }
    }

    #[test]
    fn parse_shell_variable() {
        let (file, errors) = parse("hash := `git rev-parse HEAD`");
        assert!(errors.is_empty());

        if let Item::Variable(var) = &file.items[0].node {
            if let VariableValue::Shell(shell) = &var.value.node {
                assert_eq!(shell.command.node, "git rev-parse HEAD");
            } else {
                panic!("expected shell expansion");
            }
        }
    }

    #[test]
    fn parse_annotation() {
        let (file, errors) = parse("@timeout 5m\nbuild:\n\tcargo build");
        assert!(errors.is_empty());

        if let Item::Task(task) = &file.items[0].node {
            assert_eq!(task.annotations.len(), 1);
            if let AnnotationKind::Timeout(val) = &task.annotations[0].node.kind {
                assert_eq!(val.node, "5m");
            }
        }
    }

    #[test]
    fn parse_ssh_annotation() {
        let (file, errors) =
            parse("@ssh host=user@host.example.com user=deploy\nremote:\n\techo hi");
        assert!(errors.is_empty());

        if let Item::Task(task) = &file.items[0].node {
            if let AnnotationKind::Ssh(ssh) = &task.annotations[0].node.kind {
                assert_eq!(ssh.options.len(), 2);
                assert_eq!(ssh.options[0].node.key.node, "host");
                assert_eq!(ssh.options[0].node.value.node, "user@host.example.com");
                assert_eq!(ssh.options[1].node.key.node, "user");
                assert_eq!(ssh.options[1].node.value.node, "deploy");
            }
        }
    }

    #[test]
    fn parse_ssh_annotation_with_workdir() {
        let (file, errors) =
            parse("@ssh host=wseaton@10.14.217.13 workdir=/home/wseaton/git/vllm\ntest:\n\tpwd");
        assert!(errors.is_empty());

        if let Item::Task(task) = &file.items[0].node {
            if let AnnotationKind::Ssh(ssh) = &task.annotations[0].node.kind {
                assert_eq!(ssh.options.len(), 2);
                assert_eq!(ssh.options[0].node.key.node, "host");
                assert_eq!(ssh.options[0].node.value.node, "wseaton@10.14.217.13");
                assert_eq!(ssh.options[1].node.key.node, "workdir");
                assert_eq!(ssh.options[1].node.value.node, "/home/wseaton/git/vllm");
            }
        }
    }

    #[test]
    fn spans_are_tracked() {
        let source = "build:\n\techo hello";
        let (file, _) = parse(source);

        if let Item::Task(task) = &file.items[0].node {
            assert_eq!(task.name.span.text(source), "build");
        }
    }

    #[test]
    fn parse_task_with_parameters() {
        let (file, errors) = parse("release version:\n\tcargo set-version {{version}}");
        assert!(errors.is_empty(), "errors: {:?}", errors);

        if let Item::Task(task) = &file.items[0].node {
            assert_eq!(task.name.node, "release");
            assert_eq!(task.parameters.len(), 1);
            assert_eq!(task.parameters[0].node.name.node, "version");
            assert!(task.parameters[0].node.default.is_none());
        } else {
            panic!("expected task");
        }
    }

    #[test]
    fn parse_task_with_default_parameter() {
        let (file, errors) = parse("release version=\"0.1.0\":\n\tcargo set-version {{version}}");
        assert!(errors.is_empty(), "errors: {:?}", errors);

        if let Item::Task(task) = &file.items[0].node {
            assert_eq!(task.parameters.len(), 1);
            assert_eq!(task.parameters[0].node.name.node, "version");
            if let Some(default) = &task.parameters[0].node.default {
                if let ParameterDefault::Literal(val) = &default.node {
                    assert_eq!(val, "0.1.0");
                } else {
                    panic!("expected literal default");
                }
            } else {
                panic!("expected default value");
            }
        } else {
            panic!("expected task");
        }
    }

    #[test]
    fn parse_task_with_multiple_parameters() {
        let (file, errors) = parse("deploy env version=\"latest\":\n\techo {{env}} {{version}}");
        assert!(errors.is_empty(), "errors: {:?}", errors);

        if let Item::Task(task) = &file.items[0].node {
            assert_eq!(task.parameters.len(), 2);
            assert_eq!(task.parameters[0].node.name.node, "env");
            assert!(task.parameters[0].node.default.is_none());
            assert_eq!(task.parameters[1].node.name.node, "version");
            assert!(task.parameters[1].node.default.is_some());
        } else {
            panic!("expected task");
        }
    }

    #[test]
    fn parse_context_block() {
        let source = "@context default\n@ssh host=user@host\n@timeout 5m\n@end\n";
        let (file, errors) = parse(source);
        assert!(errors.is_empty(), "errors: {:?}", errors);

        if let Item::ContextBlock(ctx) = &file.items[0].node {
            assert_eq!(ctx.name.node, "default");
            assert_eq!(ctx.annotations.len(), 2);
            assert!(matches!(
                ctx.annotations[0].node.kind,
                AnnotationKind::Ssh(_)
            ));
            assert!(matches!(
                ctx.annotations[1].node.kind,
                AnnotationKind::Timeout(_)
            ));
        } else {
            panic!("expected context block, got {:?}", file.items[0].node);
        }
    }

    #[test]
    fn parse_use_annotation() {
        let source = "@use remote\ntask:\n\techo hi";
        let (file, errors) = parse(source);
        assert!(errors.is_empty(), "errors: {:?}", errors);

        if let Item::Task(task) = &file.items[0].node {
            assert_eq!(task.annotations.len(), 1);
            if let AnnotationKind::Use(name) = &task.annotations[0].node.kind {
                assert_eq!(name.node, "remote");
            } else {
                panic!("expected Use annotation");
            }
        } else {
            panic!("expected task");
        }
    }
}
