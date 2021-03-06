use crate::common::cluster::{
    Cluster, MigrationMeta, MigrationTaskMeta, Node, PeerProxy, Proxy, Range, RangeList, ReplMeta,
    ReplPeer, SlotRange, SlotRangeTag,
};
use crate::common::cluster::{DBName, Role};
use crate::common::config::ClusterConfig;
use crate::common::utils::SLOT_NUM;
use chrono::{DateTime, NaiveDateTime, Utc};
use itertools::Itertools;
use std::cmp::min;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;

const NODES_PER_PROXY: usize = 2;
const CHUNK_PARTS: usize = 2;
pub const CHUNK_HALF_NODE_NUM: usize = 2;
const CHUNK_NODE_NUM: usize = 4;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProxyResource {
    pub proxy_address: String,
    pub node_addresses: [String; NODES_PER_PROXY],
}

type ProxySlot = String;

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
enum ChunkRolePosition {
    Normal,
    FirstChunkMaster,
    SecondChunkMaster,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
struct MigrationSlotRangeStore {
    range_list: RangeList,
    is_migrating: bool, // migrating or importing
    meta: MigrationMetaStore,
}

impl MigrationSlotRangeStore {
    fn to_slot_range(&self, chunks: &[ChunkStore]) -> SlotRange {
        let src_chunk = chunks.get(self.meta.src_chunk_index).expect("get_cluster");
        let src_proxy_address = src_chunk
            .proxy_addresses
            .get(self.meta.src_chunk_part)
            .expect("get_cluster")
            .clone();
        let src_node_index =
            Self::chunk_part_to_node_index(self.meta.src_chunk_part, src_chunk.role_position);
        let src_node_address = src_chunk
            .node_addresses
            .get(src_node_index)
            .expect("get_cluster")
            .clone();

        let dst_chunk = chunks.get(self.meta.dst_chunk_index).expect("get_cluster");
        let dst_proxy_address = dst_chunk
            .proxy_addresses
            .get(self.meta.dst_chunk_part)
            .expect("get_cluster")
            .clone();
        let dst_node_index =
            Self::chunk_part_to_node_index(self.meta.dst_chunk_part, dst_chunk.role_position);
        let dst_node_address = dst_chunk
            .node_addresses
            .get(dst_node_index)
            .expect("get_cluster")
            .clone();

        let meta = MigrationMeta {
            epoch: self.meta.epoch,
            src_proxy_address,
            src_node_address,
            dst_proxy_address,
            dst_node_address,
        };
        if self.is_migrating {
            SlotRange {
                range_list: self.range_list.clone(),
                tag: SlotRangeTag::Migrating(meta),
            }
        } else {
            SlotRange {
                range_list: self.range_list.clone(),
                tag: SlotRangeTag::Importing(meta),
            }
        }
    }

    fn chunk_part_to_node_index(chunk_part: usize, role_position: ChunkRolePosition) -> usize {
        match (chunk_part, role_position) {
            (0, ChunkRolePosition::SecondChunkMaster) => 3,
            (1, ChunkRolePosition::FirstChunkMaster) => 2,
            (i, _) => 2 * i,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
struct MigrationMetaStore {
    epoch: u64,
    src_chunk_index: usize,
    src_chunk_part: usize,
    dst_chunk_index: usize,
    dst_chunk_part: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ChunkStore {
    role_position: ChunkRolePosition,
    stable_slots: [Option<SlotRange>; CHUNK_PARTS],
    migrating_slots: [Vec<MigrationSlotRangeStore>; CHUNK_PARTS],
    proxy_addresses: [String; CHUNK_PARTS],
    node_addresses: [String; CHUNK_NODE_NUM],
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ClusterStore {
    name: DBName,
    chunks: Vec<ChunkStore>,
    config: ClusterConfig,
}

#[derive(Debug)]
struct MigrationSlots {
    ranges: Vec<Range>,
    meta: MigrationMetaStore,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct MetaStore {
    global_epoch: u64,
    cluster: Option<ClusterStore>,
    // proxy_address => nodes and cluster_name
    all_proxies: HashMap<String, ProxyResource>,
    // proxy addresses
    failed_proxies: HashSet<String>,
    // failed_proxy_address => reporter_id => time,
    failures: HashMap<String, HashMap<String, i64>>,
}

impl Default for MetaStore {
    fn default() -> Self {
        Self {
            global_epoch: 0,
            cluster: None,
            all_proxies: HashMap::new(),
            failed_proxies: HashSet::new(),
            failures: HashMap::new(),
        }
    }
}

impl MetaStore {
    pub fn get_global_epoch(&self) -> u64 {
        self.global_epoch
    }

    pub fn bump_global_epoch(&mut self) -> u64 {
        self.global_epoch += 1;
        self.global_epoch
    }

    pub fn get_proxies(&self) -> Vec<String> {
        self.all_proxies.keys().cloned().collect()
    }

    pub fn get_proxy_by_address(&self, address: &str) -> Option<Proxy> {
        let all_nodes = &self.all_proxies;
        let cluster_opt = self.get_cluster();

        let node_resource = all_nodes.get(address)?;

        let cluster = match cluster_opt {
            Some(cluster) => cluster,
            None => {
                return Some(Proxy::new(
                    address.to_string(),
                    self.global_epoch,
                    vec![],
                    node_resource.node_addresses.to_vec(),
                    vec![],
                    HashMap::new(),
                ));
            }
        };

        let cluster_name = cluster.get_name().clone();
        let epoch = self.global_epoch;
        let nodes: Vec<Node> = cluster
            .get_nodes()
            .iter()
            .filter(|node| node.get_proxy_address() == address)
            .cloned()
            .collect();

        let (peers, free_nodes) = if nodes.is_empty() {
            let free_nodes = node_resource.node_addresses.to_vec();
            (vec![], free_nodes)
        } else {
            let peers = cluster
                .get_nodes()
                .iter()
                .filter(|n| n.get_role() == Role::Master && n.get_proxy_address() != address)
                .cloned()
                .group_by(|node| node.get_proxy_address().to_string())
                .into_iter()
                .map(|(proxy_address, nodes)| {
                    // Collect all slots from masters.
                    let slots = nodes.map(Node::into_slots).flatten().collect();
                    PeerProxy {
                        proxy_address,
                        cluster_name: cluster_name.clone(),
                        slots,
                    }
                })
                .collect();
            (peers, vec![])
        };
        let proxy = Proxy::new(
            address.to_string(),
            epoch,
            nodes,
            free_nodes,
            peers,
            HashMap::new(),
        );
        Some(proxy)
    }

    pub fn get_cluster_names(&self) -> Vec<DBName> {
        match &self.cluster {
            Some(cluster_store) => vec![cluster_store.name.clone()],
            None => vec![],
        }
    }

    pub fn get_cluster_by_name(&self, db_name: &str) -> Option<Cluster> {
        let db_name = DBName::from(&db_name).ok()?;
        let cluster_store = self.cluster.as_ref()?;
        if cluster_store.name != db_name {
            return None;
        }

        self.get_cluster()
    }

    fn get_cluster(&self) -> Option<Cluster> {
        let cluster_store = self.cluster.as_ref()?;
        let cluster_name = cluster_store.name.clone();

        let nodes = cluster_store
            .chunks
            .iter()
            .map(|chunk| {
                let mut nodes = vec![];
                for i in 0..CHUNK_NODE_NUM {
                    let address = chunk
                        .node_addresses
                        .get(i)
                        .expect("MetaStore::get_cluster_by_name: failed to get node")
                        .clone();
                    let proxy_address = chunk
                        .proxy_addresses
                        .get(i / 2)
                        .expect("MetaStore::get_cluster_by_name: failed to get proxy")
                        .clone();

                    // get slots
                    let mut slots = vec![];
                    let (first_slot_index, second_slot_index) = match chunk.role_position {
                        ChunkRolePosition::Normal => (0, 2),
                        ChunkRolePosition::FirstChunkMaster => (0, 1),
                        ChunkRolePosition::SecondChunkMaster => (2, 3),
                    };
                    if i == first_slot_index {
                        let mut first_slots = vec![];
                        if let Some(stable_slots) = &chunk.stable_slots[0] {
                            first_slots.push(stable_slots.clone());
                        }
                        slots.append(&mut first_slots);
                        let slot_ranges: Vec<_> = chunk.migrating_slots[0]
                            .iter()
                            .map(|slot_range_store| {
                                slot_range_store.to_slot_range(&cluster_store.chunks)
                            })
                            .collect();
                        slots.extend(slot_ranges);
                    }
                    if i == second_slot_index {
                        let mut second_slots = vec![];
                        if let Some(stable_slots) = &chunk.stable_slots[1] {
                            second_slots.push(stable_slots.clone());
                        }
                        slots.append(&mut second_slots);
                        let slot_ranges: Vec<_> = chunk.migrating_slots[1]
                            .iter()
                            .map(|slot_range_store| {
                                slot_range_store.to_slot_range(&cluster_store.chunks)
                            })
                            .collect();
                        slots.extend(slot_ranges);
                    }

                    // get repl
                    let mut role = Role::Master;
                    match chunk.role_position {
                        ChunkRolePosition::Normal if i % 2 == 1 => role = Role::Replica,
                        ChunkRolePosition::FirstChunkMaster if i >= CHUNK_HALF_NODE_NUM => {
                            role = Role::Replica
                        }
                        ChunkRolePosition::SecondChunkMaster if i < CHUNK_HALF_NODE_NUM => {
                            role = Role::Replica
                        }
                        _ => (),
                    }

                    let peer_index = match i {
                        0 => 3,
                        1 => 2,
                        2 => 1,
                        3 | _ => 0,
                    };
                    let peer = ReplPeer {
                        node_address: chunk
                            .node_addresses
                            .get(peer_index)
                            .expect("MetaStore::get_cluster_by_name: failed to get peer node")
                            .clone(),
                        proxy_address: chunk
                            .proxy_addresses
                            .get(peer_index / 2)
                            .expect("MetaStore::get_cluster_by_name: failed to get peer proxy")
                            .clone(),
                    };
                    let repl = ReplMeta::new(role, vec![peer]);

                    let node = Node::new(address, proxy_address, cluster_name.clone(), slots, repl);
                    nodes.push(node);
                }
                nodes
            })
            .flatten()
            .collect();

        let cluster = Cluster::new(
            cluster_store.name.clone(),
            self.global_epoch,
            nodes,
            cluster_store.config.clone(),
        );
        Some(cluster)
    }

    pub fn add_failure(&mut self, address: String, reporter_id: String) {
        let now = Utc::now();
        self.bump_global_epoch();
        self.failures
            .entry(address)
            .or_insert_with(HashMap::new)
            .insert(reporter_id, now.timestamp());
    }

    pub fn get_failures(
        &mut self,
        falure_ttl: chrono::Duration,
        failure_quorum: u64,
    ) -> Vec<String> {
        let now = Utc::now();
        for reporter_map in self.failures.values_mut() {
            reporter_map.retain(|_, report_time| {
                let report_datetime =
                    DateTime::<Utc>::from_utc(NaiveDateTime::from_timestamp(*report_time, 0), Utc);
                now - report_datetime < falure_ttl
            });
        }
        self.failures
            .retain(|_, proxy_failure_map| !proxy_failure_map.is_empty());
        self.failures
            .iter()
            .filter(|(_, v)| v.len() >= failure_quorum as usize)
            .map(|(address, _)| address.clone())
            .collect()
    }

    pub fn add_proxy(
        &mut self,
        proxy_address: String,
        nodes: [String; NODES_PER_PROXY],
    ) -> Result<(), MetaStoreError> {
        if proxy_address.split(':').count() != 2 {
            return Err(MetaStoreError::InvalidProxyAddress);
        }

        self.bump_global_epoch();

        self.all_proxies
            .entry(proxy_address.clone())
            .or_insert_with(|| ProxyResource {
                proxy_address: proxy_address.clone(),
                node_addresses: nodes,
            });

        self.failed_proxies.remove(&proxy_address);
        self.failures.remove(&proxy_address);

        Ok(())
    }

    pub fn add_cluster(&mut self, db_name: String, node_num: usize) -> Result<(), MetaStoreError> {
        let db_name = DBName::from(&db_name).map_err(|_| MetaStoreError::InvalidClusterName)?;
        if self.cluster.is_some() {
            return Err(MetaStoreError::OnlySupportOneCluster);
        }

        if node_num % 4 != 0 {
            return Err(MetaStoreError::InvalidNodeNum);
        }
        let proxy_num =
            NonZeroUsize::new(node_num / 2).ok_or_else(|| MetaStoreError::InvalidNodeNum)?;

        let proxy_resource_arr = self.consume_proxy(proxy_num)?;
        let chunk_stores = Self::proxy_resource_to_chunk_store(proxy_resource_arr, true);

        let cluster_store = ClusterStore {
            name: db_name,
            chunks: chunk_stores,
            config: ClusterConfig::default(),
        };

        self.cluster = Some(cluster_store);
        self.bump_global_epoch();
        Ok(())
    }

    fn proxy_resource_to_chunk_store(
        proxy_resource_arr: Vec<[ProxyResource; CHUNK_HALF_NODE_NUM]>,
        with_slots: bool,
    ) -> Vec<ChunkStore> {
        let master_num = proxy_resource_arr.len() * 2;
        let average = SLOT_NUM / master_num;
        let remainder = SLOT_NUM - average * master_num;
        let mut chunk_stores = vec![];
        let mut curr_slot = 0;
        for (i, chunk) in proxy_resource_arr.into_iter().enumerate() {
            let a = 2 * i;
            let b = a + 1;

            let mut create_slots = |index| {
                let r = (index < remainder) as usize;
                let start = curr_slot;
                let end = curr_slot + average + r;
                curr_slot = end;
                SlotRange {
                    range_list: RangeList::from_single_range(Range(start, end - 1)),
                    tag: SlotRangeTag::None,
                }
            };

            let stable_slots = if with_slots {
                [Some(create_slots(a)), Some(create_slots(b))]
            } else {
                [None, None]
            };

            let first_proxy = chunk[0].clone();
            let second_proxy = chunk[1].clone();
            let chunk_store = ChunkStore {
                role_position: ChunkRolePosition::Normal,
                stable_slots,
                migrating_slots: [vec![], vec![]],
                proxy_addresses: [
                    first_proxy.proxy_address.clone(),
                    second_proxy.proxy_address.clone(),
                ],
                node_addresses: [
                    first_proxy.node_addresses[0].clone(),
                    first_proxy.node_addresses[1].clone(),
                    second_proxy.node_addresses[0].clone(),
                    second_proxy.node_addresses[1].clone(),
                ],
            };
            chunk_stores.push(chunk_store);
        }
        chunk_stores
    }

    pub fn remove_cluster(&mut self, db_name: String) -> Result<(), MetaStoreError> {
        let db_name = DBName::from(&db_name).map_err(|_| MetaStoreError::InvalidClusterName)?;

        match &self.cluster {
            None => return Err(MetaStoreError::ClusterNotFound),
            Some(cluster) if cluster.name != db_name => {
                return Err(MetaStoreError::ClusterNotFound);
            }
            _ => (),
        }
        self.cluster = None;

        self.bump_global_epoch();
        Ok(())
    }

    pub fn auto_add_nodes(
        &mut self,
        db_name: String,
        num: Option<usize>,
    ) -> Result<Vec<Node>, MetaStoreError> {
        let db_name = DBName::from(&db_name).map_err(|_| MetaStoreError::InvalidClusterName)?;

        let existing_node_num = match self.cluster.as_ref() {
            None => return Err(MetaStoreError::ClusterNotFound),
            Some(cluster) => {
                if cluster.name != db_name {
                    return Err(MetaStoreError::ClusterNotFound);
                }
                cluster.chunks.len() * 4
            }
        };

        let num = match num {
            None => existing_node_num,
            Some(num) => num,
        };

        if num % 4 != 0 {
            return Err(MetaStoreError::InvalidNodeNum);
        }
        let proxy_num = NonZeroUsize::new(num / 2).ok_or_else(|| MetaStoreError::InvalidNodeNum)?;

        let proxy_resource_arr = self.consume_proxy(proxy_num)?;
        let mut chunks = Self::proxy_resource_to_chunk_store(proxy_resource_arr, false);

        match self.cluster {
            None => return Err(MetaStoreError::ClusterNotFound),
            Some(ref mut cluster) => {
                cluster.chunks.append(&mut chunks);
            }
        }

        let cluster = self.get_cluster().expect("auto_add_nodes");
        let nodes = cluster.get_nodes();
        let new_nodes = nodes
            .get((nodes.len() - num)..)
            .expect("auto_add_nodes: get nodes")
            .to_vec();

        self.bump_global_epoch();
        Ok(new_nodes)
    }

    pub fn remove_proxy(&mut self, proxy_address: String) -> Result<(), MetaStoreError> {
        if let Some(cluster) = self.get_cluster() {
            if cluster
                .get_nodes()
                .iter()
                .any(|node| node.get_proxy_address() == proxy_address)
            {
                return Err(MetaStoreError::InUse);
            }
        }

        self.all_proxies.remove(&proxy_address);
        self.failed_proxies.remove(&proxy_address);
        self.failures.remove(&proxy_address);
        self.bump_global_epoch();
        Ok(())
    }

    pub fn migrate_slots(&mut self, db_name: String) -> Result<(), MetaStoreError> {
        let db_name = DBName::from(&db_name).map_err(|_| MetaStoreError::InvalidClusterName)?;
        let new_epoch = self.global_epoch + 1;

        {
            let cluster = match self.cluster.as_mut() {
                None => return Err(MetaStoreError::ClusterNotFound),
                Some(cluster) => {
                    if db_name != cluster.name {
                        return Err(MetaStoreError::ClusterNotFound);
                    }
                    cluster
                }
            };

            let running_migration = cluster
                .chunks
                .iter()
                .any(|chunk| !chunk.migrating_slots.iter().any(|slots| slots.is_empty()));
            if running_migration {
                return Err(MetaStoreError::MigrationRunning);
            }

            let migration_slots = Self::remove_slots_from_src(cluster, new_epoch);
            Self::assign_dst_slots(cluster, migration_slots);
        }

        self.bump_global_epoch();

        Ok(())
    }

    fn remove_slots_from_src(cluster: &mut ClusterStore, epoch: u64) -> Vec<MigrationSlots> {
        let dst_chunk_num = cluster
            .chunks
            .iter()
            .filter(|chunk| chunk.stable_slots[0].is_none() && chunk.stable_slots[1].is_none())
            .count();
        let dst_master_num = dst_chunk_num * 2;
        let master_num = cluster.chunks.len() * 2;
        let src_chunk_num = cluster.chunks.len() - dst_chunk_num;
        let src_master_num = src_chunk_num * 2;
        let average = SLOT_NUM / master_num;
        let remainder = SLOT_NUM - average * master_num;

        let mut curr_dst_master_index = 0;
        let mut migration_slots = vec![];
        let mut curr_dst_slots = vec![];
        let mut curr_slots_num = 0;

        for (src_chunk_index, src_chunk) in cluster.chunks.iter_mut().enumerate() {
            for (src_chunk_part, slot_range) in src_chunk.stable_slots.iter_mut().enumerate() {
                if let Some(slot_range) = slot_range {
                    while curr_dst_master_index != dst_master_num {
                        let src_master_index = src_chunk_index * 2 + src_chunk_part;
                        let src_r = (src_master_index < remainder) as usize; // true will be 1, false will be 0
                        let dst_master_index = src_master_num + curr_dst_master_index;
                        let dst_r = (dst_master_index < remainder) as usize; // true will be 1, false will be 0
                        let src_final_num = average + src_r;
                        let dst_final_num = average + dst_r;

                        if slot_range.get_range_list().get_slots_num() <= src_final_num {
                            break;
                        }

                        let need_num = dst_final_num - curr_slots_num;
                        let available_num =
                            slot_range.get_range_list().get_slots_num() - src_final_num;
                        let remove_num = min(need_num, available_num);
                        let num = slot_range
                            .get_range_list()
                            .get_ranges()
                            .last()
                            .map(|r| r.end() - r.start() + 1)
                            .expect("remove_slots_from_src: slots > average + src_r >= 0");

                        if remove_num >= num {
                            let range = slot_range
                                .get_mut_range_list()
                                .get_mut_ranges()
                                .pop()
                                .expect("remove_slots_from_src: need_num >= num");
                            curr_dst_slots.push(range);
                            curr_slots_num += num;
                        } else {
                            let range = slot_range
                                .get_mut_range_list()
                                .get_mut_ranges()
                                .last_mut()
                                .expect("remove_slots_from_src");
                            let end = range.end();
                            let start = end - remove_num + 1;
                            *range.end_mut() -= remove_num;
                            curr_dst_slots.push(Range(start, end));
                            curr_slots_num += remove_num;
                        }

                        // reset current state
                        if curr_slots_num >= dst_final_num
                            || slot_range.get_range_list().get_slots_num() <= src_final_num
                        {
                            // assert curr_dst_slots.is_not_empty()
                            migration_slots.push(MigrationSlots {
                                meta: MigrationMetaStore {
                                    epoch,
                                    src_chunk_index,
                                    src_chunk_part,
                                    dst_chunk_index: src_chunk_num + (curr_dst_master_index / 2),
                                    dst_chunk_part: curr_dst_master_index % 2,
                                },
                                ranges: curr_dst_slots.drain(..).collect(),
                            });
                            if curr_slots_num >= dst_final_num {
                                curr_dst_master_index += 1;
                                curr_slots_num = 0;
                            }
                            if slot_range.get_range_list().get_slots_num() <= src_final_num {
                                break;
                            }
                        }
                    }
                }
            }
        }

        migration_slots
    }

    fn assign_dst_slots(cluster: &mut ClusterStore, migration_slots: Vec<MigrationSlots>) {
        for migration_slot_range in migration_slots.into_iter() {
            let MigrationSlots { ranges, meta } = migration_slot_range;

            {
                let src_chunk = cluster
                    .chunks
                    .get_mut(meta.src_chunk_index)
                    .expect("assign_dst_slots");
                let migrating_slots = src_chunk
                    .migrating_slots
                    .get_mut(meta.src_chunk_part)
                    .expect("assign_dst_slots");
                let slot_range = MigrationSlotRangeStore {
                    range_list: RangeList::new(ranges.clone()),
                    is_migrating: true,
                    meta: meta.clone(),
                };
                migrating_slots.push(slot_range);
            }
            {
                let dst_chunk = cluster
                    .chunks
                    .get_mut(meta.dst_chunk_index)
                    .expect("assign_dst_slots");
                let migrating_slots = dst_chunk
                    .migrating_slots
                    .get_mut(meta.dst_chunk_part)
                    .expect("assign_dst_slots");
                let slot_range = MigrationSlotRangeStore {
                    range_list: RangeList::new(ranges.clone()),
                    is_migrating: false,
                    meta,
                };
                migrating_slots.push(slot_range);
            }
        }
    }

    pub fn commit_migration(&mut self, task: MigrationTaskMeta) -> Result<(), MetaStoreError> {
        let cluster = self
            .cluster
            .as_mut()
            .ok_or_else(|| MetaStoreError::ClusterNotFound)?;
        let task_epoch = match &task.slot_range.tag {
            SlotRangeTag::None => return Err(MetaStoreError::InvalidMigrationTask),
            SlotRangeTag::Migrating(meta) => meta.epoch,
            SlotRangeTag::Importing(meta) => meta.epoch,
        };

        let (src_chunk_index, src_chunk_part) = cluster
            .chunks
            .iter()
            .enumerate()
            .flat_map(|(i, chunk)| {
                chunk
                    .migrating_slots
                    .iter()
                    .enumerate()
                    .map(move |(j, slot_range_stores)| (i, j, slot_range_stores))
            })
            .flat_map(|(i, j, slot_range_stores)| {
                slot_range_stores
                    .iter()
                    .map(move |slot_range_store| (i, j, slot_range_store))
            })
            .find(|(_, _, slot_range_store)| {
                slot_range_store.range_list == task.slot_range.range_list
                    && slot_range_store.meta.epoch == task_epoch
                    && slot_range_store.is_migrating
            })
            .map(|(i, j, _)| (i, j))
            .ok_or_else(|| MetaStoreError::MigrationTaskNotFound)?;

        let (dst_chunk_index, dst_chunk_part) = cluster
            .chunks
            .iter()
            .enumerate()
            .flat_map(|(i, chunk)| {
                chunk
                    .migrating_slots
                    .iter()
                    .enumerate()
                    .map(move |(j, slot_range_stores)| (i, j, slot_range_stores))
            })
            .flat_map(|(i, j, slot_range_stores)| {
                slot_range_stores
                    .iter()
                    .map(move |slot_range_store| (i, j, slot_range_store))
            })
            .find(|(_, _, slot_range_store)| {
                slot_range_store.range_list == task.slot_range.range_list
                    && slot_range_store.meta.epoch == task_epoch
                    && !slot_range_store.is_migrating
            })
            .map(|(i, j, _)| (i, j))
            .ok_or_else(|| MetaStoreError::MigrationTaskNotFound)?;

        let meta = MigrationMetaStore {
            epoch: task_epoch,
            src_chunk_index,
            src_chunk_part,
            dst_chunk_index,
            dst_chunk_part,
        };

        for chunk in &mut cluster.chunks {
            for migrating_slots in chunk.migrating_slots.iter_mut() {
                migrating_slots.retain(|slot_range_store| {
                    !(slot_range_store.is_migrating
                        && slot_range_store.range_list == task.slot_range.range_list
                        && slot_range_store.meta == meta)
                })
            }
        }

        for chunk in &mut cluster.chunks {
            let removed_slots =
                chunk
                    .migrating_slots
                    .iter_mut()
                    .enumerate()
                    .find_map(|(j, migrating_slots)| {
                        migrating_slots
                            .iter()
                            .position(|slot_range_store| {
                                !slot_range_store.is_migrating
                                    && slot_range_store.meta == meta
                                    && slot_range_store.range_list == task.slot_range.range_list
                            })
                            .map(|index| (j, migrating_slots.remove(index).range_list))
                    });
            if let Some((j, mut range_list)) = removed_slots {
                match chunk.stable_slots.get_mut(j).expect("commit_migration") {
                    Some(stable_slots) => {
                        stable_slots
                            .get_mut_range_list()
                            .merge_another(&mut range_list);
                    }
                    stable_slots => {
                        let slot_range = SlotRange {
                            range_list,
                            tag: SlotRangeTag::None,
                        };
                        *stable_slots = Some(slot_range);
                    }
                }
                break;
            }
        }

        self.bump_global_epoch();
        Ok(())
    }

    fn get_free_proxies(&self) -> Vec<String> {
        let failed_proxies = self.failed_proxies.clone();
        let failures = self.failures.clone();
        let occupied_proxies = self
            .cluster
            .as_ref()
            .map(|cluster| {
                cluster
                    .chunks
                    .iter()
                    .map(|chunk| chunk.proxy_addresses.to_vec())
                    .flatten()
                    .collect::<HashSet<String>>()
            })
            .unwrap_or_else(HashSet::new);

        let mut free_proxies = vec![];
        for proxy_resource in self.all_proxies.values() {
            let proxy_address = &proxy_resource.proxy_address;
            if failed_proxies.contains(proxy_address) {
                continue;
            }
            if failures.contains_key(proxy_address) {
                continue;
            }
            if occupied_proxies.contains(proxy_address) {
                continue;
            }
            free_proxies.push(proxy_address.clone());
        }
        free_proxies
    }

    fn build_link_table(&self) -> HashMap<String, HashMap<String, usize>> {
        let mut link_table: HashMap<String, HashMap<String, usize>> = HashMap::new();
        for proxy_resource in self.all_proxies.values() {
            let first = proxy_resource.proxy_address.clone();
            let first_host = first
                .split(':')
                .next()
                .expect("build_link_table")
                .to_string();
            for proxy_resource in self.all_proxies.values() {
                let second = proxy_resource.proxy_address.clone();
                let second_host = second
                    .split(':')
                    .next()
                    .expect("build_link_table")
                    .to_string();
                if first_host == second_host {
                    continue;
                }
                link_table
                    .entry(first_host.clone())
                    .or_insert_with(HashMap::new)
                    .entry(second_host.clone())
                    .or_insert(0);
                link_table
                    .entry(second_host.clone())
                    .or_insert_with(HashMap::new)
                    .entry(first_host.clone())
                    .or_insert(0);
            }
        }

        if let Some(cluster) = self.cluster.as_ref() {
            for chunk in cluster.chunks.iter() {
                let first = chunk.proxy_addresses[0].clone();
                let second = chunk.proxy_addresses[1].clone();
                let first_host = first
                    .split(':')
                    .next()
                    .expect("build_link_table")
                    .to_string();
                let second_host = second
                    .split(':')
                    .next()
                    .expect("build_link_table")
                    .to_string();
                let linked_num = link_table
                    .entry(first_host.clone())
                    .or_insert_with(HashMap::new)
                    .entry(second_host.clone())
                    .or_insert(0);
                *linked_num += 1;
                let linked_num = link_table
                    .entry(second_host)
                    .or_insert_with(HashMap::new)
                    .entry(first_host)
                    .or_insert(0);
                *linked_num += 1;
            }
        }
        link_table
    }

    fn consume_proxy(
        &self,
        proxy_num: NonZeroUsize,
    ) -> Result<Vec<[ProxyResource; CHUNK_HALF_NODE_NUM]>, MetaStoreError> {
        // host => proxies
        let mut host_proxies: HashMap<String, Vec<ProxySlot>> = HashMap::new();
        for proxy_address in self.get_free_proxies().into_iter() {
            let host = proxy_address
                .split(':')
                .next()
                .expect("consume_proxy: get host from address")
                .to_string();
            host_proxies
                .entry(host)
                .or_insert_with(Vec::new)
                .push(proxy_address);
        }

        host_proxies = Self::remove_redundant_chunks(host_proxies, proxy_num)?;

        let link_table = self.build_link_table();

        let new_added_proxy_resource = Self::allocate_chunk(host_proxies, link_table, proxy_num)?;
        let new_proxies = new_added_proxy_resource
            .into_iter()
            .map(|[a, b]| {
                [
                    self.all_proxies
                        .get(&a)
                        .expect("consume_proxy: get proxy resource")
                        .clone(),
                    self.all_proxies
                        .get(&b)
                        .expect("consume_proxy: get proxy resource")
                        .clone(),
                ]
            })
            .collect();
        Ok(new_proxies)
    }

    fn remove_redundant_chunks(
        mut host_proxies: HashMap<String, Vec<ProxySlot>>,
        expected_num: NonZeroUsize,
    ) -> Result<HashMap<String, Vec<ProxySlot>>, MetaStoreError> {
        let mut free_proxy_num: usize = host_proxies.values().map(|proxies| proxies.len()).sum();
        let mut max_proxy_num = host_proxies
            .values()
            .map(|proxies| proxies.len())
            .max()
            .unwrap_or(0);

        for proxies in host_proxies.values_mut() {
            if proxies.len() == max_proxy_num {
                // Only remove proxies in the host which as too many proxies.
                while max_proxy_num * 2 > free_proxy_num {
                    proxies.pop();
                    free_proxy_num -= 1;
                    max_proxy_num -= 1;
                }
                break;
            }
        }

        if free_proxy_num < expected_num.get() {
            return Err(MetaStoreError::NoAvailableResource);
        }
        Ok(host_proxies)
    }

    fn allocate_chunk(
        mut host_proxies: HashMap<String, Vec<ProxySlot>>,
        mut link_table: HashMap<String, HashMap<String, usize>>,
        expected_num: NonZeroUsize,
    ) -> Result<Vec<[String; CHUNK_HALF_NODE_NUM]>, MetaStoreError> {
        let max_proxy_num = host_proxies
            .values()
            .map(|proxies| proxies.len())
            .max()
            .unwrap_or(0);
        let sum_proxy_num = host_proxies.values().map(|proxies| proxies.len()).sum();

        if sum_proxy_num < expected_num.get() {
            return Err(MetaStoreError::NoAvailableResource);
        }

        if max_proxy_num * 2 > sum_proxy_num {
            return Err(MetaStoreError::ResourceNotBalance);
        }

        let mut new_proxy_pairs = vec![];
        while new_proxy_pairs.len() * 2 < expected_num.get() {
            let (first_host, first_address) = {
                let (max_host, max_proxy_host) = host_proxies
                    .iter_mut()
                    .max_by_key(|(_host, proxies)| proxies.len())
                    .expect("allocate_chunk: invalid state. cannot find any host");
                (
                    max_host.clone(),
                    max_proxy_host
                        .pop()
                        .expect("allocate_chunk: cannot find free proxy"),
                )
            };

            let (second_host, second_address) = {
                let peers = link_table
                    .get(&first_host)
                    .expect("allocate_chunk: invalid state, cannot get link table entry");

                let second_host = peers
                    .iter()
                    .filter(|(host, _)| {
                        let free_count = host_proxies.get(*host).map(|proxies| proxies.len());
                        **host != first_host && free_count != None && free_count != Some(0)
                    })
                    .min_by_key(|(_, count)| **count)
                    .map(|t| t.0.clone())
                    .expect("allocate_chunk: invalid state, cannot get free proxy");

                let second_address = host_proxies
                    .get_mut(&second_host)
                    .expect("allocate_chunk: get second host")
                    .pop()
                    .expect("allocate_chunk: get second address");
                (second_host, second_address)
            };

            *link_table
                .get_mut(&first_host)
                .expect("allocate_chunk: link table")
                .get_mut(&second_host)
                .expect("allocate_chunk: link table") += 1;
            *link_table
                .get_mut(&second_host)
                .expect("allocate_chunk: link table")
                .get_mut(&first_host)
                .expect("allocate_chunk: link table") += 1;

            new_proxy_pairs.push([first_address, second_address]);
        }

        Ok(new_proxy_pairs)
    }

    pub fn replace_failed_proxy(
        &mut self,
        failed_proxy_address: String,
    ) -> Result<Proxy, MetaStoreError> {
        if !self.all_proxies.contains_key(&failed_proxy_address) {
            return Err(MetaStoreError::HostNotFound);
        }

        let not_in_use = self
            .get_cluster()
            .and_then(|cluster| {
                cluster
                    .get_nodes()
                    .iter()
                    .find(|node| node.get_proxy_address() == failed_proxy_address)
                    .map(|_| ())
            })
            .is_none();
        if not_in_use {
            self.failed_proxies.insert(failed_proxy_address);
            return Err(MetaStoreError::NotInUse);
        }

        self.takeover_master(failed_proxy_address.clone())?;

        let proxy_resource = self.consume_new_proxy(failed_proxy_address.clone())?;
        {
            let cluster = self
                .cluster
                .as_mut()
                .expect("replace_failed_proxy: get cluster");
            for chunk in cluster.chunks.iter_mut() {
                if chunk.proxy_addresses[0] == failed_proxy_address {
                    chunk.proxy_addresses[0] = proxy_resource.proxy_address.clone();
                    chunk.node_addresses[0] = proxy_resource.node_addresses[0].clone();
                    chunk.node_addresses[1] = proxy_resource.node_addresses[1].clone();
                    break;
                } else if chunk.proxy_addresses[1] == failed_proxy_address {
                    chunk.proxy_addresses[1] = proxy_resource.proxy_address.clone();
                    chunk.node_addresses[2] = proxy_resource.node_addresses[0].clone();
                    chunk.node_addresses[3] = proxy_resource.node_addresses[1].clone();
                    break;
                }
            }
        }

        self.failed_proxies.insert(failed_proxy_address);
        self.bump_global_epoch();
        Ok(self
            .get_proxy_by_address(&proxy_resource.proxy_address)
            .expect("replace_failed_proxy"))
    }

    fn takeover_master(&mut self, failed_proxy_address: String) -> Result<(), MetaStoreError> {
        self.bump_global_epoch();

        let cluster = self
            .cluster
            .as_mut()
            .ok_or_else(|| MetaStoreError::ClusterNotFound)?;
        for chunk in cluster.chunks.iter_mut() {
            if chunk.proxy_addresses[0] == failed_proxy_address {
                chunk.role_position = ChunkRolePosition::SecondChunkMaster;
                break;
            } else if chunk.proxy_addresses[1] == failed_proxy_address {
                chunk.role_position = ChunkRolePosition::FirstChunkMaster;
                break;
            }
        }
        Ok(())
    }

    fn consume_new_proxy(
        &mut self,
        failed_proxy_address: String,
    ) -> Result<ProxyResource, MetaStoreError> {
        let free_hosts: HashSet<String> = self
            .get_free_proxies()
            .into_iter()
            .map(|address| {
                address
                    .split(':')
                    .next()
                    .expect("consume_new_proxy: split address")
                    .to_string()
            })
            .collect();
        let link_table = self.build_link_table();

        let failed_proxy_host = failed_proxy_address
            .split(':')
            .next()
            .ok_or_else(|| MetaStoreError::InvalidProxyAddress)?
            .to_string();
        let link_count_table = link_table
            .get(&failed_proxy_host)
            .expect("consume_new_proxy: cannot find failed proxy");
        let peer_host = link_count_table
            .iter()
            .filter(|(peer_host, _)| free_hosts.contains(*peer_host))
            .min_by_key(|(_, count)| *count)
            .map(|(peer_address, _)| peer_address)
            .ok_or_else(|| MetaStoreError::NoAvailableResource)?;

        let peer_proxy = self
            .get_free_proxies()
            .iter()
            .find(|address| {
                peer_host
                    == address
                        .split(':')
                        .next()
                        .expect("consume_new_proxy: split address")
            })
            .expect("consume_new_proxy: get peer address")
            .to_string();

        let new_proxy = self
            .all_proxies
            .get(&peer_proxy)
            .expect("consume_new_proxy: cannot find peer proxy")
            .clone();
        Ok(new_proxy)
    }
}

#[derive(Debug)]
pub enum MetaStoreError {
    InUse,
    NotInUse,
    NoAvailableResource,
    ResourceNotBalance,
    AlreadyExisted,
    ClusterNotFound,
    HostNotFound,
    InvalidNodeNum,
    InvalidClusterName,
    InvalidMigrationTask,
    InvalidProxyAddress,
    MigrationTaskNotFound,
    OnlySupportOneCluster,
    MigrationRunning,
    NotSupported,
}

impl fmt::Display for MetaStoreError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Error for MetaStoreError {
    fn cause(&self) -> Option<&dyn Error> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add_testing_proxies(store: &mut MetaStore, host_num: usize, proxy_per_host: usize) {
        for host_index in 1..=host_num {
            for i in 1..=proxy_per_host {
                let proxy_address = format!("127.0.0.{}:70{:02}", host_index, i);
                let node_addresses = [
                    format!("127.0.0.{}:60{:02}", host_index, i * 2),
                    format!("127.0.0.{}:60{:02}", host_index, i * 2 + 1),
                ];
                store.add_proxy(proxy_address, node_addresses).unwrap();
            }
        }
    }

    #[test]
    fn test_add_and_remove_proxy() {
        let mut store = MetaStore::default();
        let proxy_address = "127.0.0.1:7000";
        let nodes = ["127.0.0.1:6000".to_string(), "127.0.0.1:6001".to_string()];

        assert!(store
            .add_proxy("127.0.0.1".to_string(), nodes.clone())
            .is_err());

        store
            .add_proxy(proxy_address.to_string(), nodes.clone())
            .unwrap();
        assert_eq!(store.get_global_epoch(), 1);
        assert_eq!(store.all_proxies.len(), 1);
        let resource = store.all_proxies.get(proxy_address).unwrap();
        assert_eq!(resource.proxy_address, proxy_address);
        assert_eq!(resource.node_addresses, nodes);

        assert_eq!(store.get_proxies(), vec![proxy_address.to_string()]);

        let proxy = store.get_proxy_by_address(proxy_address).unwrap();
        assert_eq!(proxy.get_address(), proxy_address);
        assert_eq!(proxy.get_epoch(), 1);
        assert_eq!(proxy.get_nodes().len(), 0);
        assert_eq!(proxy.get_peers().len(), 0);
        assert_eq!(proxy.get_free_nodes().len(), 2);

        store.remove_proxy(proxy_address.to_string()).unwrap();
    }

    #[test]
    fn test_add_and_remove_cluster() {
        let mut store = MetaStore::default();
        add_testing_proxies(&mut store, 4, 3);
        let proxies: Vec<_> = store
            .get_proxies()
            .into_iter()
            .filter_map(|proxy_address| store.get_proxy_by_address(&proxy_address))
            .collect();
        let original_free_node_num: usize = proxies
            .iter()
            .map(|proxy| proxy.get_free_nodes().len())
            .sum();

        let epoch1 = store.get_global_epoch();

        let db_name = "test_db".to_string();
        store.add_cluster(db_name.clone(), 4).unwrap();
        let epoch2 = store.get_global_epoch();
        assert!(epoch1 < epoch2);

        let names: Vec<String> = store
            .get_cluster_names()
            .into_iter()
            .map(|db_name| db_name.to_string())
            .collect();
        assert_eq!(names, vec![db_name.clone()]);

        let cluster = store.get_cluster_by_name(&db_name).unwrap();
        assert_eq!(cluster.get_nodes().len(), 4);

        check_cluster_slots(cluster.clone(), 4);

        let proxies: Vec<_> = store
            .get_proxies()
            .into_iter()
            .filter_map(|proxy_address| store.get_proxy_by_address(&proxy_address))
            .collect();
        let free_node_num: usize = proxies
            .iter()
            .map(|proxy| proxy.get_free_nodes().len())
            .sum();
        assert_eq!(free_node_num, original_free_node_num - 4);

        let r = store.add_cluster("another_db".to_string(), 4);
        assert!(r.is_err());
        let epoch3 = store.get_global_epoch();
        assert_eq!(epoch2, epoch3);

        for node in cluster.get_nodes() {
            let proxy = store
                .get_proxy_by_address(&node.get_proxy_address())
                .unwrap();
            assert_eq!(proxy.get_free_nodes().len(), 0);
            assert_eq!(proxy.get_nodes().len(), 2);
            let proxy_port = node
                .get_proxy_address()
                .split(':')
                .nth(1)
                .unwrap()
                .parse::<usize>()
                .unwrap();
            let node_port = node
                .get_address()
                .split(':')
                .nth(1)
                .unwrap()
                .parse::<usize>()
                .unwrap();
            assert_eq!(proxy_port - 7000, (node_port - 6000) / 2);
        }

        let node_addresses_set: HashSet<String> = cluster
            .get_nodes()
            .iter()
            .map(|node| node.get_address().to_string())
            .collect();
        assert_eq!(node_addresses_set.len(), cluster.get_nodes().len());
        let proy_addresses_set: HashSet<String> = cluster
            .get_nodes()
            .iter()
            .map(|node| node.get_proxy_address().to_string())
            .collect();
        assert_eq!(proy_addresses_set.len() * 2, cluster.get_nodes().len());

        store.remove_cluster(db_name.clone()).unwrap();
        let epoch4 = store.get_global_epoch();
        assert!(epoch3 < epoch4);

        let proxies: Vec<_> = store
            .get_proxies()
            .into_iter()
            .filter_map(|proxy_address| store.get_proxy_by_address(&proxy_address))
            .collect();
        let free_node_num: usize = proxies
            .iter()
            .map(|proxy| proxy.get_free_nodes().len())
            .sum();
        assert_eq!(free_node_num, original_free_node_num);
    }

    #[test]
    fn test_failures() {
        let mut store = MetaStore::default();
        const ALL_PROXIES: usize = 4 * 3;
        add_testing_proxies(&mut store, 4, 3);
        assert_eq!(store.get_free_proxies().len(), ALL_PROXIES);

        let original_proxy_num = store.get_proxies().len();
        let failed_address = "127.0.0.1:7001";
        assert!(store.get_proxy_by_address(failed_address).is_some());
        let epoch1 = store.get_global_epoch();

        store.add_failure(failed_address.to_string(), "reporter_id".to_string());
        let epoch2 = store.get_global_epoch();
        assert!(epoch1 < epoch2);
        let proxy_num = store.get_proxies().len();
        assert_eq!(proxy_num, original_proxy_num);
        assert_eq!(store.get_free_proxies().len(), ALL_PROXIES - 1);

        assert_eq!(
            store.get_failures(chrono::Duration::max_value(), 1),
            vec![failed_address.to_string()]
        );
        assert!(store
            .get_failures(chrono::Duration::max_value(), 2)
            .is_empty(),);
        store.remove_proxy(failed_address.to_string()).unwrap();
        let epoch3 = store.get_global_epoch();
        assert!(epoch2 < epoch3);

        let db_name = "test_db".to_string();
        store.add_cluster(db_name.clone(), 4).unwrap();
        assert_eq!(store.get_free_proxies().len(), ALL_PROXIES - 3);
        let epoch4 = store.get_global_epoch();
        assert!(epoch3 < epoch4);

        let cluster = store.get_cluster_by_name(&db_name).unwrap();
        check_cluster_slots(cluster.clone(), 4);

        let failed_proxy_address = cluster
            .get_nodes()
            .get(0)
            .unwrap()
            .get_proxy_address()
            .to_string();
        store.add_failure(failed_proxy_address.clone(), "reporter_id".to_string());
        assert_eq!(store.get_free_proxies().len(), 9);
        let epoch5 = store.get_global_epoch();
        assert!(epoch4 < epoch5);

        let proxy_num = store.get_proxies().len();
        assert_eq!(proxy_num, original_proxy_num - 1);
        assert_eq!(
            store.get_failures(chrono::Duration::max_value(), 1),
            vec![failed_proxy_address.clone()]
        );

        let new_proxy = store
            .replace_failed_proxy(failed_proxy_address.clone())
            .unwrap();
        assert_ne!(new_proxy.get_address(), failed_proxy_address);
        let epoch6 = store.get_global_epoch();
        assert!(epoch5 < epoch6);

        let cluster = store.get_cluster_by_name(&db_name).unwrap();
        assert_eq!(
            cluster
                .get_nodes()
                .iter()
                .filter(|node| node.get_proxy_address() == &failed_proxy_address)
                .count(),
            0
        );
        assert_eq!(
            cluster
                .get_nodes()
                .iter()
                .filter(|node| node.get_proxy_address() == new_proxy.get_address())
                .count(),
            2
        );
        for node in cluster.get_nodes().iter() {
            if node.get_proxy_address() != new_proxy.get_address() {
                assert_eq!(node.get_role(), Role::Master);
            } else {
                assert_eq!(node.get_role(), Role::Replica);
            }
        }

        // Recover proxy
        let nodes = store
            .all_proxies
            .get(&failed_proxy_address)
            .unwrap()
            .node_addresses
            .clone();
        store
            .add_proxy(failed_proxy_address.clone(), nodes)
            .unwrap();
        assert_eq!(
            store.get_failures(chrono::Duration::max_value(), 1).len(),
            0
        );
        let epoch7 = store.get_global_epoch();
        assert!(epoch6 < epoch7);
    }

    const DB_NAME: &'static str = "test_db";

    fn test_migration_helper(
        host_num: usize,
        proxy_per_host: usize,
        start_node_num: usize,
        added_node_num: usize,
    ) {
        let mut store = init_migration_test_store(host_num, proxy_per_host, start_node_num);
        test_scaling(&mut store, host_num * proxy_per_host, added_node_num);
    }

    fn init_migration_test_store(
        host_num: usize,
        proxy_per_host: usize,
        start_node_num: usize,
    ) -> MetaStore {
        let mut store = MetaStore::default();
        let all_proxy_num = host_num * proxy_per_host;
        add_testing_proxies(&mut store, host_num, proxy_per_host);
        assert_eq!(store.get_free_proxies().len(), all_proxy_num);

        let db_name = DB_NAME.to_string();
        store.add_cluster(db_name.clone(), start_node_num).unwrap();
        let cluster = store.get_cluster_by_name(&db_name).unwrap();
        assert_eq!(cluster.get_nodes().len(), start_node_num);
        assert_eq!(
            store.get_free_proxies().len(),
            all_proxy_num - start_node_num / 2
        );

        store
    }

    fn test_scaling(store: &mut MetaStore, all_proxy_num: usize, added_node_num: usize) {
        let db_name = DB_NAME.to_string();
        let start_node_num = store
            .get_cluster_by_name(&db_name)
            .unwrap()
            .get_nodes()
            .len();

        let epoch1 = store.get_global_epoch();
        let nodes = store
            .auto_add_nodes(db_name.clone(), Some(added_node_num))
            .unwrap();
        let epoch2 = store.get_global_epoch();
        assert!(epoch1 < epoch2);
        assert_eq!(nodes.len(), added_node_num);
        let cluster = store.get_cluster_by_name(&db_name).unwrap();
        assert_eq!(cluster.get_nodes().len(), start_node_num + added_node_num);
        assert_eq!(
            store.get_free_proxies().len(),
            all_proxy_num - start_node_num / 2 - added_node_num / 2
        );

        store.migrate_slots(db_name.clone()).unwrap();
        let epoch3 = store.get_global_epoch();
        assert!(epoch2 < epoch3);

        let cluster = store.get_cluster_by_name(&db_name).unwrap();
        assert_eq!(cluster.get_nodes().len(), start_node_num + added_node_num);
        for (i, node) in cluster.get_nodes().iter().enumerate() {
            if i < start_node_num {
                if node.get_role() == Role::Replica {
                    continue;
                }
                let slots = node.get_slots();
                // Some src slots might not need to transfer.
                assert!(slots.len() >= 1);
                assert!(slots[0].tag.is_stable());
                for slot_range in slots.iter().skip(1) {
                    assert!(slot_range.tag.is_migrating());
                }
            } else {
                if node.get_role() == Role::Replica {
                    continue;
                }
                let slots = node.get_slots();
                assert!(slots.len() >= 1);
                for slot_range in slots.iter() {
                    assert!(slot_range.tag.is_importing());
                }
            }
        }

        let slot_range_set: HashSet<_> = cluster
            .get_nodes()
            .iter()
            .filter(|node| node.get_role() == Role::Master)
            .flat_map(|node| node.get_slots().iter())
            .filter_map(|slot_range| match slot_range.tag {
                SlotRangeTag::Migrating(_) => Some(slot_range.clone()),
                _ => None,
            })
            .collect();

        for slot_range in slot_range_set.into_iter() {
            let task_meta = MigrationTaskMeta {
                db_name: DBName::from(&db_name).unwrap(),
                slot_range,
            };
            store.commit_migration(task_meta).unwrap();
        }

        let cluster = store.get_cluster_by_name(&db_name).unwrap();
        check_cluster_slots(cluster, start_node_num + added_node_num);
    }

    fn check_cluster_slots(cluster: Cluster, node_num: usize) {
        assert_eq!(cluster.get_nodes().len(), node_num);
        let master_num = cluster.get_nodes().len() / 2;
        let average_slots_num = SLOT_NUM / master_num;

        let mut visited = Vec::with_capacity(SLOT_NUM);
        for _ in 0..SLOT_NUM {
            visited.push(false);
        }

        for node in cluster.get_nodes() {
            let slots = node.get_slots();
            if node.get_role() == Role::Master {
                assert_eq!(slots.len(), 1);
                assert_eq!(slots[0].tag, SlotRangeTag::None);
                let slots_num = slots[0].get_range_list().get_slots_num();
                let delta = slots_num.checked_sub(average_slots_num).unwrap();
                assert!(delta <= 1);

                for range in slots[0].get_range_list().get_ranges().iter() {
                    for i in range.start()..=range.end() {
                        assert!(!visited.get(i).unwrap());
                        *visited.get_mut(i).unwrap() = true;
                    }
                }
            } else {
                assert!(slots.is_empty());
            }
        }
        for v in visited.iter() {
            assert!(*v);
        }

        let mut last_node_slot_num = usize::max_value();
        for node in cluster.get_nodes() {
            if node.get_role() == Role::Replica {
                continue;
            }
            let curr_num = node
                .get_slots()
                .iter()
                .map(|slots| slots.get_range_list().get_slots_num())
                .sum();
            assert!(last_node_slot_num >= curr_num);
            last_node_slot_num = curr_num;
        }
    }

    #[test]
    fn test_migration() {
        // Can increase them to cover more cases.
        const MAX_HOST_NUM: usize = 6;
        const MAX_PROXY_PER_HOST: usize = 6;

        for host_num in 2..=MAX_HOST_NUM {
            for proxy_per_host in 1..=MAX_PROXY_PER_HOST {
                let chunk_num = host_num * proxy_per_host / 2;
                for i in 1..chunk_num {
                    let added_chunk_num = chunk_num - i;
                    if added_chunk_num == 0 {
                        continue;
                    }
                    for j in 1..=added_chunk_num {
                        assert!(i + j <= chunk_num);
                        // println!("{} {} {} {}", host_num, proxy_per_host, 4*i, 4*j);
                        test_migration_helper(host_num, proxy_per_host, 4 * i, 4 * j);
                    }
                }
            }
        }
    }

    #[test]
    fn test_multiple_migration() {
        let host_num = 4;
        let proxy_per_host = 3;
        let start_node_num = 4;
        let added_node_num = 4;
        let mut store = init_migration_test_store(host_num, proxy_per_host, start_node_num);
        test_scaling(&mut store, host_num * proxy_per_host, added_node_num);
        test_scaling(&mut store, host_num * proxy_per_host, added_node_num);
        test_scaling(&mut store, host_num * proxy_per_host, added_node_num);
        test_scaling(&mut store, host_num * proxy_per_host, added_node_num);
        test_scaling(&mut store, host_num * proxy_per_host, added_node_num);
    }
}
