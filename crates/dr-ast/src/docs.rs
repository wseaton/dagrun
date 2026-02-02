//! documentation for dagrun annotations and syntax
//! shared between rustdoc, LSP hover, and any future tooling

/// annotation documentation with name, syntax, description, and examples
pub struct AnnotationDoc {
    pub name: &'static str,
    pub syntax: &'static str,
    pub description: &'static str,
    pub options: &'static [(&'static str, &'static str)],
    pub example: &'static str,
}

impl AnnotationDoc {
    /// format as markdown for LSP hover
    pub fn to_markdown(&self) -> String {
        let mut md = format!("**@{}** - {}\n\n", self.name, self.description);
        md.push_str(&format!("**Syntax:** `{}`\n\n", self.syntax));

        if !self.options.is_empty() {
            md.push_str("**Options:**\n");
            for (opt, desc) in self.options {
                md.push_str(&format!("- `{}` - {}\n", opt, desc));
            }
            md.push('\n');
        }

        if !self.example.is_empty() {
            md.push_str("**Example:**\n```\n");
            md.push_str(self.example);
            md.push_str("\n```");
        }

        md
    }
}

pub const SSH: AnnotationDoc = AnnotationDoc {
    name: "ssh",
    syntax: "#@ssh user@host [options]",
    description: "Execute task on remote host via SSH",
    options: &[
        ("workdir=/path", "Remote working directory"),
        ("identity=/path", "SSH identity file"),
        ("port=22", "SSH port"),
    ],
    example: "#@ssh deploy@prod.example.com workdir=/app",
};

pub const K8S: AnnotationDoc = AnnotationDoc {
    name: "k8s",
    syntax: "#@k8s [mode] [options]",
    description: "Execute task in Kubernetes pod",
    options: &[
        ("namespace=ns", "Kubernetes namespace"),
        ("pod=name", "Pod name or selector"),
        ("container=name", "Container name"),
        ("workdir=/path", "Working directory in container"),
    ],
    example: "#@k8s exec namespace=prod pod=api-server container=app",
};

pub const UPLOAD: AnnotationDoc = AnnotationDoc {
    name: "upload",
    syntax: "#@upload local_path:remote_path",
    description: "Upload file before task execution",
    options: &[],
    example: "#@ssh user@host\n#@upload ./config.yaml:/etc/app/config.yaml",
};

pub const DOWNLOAD: AnnotationDoc = AnnotationDoc {
    name: "download",
    syntax: "#@download remote_path:local_path",
    description: "Download file after task execution",
    options: &[],
    example: "#@ssh user@host\n#@download /var/log/app.log:./logs/app.log",
};

pub const K8S_UPLOAD: AnnotationDoc = AnnotationDoc {
    name: "k8s-upload",
    syntax: "#@k8s-upload local_path:remote_path",
    description: "Upload file to Kubernetes pod before task execution",
    options: &[],
    example: "#@k8s namespace=prod pod=api\n#@k8s-upload ./script.sh:/tmp/script.sh",
};

pub const K8S_DOWNLOAD: AnnotationDoc = AnnotationDoc {
    name: "k8s-download",
    syntax: "#@k8s-download remote_path:local_path",
    description: "Download file from Kubernetes pod after task execution",
    options: &[],
    example: "#@k8s namespace=prod pod=api\n#@k8s-download /tmp/output.log:./output.log",
};

pub const K8S_CONFIGMAP: AnnotationDoc = AnnotationDoc {
    name: "k8s-configmap",
    syntax: "#@k8s-configmap name:/mount/path",
    description: "Mount a ConfigMap into the pod",
    options: &[],
    example: "#@k8s namespace=prod\n#@k8s-configmap app-config:/etc/config",
};

pub const K8S_SECRET: AnnotationDoc = AnnotationDoc {
    name: "k8s-secret",
    syntax: "#@k8s-secret name:/mount/path",
    description: "Mount a Secret into the pod",
    options: &[],
    example: "#@k8s namespace=prod\n#@k8s-secret db-creds:/etc/secrets",
};

pub const K8S_FORWARD: AnnotationDoc = AnnotationDoc {
    name: "k8s-forward",
    syntax: "#@k8s-forward local_port:resource:remote_port",
    description: "Port forward to a Kubernetes resource during task execution",
    options: &[],
    example: "#@k8s namespace=prod\n#@k8s-forward 5432:svc/postgres:5432",
};

pub const TIMEOUT: AnnotationDoc = AnnotationDoc {
    name: "timeout",
    syntax: "#@timeout duration",
    description: "Set task execution timeout",
    options: &[],
    example: "#@timeout 10m",
};

pub const RETRY: AnnotationDoc = AnnotationDoc {
    name: "retry",
    syntax: "#@retry count",
    description: "Retry task on failure",
    options: &[],
    example: "#@retry 3",
};

pub const SERVICE: AnnotationDoc = AnnotationDoc {
    name: "service",
    syntax: "#@service [options]",
    description: "Mark task as a background service that other tasks can depend on",
    options: &[
        ("name=svc", "Service name for dependencies"),
        (
            "ready_pattern=regex",
            "Pattern to match in output when service is ready",
        ),
    ],
    example: "#@service name=db ready_pattern=ready\nstart_db:\n  docker run postgres",
};

pub const EXTERN: AnnotationDoc = AnnotationDoc {
    name: "extern",
    syntax: "#@extern [options]",
    description: "Declare an external service dependency (not managed by dagrun)",
    options: &[
        ("name=svc", "Service name"),
        ("check=cmd", "Command to check if service is available"),
    ],
    example: "#@extern name=redis check=\"redis-cli ping\"",
};

pub const PIPE_FROM: AnnotationDoc = AnnotationDoc {
    name: "pipe_from",
    syntax: "#@pipe_from task1, task2, ...",
    description: "Pipe stdout from specified tasks as stdin to this task",
    options: &[],
    example: "#@pipe_from generate_data\nprocess:\n  jq '.items[]'",
};

pub const JOIN: AnnotationDoc = AnnotationDoc {
    name: "join",
    syntax: "#@join",
    description: "Wait for all dependencies to complete (implicit barrier)",
    options: &[],
    example: "#@join\nfinalize: task1 task2 task3\n  echo \"all done\"",
};

/// get doc for an annotation by name
pub fn get_annotation_doc(name: &str) -> Option<&'static AnnotationDoc> {
    match name {
        "ssh" => Some(&SSH),
        "k8s" => Some(&K8S),
        "upload" => Some(&UPLOAD),
        "download" => Some(&DOWNLOAD),
        "k8s-upload" => Some(&K8S_UPLOAD),
        "k8s-download" => Some(&K8S_DOWNLOAD),
        "k8s-configmap" => Some(&K8S_CONFIGMAP),
        "k8s-secret" => Some(&K8S_SECRET),
        "k8s-forward" => Some(&K8S_FORWARD),
        "timeout" => Some(&TIMEOUT),
        "retry" => Some(&RETRY),
        "service" => Some(&SERVICE),
        "extern" => Some(&EXTERN),
        "pipe_from" => Some(&PIPE_FROM),
        "join" => Some(&JOIN),
        _ => None,
    }
}

/// all annotation names for completion
pub const ANNOTATION_NAMES: &[&str] = &[
    "ssh",
    "k8s",
    "upload",
    "download",
    "k8s-upload",
    "k8s-download",
    "k8s-configmap",
    "k8s-secret",
    "k8s-forward",
    "timeout",
    "retry",
    "service",
    "extern",
    "pipe_from",
    "join",
];
