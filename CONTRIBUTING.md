# 参与开发

## 环境搭建

照 [docs/BUILD.md](docs/BUILD.md) 走一遍：前端工程与后端依赖清单按文档里的命令生成，
再把本仓库的 `src/` 与 `server/src/` 放进去。仓库只保存业务源码，所以"clone 完直接跑"不成立。

日常开发两个终端：

```bash
# 终端 1：Rust 服务（.env 里 STATIC_DIR 留空，前端交给 next dev）
cd build/server && cargo run

# 终端 2：Next 开发服务器，/api 由 next.config 的 rewrites 代理到 8080
cd build/web && npm run dev
```

改 Rust 用 `cargo run`，改前端靠热更新。要看真实 token 下的数据就得有一个有效的社区
access-token；只想改界面用 demo 模式（`sessionStorage.setItem("qingfeng.demo","1")`）。

## 提交前的门

```bash
cd build/server && cargo test && cargo build --release
cd build/web    && npm run build          # 内含 TypeScript 检查
```

`cargo test` 必须零 failed。前端把 `noUnusedLocals: true` 打开（BUILD.md 1.4），
不然死代码会一路带到 PR 里。

## 代码约定

**Rust**

- 模块职责按 `docs/ARCHITECTURE.md` 的划分走：社区接口只在 `community.rs`，
  AI 只在 `ai.rs`，落盘只在 `records.rs` / `case_times.rs` / `auto.rs`。`main.rs` 只做路由和编排。
- 上游字段一律按"可能不存在、可能是数字也可能是字符串"处理，用现成的
  `int()` / `text()` / `pick_text()`，不要 `as` 硬转或 `unwrap()`。
- 并发用 `tokio::task::JoinSet`（仓库既有模式），不引入 `futures`、`rayon` 之类新依赖。
- 加依赖要慎重：本项目的依赖面是刻意的最小集，任何"要 OpenSSL / cmake 才能编"的
  crate 都要在 PR 里说明理由。
- 注释写"为什么"，尤其是各处看起来多余的容错和超时——它们都对应过一次真实故障。

**TypeScript**

- 所有网络调用只出现在 `src/lib/api.ts`；界面组件不拼 URL、不读 token。
- `ReviewApi` 是前后端的契约，加接口要同时改 `api.ts` 和 `demo.ts`，
  否则 demo 模式会当场崩。
- 样式全在 `src/index.css`，不引入 CSS 框架、不写内联样式表；类名沿用现有的
  kebab-case。
- 模式切换相关代码要守住 keep-alive 约束：`Console` / `CompositionApp` 的副作用
  在 `active === false` 时要在 effect **内部**提前返回，不要靠条件渲染卸载子树。

## 加一个接口的最小路径

1. `community.rs` 加方法（只发请求、只解析字段）。
2. `main.rs` 加 handler + `.route(...)`，返回结构用 `#[serde(rename_all = "camelCase")]`。
3. `api.ts` 加类型和方法；`TIMEOUTS` 常量按需扩，别用无限等待。
4. `demo.ts` 补一个假实现。
5. 单元测试放在对应模块的 `#[cfg(test)]` 里，用临时目录，不要碰 `QF_DATA_DIR` 环境变量。

## 不在范围内

- 绕过平台风控、批量注册、抢注、抓取隐私数据之类的功能。
- 把社区 token 或 AI 密钥写进任何会返回给浏览器的响应里（只允许掩码）。
- 多用户账号体系、桌面壳这类本项目定位之外的扩展——想做就在自己的 fork 里做。

## Issue

请带上：`/api/version` 的输出、复现步骤、`QF_DATA_DIR/qingfeng.log` 相关片段、
以及浏览器 Network 面板里失败请求的**状态码和响应 message**。
**不要**贴完整 access-token 或 AI 密钥，脱敏成掩码再发。
