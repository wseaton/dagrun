#![allow(dead_code)]

use petgraph::algo::{is_cyclic_directed, toposort};
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::HashMap;
use thiserror::Error;

use crate::config::{Config, Task};

#[derive(Error, Debug)]
pub enum DagError {
    #[error("cycle detected in task graph")]
    CycleDetected,
    #[error("task not found: {0}")]
    TaskNotFound(String),
}

pub struct TaskGraph {
    graph: DiGraph<String, ()>,
    node_map: HashMap<String, NodeIndex>,
    tasks: HashMap<String, Task>,
}

impl TaskGraph {
    pub fn from_config(config: Config) -> Result<Self, DagError> {
        let mut graph = DiGraph::new();
        let mut node_map = HashMap::new();

        // add all tasks as nodes
        for name in config.tasks.keys() {
            let idx = graph.add_node(name.clone());
            node_map.insert(name.clone(), idx);
        }

        // add edges for dependencies (dep -> task, meaning dep runs first)
        for task in config.tasks.values() {
            let task_idx = node_map[&task.name];
            for dep in &task.depends_on {
                let dep_idx = node_map[dep];
                graph.add_edge(dep_idx, task_idx, ());
            }
        }

        if is_cyclic_directed(&graph) {
            return Err(DagError::CycleDetected);
        }

        Ok(TaskGraph {
            graph,
            node_map,
            tasks: config.tasks,
        })
    }

    /// get execution order for entire graph
    pub fn execution_order(&self) -> Result<Vec<&Task>, DagError> {
        let sorted = toposort(&self.graph, None).map_err(|_| DagError::CycleDetected)?;

        Ok(sorted
            .into_iter()
            .map(|idx| {
                let name = &self.graph[idx];
                &self.tasks[name]
            })
            .collect())
    }

    /// get execution order to run a specific task (including deps)
    pub fn execution_order_for(&self, target: &str) -> Result<Vec<&Task>, DagError> {
        let target_idx = self
            .node_map
            .get(target)
            .ok_or_else(|| DagError::TaskNotFound(target.to_string()))?;

        // find all ancestors of target (tasks that must run before it)
        let mut required: HashMap<NodeIndex, bool> = HashMap::new();
        self.collect_ancestors(*target_idx, &mut required);
        required.insert(*target_idx, true);

        // filter toposort to only required nodes
        let sorted = toposort(&self.graph, None).map_err(|_| DagError::CycleDetected)?;

        Ok(sorted
            .into_iter()
            .filter(|idx| required.contains_key(idx))
            .map(|idx| {
                let name = &self.graph[idx];
                &self.tasks[name]
            })
            .collect())
    }

    fn collect_ancestors(&self, node: NodeIndex, visited: &mut HashMap<NodeIndex, bool>) {
        for neighbor in self
            .graph
            .neighbors_directed(node, petgraph::Direction::Incoming)
        {
            if let std::collections::hash_map::Entry::Vacant(e) = visited.entry(neighbor) {
                e.insert(true);
                self.collect_ancestors(neighbor, visited);
            }
        }
    }

    /// get tasks that can run in parallel (no deps on each other)
    pub fn parallel_groups(&self) -> Result<Vec<Vec<&Task>>, DagError> {
        let mut groups: Vec<Vec<&Task>> = Vec::new();
        let mut completed: HashMap<NodeIndex, bool> = HashMap::new();

        loop {
            // find all nodes whose deps are all completed
            let ready: Vec<NodeIndex> = self
                .graph
                .node_indices()
                .filter(|idx| !completed.contains_key(idx))
                .filter(|idx| {
                    self.graph
                        .neighbors_directed(*idx, petgraph::Direction::Incoming)
                        .all(|dep| completed.contains_key(&dep))
                })
                .collect();

            if ready.is_empty() {
                break;
            }

            let group: Vec<&Task> = ready
                .iter()
                .map(|idx| {
                    let name = &self.graph[*idx];
                    &self.tasks[name]
                })
                .collect();

            for idx in ready {
                completed.insert(idx, true);
            }

            groups.push(group);
        }

        Ok(groups)
    }

    pub fn task(&self, name: &str) -> Option<&Task> {
        self.tasks.get(name)
    }

    pub fn task_names(&self) -> Vec<&str> {
        self.tasks.keys().map(|s| s.as_str()).collect()
    }

    /// export graph to DOT format for graphviz
    pub fn to_dot(&self) -> String {
        let mut dot = String::from("digraph dagrun {\n");
        dot.push_str("    rankdir=TB;\n");
        dot.push_str("    node [shape=box, style=\"rounded,filled\", fontname=\"Helvetica\"];\n\n");

        // add legend
        dot.push_str("    subgraph cluster_legend {\n");
        dot.push_str("        label=\"Legend\";\n");
        dot.push_str("        style=dashed;\n");
        dot.push_str("        fontsize=10;\n");
        dot.push_str("        legend_local [label=\"Local\" fillcolor=\"#f0f0f0\"];\n");
        dot.push_str("        legend_ssh [label=\"SSH\" fillcolor=\"#a8d5ff\"];\n");
        dot.push_str("        legend_k8s [label=\"K8s\" fillcolor=\"#b8e6b8\"];\n");
        dot.push_str("        legend_svc [label=\"Service\" fillcolor=\"#ffe4b3\"];\n");
        dot.push_str("        legend_join [label=\"Join\" shape=diamond fillcolor=\"#e8e8e8\"];\n");
        dot.push_str("    }\n\n");

        // add nodes with type-specific styling
        for name in self.tasks.keys() {
            let task = &self.tasks[name];

            // determine node style based on task type
            let (color, shape, type_prefix) = if task.is_join() {
                ("#e8e8e8", "diamond", "")
            } else if task.k8s.is_some() {
                let mode = task.k8s.as_ref().map(|k| match k.mode {
                    crate::config::K8sMode::Job => "job",
                    crate::config::K8sMode::Exec => "exec",
                    crate::config::K8sMode::Apply => "apply",
                }).unwrap_or("k8s");
                ("#b8e6b8", "box", mode)
            } else if task.ssh.is_some() {
                ("#a8d5ff", "box", "ssh")
            } else if task.service.is_some() {
                ("#ffe4b3", "box", "svc")
            } else {
                ("#f0f0f0", "box", "")
            };

            // build label with type prefix and timeout
            let mut label = if type_prefix.is_empty() {
                name.clone()
            } else {
                format!("[{}] {}", type_prefix, name)
            };
            if let Some(t) = task.timeout {
                label.push_str(&format!("\\n(timeout: {:?})", t));
            }

            dot.push_str(&format!(
                "    \"{}\" [label=\"{}\" fillcolor=\"{}\" shape={}];\n",
                name, label, color, shape
            ));
        }

        dot.push('\n');

        // add edges
        for task in self.tasks.values() {
            for dep in &task.depends_on {
                dot.push_str(&format!("    \"{}\" -> \"{}\";\n", dep, task.name));
            }
            // also show service dependencies with dashed lines
            for svc in &task.service_deps {
                dot.push_str(&format!(
                    "    \"{}\" -> \"{}\" [style=dashed, color=\"#ff9900\"];\n",
                    svc, task.name
                ));
            }
        }

        dot.push_str("}\n");
        dot
    }

    /// render ASCII representation of the graph
    pub fn to_ascii(&self) -> String {
        let groups = match self.parallel_groups() {
            Ok(g) => g,
            Err(_) => return "Error: cycle detected".to_string(),
        };

        let mut output = String::new();

        // helper to get display label with type prefix
        let get_label = |task: &Task| -> String {
            let prefix = if task.is_join() {
                "◇ "
            } else if task.k8s.is_some() {
                match task.k8s.as_ref().map(|k| &k.mode) {
                    Some(crate::config::K8sMode::Job) => "☸job ",
                    Some(crate::config::K8sMode::Exec) => "☸exec ",
                    Some(crate::config::K8sMode::Apply) => "☸apply ",
                    None => "☸ ",
                }
            } else if task.ssh.is_some() {
                "⚡ "
            } else if task.service.is_some() {
                "● "
            } else {
                ""
            };
            format!("{}{}", prefix, task.name)
        };

        let max_width = groups
            .iter()
            .flat_map(|g| g.iter().map(|t| get_label(t).len()))
            .max()
            .unwrap_or(10)
            .max(10);

        // print legend
        output.push_str("Legend: ⚡=SSH  ☸=K8s  ●=Service  ◇=Join\n\n");

        for (i, group) in groups.iter().enumerate() {
            if i > 0 {
                // draw connectors from previous level
                let connector_line: String = group
                    .iter()
                    .map(|_| format!("{:^width$}", "│", width = max_width + 4))
                    .collect::<Vec<_>>()
                    .join("");
                output.push_str(&connector_line);
                output.push('\n');

                let arrow_line: String = group
                    .iter()
                    .map(|_| format!("{:^width$}", "▼", width = max_width + 4))
                    .collect::<Vec<_>>()
                    .join("");
                output.push_str(&arrow_line);
                output.push('\n');
            }

            // draw boxes for this level
            let top_line: String = group
                .iter()
                .map(|_| format!("┌{:─^width$}┐  ", "", width = max_width))
                .collect();
            output.push_str(&top_line);
            output.push('\n');

            let name_line: String = group
                .iter()
                .map(|t| format!("│{:^width$}│  ", get_label(t), width = max_width))
                .collect();
            output.push_str(&name_line);
            output.push('\n');

            let bottom_line: String = group
                .iter()
                .map(|_| format!("└{:─^width$}┘  ", "", width = max_width))
                .collect();
            output.push_str(&bottom_line);
            output.push('\n');
        }

        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, DotenvSettings, Task};

    fn make_task(name: &str, run: &str, depends_on: Vec<&str>) -> Task {
        Task {
            name: name.to_string(),
            run: Some(run.to_string()),
            depends_on: depends_on.into_iter().map(String::from).collect(),
            service_deps: vec![],
            pipe_from: vec![],
            timeout: None,
            retry: 0,
            join: false,
            ssh: None,
            k8s: None,
            service: None,
            shebang: None,
        }
    }

    #[test]
    fn test_parallel_groups() {
        let mut tasks = HashMap::new();
        tasks.insert("a".to_string(), make_task("a", "echo a", vec![]));
        tasks.insert("b".to_string(), make_task("b", "echo b", vec![]));
        tasks.insert("c".to_string(), make_task("c", "echo c", vec!["a", "b"]));

        let config = Config { tasks, dotenv: DotenvSettings::default() };
        let graph = TaskGraph::from_config(config).unwrap();
        let groups = graph.parallel_groups().unwrap();

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 2); // a and b run in parallel
        assert_eq!(groups[1].len(), 1); // c runs after
    }

    #[test]
    fn test_cycle_detection() {
        let mut tasks = HashMap::new();
        tasks.insert("a".to_string(), make_task("a", "echo a", vec!["b"]));
        tasks.insert("b".to_string(), make_task("b", "echo b", vec!["a"]));

        let config = Config { tasks, dotenv: DotenvSettings::default() };
        let result = TaskGraph::from_config(config);
        assert!(matches!(result, Err(DagError::CycleDetected)));
    }
}
