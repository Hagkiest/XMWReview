# 运行

前提：按 [BUILD.md](BUILD.md) 已经产出 `web/out/`（前端静态文件）和
`server/target/release/qingfeng-server`（可执行文件）。

## 1. 本地跑起来

```bash
cd build/server
cp env.example .env           # 编辑：至少填 AI_*，或先留空只用人工审核
STATIC_DIR=../web/out QF_DATA_DIR=./data HOST=127.0.0.1 PORT=8080 \
  ./target/release/qingfeng-server
```

看到 `无头服务模式：用浏览器访问 http://127.0.0.1:8080` 就成功了。
`.env` 也可以放在二进制同目录——两处都会读（先当前目录向上找 `.env`，再读 `<exe 目录>/.env`）。

浏览器打开那个地址，粘贴社区 access-token，进入审核台。

Windows 上把上面的环境变量设置换成对应 shell 的写法
（PowerShell：`$env:STATIC_DIR="..\web/out"`），命令本身一样。

## 2. 拿社区 access-token

在已登录的社区网页里：F12 → Network → 任意一个 `community-api` 请求 →
Request Headers → `access-token`。复制整串值。

token 存进浏览器 `localStorage`，之后同一浏览器刷新免粘贴；换设备/清缓存要重贴一次。
服务端只按 token 的哈希分区记录和偏好，任何接口都不会回传完整 token（只有掩码）。

## 3. 冒烟测试

```bash
curl -s localhost:8080/api/health          # {"ok":true} 之类
curl -s localhost:8080/api/version         # 界面右上角显示的版本号 = server/Cargo.toml 的 version
curl -s localhost:8080/api/ai-status       # 密钥掩码与冷却状态；enabled:false 表示没配 AI
curl -s -H "access-token: $TOKEN" localhost:8080/api/session
```

`/api/session` 返回 `community.hasToken = true` 才说明 token 有效。
`unread` 是社区未读数，`aiEnabled` 决定界面上 AI 卡片是否出现。

## 4. 数据落在哪

`QF_DATA_DIR`（未设置时：Windows `%LOCALAPPDATA%\QingFeng`，
其他系统 `$XDG_DATA_HOME/qingfeng`，再兜底到系统临时目录）下有：

| 文件 | 内容 | 能删吗 |
| --- | --- | --- |
| `records.json` | 按 token 分区的审核记录 | 能，历史列表会空 |
| `prefs.json` | 按 token 分区的界面偏好 | 能 |
| `auto.json` | 24 小时值守配置（含值守用的 token） | 能，回到 `.env` 初值 |
| `case_report_times.json` | 案件举报时间缓存 | 能，但下次要重新逐条核对详情 |
| `qingfeng.log` | 运行日志 | 能 |

`records.json` 是启动时从旧位置导入的（`records_pending.jsonl`、二进制同目录的 `records.json`），
历史数据不会丢。

## 5. 部署到服务器

服务本身只有明文 HTTP，**不做 TLS、不做访问控制**，所以别把它直接暴露在公网：

- `HOST=0.0.0.0`、`PORT=8080`，前面用 Nginx / Caddy / 宝塔反代终结 TLS 并加 Basic Auth 或 IP 白名单；
- 或者只监听 `127.0.0.1`，用 SSH 隧道 / 内网访问；
- `QF_DATA_DIR` 指向一个会备份的目录（`auto.json` 里存着值守 token）。

最简单的常驻方式：

```bash
mkdir -p /opt/qingfeng/data
cd /opt/qingfeng
nohup env STATIC_DIR=/opt/qingfeng/web QF_DATA_DIR=/opt/qingfeng/data \
  HOST=0.0.0.0 PORT=8080 ./qingfeng-server >> run.log 2>&1 &
```

要 systemd / Docker / 宝塔面板的部署方式，参照 Next.js 与 Rust 的通用做法搭建。
仓库只保存业务源码，镜像与编排文件按你自己的环境维护。

## 6. 24 小时值守

界面上「设置 → 服务器自动审核」开启，或写 `auto.json`。参数含义：

- `interval`：每轮之间的秒数（10–3600，默认 90）
- `threshold`：置信度阈值（0–1，默认 0.9），低于它就攒着不提交
- `preferDelete`：犹豫时选删除而不是替换
- `token`：值守用的社区 token；也可以只在 `.env` 里给 `AUTO_TOKEN`

`.env` 里的 `AUTO_*` 只在 `auto.json` 还不存在时当初始值用；之后以文件为准。

值守循环和界面走同一套提交逻辑，跳过"已被认领 / 超过 7 天 / 时间取不到"的案件，
每轮最多处理 3 条、单案件最多重试 5 次。

## 7. 测试

```bash
cd server && cargo test
```

覆盖案件分类、时间解析、满意度算法、举报体拼装、本地存储往返等。用临时目录跑，
不会碰你的 `QF_DATA_DIR`。

前端：`npm run dev` + `sessionStorage.setItem("qingfeng.demo","1")` 是不依赖后端的界面预览；
`npx tsc --noEmit` 在项目配了 `strict` + `noUnusedLocals` 后就是回归门。

## 8. 排错

| 现象 | 原因 / 处理 |
| --- | --- |
| 端口监听失败并退出 | 端口被占。服务是纯无头进程，抢不到端口就直接失败退出，换 `PORT` 或停掉旧实例 |
| 起了但浏览器打不开 | `HOST` 还是 `127.0.0.1`。服务器上要 `0.0.0.0`；另外检查 `.env` 是否被已存在的环境变量盖住（dotenvy 不覆盖） |
| 页面空白 / 只出 JSON | 没找到前端：`STATIC_DIR` 要指到 `out/` 且里面有 `index.html`。日志里会有"未找到前端静态目录" |
| 401，界面弹回 token 框 | token 过期或没带。重新粘贴 |
| AI 卡片不出现 | `AI_BASE_URL`/`AI_MODEL`/至少一个密钥没配全；看 `/api/ai-status` 和日志里的"AI 服务未配置" |
| AI 频繁失败 | 单密钥每分钟 10 次上限，超了进冷却。多配几个 `AI_API_KEY_n` |
| 第一次列表要等十几秒 | 正常：要把未审案件逐条核对举报时间。`case_report_times.json` 建起来后就快了 |
| 列表提示"统计只覆盖部分" | 扫描达到页数/时间上限（`truncated`）。案件多属预期，折叠区可能不完整 |
| 刚审完的案件还在待审里 | 提交路径没触发缓存失效；等 75 秒 TTL 过期。若持续如此说明漏了 `clear_case_cache` |
| 改了代码界面没变 | 强刷（`Ctrl+Shift+R`）。服务端对 `index.html` 已禁缓存，多半是浏览器旧标签页 |
| 历史是空的 | 记录和偏好按 token 哈希分区，换一个 token 看不到上一份；确认 `QF_DATA_DIR` 指对且目录可写 |
