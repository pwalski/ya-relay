use chrono::Utc;
use itertools::Itertools;
use std::collections::HashMap;

use crate::error::{InternalError, ServerResult, Unauthorized};
use crate::session::{NodeSession, SessionId};

use ya_client_model::NodeId;

pub struct NodesState {
    /// Constant time access using slot id optimized for forwarding.
    /// The consequence is, that we must store Option<NodeSession>, because
    /// we can't move elements after removal.
    slots: Vec<Option<NodeSession>>,
    sessions: HashMap<SessionId, u32>,
    nodes: HashMap<NodeId, u32>,
}

impl NodesState {
    pub fn new() -> NodesState {
        NodesState {
            slots: vec![],
            sessions: Default::default(),
            nodes: Default::default(),
        }
    }

    pub fn register(&mut self, mut node: NodeSession) {
        let slot = self.empty_slot();

        if slot as usize >= self.slots.len() {
            self.slots.resize(self.slots.len() + 1024, None);
        }

        self.sessions.insert(node.session, slot);
        self.nodes.insert(node.info.node_id, slot);

        node.info.slot = slot;

        self.slots[slot as usize] = Some(node);
    }

    pub fn neighbours(&self, id: SessionId, count: u32) -> ServerResult<Vec<NodeSession>> {
        let slot = *self
            .sessions
            .get(&id)
            .ok_or(Unauthorized::SessionNotFound(id))?;

        let ref_node_id = self.slots[slot as usize]
            .clone()
            .ok_or(InternalError::GettingSessionInfo(id))?
            .info
            .node_id;

        // Sort all nodes by hamming distance between node ids (number of differing bits).
        // Neighbourhood of each node should differ as much as possible, because
        // when it will be used for broadcasts, messages should reach whole network
        // with as low number of steps as possible.
        let neighbours: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| entry.as_ref().map(|entry| (idx, entry.info.node_id)))
            .sorted_by(|(_, id1), (_, id2)| {
                Ord::cmp(
                    &hamming_distance(*id1, ref_node_id),
                    &hamming_distance(*id2, ref_node_id),
                )
            })
            .map(|(idx, _)| idx)
            .collect();

        // First node will be always the node for which we are computing neighbourhood, because
        // it has hamming distance 0 from himself.
        let count = std::cmp::min(neighbours.len() - 1, count as usize);
        let neighbours = neighbours[1..=count]
            .iter()
            .filter_map(|&slot| self.slots[slot].clone())
            .collect();

        Ok(neighbours)
    }

    pub fn update_seen(&mut self, id: SessionId) -> ServerResult<()> {
        match self.sessions.get(&id) {
            None => return Err(Unauthorized::SessionNotFound(id).into()),
            Some(&slot) => match self.slots.get_mut(slot as usize) {
                Some(Some(node)) => node.last_seen = Utc::now(),
                _ => return Err(InternalError::GettingSessionInfo(id).into()),
            },
        };
        Ok(())
    }

    pub fn get_by_slot(&self, slot: u32) -> Option<NodeSession> {
        self.slots.get(slot as usize).cloned().flatten()
    }

    pub fn get_by_session(&self, id: SessionId) -> Option<NodeSession> {
        match self.sessions.get(&id) {
            None => None,
            Some(&slot) => self.slots.get(slot as usize).cloned().flatten(),
        }
    }

    pub fn get_by_node_id(&self, id: NodeId) -> Option<NodeSession> {
        match self.nodes.get(&id) {
            None => None,
            Some(&slot) => self.slots.get(slot as usize).cloned().flatten(),
        }
    }

    fn empty_slot(&self) -> u32 {
        match self.slots.iter().position(|slot| slot.is_none()) {
            None => self.slots.len() as u32,
            Some(idx) => idx as u32,
        }
    }
}

impl Default for NodesState {
    fn default() -> Self {
        Self::new()
    }
}

pub fn hamming_distance(id1: NodeId, id2: NodeId) -> u32 {
    let id1 = id1.into_array();
    let id2 = id2.into_array();

    let mut hamming = 0;
    for i in 0..id1.len() {
        // Count different bits
        hamming += (id1[i] ^ id2[i]).count_ones();
    }

    hamming
}
