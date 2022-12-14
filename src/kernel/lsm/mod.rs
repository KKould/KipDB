use std::cmp::Ordering;
use std::collections::HashSet;
use std::fs;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use growable_bloom_filter::GrowableBloom;
use itertools::Itertools;
use lru::LruCache;
use serde::{Deserialize, Serialize};
use skiplist::SkipMap;
use tokio::sync::RwLock;
use crate::kernel::{CommandData, log_path, Result};
use crate::kernel::io_handler::IOHandler;
use crate::kernel::lsm::compactor::MergeShardingVec;
use crate::kernel::lsm::lsm_kv::{Config, LevelSlice, SsTableMap};
use crate::kernel::lsm::ss_table::{Scope, SsTable};
use crate::KvsError;

pub(crate) mod ss_table;
pub mod lsm_kv;
mod compactor;

pub(crate) type MemMap = SkipMap<Vec<u8>, CommandData>;

/// MetaInfo序列化长度定长
/// 注意MetaInfo序列化时，需要使用类似BinCode这样的定长序列化框架，否则若类似Rmp的话会导致MetaInfo在不同数据时，长度不一致
const TABLE_META_INFO_SIZE: usize = 40;

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct MetaInfo {
    level: u64,
    version: u64,
    data_part_len: u64,
    index_len: u64,
    crc_code: u64
}

#[derive(Serialize, Deserialize)]
struct ExtraInfo {
    vec_index: Vec<(Vec<u8>, Position)>,
    scope: Scope,
    filter: GrowableBloom,
    size_of_data: usize,
}

#[derive(Debug)]
struct MemTable {
    // MemTable切片，管理MemTable和ImmutableMemTable
    mem_table_slice: RwLock<[(MemMap, u64); 2]>
}

#[derive(Debug)]
pub(crate) struct Manifest {
    _path: Arc<PathBuf>,
    /// SSTable有序存储集合
    ss_tables_map: SsTableMap,
    /// Level层级Vec
    /// 以索引0为level-0这样的递推，存储文件的gen值
    level_slice: LevelSlice,
    /// SSTable集合占有磁盘大小
    size_of_disk: u64,
    /// 用于防止SSTable重合的同步Buffer
    /// 内部会存储SSTable的Gen，
    /// 判定meet成功时移除对应Gen，避免收集重复SSTable
    sync_buffer_of_meet: Mutex<HashSet<i64>>,
    position_cache: tokio::sync::Mutex<LruCache<(i64, Position), Vec<CommandData>>>
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Hash, PartialOrd, Eq)]
pub(crate) struct Position {
    start: u64,
    len: usize
}

impl MemTable {
    pub(crate) fn new(mem_map: MemMap) -> Self {
        let mem_occupied = mem_map.iter()
            .map(|(key, value)| {
                (key.len() + value.get_data_len_for_rmp()) as u64
            })
            .sum();
        MemTable { mem_table_slice: RwLock::new([(mem_map, mem_occupied), (MemMap::new(), 0)]) }
    }

    pub(crate) async fn insert_data(&self, key: Vec<u8>, value: CommandData) {
        let mut mem_table_slice = self.mem_table_slice.write().await;

        mem_table_slice[0].1 += (key.len() + value.get_data_len_for_rmp()) as u64;
        let _ignore = mem_table_slice[0].0.insert(key, value);
    }

    pub(crate) async fn mem_table_is_empty(&self) -> bool {
        let mem_table_slice = self.mem_table_slice.read().await;

        mem_table_slice[0].0.is_empty()
    }

    pub(crate) async fn mem_table_len(&self) -> usize {
        let mem_table_slice = self.mem_table_slice.read().await;

        mem_table_slice[0].0.len()
    }

    async fn is_threshold_exceeded_minor(&self, threshold_size_with_mem_table: u64) -> bool {
        self.mem_table_slice.read()
            .await[0].1 > threshold_size_with_mem_table
    }

    /// MemTable交换并分解
    async fn table_swap(&self) -> (Vec<Vec<u8>>, Vec<CommandData>){
        let mut mem_table_slice = self.mem_table_slice.write().await;

        mem_table_slice.swap(0, 1);
        mem_table_slice[0] = (MemMap::new(), 0);
        mem_table_slice[1].0
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .unzip()
    }

    async fn get_cmd_data(&self, key: &[u8]) -> Option<CommandData> {
        let mem_table_slice = self.mem_table_slice.read().await;

        mem_table_slice[0].0.get(key)
            .or_else(|| mem_table_slice[1].0.get(key))
            .map(CommandData::clone)
    }
}

impl MetaInfo {
    /// 将MetaInfo自身写入对应的IOHandler之中
    async fn write_to_file_and_flush(&self, io_handler: &IOHandler) -> Result<()> {
        let _ignore = io_handler.write(bincode::serialize(&self)?).await?;
        io_handler.flush().await?;
        Ok(())
    }

    /// 从对应文件的IOHandler中将MetaInfo读取出来
    async fn read_to_file(io_handler: &IOHandler) -> Result<Self> {
        let start_pos = io_handler.file_size().await? - TABLE_META_INFO_SIZE as u64;
        let table_meta_info = io_handler.read_with_pos(start_pos, TABLE_META_INFO_SIZE).await?;

        Ok(bincode::deserialize(table_meta_info.as_slice())?)
    }
}

impl Manifest {
    pub(crate) fn new(mut ss_tables_map: SsTableMap, path: Arc<PathBuf>, cache_size: usize) -> Result<Self> {
        // 获取ss_table分级Vec
        let level_slice = Self::level_layered(&mut ss_tables_map);

        let sync_buffer_of_meet = Mutex::new(ss_tables_map.keys()
            .map(i64::clone)
            .collect());

        let size_of_disk = ss_tables_map.values()
            .map(SsTable::get_size_of_disk)
            .sum();

        let position_cache = tokio::sync::Mutex::new(LruCache::new(NonZeroUsize::new(cache_size)
            .ok_or(KvsError::CacheSizeOverFlow)?));

        Ok(Self { _path: path, ss_tables_map, level_slice, size_of_disk, sync_buffer_of_meet, position_cache })
    }

    /// 使用ss_tables返回LevelVec
    /// 由于ss_tables是有序的，level_vec的内容应当是从L0->LN，旧->新
    fn level_layered(ss_tables: &mut SsTableMap) -> LevelSlice {
        let mut level_slice = [Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        for ss_table in ss_tables.values() {
            let level = ss_table.get_level();
            level_slice[level].push(ss_table.get_gen());
        }
        level_slice
    }

    #[allow(clippy::unwrap_used)]
    pub(crate) async fn insert_ss_table_with_index(&mut self, ss_table: SsTable, index: usize) {
        let gen = ss_table.get_gen();
        let level = ss_table.get_level();

        self.size_of_disk += ss_table.get_size_of_disk();
        let _ignore = self.ss_tables_map.insert(gen, ss_table);
        self.level_slice[level].insert(index, gen);
        let _ignore1 = self.sync_buffer_of_meet.lock().unwrap()
            .insert(gen);
    }

    #[allow(clippy::unwrap_used)]
    pub(crate) async fn insert_ss_table_with_index_batch(&mut self, ss_tables: Vec<SsTable>, index: usize) {
        let vec_gen = ss_tables.into_iter()
            .map(|ss_table| {
                let gen = ss_table.get_gen();
                let level = ss_table.get_level();

                self.size_of_disk += ss_table.get_size_of_disk();
                let _ignore = self.ss_tables_map.insert(gen, ss_table);
                self.level_slice[level].insert(index, gen);
                gen
            })
            .collect_vec();

        let mut sync_buffer_of_meet = self.sync_buffer_of_meet.lock().unwrap();

        sync_buffer_of_meet.extend(vec_gen);
    }

    /// 删除指定的过期gen
    #[allow(clippy::unwrap_used)]
    pub(crate) async fn retain_with_vec_gen_and_level(&mut self, vec_expired_gen: &[i64]) -> Result<()> {
        self.size_of_disk -= vec_expired_gen.iter()
            .map(|gen| self.get_ss_table(gen).map(SsTable::get_size_of_disk).unwrap_or(0))
            .sum::<u64>();

        // 遍历过期Vec对数据进行旧文件删除
        for expired_gen in vec_expired_gen.iter() {
            let _ignore = self.ss_tables_map.remove(expired_gen);
            fs::remove_file(log_path(&self._path, *expired_gen))?;
        }

        // 将存储的Level表中含有该gen的SSTable一并删除
        for vec_level in &mut self.level_slice {
            vec_level.retain(|gen| !vec_expired_gen.contains(gen));
        }
        self.sync_buffer_of_meet.lock().unwrap()
            .retain(|gen| !vec_expired_gen.contains(gen));

        Ok(())
    }

    pub(crate) fn get_level_vec(&self, level: usize) -> &Vec<i64> {
        &self.level_slice[level]
    }

    pub(crate) fn get_vec_ss_table_with_level(&self, level: usize) -> Vec<&SsTable> {
        self.level_slice[level]
            .iter()
            .filter_map(|gen| self.ss_tables_map.get(gen))
            .collect_vec()
    }

    pub(crate) fn get_ss_table(&self, gen: &i64) -> Option<&SsTable> {
        self.ss_tables_map.get(gen)
    }

    fn is_threshold_exceeded_major(&self, sst_size: usize, level: usize, sst_magnification: usize) -> bool {
        self.level_slice[level].len() > (sst_size.pow(level as u32) * sst_magnification)
    }

    /// 使用Key从现有SSTables中获取对应的数据
    pub(crate) async fn get_data_for_ss_tables(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        // Level 0的SSTable是无序且SSTable间的数据是可能重复的
        for ss_table in self.get_vec_ss_table_with_level(0).iter().rev() {
            if let Some(cmd_data) = ss_table.query_with_key(key, &self.position_cache).await? {
                return Ok(cmd_data.get_value_owner());
            }
        }
        // Level 1-7的数据排布有序且唯一，因此在每一个等级可以直接找到唯一一个Key可能在范围内的SSTable
        let key_scope = Scope::from_key(key);
        for level in 1..7 {
            if let Some(ss_table) = self.get_vec_ss_table_with_level(level)
                .iter()
                .rfind(|ss_table| ss_table.get_scope().meet(&key_scope))
            {
                if let Some(cmd_data) = ss_table.query_with_key(key, &self.position_cache).await? {
                    return Ok(cmd_data.get_value_owner());
                }
            }
        }

        Ok(None)
    }

    pub(crate) fn get_ss_table_batch(&self, vec_gen: &[i64]) -> Option<Vec<&SsTable>> {
        vec_gen.iter()
            .map(|gen| self.get_ss_table(gen))
            .collect::<Option<Vec<&SsTable>>>()
    }

    #[allow(clippy::unwrap_used)]
    pub(crate) fn get_meet_scope_ss_tables(&self, level: usize, scope: &Scope) -> Vec<&SsTable> {
        self.get_level_vec(level).iter()
            .filter_map(|gen| self.get_ss_table(gen))
            .filter(|ss_table| ss_table.get_scope().meet(scope) &&
                self.sync_buffer_of_meet.lock().unwrap().remove(&ss_table.get_gen()))
            .collect_vec()
    }

    pub(crate) fn get_index(&self, level: usize, source_gen: i64) -> Option<usize> {
        self.level_slice[level].iter()
            .enumerate()
            .find(|(_ , gen)| source_gen.eq(*gen))
            .map(|(index, _)| index)
    }
}

impl Position {
    /// 通过稀疏索引与指定Key进行获取对应Position
    pub(crate) fn from_sparse_index_with_key<'a>(sparse_index: &'a SkipMap<Vec<u8>, Position>, key: &'a [u8]) -> Option<&'a Self> {
        sparse_index.into_iter()
            .rev()
            .find(|(key_item, _)| !key.cmp(key_item).eq(&Ordering::Less))
            .map(|(_, value_item)| value_item)
    }
}

/// CommandData数据分片，尽可能将数据按给定的分片大小：file_size，填满一片（可能会溢出一些）
/// 保持原有数据的顺序进行分片，所有第一片分片中最后的值肯定会比其他分片开始的值Key排序较前（如果vec_data是以Key从小到大排序的话）
async fn data_sharding(mut vec_data: Vec<CommandData>, file_size: usize, config: &Config, with_gen: bool) -> MergeShardingVec {
    // 向上取整计算STable数量
    let part_size = (vec_data.iter()
        .map(|cmd| cmd.get_data_len_for_rmp())
        .sum::<usize>() + file_size - 1) / file_size;

    vec_data.reverse();
    let mut vec_sharding = vec![(0, Vec::new()); part_size];
    let slice = vec_sharding.as_mut_slice();
    for i in 0 .. part_size {
        // 减小create_gen影响的时间
        if with_gen {
            slice[i].0 = config.create_gen()
        }
        let mut data_len = 0;
        while !vec_data.is_empty() {
            if let Some(cmd_data) = vec_data.pop() {
                data_len += cmd_data.get_data_len_for_rmp();
                if data_len >= file_size && i < part_size - 1 {
                    slice[i + 1].1.push(cmd_data);
                    break
                }
                slice[i].1.push(cmd_data);
            } else { break }
        }
    }
    // 过滤掉没有数据的切片
    vec_sharding.retain(|(_, vec)| !vec.is_empty());
    vec_sharding
}

#[test]
fn test_meta_info() -> Result<()> {
    let info = MetaInfo {
        level: 0,
        version: 0,
        data_part_len: 0,
        index_len: 0,
        crc_code: 0
    };

    let vec_u8 = bincode::serialize(&info)?;

    assert_eq!(vec_u8.len(), TABLE_META_INFO_SIZE);

    Ok(())
}