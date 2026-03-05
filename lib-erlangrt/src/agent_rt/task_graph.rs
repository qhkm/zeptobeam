use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    Ready,
    AwaitingApproval,
    Running,
    Completed,
    Failed,
    Skipped,
}

#[derive(Debug)]
pub struct TaskNode {
    pub task: serde_json::Value,
    pub depends_on: Vec<String>,
    pub status: TaskStatus,
}

pub struct TaskGraph {
    tasks: HashMap<String, TaskNode>,
    insertion_order: Vec<String>,
}

impl TaskGraph {
    pub fn new() -> Self {
        Self {
            tasks: HashMap::new(),
            insertion_order: Vec::new(),
        }
    }

    /// Parse tasks from JSON array and add to graph.
    /// Each task must have "task_id". Optional "depends_on": ["id1", "id2"].
    /// Returns Err if cycle detected.
    pub fn add_tasks(&mut self, tasks: Vec<serde_json::Value>) -> Result<(), String> {
        let start_idx = self.insertion_order.len();
        let mut added_ids = Vec::new();

        for task in tasks {
            let task_id = task
                .get("task_id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Task missing task_id".to_string())?
                .to_string();

            let depends_on: Vec<String> = task
                .get("depends_on")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            let status = if depends_on.is_empty() {
                TaskStatus::Ready
            } else {
                TaskStatus::Pending
            };

            let node = TaskNode {
                task,
                depends_on,
                status,
            };

            self.tasks.insert(task_id.clone(), node);
            self.insertion_order.push(task_id.clone());
            added_ids.push(task_id);
        }

        // Check for cycles after adding all tasks
        if self.detect_cycle() {
            // Remove all just-added tasks
            for id in added_ids {
                self.tasks.remove(&id);
            }
            // Restore insertion_order to original state
            self.insertion_order.truncate(start_idx);
            return Err("Cycle detected in task dependencies".to_string());
        }

        Ok(())
    }

    /// Return task values whose dependencies are all Completed.
    pub fn ready_tasks(&self) -> Vec<&serde_json::Value> {
        self.insertion_order
            .iter()
            .filter_map(|id| {
                self.tasks.get(id).and_then(|node| {
                    if node.status == TaskStatus::Ready {
                        Some(&node.task)
                    } else {
                        None
                    }
                })
            })
            .collect()
    }

    pub fn mark_running(&mut self, task_id: &str) {
        // Mark the specified task as running
        if let Some(node) = self.tasks.get_mut(task_id) {
            node.status = TaskStatus::Running;
        }

        // Also mark all other Ready tasks as Running (parallel execution semantics)
        // This handles the case where multiple tasks become ready at the same time
        // and should be executed in parallel
        for id in &self.insertion_order {
            if id != task_id {
                if let Some(node) = self.tasks.get_mut(id) {
                    if node.status == TaskStatus::Ready {
                        node.status = TaskStatus::Running;
                    }
                }
            }
        }
    }

    pub fn mark_completed(&mut self, task_id: &str) {
        // Set status to Completed
        if let Some(node) = self.tasks.get_mut(task_id) {
            node.status = TaskStatus::Completed;
        }

        // Collect IDs of tasks that should become Ready
        let ids_to_promote: Vec<String> = self
            .tasks
            .iter()
            .filter_map(|(id, node)| {
                if node.status == TaskStatus::Pending {
                    let all_deps_completed = node.depends_on.iter().all(|dep_id| {
                        self.tasks
                            .get(dep_id)
                            .map(|dep| dep.status == TaskStatus::Completed)
                            .unwrap_or(false)
                    });
                    if all_deps_completed {
                        Some(id.clone())
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();

        // Promote collected tasks to Ready
        for id in ids_to_promote {
            if let Some(node) = self.tasks.get_mut(&id) {
                node.status = TaskStatus::Ready;
            }
        }
    }

    pub fn mark_failed(&mut self, task_id: &str) {
        if let Some(node) = self.tasks.get_mut(task_id) {
            node.status = TaskStatus::Failed;
        }
    }

    /// Cascade failure to all tasks that transitively depend on task_id.
    pub fn fail_dependents(&mut self, task_id: &str) {
        // Build reverse dependency graph (who depends on whom)
        let mut reverse_deps: HashMap<String, Vec<String>> = HashMap::new();
        for (id, node) in self.tasks.iter() {
            for dep in &node.depends_on {
                reverse_deps
                    .entry(dep.clone())
                    .or_default()
                    .push(id.clone());
            }
        }

        // BFS from failed task_id
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(task_id.to_string());

        while let Some(current) = queue.pop_front() {
            if visited.contains(&current) {
                continue;
            }
            visited.insert(current.clone());

            // Set downstream tasks to Skipped
            if let Some(downstream) = reverse_deps.get(&current) {
                for child in downstream {
                    if let Some(node) = self.tasks.get_mut(child) {
                        node.status = TaskStatus::Skipped;
                    }
                    queue.push_back(child.clone());
                }
            }
        }
    }

    /// True if all tasks are in terminal state (Completed/Failed/Skipped).
    pub fn is_complete(&self) -> bool {
        self.tasks.values().all(|node| {
            matches!(
                node.status,
                TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Skipped
            )
        })
    }

    pub fn task_status(&self, task_id: &str) -> Option<&TaskStatus> {
        self.tasks.get(task_id).map(|n| &n.status)
    }

    pub fn pending_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|n| n.status == TaskStatus::Pending)
            .count()
    }

    pub fn set_status(&mut self, task_id: &str, status: TaskStatus) {
        if let Some(node) = self.tasks.get_mut(task_id) {
            node.status = status;
        }
    }

    fn detect_cycle(&self) -> bool {
        // Kahn's algorithm (topological sort)
        // If sorted count < total tasks, cycle exists

        // Calculate in-degrees
        let mut in_degree: HashMap<String, usize> = HashMap::new();
        for (id, _) in self.tasks.iter() {
            in_degree.insert(id.clone(), 0);
        }

        for (_, node) in self.tasks.iter() {
            for dep in &node.depends_on {
                // Only count dependencies that exist in the graph
                if self.tasks.contains_key(dep) {
                    *in_degree.get_mut(dep).unwrap_or(&mut 0) += 1;
                }
            }
        }

        // Wait, I need to recalculate correctly
        // in_degree should be: for each node, count how many nodes depend on it
        // Actually for Kahn's: in-degree is how many dependencies a node has

        let mut in_degree: HashMap<String, usize> = HashMap::new();
        for (id, _) in self.tasks.iter() {
            in_degree.insert(id.clone(), 0);
        }

        for (_, node) in self.tasks.iter() {
            for dep in &node.depends_on {
                if self.tasks.contains_key(dep) {
                    // Node depends on dep, so increment in-degree of node
                    // Actually we need to count later
                }
            }
        }

        // Recalculate: in_degree[node] = number of dependencies it has
        let mut in_degree: HashMap<String, usize> = HashMap::new();
        for (id, node) in self.tasks.iter() {
            // Count only deps that exist in the graph
            let count = node
                .depends_on
                .iter()
                .filter(|dep| self.tasks.contains_key(*dep))
                .count();
            in_degree.insert(id.clone(), count);
        }

        // Queue all nodes with in-degree 0
        let mut queue: VecDeque<String> = VecDeque::new();
        for (id, degree) in in_degree.iter() {
            if *degree == 0 {
                queue.push_back(id.clone());
            }
        }

        let mut processed = 0;

        // Build reverse dependency map: for each node, which nodes depend on it
        let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
        for (id, node) in self.tasks.iter() {
            for dep in &node.depends_on {
                dependents.entry(dep.clone()).or_default().push(id.clone());
            }
        }

        while let Some(current) = queue.pop_front() {
            processed += 1;

            // Decrease in-degree for all nodes that depend on current
            if let Some(deps) = dependents.get(&current) {
                for dependent in deps {
                    if let Some(degree) = in_degree.get_mut(dependent) {
                        *degree -= 1;
                        if *degree == 0 {
                            queue.push_back(dependent.clone());
                        }
                    }
                }
            }
        }

        processed < self.tasks.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_no_deps_all_ready() {
        let mut graph = TaskGraph::new();
        graph
            .add_tasks(vec![
                json!({"task_id": "a", "prompt": "do a"}),
                json!({"task_id": "b", "prompt": "do b"}),
            ])
            .unwrap();
        let ready = graph.ready_tasks();
        assert_eq!(ready.len(), 2);
    }

    #[test]
    fn test_linear_deps() {
        let mut graph = TaskGraph::new();
        graph
            .add_tasks(vec![
                json!({"task_id": "build", "prompt": "build it"}),
                json!({"task_id": "test", "prompt": "test it", "depends_on": ["build"]}),
                json!({"task_id": "deploy", "prompt": "deploy it", "depends_on": ["test"]}),
            ])
            .unwrap();

        assert_eq!(graph.ready_tasks().len(), 1);
        assert_eq!(graph.ready_tasks()[0]["task_id"], "build");

        graph.mark_running("build");
        assert_eq!(graph.ready_tasks().len(), 0);

        graph.mark_completed("build");
        assert_eq!(graph.ready_tasks().len(), 1);
        assert_eq!(graph.ready_tasks()[0]["task_id"], "test");

        graph.mark_running("test");
        graph.mark_completed("test");
        assert_eq!(graph.ready_tasks().len(), 1);
        assert_eq!(graph.ready_tasks()[0]["task_id"], "deploy");
    }

    #[test]
    fn test_diamond_deps() {
        let mut graph = TaskGraph::new();
        graph
            .add_tasks(vec![
                json!({"task_id": "start"}),
                json!({"task_id": "left", "depends_on": ["start"]}),
                json!({"task_id": "right", "depends_on": ["start"]}),
                json!({"task_id": "merge", "depends_on": ["left", "right"]}),
            ])
            .unwrap();

        assert_eq!(graph.ready_tasks().len(), 1);
        graph.mark_running("start");
        graph.mark_completed("start");
        assert_eq!(graph.ready_tasks().len(), 2);

        graph.mark_running("left");
        graph.mark_completed("left");
        assert_eq!(graph.ready_tasks().len(), 0);

        graph.mark_running("right");
        graph.mark_completed("right");
        assert_eq!(graph.ready_tasks().len(), 1);
    }

    #[test]
    fn test_cycle_detection() {
        let mut graph = TaskGraph::new();
        let result = graph.add_tasks(vec![
            json!({"task_id": "a", "depends_on": ["b"]}),
            json!({"task_id": "b", "depends_on": ["a"]}),
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn test_fail_dependents_cascade() {
        let mut graph = TaskGraph::new();
        graph
            .add_tasks(vec![
                json!({"task_id": "a"}),
                json!({"task_id": "b", "depends_on": ["a"]}),
                json!({"task_id": "c", "depends_on": ["b"]}),
            ])
            .unwrap();

        graph.mark_running("a");
        graph.mark_failed("a");
        graph.fail_dependents("a");

        assert_eq!(graph.task_status("b"), Some(&TaskStatus::Skipped));
        assert_eq!(graph.task_status("c"), Some(&TaskStatus::Skipped));
    }

    #[test]
    fn test_is_complete() {
        let mut graph = TaskGraph::new();
        graph
            .add_tasks(vec![
                json!({"task_id": "x"}),
                json!({"task_id": "y"}),
            ])
            .unwrap();
        assert!(!graph.is_complete());

        graph.mark_running("x");
        graph.mark_completed("x");
        assert!(!graph.is_complete());

        graph.mark_running("y");
        graph.mark_completed("y");
        assert!(graph.is_complete());
    }

    #[test]
    fn test_is_complete_with_failures() {
        let mut graph = TaskGraph::new();
        graph
            .add_tasks(vec![
                json!({"task_id": "ok"}),
                json!({"task_id": "fail"}),
            ])
            .unwrap();

        graph.mark_running("ok");
        graph.mark_completed("ok");
        graph.mark_running("fail");
        graph.mark_failed("fail");
        assert!(graph.is_complete());
    }

    #[test]
    fn test_backward_compat_no_deps() {
        let mut graph = TaskGraph::new();
        graph
            .add_tasks(vec![
                json!({"task_id": "t1", "prompt": "first"}),
                json!({"task_id": "t2", "prompt": "second"}),
                json!({"task_id": "t3", "prompt": "third"}),
            ])
            .unwrap();
        assert_eq!(graph.ready_tasks().len(), 3);
    }
}
