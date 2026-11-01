# 构建

本仓库**只有源码**。`package.json`、`tsconfig.json`、`next.config.ts`、`Cargo.toml`
都要你自己用命令生成——所以照着这份文档敲一遍就能跑起来，跳过它则什么都跑不起来。

命令按 bash（Linux / macOS / Windows 的 Git Bash）写。
**本文的每一步都实测走通过**：`npm run build` 产出 `out/index.html`，
`cargo test` 全绿，`cargo build --release` 出来的二进制能同时托管前端页面和 `/api`。

## 0. 前置条件

| 工具 | 版本 | 说明 |
| --- | --- | --- |
| Node.js | ≥ 20 | Next 16 的要求；随带 npm |
| Rust | stable ≥ 1.85 | `cargo init` 默认生成 `edition = "2024"`；改回 `"2021"` 可降到 1.75+ |
| Git | 任意 | 只是版本管理，构建本身不依赖 |

不需要数据库，不需要 Docker，不需要任何云账号。

## 0.1 目录约定

在**本仓库的上一级**建工作目录，并用 `$QF` 指代仓库根，后面所有 `cp` 都靠它：

```bash
mkdir -p build && cd build
QF="$(cd ../qingfeng-open && pwd)"
```

最终布局：

```
build/
├── web/            create-next-app 生成，放入 $QF/src
└── server/         cargo init 生成，放入 $QF/server/src + build.rs + assets
```

`$QF` 只在当前 shell 有效，重开终端要重新赋值。

## 1. 前端

### 1.1 生成 Next.js 工程

```bash
npx create-next-app@16 web
```

**用交互式向导**，因为 v16 的这些开关只有肯定形式（`--tailwind`、`--eslint`、`--src-dir`），
没有 `--no-tailwind` 之类的反向参数。回答：

- TypeScript：**Yes**
- ESLint：**No**（本项目样式全手写，用不到 lint）
- Tailwind CSS：**No** ← 关键，否则默认配置里会带一条把 `*.css` 交给 Tailwind loader 的 turbopack 规则
- `src/` directory：**No** ← 关键。要 `app/` 落在工程根，好让本仓库的 `src/` 保持纯源码
- App Router：**Yes**
- Import alias：默认 `@/*`，不用改
- 是否包含 `AGENTS.md` / React Compiler / Cache Components：都 **No**

偷懒写法（会用默认值，因此**会带上 Tailwind 和 ESLint**）：

```bash
npx create-next-app@16 web --ts --app --import-alias "@/*" --use-npm --yes --disable-git
```

选这条路的话，接着执行下面的清理步骤即可。清掉自带模板，避免和本项目的 `src/index.css` 打架：

```bash
cd web
rm -f  app/globals.css app/favicon.ico
rm -rf public .next
rm -f  eslint.config.mjs AGENTS.md README.md    # 存在就删，不存在报错无所谓
```

（`app/layout.tsx` 默认用 `next/font/google` 拉 Geist 字体，1.3 会整份替换掉它，
所以最终构建不需要访问 Google Fonts。）

### 1.2 放入本仓库源码

把仓库的 `src/` **整目录**拷到工程根：

```bash
cp -r "$QF/src" ./
```

结果应为 `web/src/App.tsx`、`web/src/index.css`、`web/src/lib/api.ts`、`web/src/lib/demo.ts`。

### 1.3 接上 App Router 的两个壳

`app/layout.tsx` 换成（我们的全局样式在 `src/index.css`）：

```tsx
import type { ReactNode } from "react"
import "../src/index.css"

export const metadata = {
  title: "清风 · 审核控制台",
  description: "社区内容安全审核工作台，支持 AI 辅助与人工审核界面。",
}

export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html lang="zh-CN">
      <body>{children}</body>
    </html>
  )
}
```

`app/page.tsx` 换成一行挂载：

```tsx
import ModerationWorkspace from "../src/App"

export default function Page() {
  return <ModerationWorkspace />
}
```

`src/App.tsx` 顶部已经带 `"use client"`，不需要再拆客户端边界。

### 1.4 TypeScript 设置

v16 生成的 `tsconfig.json` 已经是 `strict: true`，`include` 是 `**/*.ts` / `**/*.tsx`，
所以放进来的 `src/` 会自动纳入检查，不用改路径别名（本项目全用相对导入）。
本仓库源码是在 `strict` + `noUnusedLocals` 下写的，建议再补一项把它变成回归门：

```jsonc
"noUnusedLocals": true
```

`next build` 会跑 TypeScript 检查，这一项加上后残留的未使用变量会直接挡住构建。

### 1.5 静态导出

服务端要自己托管前端，所以生产构建必须是**静态导出**。
**整份替换** `next.config.ts`（v16 生成的是 `.ts`；更早版本是 `next.config.mjs`，内容通用）：

```ts
import type { NextConfig } from "next"

const isDev = process.env.NODE_ENV === "development"

const nextConfig: NextConfig = isDev
  ? {
      // 开发时把 /api 代理给本机 Rust 服务，省掉跨域配置
      async rewrites() {
        return [
          {
            source: "/api/:path*",
            destination: "http://127.0.0.1:8080/api/:path*",
          },
        ]
      },
    }
  : {
      // 生产：产出纯静态文件到 out/
      output: "export",
      images: { unoptimized: true },
    }

export default nextConfig
```

三个必须留意的冲突：

- `output: "export"` 与 `rewrites` 互斥，所以要像上面这样按环境分开。
- 默认模板里的 `cacheComponents: true` / `partialPrefetching: true` **必须删掉**。
  Cache Components 会隐含打开 PPR，静态导出下构建以
  `Error: Invariant: PPR cannot be enabled in export mode` 失败（页面已经生成完才报，
  看着像成功）。
- 默认模板里的 `turbopack.rules`（把 `*.css` 交给 `@tailwindcss/turbopack`）也要删；
  我们的 `src/index.css` 是纯 CSS，没有 `@import "tailwindcss"`。

本项目不用 `next/image`，`images.unoptimized` 只是给以后留个保险。

### 1.6 构建

```bash
npm run build                # 产物在 out/，v16 默认走 Turbopack，实测可用
ls out/index.html
```

如果 `web/` 建在另一个带 `pnpm-lock.yaml` / `package-lock.json` 的目录里面，
构建会提示 "We detected multiple lockfiles" 并猜错 workspace 根。
在两个分支的对象里各加一行消除歧义：

```ts
turbopack: { root: process.cwd() },
```

（向导里选了 Tailwind 的话，同一个 `turbopack` 键下会带 `rules`，按 1.5 删掉。）

想先只看界面、完全不碰后端：`npm run dev` 起开发服务器，然后在浏览器控制台执行

```js
sessionStorage.setItem("qingfeng.demo", "1")
location.reload()
```

界面会走 `src/lib/demo.ts` 的内存假数据。

## 2. 后端

### 2.1 生成 crate

```bash
cd ..                       # 回到 build/
cargo init --name qingfeng-server server
cd server
```

### 2.2 放入源码

```bash
cp "$QF"/server/src/*.rs ./src/
cp    "$QF/server/build.rs" ./
cp -r "$QF/server/assets" ./
cp    "$QF/server/env.example" ./env.example
```

`build.rs` 必须在 crate 根（和 `Cargo.toml` 同级），`assets/icon.ico` 要在它同级目录下的
`assets/` 里。找不到 `icon.ico`，或所在平台没有 Windows SDK 的 `rc.exe` 时，
`build.rs` 只打一条 warning 并跳过图标嵌入，不影响编译。

`cargo init` 生成的 `src/main.rs` 会被上面的拷贝覆盖掉；确认 `src/` 里正好是本仓库那
8 个 `.rs`，没有 `cargo init` 留下的其他文件。

### 2.3 依赖

```bash
cargo add axum@0.8
cargo add dotenvy@0.15
cargo add rustls@0.23 --no-default-features --features ring,std,tls12
cargo add reqwest@0.12 --no-default-features --features json,rustls-tls
cargo add serde --features derive
cargo add serde_json
cargo add sha2@0.10
cargo add tokio --features macros,rt-multi-thread,net,signal,time
cargo add tower-http@0.6 --features cors,fs
cargo add webpki-roots
```

要点：

- `rustls` 只用 `ring` 后端，避开 `aws-lc-rs` 在 Windows 上还要 cmake/nasm。
- `reqwest` 必须关掉默认特性再显式给 `json,rustls-tls`，否则会把 native-tls / OpenSSL 拉进来。
- `tokio` 的 `signal` 和 `time` 特性要写上：优雅退出用 `tokio::signal`，
  各处超时用 `tokio::time::timeout`。少了它们在干净环境里会编译失败。
- **不需要** `tokio-postgres` / `tokio-postgres-rustls`（记录走本地 JSON）、
  `tao` / `wry` / `tray-icon`（没有桌面壳）。也别加 `[features]` 段，
  `cargo build` 直接就是无头服务。

生成完的 `Cargo.toml` 应该长这样（`cargo add` 会写精确到 patch 的版本号，
下面是实测产物；`version` 字段界面右上角会显示，记得改）：

```toml
[package]
name = "qingfeng-server"
version = "1.0.0"          # 界面右上角显示的版本号取自这里
edition = "2024"           # cargo init 给什么就留什么

[dependencies]
axum = "0.8"
dotenvy = "0.15"
rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "net", "signal", "time"] }
tower-http = { version = "0.6", features = ["cors", "fs"] }
webpki-roots = "1"
```

`edition = "2024"` 要求 Rust ≥ 1.85。源码本身 2021 也能编，
要支持更老的工具链就把这一行改成 `"2021"`。

`[profile.release]` 不用调，默认即可。

### 2.4 测试与构建

```bash
cargo test                       # 单元测试：分类、时间解析、满意度、本地存储等
cargo build --release            # 产物 target/release/qingfeng-server
```

`cargo test` 用临时目录跑，不会碰你 `QF_DATA_DIR` 里的真实记录。

### 2.5 可选：把 AI 配置编进二进制

`build.rs` 会读三个环境变量，把值写进 `OUT_DIR/builtin_keys.rs` 的常量；不设就是空字符串，
运行期自动回落到 `.env`。**这是防"发布目录里躺着明文密钥"，不是加密**——
混淆用的固定密钥就写在 `src/ai.rs` 里，别拿它保护真正的秘密。

```bash
# 混淆规则：逐字节与 ASCII 串 "qingfeng-review-2026-key-obfuscation" 循环异或，再标准 Base64
QINGFENG_AI_KEYS_OBF='<obf(AI_API_KEY[,…])>' \
QINGFENG_AI_BASE_URL='<obf(https://…/v1)>' \
QINGFENG_AI_MODEL='<obf(模型名)>' \
cargo build --release
```

不想要这一步就照常用 `.env`，`ai.rs` 优先读环境变量。

## 3. 构建完成的判断标准

- `web/out/index.html` 存在，`web/out/_next/` 里有 chunk。
- `cargo test` 输出里没有 `failed`。
- 在 `server/` 下用 `STATIC_DIR=../web/out` 启动二进制，日志出现
  `托管前端静态文件：…`，`curl localhost:8080/api/health` 返回 `{"ok":true,…}`。

三条都满足，接下来看 [RUN.md](RUN.md)：怎么起服务、怎么粘贴 token、怎么排错。
想改行为再回来读 [ARCHITECTURE.md](ARCHITECTURE.md)。
