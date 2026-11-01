# 清风 · 风纪审核

面向少儿编程社区的**内容审核工作台**。它通过社区公开的 HTTP 接口拉取「风纪审核」待审案件，
在本地完成分拣、AI 辅助研判与满意度核算，审核员确认后回写结论。不改动社区本身，不注入脚本，
不接触与审核无关的数据。

评论与作品是两套完全独立的审核模式：案件来源、待审列表、历史记录互不相通，用左下角的开关切换，
切换时两边状态都保留。

## 功能

- **评论审核**：待审案件按「可处理 / 超期 / 时间未知」自动分类；超期案件只折叠展示，不分析、不提交。
- **作品审核**：拉取作品举报列表，在内嵌网页里核对作品内容，按举报明细逐条给出结论。
- **AI 辅助**：把案件内容与社区规范交给模型，产出「维持 / 删除 + 理由」的建议；支持工具调用，
  模型不支持的网关自动降级为纯文本解析。
- **密钥轮换**：`AI_API_KEY`、`AI_API_KEY_1`…`_9`、`AI_API_KEY_FALLBACK`、`AI_API_KEYS` 多把密钥
  轮流使用，单把限流或失效自动切换；状态接口只返回掩码。
- **自动值守**：可按置信度阈值自动提交高置信度案件，值守参数落在 `auto.json`，重启不丢。
- **满意度**：按社区判决列表抓取赞同/反对票，扣除审核员自己那一票后计算比例，支持手动刷新。
- **本地持久化**：审核记录、偏好、案件举报时间全部落盘为 JSON，刷新与重启都不丢。
- **无框架前端**：手写组件与 CSS，切换模式不重新挂载，列表与滚动位置保持不变。

## 技术栈

| 层 | 选型 |
| --- | --- |
| 前端 | Next.js 16（App Router，静态导出）+ React 19 + TypeScript 5 |
| 后端 | Rust 2021 + axum 0.8 + tokio |
| HTTP | reqwest + rustls（纯 Rust TLS，无需系统 OpenSSL） |
| 存储 | 本地 JSON（按 token 的 SHA-256 分区） |
| 样式 | 原生 CSS（`src/index.css`），无 UI 框架 |

## 目录结构

```
qingfeng-open/
├── src/                     前端源码
│   ├── App.tsx              评论审核 + 作品审核两套界面的全部组件
│   ├── index.css            全局样式
│   └── lib/
│       ├── api.ts           REST 客户端类型与封装，前端唯一的数据入口
│       └── demo.ts          内存假数据：不启动服务端也能预览界面
├── server/                  后端源码
│   ├── src/
│   │   ├── main.rs          路由、案件分类分页、多级缓存
│   │   ├── community.rs     社区接口封装（token 只在服务端流转）
│   │   ├── ai.rs            提示词、密钥轮换、限流与冷却
│   │   ├── auto.rs          24 小时值守循环
│   │   ├── satisfaction.rs  满意度后台刷新
│   │   ├── records.rs       审核记录与偏好
│   │   ├── case_times.rs    案件举报时间缓存
│   │   └── error.rs         统一错误结构
│   ├── build.rs             可选：把 AI 配置混淆后编进二进制
│   ├── assets/              可执行文件图标资源
│   └── env.example          配置模板
└── docs/                    架构、构建、运行说明
```

## 快速开始

```bash
# 1. 前端（详见 docs/BUILD.md 第 1 节，需要先用 create-next-app 生成工程再覆盖 src/）
npm install && npm run build          # 产物在 out/

# 2. 后端（详见 docs/BUILD.md 第 2 节）
cd server
cargo build --release --no-default-features

# 3. 运行
STATIC_DIR=../web/out QF_DATA_DIR=./data HOST=127.0.0.1 PORT=8080 \
  ./target/release/qingfeng-server
```

浏览器打开 `http://127.0.0.1:8080`，粘贴社区 access-token 即可开始。完整步骤、依赖清单、
常见报错都写在文档里：

1. **[docs/BUILD.md](docs/BUILD.md)** — 从零生成工程骨架并放入本仓库源码，含每一条依赖和版本。
2. **[docs/RUN.md](docs/RUN.md)** — 构建、启动、部署到服务器、拿 token 的位置、排错表。
3. **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)** — 案件分类、缓存策略、提交载荷等实现细节。

只想过一遍界面：`src/lib/demo.ts` 提供完整的内存假数据，不需要后端。起一个开发服务器后，
在浏览器控制台执行

```js
sessionStorage.setItem("qingfeng.demo", "1"); location.reload();
```

## 配置

复制 `server/env.example` 为 `server/.env`（二进制同目录的 `.env` 也会读）。

```ini
HOST=0.0.0.0          # 部署到服务器改成 0.0.0.0；默认只在机器内部可见
PORT=8080
STATIC_DIR=../out     # 前端构建产物目录
QF_DATA_DIR=./data    # 记录、偏好、值守配置的落盘目录

AI_BASE_URL=https://<OpenAI 兼容网关>/v1
AI_MODEL=<模型名>
AI_API_KEY=sk-...     # 不填就没有 AI 建议，人工审核照常
```

其余可选项（多密钥轮换、值守阈值、社区接口地址镜像）见
[server/env.example](server/env.example)；`docs/RUN.md` 里有逐项说明。

## 关键机制

### 认领状态从哪来

列表接口只给 `reportNum`，**不给审核员**。谁能进待审列表取决于举报条数：

| 案件举报数 | 认领信息 |
| --- | --- |
| `reportNum = 1` | 列表自带的 `checkerName` 就是最终结果 |
| `reportNum > 1` | 必须查 `/report/get-info`，取 `list` 里**第一条**（按举报时间倒序，最新在前）的 `checkerName` |

`checkerName` 为空或 `-` / `--` / `—` 表示没人认领；有名字就是已认领，整案从待审列表移到隐藏组。
服务端会把详情里的结论回填到列表项，所以界面上看到的认领状态始终是可信的那个。

### 缓存

| 缓存 | 内容 | 存活范围 |
| --- | --- | --- |
| 案件举报时间 | 详情里的最早举报时间，落盘 `case_report_times.json` | 重启后复用 |
| 认领信息 | 多举报案件最新一条的 `checkerName`，**只保留最近 5 个**（LRU，10 分钟） | 进程内 |
| 分类结果 | 整表扫描结果，4 个槽位、75 秒；过期后先回旧数据再后台重算 | 进程内 |

### 提交

`reportDetailIds` 取 `/report/get-info` 返回的**全部**举报记录 id，多条举报一次提交完。

## HTTP 接口一览

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/api/health` | 探活 |
| GET | `/api/version` | 版本与构建号（前端据此自动刷新） |
| GET | `/api/session` | 当前 token 掩码、AI 是否可用 |
| GET | `/api/cases` | 评论案件：正常组 + 隐藏组，各自分页 |
| GET | `/api/cases/{id}` | 案件详情 |
| POST | `/api/cases/{id}/ai-suggestion` | 生成 AI 建议 |
| POST | `/api/cases/{id}/verify` | 提交审核结论 |
| GET/POST | `/api/composition/*` | 作品审核的同构接口 |
| GET | `/api/ai-status` | 密钥掩码、限流与冷却状态（永不返回明文） |
| GET/POST | `/api/records`、`/api/prefs` | 审核记录与偏好 |
| GET/POST | `/api/auto-config` | 24 小时值守配置 |

所有接口都靠请求头 `access-token` 识别身份，没有登录接口。

## 部署

单文件二进制 + 静态目录即可上线，没有运行时依赖。放到内网或服务器后：

- 反向代理加 TLS（服务本身只监听明文 HTTP）；
- **加访问控制**：这个服务没有登录墙，谁能连上端口，谁就能用你配置的 AI 和粘贴进去的 token；
- `QF_DATA_DIR` 指向持久卷，否则重启会丢记录。

## 数据与隐私

- 社区 access-token 由浏览器发到服务端，只用于当场调社区接口，界面拿到的永远是掩码。
- 审核记录、偏好、值守配置写在 `QF_DATA_DIR` 下，按 token 的 SHA-256 归属；换 token 就是另一份数据。
- 送给模型的只有案件本身的评论、举报理由和作品标题，不包含 token。

## 常见问题

**案件明明没人认领，却出现在「已认领」里？**
该案件多半有多条举报记录。列表接口不返回审核员，程序会去查详情取最新一条的 `checkerName`；
如果详情请求失败（超时、token 失效），会退回列表值，下次刷新会自动纠正。查日志里的
`核对举报时间`、`分类扫描` 可以看到这一轮是否补查成功。

**切页 / 切模式之后列表没变？**
分类结果有 75 秒缓存，期间直接复用；超过后先返回旧数据、后台重算，最多滞后一个轮询周期（60 秒）。
刚提交的案件会主动让缓存失效，不会还挂在待审里。

**首屏慢？**
第一次冷启动要扫列表并核对未缓存案件的详情。之后靠 `case_report_times.json` 和认领缓存，只补新案件。

**端口被占？**
换 `PORT`，或先停掉已在运行的实例。服务抢不到端口会直接报错退出。

**AI 按钮点了没反应？**
先看 `/api/ai-status`：没有配置时返回 `enabled:false`，配置了但都在冷却会显示剩余秒数。

**token 过期？**
社区 access-token 会失效，重新在浏览器 F12 的 Network 里复制当前 `access-token` 请求头即可。

## 合规

本软件用于社区自管自治的内容审核，不提供绕过平台风控、批量注册、抓取隐私数据等功能。
社区接口地址与界面底部的备案号属于部署方的信息，二次发布前请按自己的合规要求替换或删除。

## 参与贡献

提交前请读 [CONTRIBUTING.md](CONTRIBUTING.md)：代码分工、样式与网络请求的边界、新增接口的最小路径。

## 许可证

[MIT](LICENSE)
