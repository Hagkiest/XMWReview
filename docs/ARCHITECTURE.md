# 架构

面向要改行为的人。只讲"读代码不容易看出来"的部分：为什么这么绕、哪些约束不能动。

## 全局

```
浏览器（Next.js 静态导出，由 Rust 同源托管）
   │  fetch /api/*      请求头 access-token: <社区 token>
   ▼
axum 服务  ─── 案件分类缓存（多槽 + stale-while-revalidate）
   │
   ├─ community.rs ──▶ 社区 HTTP 接口（评论列表 / 作品列表 / 详情 / 提交 / 已审核）
   ├─ ai.rs ────────▶ OpenAI 兼容网关（多密钥轮换、限流、冷却）
   ├─ auto.rs ──────▶ 24 小时值守循环（后台任务，和界面走同一套代码）
   ├─ satisfaction.rs ▶ 每日满意度刷新（后台任务）
   └─ records.rs / case_times.rs ──▶ data_dir() 下的 JSON 文件
```

只有两样东西会离开这台机器：社区接口请求和 AI 请求。没有遥测、没有第三方依赖服务。

## 身份：token 就是全部

`session_ctx(headers)` 从 `access-token` 请求头取社区 token，**同步**完成，不查任何存储。
token 既是调用社区接口的凭证，也是审核记录与偏好的归属键——没有账号体系、没有会话票据、没有登录接口。

- `SessionCtx::key()` = `"token:" + sha256(token)`，是记录和偏好的归属键。
- 前端把 token 存在 `localStorage["qingfeng.access-token"]`，刷新页面直接复用。
- token 只在服务端到社区这一跳使用，任何接口都不会回传完整值（只回掩码）。
- 未带 token → 401 / `code 10022` / "请先粘贴社区 access-token"，前端据此显示粘贴框。

**不要**把这个模型改成"服务端存 token"：前端每请求都带头，服务端才是无状态的，
这一点让 `records.json` 的多 token 分区、以及"换浏览器只需重贴 token"能够成立。

## 案件分类：为什么要扫全表

上游列表接口 `reportStatus=0` 只给"未审核"，但里面混着三类不该人工处理的案件：
**已被别人认领**、**发布超过 7 天**、**举报时间取不到**。这三类统一进 `hiddenList`，
界面折叠显示，不参与自动审核，也不能提交。

- `claimed` 判定分两种：`reportNum = 1` 用列表自带的 `checkerName`；`reportNum > 1`
  必须查 `/report/get-info`，取 `list[0].checkerName`（该 list 按举报时间倒序，最新在前）。
  **列表接口没有审核员字段**，只看列表会把还没人认领的多举报案件误判成"已认领"。
  详情结论按案件号进 `ClaimCache`（LRU 5 条、10 分钟），并回填到列表项，
  所以前端拿到的 `checkerName` 一定可信。
- `stale` = 举报时间距今 > `auto::CASE_MAX_AGE_SECS`（7 天）。
- `unknown` = 时间字段解析不出来。**列表接口给的时间不可信**，要用详情接口核对；
  核对结果写进 `case_times.rs` 的 `case_report_times.json`，下次不再请求。

所以一次 `/api/cases` 最坏要发：N 页列表 + 若干条详情。为此有三层优化，改动前先看清：

1. **重叠扫描**（`classified_cases`）：一个 `JoinSet` 同时跑 `Sweep::Page` 和
   `Sweep::Detail`。第 1 页拿到 `total` 就算出真实页数，之后每来一页立刻查时间缓存，
   未命中的当场派详情任务。`PAGE_WINDOW = 4` 限制在飞页数，
   `RESOLVE_CONCURRENCY = 6` 限制在飞详情数，**两个预算在循环每一轮都检查**，
   不然会超支。串行版本好看懂但慢 5–10 倍。
2. **多槽缓存 + stale-while-revalidate**：`CASE_CACHE_SLOTS = 4` 份
   （评论/作品 × 是否带搜索之类），`CASE_CACHE_TTL = 75s`。TTL 必须**长于前端 60 秒轮询**，
   否则每次轮询都是冷缓存。过期时先立即返回旧数据、后台 spawn 重算
   （`case_refreshing` 原子标志防重复重算）。
3. **预算**：`CLASSIFY_BUDGET = 8s`、`RESOLVE_BUDGET = 20s`、`CLASSIFY_MAX_PAGES = 20`。
   任一到了就带着 `truncated = true` 返回，界面会提示统计只覆盖部分。

**提交案件后必须让缓存失效**（`clear_case_cache`）。否则下一个 60 秒轮询还会把刚审完的案件
显示成待审。加新提交路径时最容易漏这一步。

作品模式同构但没有详情扇出（`classified_compositions`），因为作品列表自带 `lastReportTime`；
只有 `reportNum > 1` 的作品才会补一次详情取认领信息（`apply_claim_names`）。

## 审核提交

`verify` 的请求体形状是社区定的，字段名和取值都不能自己发明：

```jsonc
{
  "reportId": 57880,
  "reporterIsSend": 1,          // 作品提交时 userIsSend/reporterIsSend 都是 0/1 的固定组合
  "userIsSend": 1,
  "reportDetailIds": [80440],   // 来自 get-info 的 List[].id，不是案件号
  "action": 3,                  // 1 正常显示 / 2 替换 / 3 删除
  "remark": "…",                // 上限 20 字，超了上游会拒
  "userMessage": "…",
  "reporterMessage": "…"
}
```

- `reportDetailIds` **必须是该案件全部举报记录号**；漏了就只受理一部分。
- 作品提交走 `/report/verify-composition`，且**不带 `userMessage`**、`userIsSend: 0`。
- 评论走 `/report/verify-comment`。
- `community.rs::verify_case` 会重新拉详情收集这些 id；`verify_report` 复用已有详情。
  后台自动审核走的是同一对函数，别在处理器里另写一份拼装逻辑。

## AI 建议

`ai.rs`：

- 提示词 = 固定 `SYSTEM_PROMPT` + 从社区规范 4.0 摘的 `COMMUNITY_RULES`。
- 优先用 tool call（`submit_review`）拿结构化结论；上游不支持 tools 时置
  `TOOLS_UNSUPPORTED` 全局标志，回落到"要求模型直接输出 JSON"的提示词。
- 结论容错：`action` 可能是数字，也可能是"删除"/`2（替换）`/`3.0`，见 `action_of`。
- 密钥轮换顺序 `AI_API_KEY → AI_API_KEY_1..9 → AI_API_KEY_FALLBACK → AI_API_KEYS`，
  每个密钥每分钟最多 10 次（`KEY_RATE_LIMIT`），失败进冷却；
  `/api/ai-status` 返回每个槽位的掩码、状态和剩余冷却秒数，永远不含明文。
- 配置来源优先级：**环境变量 > `build.rs` 编进二进制的混淆常量**。两者都空则 AI 整体不可用，
  服务照常跑人工审核。

## 记录、偏好与满意度

`records.rs` 是纯本地 JSON：

- `data_dir()/records.json` — 按 `token:{hash}` 分区的审核记录。
- `data_dir()/prefs.json` — 同样按 key 分区的界面偏好。
- 启动时导入 `records_pending.jsonl`（旧版本的离线兜底文件）和二进制同目录的旧 `records.json`，
  历史数据不会丢。
- `configured()` 恒为 `false`：这是"没有云端"的诚实信号，前端已不再依赖它，
  但删掉它会让未来的同步实现失去挂载点。
- 写盘在锁内做**全量重写**，量级只有几百条，不值得上数据库。记录上千条再考虑换存储。

满意度：`satisfaction = (其他用户赞同数) / (赞同 + 反对)`，其中赞同数要**减掉审核员自己那一票**，
并且**不要相信上游的 `userAgreeRatio`**（口径和计数对不上）。提交满 24 小时才允许取
（`records.rs::REFRESH_MS`），后台每 10 分钟扫 12 条（`satisfaction.rs`）。
作品审核不抓满意度。

## 两个后台循环

`auto::spawn` 和 `satisfaction::spawn` 都在**端口绑定成功之后**才启动——
否则重复启动的第二个进程也会跑一套循环，两个进程抢同一批案件。
无头版直接 `std::process::exit(1)` 退出，桌面版会复用已运行实例。

值守循环每 tick：取待审列表 → 跳过 hidden → 问 AI → 置信度 ≥ `threshold` 才提交 →
每 tick 最多 `PER_TICK = 3` 条、总耗时不超过 `TICK_BUDGET_SECS = 180`、单案件最多重试
`MAX_ATTEMPTS = 5` 次。配置落 `data_dir()/auto.json`，首次可用 `AUTO_*` 环境变量做初值
（仅当 `auto.json` 不存在时）。

## 前端结构

`src/App.tsx` 是一个文件的两个模式，不是两个应用：

- `App`（`:859`）负责身份：读 `localStorage` 的 token → `createApi({ token })`；
  没有 token 就显示 `Login`（只有一个输入框）。
- `Console` = 评论审核，`CompositionApp` = 作品审核。两者挂在 `.mode-stage` 里
  **同时保活**，切换只做交叉淡入 + `inert`，不卸载子树：切回来列表状态、选中项、
  滚动位置都还在，也不会重新发一次 8–20 秒的案件扫描。
  非活动侧的 `useEffect` 内部要检查 `active`，不要在活动切换时 remount。
- `src/lib/api.ts` 是唯一的网络层，所有类型都在这里；界面不拼 URL、不碰 token。
- `src/lib/demo.ts` 实现同一个 `ReviewApi` 接口，`sessionStorage["qingfeng.demo"] = "1"`
  即可无后端预览界面。
- 浏览器侧的历史缓存按 **token 掩码** 分区（`owner`），换 token 不会串数据。

## 别踩的坑

- **`index.html` 绝不缓存**（`main.rs::cache_headers`）。否则重新构建后浏览器还加载旧 HTML，
  指向已删除的 chunk，表现为"改了代码界面没变"。
- 静态目录判断用 `STATIC_DIR`；未设置时回落 `<exe 目录>/web`，再回落 `../out`。
- `dotenvy` **不覆盖已存在的环境变量**。容器里如果已注入 `HOST`，`.env` 里写的值会被忽略。
- 案件数据全链路按字段容错解析（社区接口字段名变过好几次，见 `pick_text`、
  `REPORT_TIME_KEYS`），新字段先按"可能不存在"写。
- 单测用 `RecordsStore::at(dir, …)` 而不是改 `QF_DATA_DIR`：几个 `#[tokio::test]`
  并发跑，抢同一个环境变量会互相踩。
