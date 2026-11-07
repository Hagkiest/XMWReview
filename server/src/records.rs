//! 审核记录与偏好：**纯本地 JSON**，不连任何数据库。
//!
//! 这是开源版与主版本差别最大的一个模块。主版本把记录写进云端 PostgreSQL
//! 并按应用账号跨设备同步；这里没有账号体系，所以：
//!
//! - 存储落在 `data_dir()/records.json` 和 `data_dir()/prefs.json`，两个文件都是
//!   `{ 归属键: 内容 }` 的扁平 JSON，可以直接打开发看、也可以整目录备份；
//! - 归属键只有 `token_hash(社区 access-token)` 一种，换 token 就是换一份历史；
//! - 隐私口径不变：磁盘上不落明文 token，只落 SHA-256 和掩码；
//! - 接口签名和主版本保持一致（方法全是 `async`），`main.rs` / `satisfaction.rs`
//!   不需要因为「本地还是云端」写分支。
//!
//! 写入路径同样是「先本地」：服务不再主动写审核记录，历史由前端遍历社区
//! 「我已判决」列表得到（见 `main.rs` 的 `synced_history`），这里的记录只在
//! 满意度回填、判决号回填和删除时变化。

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

use serde::{Deserialize, Serialize};

use crate::log;

/// 一条审核记录（前端 camelCase 直接可用）。
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub case_id: i64,
    pub judge_record_id: i64,
    pub nickname: String,
    pub content: String,
    pub action: i16,
    pub remark: String,
    /// auto = 服务器自动审核，manual = 人工提交
    pub source: String,
    pub confidence: Option<f32>,
    pub satisfaction: Option<f32>,
    pub satisfaction_fetched: bool,
    /// 上次刷新满意度的时间（epoch 毫秒），0 = 还没刷过。
    /// 旧版本 pending 文件没有这个字段，所以带 default。
    #[serde(default)]
    pub satisfaction_at: i64,
    /// 手动获取机会用掉了没有（真正取到值才算）。
    #[serde(default)]
    pub satisfaction_manual_used: bool,
    /// 系统自动获取机会用掉了没有（真正取到值才算）。
    #[serde(default)]
    pub satisfaction_auto_used: bool,
    pub case_report_time: String,
    /// epoch 毫秒，前端直接 new Date() 用
    pub created_at: i64,
    pub token_masked: String,
    /// 本地存储没有「待同步」状态，字段保留是为了和主版本的前端类型一致。
    #[serde(default, skip_serializing_if = "is_false")]
    pub pending: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// 一条记录在磁盘上的形状：归属键 → 该键下的记录列表。
type Store = HashMap<String, Vec<Record>>;

/// 偏好设置的一条：`{ 归属键: { name, data, updatedMs } }`。
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct PrefEntry {
    name: String,
    data: serde_json::Value,
    updated_ms: i64,
}

/// 待刷新满意度的一条记录。
#[derive(Clone, Debug)]
pub struct DueSatisfaction {
    pub account_key: String,
    pub case_id: i64,
    pub judge_record_id: i64,
}

/// 满意度 / 判决号的刷新周期：和主版本的 SQL `INTERVAL '5 hours'` 对齐。
const REFRESH_MS: i64 = 5 * 60 * 60 * 1000;

#[derive(Clone)]
pub struct RecordsStore {
    inner: Arc<Inner>,
}

struct Inner {
    records_path: PathBuf,
    prefs_path: PathBuf,
    /// 内存里持一份，读多写少的场景避免每次都解析文件；落盘时整份覆盖。
    records: Mutex<Store>,
    prefs: Mutex<HashMap<String, PrefEntry>>,
}

impl RecordsStore {
    pub fn new(exe_dir: &PathBuf) -> Self {
        Self::at(crate::data_dir(), exe_dir)
    }

    /// 指定数据目录的那一份：单测用它，免得几个用例抢同一个 QF_DATA_DIR 环境变量。
    fn at(dir: PathBuf, exe_dir: &PathBuf) -> Self {
        let records_path = dir.join("records.json");
        let prefs_path = dir.join("prefs.json");

        // 主版本把「云端没连上的记录」写在 records_pending.jsonl；这里把它读成正式记录，
        // 老部署目录换到开源版时历史不会凭空消失（旧文件不删，避免误操作丢数据）。
        let mut records: Store = read_json(&records_path).unwrap_or_default();
        let pending_path = dir.join("records_pending.jsonl");
        let mut imported = 0usize;
        if let Ok(text) = std::fs::read_to_string(&pending_path) {
            for line in text.lines() {
                let Ok((key, record)) = serde_json::from_str::<(String, Record)>(line) else {
                    continue;
                };
                let list = records.entry(key).or_default();
                if list.iter().any(|item| item.case_id == record.case_id) {
                    continue;
                }
                let mut record = record;
                record.pending = false;
                list.push(record);
                imported += 1;
            }
        }
        if imported > 0 {
            log(format!("从本地待同步文件导入 {imported} 条历史审核记录"));
            write_json(&records_path, &records);
        }
        // 更早的版本把这两个文件写在 exe 同目录，首次启动复制过来。
        if records.is_empty() {
            if let Some(legacy) = read_json(&exe_dir.join("records.json")) {
                records = legacy;
            }
        }

        // prefs_path 要先读一次再移进结构体：结构体字段按书写顺序移动所有权。
        let prefs: HashMap<String, PrefEntry> = read_json(&prefs_path).unwrap_or_default();
        Self {
            inner: Arc::new(Inner {
                records_path,
                prefs_path,
                records: Mutex::new(records),
                prefs: Mutex::new(prefs),
            }),
        }
    }

    /// 开源版没有云端库：前端据此显示「仅本地保存」。
    pub fn configured(&self) -> bool {
        false
    }

    /// 读取当前身份可见的审核记录。没有云端，第二个返回值恒为 false。
    pub async fn list_for(
        &self,
        account_key: Option<&str>,
        token: &str,
        limit: i64,
    ) -> (Vec<Record>, bool) {
        let keys = visible_keys(account_key, token);
        if keys.is_empty() {
            return (Vec::new(), false);
        }
        let guard = self.records();
        let mut merged: Vec<Record> = keys
            .iter()
            .flat_map(|key| guard.get(key).cloned().unwrap_or_default())
            .collect();
        // 多个键可能指向同一批记录（传了 account_key 又传了 token），按案件号去重。
        merged.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        let mut seen = HashSet::new();
        merged.retain(|record| seen.insert(record.case_id));
        merged.truncate(limit.max(1) as usize);
        (merged, false)
    }

    /// 回填满意度（只更新当前归属键下的记录）。
    /// `source`：manual = 界面手动获取，auto = 后台每日刷新；只有取到值才调用，用掉对应机会。
    pub async fn set_satisfaction_by_key(
        &self,
        account_key: &str,
        case_id: i64,
        judge_record_id: i64,
        satisfaction: Option<f32>,
        source: &str,
    ) -> Result<(), String> {
        self.update(account_key, case_id, |record| {
            record.satisfaction = satisfaction;
            record.satisfaction_fetched = true;
            record.satisfaction_at = now_ms();
            if source == "manual" {
                record.satisfaction_manual_used = true;
            } else {
                record.satisfaction_auto_used = true;
            }
            if judge_record_id > 0 {
                record.judge_record_id = judge_record_id;
            }
        })
    }

    /// 每批案件的两次获取机会有没有用掉：(manual_used, auto_used)。
    pub async fn satisfaction_slots(
        &self,
        account_key: &str,
        case_ids: &[i64],
    ) -> Result<HashMap<i64, (bool, bool)>, String> {
        let mut slots = HashMap::new();
        if case_ids.is_empty() {
            return Ok(slots);
        }
        let guard = self.records();
        if let Some(list) = guard.get(account_key) {
            for record in list {
                if case_ids.contains(&record.case_id) {
                    slots.insert(
                        record.case_id,
                        (record.satisfaction_manual_used, record.satisfaction_auto_used),
                    );
                }
            }
        }
        Ok(slots)
    }

    /// 记一次「已经尝试过」的时间：暂时取不到满意度时用，避免每轮都撞同一个案件。
    pub async fn mark_satisfaction_attempt(
        &self,
        account_key: &str,
        case_id: i64,
    ) -> Result<(), String> {
        self.update(account_key, case_id, |record| {
            record.satisfaction_at = now_ms();
        })
    }

    /// 已满 24 小时、且 24 小时内没刷新过满意度的记录（后台每日刷新用）。
    /// 「正常显示」（action=1）的案件不抓取满意度，直接排除。
    pub async fn due_satisfaction(&self, limit: i64) -> Result<Vec<DueSatisfaction>, String> {
        let deadline = now_ms() - REFRESH_MS;
        let guard = self.records();
        let mut due: Vec<DueSatisfaction> = Vec::new();
        for (key, list) in guard.iter() {
            for record in list {
                if record.action == 1
                    || record.satisfaction_auto_used
                    || record.created_at > deadline
                    || (record.satisfaction_at != 0 && record.satisfaction_at > deadline)
                {
                    continue;
                }
                due.push(DueSatisfaction {
                    account_key: key.clone(),
                    case_id: record.case_id,
                    judge_record_id: record.judge_record_id,
                });
            }
        }
        // 案件号小的先处理：同一把 token 下，越早的案件越早满 24 小时。
        due.sort_by_key(|item| item.case_id);
        due.truncate(limit.max(0) as usize);
        Ok(due)
    }

    /// 这批案件里「正常显示」（action=1）的记录：不抓取满意度。
    pub async fn normal_display_cases(
        &self,
        account_key: &str,
        case_ids: &[i64],
    ) -> Result<HashSet<i64>, String> {
        let mut excluded = HashSet::new();
        if case_ids.is_empty() {
            return Ok(excluded);
        }
        let guard = self.records();
        if let Some(list) = guard.get(account_key) {
            for record in list {
                if record.action == 1 && case_ids.contains(&record.case_id) {
                    excluded.insert(record.case_id);
                }
            }
        }
        Ok(excluded)
    }

    /// 删除一条审核记录。归属键可以多个，同 token 的旧键一起清掉。
    pub async fn delete_for(
        &self,
        account_key: Option<&str>,
        token: &str,
        case_id: i64,
    ) -> Result<(), String> {
        let keys = visible_keys(account_key, token);
        if keys.is_empty() {
            return Ok(());
        }
        let mut guard = self.records();
        let mut changed = false;
        for key in &keys {
            if let Some(list) = guard.get_mut(key) {
                let before = list.len();
                list.retain(|record| record.case_id != case_id);
                changed |= list.len() != before;
            }
        }
        drop(guard);
        if changed {
            self.save_records();
        }
        Ok(())
    }

    /// 回填判决记录 ID（提交后同步一次；已有非零值时不覆盖）。
    pub async fn set_judge_record_id_by_key(
        &self,
        account_key: &str,
        case_id: i64,
        judge_record_id: i64,
    ) -> Result<(), String> {
        if judge_record_id <= 0 {
            return Ok(());
        }
        self.update(account_key, case_id, |record| {
            if record.judge_record_id == 0 {
                record.judge_record_id = judge_record_id;
            }
        })
    }

    // ---- 偏好设置 ----

    /// 读取指定归属键的偏好；没有记录时返回空对象。第二个值是更新时间（毫秒）。
    pub async fn get_prefs_by_key(
        &self,
        account_key: &str,
    ) -> Result<(serde_json::Value, i64), String> {
        let guard = self.prefs();
        Ok(match guard.get(account_key) {
            Some(entry) => (entry.data.clone(), entry.updated_ms),
            None => (serde_json::json!({}), 0),
        })
    }

    /// 保存指定归属键的偏好（整份覆盖）。
    pub async fn save_prefs_by_key(
        &self,
        account_key: &str,
        account_name: &str,
        data: &serde_json::Value,
    ) -> Result<(), String> {
        let entry = PrefEntry {
            name: account_name.to_string(),
            data: data.clone(),
            updated_ms: now_ms(),
        };
        self.prefs().insert(account_key.to_string(), entry);
        self.save_prefs();
        Ok(())
    }

    // ---- 内部：读写与锁 ----

    fn records(&self) -> MutexGuard<'_, Store> {
        match self.inner.records.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn prefs(&self) -> MutexGuard<'_, HashMap<String, PrefEntry>> {
        match self.inner.prefs.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 改一条记录并落盘；找不到就报错，调用方按「记录不存在」处理。
    fn update(
        &self,
        account_key: &str,
        case_id: i64,
        patch: impl FnOnce(&mut Record),
    ) -> Result<(), String> {
        let mut guard = self.records();
        match guard.get_mut(account_key) {
            Some(list) => match list.iter_mut().find(|record| record.case_id == case_id) {
                Some(record) => {
                    patch(record);
                    drop(guard);
                    self.save_records();
                    Ok(())
                }
                None => Err(format!("找不到案件 #{case_id} 的记录")),
            },
            None => Err(format!("归属键 {account_key} 下没有记录")),
        }
    }

    fn save_records(&self) {
        let guard = self.records();
        write_json(&self.inner.records_path, &*guard);
    }

    fn save_prefs(&self) {
        let guard = self.prefs();
        write_json(&self.inner.prefs_path, &*guard);
    }
}

/// 读一个 JSON 文件；不存在或损坏时返回 None（损坏会留一条日志，不让服务起不来）。
fn read_json<T: for<'de> Deserialize<'de>>(path: &PathBuf) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<T>(&text) {
        Ok(value) => Some(value),
        Err(error) => {
            log(format!(
                "本地数据文件 {} 解析失败，按空处理：{error}",
                path.display()
            ));
            None
        }
    }
}

/// 整份覆盖：先写 .tmp 再改名，避免断电 / 并发时留下半个 JSON。
fn write_json<T: Serialize>(path: &PathBuf, value: &T) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let temp = path.with_extension("json.tmp");
    let text = match serde_json::to_string_pretty(value) {
        Ok(text) => text,
        Err(error) => {
            log(format!("写入 {} 失败：{error}", path.display()));
            return;
        }
    };
    if let Err(error) = std::fs::write(&temp, text) {
        log(format!("写入 {} 失败：{error}", temp.display()));
        return;
    }
    // Windows 上 rename 不能覆盖已存在文件，先删。
    if std::fs::rename(&temp, path).is_err() {
        let _ = std::fs::remove_file(path);
        if let Err(error) = std::fs::rename(&temp, path) {
            log(format!("写入 {} 失败：{error}", path.display()));
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or_default()
}

/// 一次查询要覆盖的归属键。没有账号体系，所以只有 token 哈希一种；
/// 保留多键的形状，是为了和主版本的调用签名一致。
pub fn visible_keys(_account_key: Option<&str>, token: &str) -> Vec<String> {
    let hash = token_hash(token);
    if hash.is_empty() {
        Vec::new()
    } else {
        vec![hash]
    }
}

/// 解 JWT payload（不做签名校验，只读身份信息）。
fn jwt_payload(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = crate::ai::b64_decode(payload);
    if bytes.is_empty() {
        return None;
    }
    serde_json::from_slice::<serde_json::Value>(&bytes).ok()
}

/// 当前社区用户 ID：历史同步用它匹配已审核案件的 checkerId。
pub(crate) fn token_user_id(token: &str) -> Option<i64> {
    let payload = jwt_payload(token)?;
    for field in ["userId", "uid", "user_id", "sub", "accountId", "id"] {
        let raw = &payload[field];
        let value = raw
            .as_i64()
            .or_else(|| raw.as_str().and_then(|text| text.trim().parse::<i64>().ok()));
        if let Some(value) = value.filter(|value| *value > 0) {
            return Some(value);
        }
    }
    None
}

/// 诊断用：只返回 JWT payload 的字段名，不返回值（避免泄露 token 内容）。
pub(crate) fn token_payload_keys(token: &str) -> Vec<String> {
    jwt_payload(token)
        .and_then(|value| {
            value
                .as_object()
                .map(|object| object.keys().cloned().collect::<Vec<_>>())
        })
        .unwrap_or_default()
}

/// SHA-256 十六进制：磁盘上不存明文 token，只存这个。
pub fn token_hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut hasher = Sha256::new();
    hasher.update(trimmed.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub(crate) fn mask_token(token: &str) -> String {
    let chars: Vec<char> = token.trim().chars().collect();
    if chars.len() <= 8 {
        return "****".to_string();
    }
    let head: String = chars.iter().take(6).collect();
    let tail: String = chars.iter().skip(chars.len() - 4).collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(case_id: i64, action: i16, created_at: i64) -> Record {
        Record {
            case_id,
            judge_record_id: 0,
            nickname: "用户".to_string(),
            content: "内容".to_string(),
            action,
            remark: String::new(),
            source: "manual".to_string(),
            confidence: None,
            satisfaction: None,
            satisfaction_fetched: false,
            satisfaction_at: 0,
            satisfaction_manual_used: false,
            satisfaction_auto_used: false,
            case_report_time: String::new(),
            created_at,
            token_masked: "ma…sk".to_string(),
            pending: false,
        }
    }

    fn dir_of(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("qf-oss-records-{tag}-{}", std::process::id()))
    }

    /// 每个用例一个临时目录，别碰真实数据。
    fn store(tag: &str) -> RecordsStore {
        let dir = dir_of(tag);
        let _ = std::fs::remove_dir_all(&dir);
        RecordsStore::at(dir.clone(), &dir)
    }

    #[test]
    fn token_hash_is_stable_and_hides_secret() {
        let token = "sk-test-1234567890abcdef";
        let hash = token_hash(token);
        assert_eq!(hash.len(), 64);
        assert_eq!(hash, token_hash(&format!("  {token}  ")));
        assert!(!hash.contains("1234567890"));
        assert!(token_hash("   ").is_empty());
    }

    #[test]
    fn visible_keys_only_cover_token_hash() {
        let token = "sk-test-1234567890abcdef";
        assert_eq!(visible_keys(None, token), vec![token_hash(token)]);
        assert!(visible_keys(Some("acct:7"), "   ").is_empty());
    }

    #[test]
    fn token_user_id_reads_community_id() {
        // payload: {"userId":3369552}
        let token = "x.eyJ1c2VySWQiOjMzNjk1NTJ9.y";
        assert_eq!(token_user_id(token), Some(3_369_552));
        assert_eq!(token_user_id("not-a-jwt"), None);
    }

    #[tokio::test]
    async fn local_store_roundtrips_and_survives_reload() {
        let tag = "roundtrip";
        let token = "aaaaaaaa.bbbbbbbb";
        let key = token_hash(token);
        let store = store(tag);
        let (list, online) = store.list_for(None, token, 50).await;
        assert!(list.is_empty() && !online);

        store
            .save_prefs_by_key(&key, "访客", &serde_json::json!({ "interval": 60 }))
            .await
            .unwrap();
        let (prefs, updated) = store.get_prefs_by_key(&key).await.unwrap();
        assert_eq!(prefs["interval"], 60);
        assert!(updated > 0);

        // 直接往文件里塞一条记录，再用同一个目录重建 store，验证真的落盘了。
        let dir = dir_of(tag);
        let mut seeded: Store = HashMap::new();
        seeded.insert(key.clone(), vec![record(101, 3, now_ms())]);
        write_json(&dir.join("records.json"), &seeded);

        let reloaded = RecordsStore::at(dir.clone(), &dir);
        let (list, _) = reloaded.list_for(None, token, 50).await;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].case_id, 101);

        reloaded
            .set_satisfaction_by_key(&key, 101, 555, Some(0.75), "manual")
            .await
            .unwrap();
        let (list, _) = reloaded.list_for(None, token, 50).await;
        assert_eq!(list[0].judge_record_id, 555);
        assert_eq!(list[0].satisfaction, Some(0.75));
        assert!(list[0].satisfaction_at > 0);

        reloaded.delete_for(None, token, 101).await.unwrap();
        let (list, _) = reloaded.list_for(None, token, 50).await;
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn due_satisfaction_skips_normal_display_and_fresh_records() {
        let dir = dir_of("due");
        let _ = std::fs::remove_dir_all(&dir);

        let key = "k".to_string();
        let old = now_ms() - 2 * REFRESH_MS;
        let mut seeded: Store = HashMap::new();
        seeded.insert(
            key.clone(),
            vec![
                record(1, 3, old),      // 该刷
                record(2, 1, old),      // 正常显示，跳过
                record(3, 3, now_ms()), // 未满 24 小时，跳过
                record(4, 2, old),      // 该刷
            ],
        );
        // 直接写文件再按同一目录重建：模拟「昨天记下的记录，今天后台来刷」。
        write_json(&dir.join("records.json"), &seeded);

        let store = RecordsStore::at(dir.clone(), &dir);
        let due = store.due_satisfaction(10).await.unwrap();
        let ids: Vec<i64> = due.iter().map(|item| item.case_id).collect();
        assert_eq!(ids, vec![1, 4]);
        assert!(due.iter().all(|item| item.account_key == key));

        let excluded = store
            .normal_display_cases(&key, &[1, 2, 3])
            .await
            .unwrap();
        assert!(excluded.contains(&2) && !excluded.contains(&1));
    }
}
