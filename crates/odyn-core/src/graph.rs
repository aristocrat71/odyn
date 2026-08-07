//! The brain graph: nodes for memories, weighted edges for wikilinks,
//! similarity and co-injection, positions from a force layout. Cached in
//! `graph_cache` and invalidated by every sync or injection write. The recall
//! walk runs over these same edges: what the graph view draws is what
//! retrieval traverses.

use serde::{Deserialize, Serialize};

use crate::storage::{Storage, StorageError};

/// Neighbors examined per node when looking for similarity edges.
const NEIGHBORS: usize = 8;
const CO_INJECTION_MIN: i64 = 2;
/// A deliberate `[[link]]` outranks any derived edge: similarity tops out at
/// 1.0 and co-injection below it, so 1.25 keeps authored edges the strongest.
const LINK_WEIGHT: f32 = 1.25;
const ITERATIONS: usize = 300;
/// Positions land inside [-EXTENT, EXTENT] on both axes.
const EXTENT: f32 = 450.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphNode {
    pub id: i64,
    pub display_id: String,
    pub content: String,
    pub hits: i64,
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeKind {
    Link,
    Similarity,
    CoInjection,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphEdge {
    pub a: i64,
    pub b: i64,
    pub kind: EdgeKind,
    /// The walk's edge strength: links fixed at [`LINK_WEIGHT`], similarity
    /// the cosine itself, co-injection rising with the shared-use count.
    pub weight: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("could not encode the graph cache: {0}")]
    Encode(#[from] serde_json::Error),
}

/// The cached graph, or a freshly computed one. A cache that no longer parses
/// (an older Odyn wrote it) is recomputed, never an error.
pub fn brain_graph(storage: &Storage, similarity_threshold: f32) -> Result<Graph, GraphError> {
    if let Some(payload) = storage.cached_graph()? {
        if let Ok(graph) = serde_json::from_str::<Graph>(&payload) {
            return Ok(graph);
        }
    }
    let graph = compute(storage, similarity_threshold)?;
    storage.store_graph(&serde_json::to_string(&graph)?)?;
    Ok(graph)
}

fn compute(storage: &Storage, similarity_threshold: f32) -> Result<Graph, GraphError> {
    let stats = storage.stats_for(storage.list_memories()?)?;
    let mut nodes: Vec<GraphNode> = stats
        .into_iter()
        .map(|stats| GraphNode {
            id: stats.memory.id,
            display_id: stats.memory.display_id(),
            content: stats.memory.content,
            hits: stats.hits,
            x: 0.0,
            y: 0.0,
        })
        .collect();

    let mut edges = Vec::new();
    // Authored links first: two notes linking each other are still one edge.
    let mut linked = std::collections::HashSet::new();
    for (from, to) in storage.links()? {
        let (a, b) = (from.min(to), from.max(to));
        if linked.insert((a, b)) {
            edges.push(GraphEdge {
                a,
                b,
                kind: EdgeKind::Link,
                weight: LINK_WEIGHT,
            });
        }
    }
    for node in nodes.iter() {
        for (other, distance) in storage.neighbors(node.id, NEIGHBORS)? {
            // Embeddings are unit vectors, so cos = 1 - d²/2.
            let similarity = 1.0 - (distance * distance) as f32 / 2.0;
            if similarity >= similarity_threshold && node.id < other {
                edges.push(GraphEdge {
                    a: node.id,
                    b: other,
                    kind: EdgeKind::Similarity,
                    weight: similarity,
                });
            }
        }
    }
    for (a, b, count) in storage.co_injections(CO_INJECTION_MIN)? {
        edges.push(GraphEdge {
            a,
            b,
            kind: EdgeKind::CoInjection,
            // Rises with shared use, saturating short of any similarity edge.
            weight: count as f32 / (count as f32 + 2.0),
        });
    }

    layout(&mut nodes, &edges);
    Ok(Graph { nodes, edges })
}

/// Fruchterman–Reingold: all-pairs repulsion, springs on link and similarity
/// edges, gravity to the center, cooling over the run. Deterministic — seed
/// positions are a golden-angle spiral, not random — so the same brain always
/// draws the same map.
fn layout(nodes: &mut [GraphNode], edges: &[GraphEdge]) {
    let count = nodes.len();
    if count == 0 {
        return;
    }
    for (index, node) in nodes.iter_mut().enumerate() {
        let radius = (index as f32).sqrt() * 24.0;
        let angle = index as f32 * 2.399_963;
        node.x = radius * angle.cos();
        node.y = radius * angle.sin();
    }
    if count == 1 {
        return;
    }

    let index_of: std::collections::HashMap<i64, usize> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id, index))
        .collect();
    let springs: Vec<(usize, usize)> = edges
        .iter()
        .filter(|edge| matches!(edge.kind, EdgeKind::Similarity | EdgeKind::Link))
        .filter_map(|edge| Some((*index_of.get(&edge.a)?, *index_of.get(&edge.b)?)))
        .collect();

    let k = (4.0 * EXTENT * EXTENT / count as f32).sqrt();
    let mut temperature = EXTENT / 2.0;
    let cooling = temperature / ITERATIONS as f32;
    let mut shifts = vec![(0.0f32, 0.0f32); count];

    for _ in 0..ITERATIONS {
        shifts.fill((0.0, 0.0));
        for a in 0..count {
            for b in (a + 1)..count {
                let dx = nodes[a].x - nodes[b].x;
                let dy = nodes[a].y - nodes[b].y;
                let squared = (dx * dx + dy * dy).max(0.01);
                let push = k * k / squared;
                shifts[a].0 += dx * push;
                shifts[a].1 += dy * push;
                shifts[b].0 -= dx * push;
                shifts[b].1 -= dy * push;
            }
        }
        for &(a, b) in &springs {
            let dx = nodes[a].x - nodes[b].x;
            let dy = nodes[a].y - nodes[b].y;
            let distance = (dx * dx + dy * dy).sqrt().max(0.1);
            let pull = distance / k;
            shifts[a].0 -= dx * pull;
            shifts[a].1 -= dy * pull;
            shifts[b].0 += dx * pull;
            shifts[b].1 += dy * pull;
        }
        for (node, &(dx, dy)) in nodes.iter_mut().zip(&shifts) {
            // Gravity keeps disconnected islands from drifting off the map.
            let dx = dx - node.x * 0.05;
            let dy = dy - node.y * 0.05;
            let length = (dx * dx + dy * dy).sqrt().max(0.01);
            let step = length.min(temperature);
            node.x += dx / length * step;
            node.y += dy / length * step;
        }
        temperature = (temperature - cooling).max(1.0);
    }

    // Fit into the extent so the frontend's initial view always frames it.
    let widest = nodes
        .iter()
        .map(|node| node.x.abs().max(node.y.abs()))
        .fold(1.0f32, f32::max);
    let scale = EXTENT / widest;
    for node in nodes {
        node.x = (node.x * scale * 10.0).round() / 10.0;
        node.y = (node.y * scale * 10.0).round() / 10.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::memory_tests::note_with_links;
    use crate::storage::tests::TempDir;
    use crate::storage::Storage;

    fn vector(axis: usize, lean: f32) -> Vec<f32> {
        let mut values = vec![0.0f32; crate::embed::FAKE_DIM];
        values[axis] = 1.0 - lean;
        values[axis + 1] = lean;
        let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
        for value in &mut values {
            *value /= norm;
        }
        values
    }

    /// Two notes close in meaning, one far, with a link far→close-a.
    fn seeded(label: &str) -> (TempDir, Storage) {
        let dir = TempDir::new(label);
        let storage = Storage::open(dir.db()).expect("open");
        let notes = vec![
            note_with_links("close-a", "close a", &[]),
            note_with_links("close-b", "close b", &[]),
            note_with_links("far-away", "far, see [[close-a]]", &["close-a"]),
        ];
        let embeddings = vec![
            ("close-a".to_string(), vector(0, 0.0)),
            ("close-b".to_string(), vector(0, 0.1)),
            ("far-away".to_string(), vector(100, 0.0)),
        ];
        storage.sync_notes(&notes, &embeddings).expect("sync");
        (dir, storage)
    }

    #[test]
    fn link_similarity_and_co_injection_edges_come_out_of_the_data() {
        let (_dir, storage) = seeded("edges");
        let conversation = storage
            .create_conversation("g", "ollama", "llama3.2:3b")
            .expect("create");
        for message in ["one", "two"] {
            let row = storage
                .append_message(
                    conversation.id,
                    crate::chat::Role::User,
                    message,
                    None,
                    None,
                )
                .expect("message");
            storage
                .record_injections(conversation.id, Some(row.id), &[1, 3])
                .expect("inject");
        }

        let graph = brain_graph(&storage, 0.78).expect("graph");
        assert_eq!(graph.nodes.len(), 3);
        let close_a = graph.nodes.iter().find(|node| node.id == 1).expect("a");
        assert_eq!(close_a.display_id, "close-a");
        assert_eq!(close_a.hits, 2);

        let by_kind = |kind: EdgeKind| -> Vec<(i64, i64, f32)> {
            graph
                .edges
                .iter()
                .filter(|edge| edge.kind == kind)
                .map(|edge| (edge.a, edge.b, edge.weight))
                .collect()
        };
        let links = by_kind(EdgeKind::Link);
        assert_eq!(links.len(), 1);
        assert_eq!((links[0].0, links[0].1), (1, 3));
        assert!((links[0].2 - LINK_WEIGHT).abs() < 1e-6);
        let similar = by_kind(EdgeKind::Similarity);
        assert_eq!(similar.len(), 1);
        assert_eq!((similar[0].0, similar[0].1), (1, 2));
        assert!(similar[0].2 >= 0.78 && similar[0].2 <= 1.0, "{similar:?}");
        // Memories 1 and 3 were injected together twice: weight 2/(2+2).
        let co = by_kind(EdgeKind::CoInjection);
        assert_eq!(co.len(), 1);
        assert_eq!((co[0].0, co[0].1), (1, 3));
        assert!((co[0].2 - 0.5).abs() < 1e-6);

        let mut positions: Vec<(i32, i32)> = graph
            .nodes
            .iter()
            .map(|node| {
                assert!(node.x.is_finite() && node.y.is_finite());
                assert!(node.x.abs() <= 451.0 && node.y.abs() <= 451.0, "{node:?}");
                ((node.x * 10.0) as i32, (node.y * 10.0) as i32)
            })
            .collect();
        positions.sort_unstable();
        positions.dedup();
        assert_eq!(positions.len(), 3, "nodes must not stack");
    }

    #[test]
    fn the_graph_is_cached_until_a_write_invalidates_it() {
        let (_dir, storage) = seeded("cache");
        let first = brain_graph(&storage, 0.78).expect("first");
        assert!(storage.cached_graph().expect("cached").is_some());
        let second = brain_graph(&storage, 0.78).expect("second");
        assert_eq!(first, second, "the cache answers unchanged");

        let notes = vec![
            note_with_links("close-a", "close a", &[]),
            note_with_links("close-b", "close b", &[]),
            note_with_links("far-away", "far, see [[close-a]]", &["close-a"]),
            note_with_links("new-arrival", "new", &[]),
        ];
        let embeddings = vec![("new-arrival".to_string(), vector(50, 0.0))];
        storage.sync_notes(&notes, &embeddings).expect("sync");
        assert!(
            storage.cached_graph().expect("cached").is_none(),
            "a sync must clear the cache"
        );
        let third = brain_graph(&storage, 0.78).expect("third");
        assert_eq!(third.nodes.len(), 4, "the new memory appears on refresh");
    }

    /// An old cache payload — edges without weights — must recompute, not error.
    #[test]
    fn an_unparseable_cache_is_recomputed() {
        let (_dir, storage) = seeded("stale-cache");
        storage
            .store_graph(r#"{"nodes":[],"edges":[{"a":1,"b":2,"kind":"similarity"}]}"#)
            .expect("plant an old payload");
        let graph = brain_graph(&storage, 0.78).expect("recompute");
        assert_eq!(graph.nodes.len(), 3);
    }
}
