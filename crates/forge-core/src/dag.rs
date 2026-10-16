//! Dependency graph (DAG) utilities: topological ordering and cycle
//! detection. Edges are stored as `task -> dependency`.

use std::collections::{BTreeMap, BTreeSet};

pub type NodeId = i64;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DagError {
    #[error("dependency cycle involving nodes {0:?}")]
    Cycle(Vec<NodeId>),
}

#[derive(Debug, Clone, Default)]
pub struct Dag {
    /// node -> set of nodes it depends on.
    adjacency: BTreeMap<NodeId, BTreeSet<NodeId>>,
}

impl Dag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_node(&mut self, id: NodeId) {
        self.adjacency.entry(id).or_default();
    }

    /// Insert an edge `task -> depends_on`.
    pub fn add_edge(&mut self, task: NodeId, depends_on: NodeId) {
        self.adjacency.entry(task).or_default().insert(depends_on);
        self.adjacency.entry(depends_on).or_default();
    }

    pub fn nodes(&self) -> Vec<NodeId> {
        self.adjacency.keys().copied().collect()
    }

    pub fn deps_of(&self, id: NodeId) -> BTreeSet<NodeId> {
        self.adjacency.get(&id).cloned().unwrap_or_default()
    }

    /// Returns `Some(nodes_in_cycle)` if the graph contains a cycle.
    pub fn detect_cycle(&self) -> Option<Vec<NodeId>> {
        match self.topological_order() {
            Ok(_) => None,
            Err(DagError::Cycle(nodes)) => Some(nodes),
        }
    }

    /// Kahn's algorithm. Dependencies are emitted before their dependents.
    /// Deterministic (BTree collections).
    pub fn topological_order(&self) -> Result<Vec<NodeId>, DagError> {
        let mut indegree: BTreeMap<NodeId, usize> = BTreeMap::new();
        for (node, deps) in &self.adjacency {
            indegree.insert(*node, deps.len());
        }
        // Reverse adjacency: dependency -> dependents.
        let mut dependents: BTreeMap<NodeId, BTreeSet<NodeId>> = BTreeMap::new();
        for (node, deps) in &self.adjacency {
            for dep in deps {
                dependents.entry(*dep).or_default().insert(*node);
            }
        }
        let mut ready: BTreeSet<NodeId> = indegree
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(n, _)| *n)
            .collect();
        let mut order = Vec::with_capacity(indegree.len());
        while let Some(node) = ready.pop_first() {
            order.push(node);
            if let Some(ds) = dependents.get(&node) {
                for d in ds {
                    let e = indegree.get_mut(d).expect("known node");
                    *e -= 1;
                    if *e == 0 {
                        ready.insert(*d);
                    }
                }
            }
        }
        if order.len() != indegree.len() {
            let cycle: Vec<NodeId> = indegree
                .iter()
                .filter(|(_, d)| **d > 0)
                .map(|(n, _)| *n)
                .collect();
            return Err(DagError::Cycle(cycle));
        }
        Ok(order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_graph_is_ok() {
        let dag = Dag::new();
        assert_eq!(dag.topological_order().unwrap(), Vec::<NodeId>::new());
        assert!(dag.detect_cycle().is_none());
    }

    #[test]
    fn linear_chain_order() {
        let mut dag = Dag::new();
        dag.add_edge(3, 2);
        dag.add_edge(2, 1);
        let order = dag.topological_order().unwrap();
        let pos = |n: NodeId| order.iter().position(|x| *x == n).unwrap();
        assert!(pos(1) < pos(2));
        assert!(pos(2) < pos(3));
    }

    #[test]
    fn diamond_order_is_valid() {
        // 1 -> 2, 1 -> 3, 2 -> 4, 3 -> 4 (dependencies: 4 depends on 2 and 3)
        let mut dag = Dag::new();
        dag.add_edge(2, 1);
        dag.add_edge(3, 1);
        dag.add_edge(4, 2);
        dag.add_edge(4, 3);
        let order = dag.topological_order().unwrap();
        let pos = |n: NodeId| order.iter().position(|x| *x == n).unwrap();
        assert!(pos(1) < pos(2) && pos(1) < pos(3));
        assert!(pos(2) < pos(4) && pos(3) < pos(4));
    }

    #[test]
    fn cycle_detected_and_reported() {
        let mut dag = Dag::new();
        dag.add_edge(1, 2);
        dag.add_edge(2, 3);
        dag.add_edge(3, 1);
        let cycle = dag.detect_cycle().expect("cycle");
        assert_eq!(cycle, vec![1, 2, 3]);
        assert!(matches!(dag.topological_order(), Err(DagError::Cycle(_))));
    }

    #[test]
    fn self_loop_is_cycle() {
        let mut dag = Dag::new();
        dag.add_edge(1, 1);
        assert!(dag.detect_cycle().is_some());
    }

    #[test]
    fn deterministic_order() {
        let mut dag = Dag::new();
        for (a, b) in [(5, 1), (4, 1), (3, 2), (2, 1)] {
            dag.add_edge(a, b);
        }
        let first = dag.topological_order().unwrap();
        let second = dag.topological_order().unwrap();
        assert_eq!(first, second);
    }
}
