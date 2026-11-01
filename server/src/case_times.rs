//! 案件举报时间缓存。
//!
//! 列表接口的 createdAt 经常为空，部分案件还会给错；只有详情里的
//! `reports[].createdAt` 才可靠。这里按案件号缓存详情里的最早举报时间，
//! 重启后从数据目录复用，避免每次刷新都重新请求一遍详情。
//!
//! 缓存值约定：
//! - 正常时间字符串：已核对过，用这个时间判断是否超期；
//! - 空字符串：详情也拿不到举报时间，按“时间未知”处理，不再反复请求。

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "case_report_times.json";

#[derive(Clone)]
pub struct CaseTimes {
    inner: Arc<Mutex<Inner>>,
    path: PathBuf,
}

struct Inner {
    times: HashMap<i64, String>,
    dirty: bool,
    /// 已经有一次落盘排在阻塞线程上了，别同时写同一个 tmp 文件。
    saving: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct Stored {
    #[serde(default)]
    times: HashMap<String, String>,
}

impl CaseTimes {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join(FILE_NAME);
        let times = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Stored>(&text).ok())
            .map(|stored| {
                stored
                    .times
                    .into_iter()
                    .filter_map(|(key, value)| key.parse::<i64>().ok().map(|id| (id, value)))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            inner: Arc::new(Mutex::new(Inner {
                times,
                dirty: false,
                saving: false,
            })),
            path,
        }
    }

    /// `Some("")` 表示已核对过但详情也没有时间；`None` 表示还没核对过。
    pub fn get(&self, case_id: i64) -> Option<String> {
        self.lock().times.get(&case_id).cloned()
    }

    /// 记录详情里的最早举报时间。
    pub fn set(&self, case_id: i64, time: &str) {
        let time = time.trim();
        if case_id <= 0 || time.is_empty() {
            return;
        }
        self.remember(case_id, time.to_string());
    }

    /// 详情成功返回但没有举报时间：记为“已核对、时间未知”，避免反复请求。
    pub fn set_unknown(&self, case_id: i64) {
        if case_id > 0 {
            self.remember(case_id, String::new());
        }
    }

    fn remember(&self, case_id: i64, time: String) {
        let mut inner = self.lock();
        if inner.times.get(&case_id) == Some(&time) {
            return;
        }
        inner.times.insert(case_id, time);
        inner.dirty = true;
    }

    /// 有变更时才落盘，并且写到阻塞线程：全量 JSON 序列化 + 写文件不该占住异步执行器。
    pub fn save_if_dirty(&self) {
        {
            let mut inner = self.lock();
            if !inner.dirty || inner.saving {
                return;
            }
            inner.saving = true;
        }
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            this.save();
            this.lock().saving = false;
        });
    }

    /// 有变更时原子写入数据目录；没有变更就跳过。
    pub fn save(&self) {
        let mut inner = self.lock();
        if !inner.dirty {
            return;
        }
        let stored = Stored {
            times: inner
                .times
                .iter()
                .map(|(id, time)| (id.to_string(), time.clone()))
                .collect(),
        };
        let Ok(text) = serde_json::to_string(&stored) else {
            return;
        };
        let temp = self.path.with_extension("json.tmp");
        if std::fs::write(&temp, text).is_err() {
            return;
        }
        // Windows 的 rename 不能覆盖已存在的文件：先删旧文件再换名。
        let saved = std::fs::rename(&temp, &self.path).is_ok()
            || (std::fs::remove_file(&self.path).is_ok()
                && std::fs::rename(&temp, &self.path).is_ok());
        if saved {
            inner.dirty = false;
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// 详情里的认领信息缓存。
///
/// 列表接口只有 reportNum，没有 checkerName；举报次数大于 1 的案件必须查一次
/// get-info，看最新那条举报的 checkerName 才知道有没有人认领。这里按案件号
/// 缓存结果，只保留最近用到的 5 个，避免每次轮询都把整页详情重拉一遍。
#[derive(Clone, Default)]
pub struct ClaimCache {
    inner: Arc<Mutex<Vec<(i64, String, Instant)>>>,
}

const CLAIM_CACHE_CAP: usize = 5;
const CLAIM_CACHE_TTL: Duration = Duration::from_secs(600);

impl ClaimCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, id: i64) -> Option<String> {
        let mut entries = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        entries.retain(|(_, _, at)| now.duration_since(*at) < CLAIM_CACHE_TTL);
        let index = entries.iter().position(|(key, _, _)| *key == id)?;
        let entry = entries.remove(index);
        let value = entry.1.clone();
        entries.push(entry);
        Some(value)
    }

    pub fn set(&self, id: i64, checker_name: &str) {
        let mut entries = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        entries.retain(|(key, _, _)| *key != id);
        entries.push((id, checker_name.to_string(), Instant::now()));
        while entries.len() > CLAIM_CACHE_CAP {
            entries.remove(0);
        }
    }

    /// 某个案件是否已经缓存过认领信息（空串也算一个有效结果）。
    pub fn contains(&self, id: i64) -> bool {
        self.get(id).is_some()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().map(|entries| entries.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_reloads_times() {
        let dir = std::env::temp_dir().join(format!("qf-case-times-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(FILE_NAME);
        let _ = std::fs::remove_file(&path);

        let store = CaseTimes::load(&dir);
        store.set(101, "2026-01-17 12:36");
        store.set_unknown(102);
        store.save();
        // 第二次保存会覆盖已存在的文件（Windows 上 rename 不能直接覆盖）。
        store.set(101, "2026-02-01 11:48");
        store.save();

        let reloaded = CaseTimes::load(&dir);
        assert_eq!(reloaded.get(101).as_deref(), Some("2026-02-01 11:48"));
        assert_eq!(reloaded.get(102).as_deref(), Some(""));
        assert!(reloaded.get(103).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn claim_cache_keeps_only_five_and_honours_lru() {
        let cache = ClaimCache::new();
        for id in 1..=7 {
            cache.set(id, "--");
        }
        assert_eq!(cache.len(), 5);
        assert!(cache.get(1).is_none());
        assert!(cache.get(2).is_none());
        assert_eq!(cache.get(7).as_deref(), Some("--"));

        // 命中会把条目移到队尾，再插入新条目时清掉的是最久未用的那个。
        cache.get(3);
        cache.set(8, "张三");
        assert_eq!(cache.len(), 5);
        assert!(cache.get(3).is_some());
        assert!(cache.get(4).is_none());
        assert!(cache.contains(8));
        assert_eq!(cache.get(8).as_deref(), Some("张三"));
    }
}
