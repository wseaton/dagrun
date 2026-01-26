use crate::{Span, Spanned};

/// Root of the AST - a complete dagrun file
#[derive(Debug, Clone, Default)]
pub struct SourceFile {
    /// All items in the file, in source order
    pub items: Vec<Spanned<Item>>,
}

/// Top-level item in a dagrun file
#[derive(Debug, Clone)]
pub enum Item {
    /// Variable assignment: `name := value` or `name := \`command\``
    Variable(VariableDecl),
    /// Task definition with optional annotations
    Task(TaskDecl),
    /// Lua block: `@lua ... @end`
    LuaBlock(LuaBlock),
    /// Set directive: `set key := value`
    SetDirective(SetDirective),
    /// Comment line (preserved for documentation)
    Comment(Comment),
}

// ============================================================================
// Variables
// ============================================================================

#[derive(Debug, Clone)]
pub struct VariableDecl {
    /// Variable name
    pub name: Spanned<String>,
    /// The `:=` token span
    pub assign_span: Span,
    /// Variable value
    pub value: Spanned<VariableValue>,
}

#[derive(Debug, Clone)]
pub enum VariableValue {
    /// Static string value
    Static(String),
    /// Shell command expansion: `` `command` ``
    Shell(ShellExpansion),
}

#[derive(Debug, Clone)]
pub struct ShellExpansion {
    /// The opening backtick span
    pub open_span: Span,
    /// The command text
    pub command: Spanned<String>,
    /// The closing backtick span (may be missing for error recovery)
    pub close_span: Option<Span>,
}

// ============================================================================
// Task Parameters
// ============================================================================

/// Task parameter definition: `name` or `name="default"`
#[derive(Debug, Clone)]
pub struct Parameter {
    /// Parameter name
    pub name: Spanned<String>,
    /// Default value (None = required, Some = optional)
    pub default: Option<Spanned<ParameterDefault>>,
}

/// Default value for a parameter
#[derive(Debug, Clone)]
pub enum ParameterDefault {
    /// Literal string value: `"value"`
    Literal(String),
    /// Variable reference: `{{varname}}`
    Variable(Interpolation),
}

// ============================================================================
// Tasks
// ============================================================================

#[derive(Debug, Clone)]
pub struct TaskDecl {
    /// Annotations preceding this task
    pub annotations: Vec<Spanned<Annotation>>,
    /// Task name
    pub name: Spanned<String>,
    /// Task parameters (between name and colon)
    pub parameters: Vec<Spanned<Parameter>>,
    /// The colon token span
    pub colon_span: Span,
    /// Dependencies after the colon
    pub dependencies: Vec<Spanned<Dependency>>,
    /// Task body (indented lines)
    pub body: Option<TaskBody>,
}

#[derive(Debug, Clone)]
pub enum Dependency {
    /// Regular task dependency
    Task(String),
    /// Service dependency: `service:name`
    Service(String),
}

#[derive(Debug, Clone)]
pub struct TaskBody {
    /// Full span of the body
    pub span: Span,
    /// Individual body lines
    pub lines: Vec<Spanned<BodyLine>>,
}

#[derive(Debug, Clone)]
pub enum BodyLine {
    /// Shebang line: `#!/path/to/interpreter`
    Shebang(Shebang),
    /// Regular command line
    Command(CommandLine),
    /// Empty/whitespace-only line within body
    Empty,
}

#[derive(Debug, Clone)]
pub struct Shebang {
    /// The `#!` prefix span
    pub prefix_span: Span,
    /// Interpreter path
    pub interpreter: Spanned<String>,
    /// Arguments to interpreter
    pub args: Vec<Spanned<String>>,
}

#[derive(Debug, Clone)]
pub struct CommandLine {
    /// Segments of the command (text and interpolations)
    pub segments: Vec<Spanned<CommandSegment>>,
}

#[derive(Debug, Clone)]
pub enum CommandSegment {
    /// Literal text
    Text(String),
    /// Variable interpolation: `{{name}}`
    Interpolation(Interpolation),
}

#[derive(Debug, Clone)]
pub struct Interpolation {
    /// Opening `{{` span
    pub open_span: Span,
    /// Variable name
    pub name: Spanned<String>,
    /// Closing `}}` span (may be missing)
    pub close_span: Option<Span>,
}

// ============================================================================
// Annotations
// ============================================================================

#[derive(Debug, Clone)]
pub struct Annotation {
    /// The `#` prefix span
    pub hash_span: Span,
    /// The `@` symbol span
    pub at_span: Span,
    /// Annotation kind and data
    pub kind: AnnotationKind,
}

#[derive(Debug, Clone)]
pub enum AnnotationKind {
    /// `@timeout duration`
    Timeout(Spanned<String>),

    /// `@retry count`
    Retry(Spanned<String>),

    /// `@pipe_from task1, task2, ...`
    PipeFrom(Vec<Spanned<String>>),

    /// `@join`
    Join,

    /// `@ssh host key=value ...`
    Ssh(SshAnnotation),

    /// `@upload local:remote`
    Upload(FileTransferAnnotation),

    /// `@download remote:local`
    Download(FileTransferAnnotation),

    /// `@service key=value ...`
    Service(ServiceAnnotation),

    /// `@extern key=value ...`
    Extern(ServiceAnnotation),

    /// `@k8s mode key=value ...`
    K8s(K8sAnnotation),

    /// `@k8s-configmap name:/path`
    K8sConfigmap(ConfigMountAnnotation),

    /// `@k8s-secret name:/path`
    K8sSecret(ConfigMountAnnotation),

    /// `@k8s-upload local:remote`
    K8sUpload(FileTransferAnnotation),

    /// `@k8s-download remote:local`
    K8sDownload(FileTransferAnnotation),

    /// `@k8s-forward local:resource:remote`
    K8sForward(PortForwardAnnotation),

    /// Unknown annotation (preserved for error recovery/linting)
    Unknown {
        name: Spanned<String>,
        rest: Option<Spanned<String>>,
    },
}

#[derive(Debug, Clone)]
pub struct SshAnnotation {
    /// Host (first positional argument)
    pub host: Option<Spanned<String>>,
    /// Key-value pairs (user=, port=, workdir=, identity=)
    pub options: Vec<Spanned<KeyValue>>,
}

#[derive(Debug, Clone)]
pub struct KeyValue {
    pub key: Spanned<String>,
    pub eq_span: Span,
    pub value: Spanned<String>,
}

#[derive(Debug, Clone)]
pub struct FileTransferAnnotation {
    pub local: Spanned<String>,
    pub colon_span: Span,
    pub remote: Spanned<String>,
}

#[derive(Debug, Clone)]
pub struct ServiceAnnotation {
    pub options: Vec<Spanned<KeyValue>>,
}

#[derive(Debug, Clone)]
pub struct K8sAnnotation {
    /// Mode keyword (job, exec, apply) if present
    pub mode: Option<Spanned<String>>,
    /// Key-value options
    pub options: Vec<Spanned<KeyValue>>,
}

#[derive(Debug, Clone)]
pub struct ConfigMountAnnotation {
    pub name: Spanned<String>,
    pub colon_span: Span,
    pub path: Spanned<String>,
}

#[derive(Debug, Clone)]
pub struct PortForwardAnnotation {
    pub local_port: Spanned<String>,
    pub first_colon: Span,
    pub resource: Spanned<String>,
    pub second_colon: Span,
    pub remote_port: Spanned<String>,
}

// ============================================================================
// Lua Blocks
// ============================================================================

#[derive(Debug, Clone)]
pub struct LuaBlock {
    /// `@lua` token span
    pub open_span: Span,
    /// Raw Lua source code
    pub content: Spanned<String>,
    /// `@end` token span (may be missing)
    pub close_span: Option<Span>,
}

// ============================================================================
// Set Directives and Comments
// ============================================================================

#[derive(Debug, Clone)]
pub struct SetDirective {
    /// `set` keyword span
    pub set_span: Span,
    /// Key name
    pub key: Spanned<String>,
    /// `:=` span
    pub assign_span: Span,
    /// Value
    pub value: Spanned<String>,
}

#[derive(Debug, Clone)]
pub struct Comment {
    /// Full comment text (including `#`)
    pub text: String,
    /// Whether this appears to be a doc comment (starts with `##`)
    pub is_doc: bool,
}
