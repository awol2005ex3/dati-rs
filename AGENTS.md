# AGENTS.md

给在本仓库继续开发的人 / Agent 的工作说明。项目概况、接口清单和配置项看 [README.md](README.md)，
这里只写**规则和坑**。动手前先确认：本仓库是 Java 版 DatI（<https://github.com/yimindev/dati>，
Spring Boot + Vue 3；本机 checkout 在 `D:/workspace_java/dati`）的 Rust 移植，
Java 树是**行为基准**，不是可选参考。

## 1. 硬约束

- **不连真实数据库。** 这台机器上所有验证一律离线：知识库用临时目录里的 SQLite，远程数据源用假
  MySQL 协议服务（见 §7）。绝不从本机连用户环境里的生产库（MySQL / PostgreSQL / Hive / Doris），
  那些地址和凭据只由用户自己复测；内网地址、账号、密码一律不写进仓库里的任何文件。
- **不碰凭据。** 真实用户名/密码/Token 不回显、不写进日志、不提交、不发到外部服务。测试里用
  `root`/`secret` 这类假值。
- **没有明确要求不要 `git commit` / `push`。** 提交信息用中文 + Conventional Commits 前缀
  （`feat: …`、`fix: …`、`test: …`），跟现有历史一致。
- **改源码只用 Edit/Write 工具**，不要用 `sed`/`awk`/`perl`/Python 脚本批量重写源文件。
- **不主动新建文档**（规划、分析、总结类）。需要的文档只有本文件和 README。
- 源码内的注释、测试名用**英文**；给用户看的文案（README、UI 字符串、错误提示）用中文，且照抄 Java。

## 2. 命令

```bash
cargo fmt --all
cargo clippy --all-targets --workspace -- -D warnings   # 门禁，必须零警告
cargo test --workspace                                  # 全程离线
cargo build --release                                   # 单文件产物
```

- Windows 上 target 目录统一 `CARGO_TARGET_DIR=D:/cargo-target`（产物在 `D:/cargo-target/{debug,release}`），
  避免仓库目录膨胀和路径过长。
- 只动后端时用 `DATI_SKIP_FRONTEND=1 cargo build` 跳过 pnpm 构建。
- **重新构建 release 前先确认没有在跑的 `dati.exe`**：Windows 会锁住 exe，`cargo build --release` 直接
  失败；停进程前跟用户确认（那是他正在用的服务）。

## 3. 分层与命名

`crates/dati-core` 不依赖 axum，`crates/dati-app` 只做 HTTP/静态资源/可执行文件。core 内部：

| 层 | 放什么 | 不放什么 |
| --- | --- | --- |
| `model/` | 数据结构、wire 形状（serde 属性）、枚举的 `as_str`/`from_stored` | SQL、异步 IO |
| `repo/` | SQL 文本、行映射、分页 | 业务判断、错误码决策 |
| `service/` | 业务规则、校验顺序、`DatiError` 组装 | HTTP 类型 |
| `connector/` | 数据源连接与元数据读取 | 知识库写入 |
| `dati-app/src/api/routes/` | 状态码、路径、请求体解包 | 业务逻辑 |

- 出错的唯一通道是 `dati_core::error::{DatiError, ErrorCode}`；HTTP 形状由
  `dati_app::api::error::AppError` 转成 `{code,message,timestamp}`，成功响应**不套信封**。
- 每个 `ErrorCode` 对应的**字符串和触发顺序从 Java 源码复制**，不要凭记忆改写文案，也不要调整
  守卫的先后（前端按文案判断，测试按精确串断言）。
- 模块级 doc comment 解释**为什么**这么设计；函数内不写复述代码的注释。三行重复好过一次提前抽象。
- 不要为不会发生的场景加兜底、feature flag、向后兼容垫片；边界（用户输入、外部 API）才校验。

## 4. SQL 与迁移

- 所有语句写 **`?` 占位符**，PostgreSQL 的 `$n` 由 `crate::db::map_placeholders` 统一改写；
  不要手写方言专用参数串。
- **不绑定 `chrono` 值**：`Any` 驱动没有 chrono 的 `Type` 实现。时间统一存 ISO-8601 UTC 文本
  （`crates/dati-core/src/time.rs`），这样字典序即时间序。
- `sqlx::any::install_default_drivers()` 必须在任何连接之前（`db.rs` 和 `Live::open` 里已调用）。
- 迁移在仓库根 `migrations/{sqlite,mysql,postgres}/`，`db.rs` 用 `../../migrations/...` 内联。
  **三个方言必须同步**，列集合、类型映射、索引名字都对得上。
- **项目未发布，迁移只保留一个 `0001_init.sql`**：改表直接编辑它，不要新增 `000N_*.sql`（已把
  `0002_rename_jdbc_url` 合并回 `0001`）。`db.rs` 的测试从 `migrations()` 推导版本列表，不要
  硬编码"有两个版本"。**发布之后这条规则反转**：`Db::migrate()` 只按版本名去重（不校验 checksum），
  届时改动必须进新版本文件，否则老库静默不升级。
- 版本表按字符串比较，名字保持 `0001_…` 这种四位前缀。

## 5. 连接器（最容易踩的一层）

`connector/live.rs` 有两种池：默认 `AnyPool`，`DbType::Doris` 走 typed `sqlx::mysql::MySqlPool`。
三条来自真实 Doris FE 的结论，改这块之前先读：

1. **会话设置。** `Any` 打开 MySQL 驱动时会发
   `SET sql_mode=(SELECT CONCAT(...)), time_zone=…, NAMES … COLLATE …`，Doris FE 拒绝非恒定表达式
   （`Set statement does't support non-constant expr.`）。`AnyConnectOptions` 不可配置，所以只能换驱动。
2. **prepare。** 很多 FE 关掉了服务端 prepared statement，而 `sqlx::query()` **一定会 prepare**（哪怕零
   绑定）。文本协议唯一入口是 `sqlx::raw_sql`，Doris 分支先把值内联成字面量再发（`inlined()`）。
3. **类型映射洞。** `TryFrom<&MySqlTypeInfo> for AnyTypeInfo` 不支持 Tiny / Decimal / Date / DateTime /
   Timestamp / Time / Year / Bit / Json / Set / Geometry，一个列映射不上就整行失败
   （`error in Any driver mapping: …`）。因此 Doris 路径自己产出 `LiveRow`，逐列按
   `i64 → u64 → f64 → String → Vec<u8> → try_get_unchecked::<AsText>` 降级。
   **不要在这条阶梯上加 `bool`**：sqlx-mysql 的 bool `compatible()` 接受 Tiny，会把 TINYINT 读成
   `true`/`false` 而丢掉数字。同理不要把 Doris 结果集塞回 `AnyRow`。

其它硬性要求：

- **Doris 分支按 `type` 字段选，不看 URL。** 用户填的是 `mysql://host:9030/…`，从 scheme 推断会得到
  `MYSQL`，然后原样复现上面的失败。日志里 `data source pooled db_type=… driver=…` 就是用来查这个的。
- **元数据查询按 schema 批量，禁止逐表。** 前端 axios 给自己 15 s（`frontend/src/api/tableinfo.ts`），
  一个慢 FE 上逐表 `information_schema.columns` 会稳定超时。列字段用
  `dialect::columns_batch_sql` + `Connectors::list_columns_batch`（一次 `WHERE table_schema = ? AND
  table_name IN (…)`，返回按表名 key 的 `HashMap`）；`service/table.rs` 的 `batch_add` 顺序是：按 schema
  列表 → 校验存在性（DS007 提前失败，不发列查询）→ 一次取回全部列。新增入口也照这个形状，不要回到
  Java 的逐表 `getColumns`（Connector/J 那条路径便宜，FE 的 information_schema 扫描不便宜）。
- 连接失败要报**原因**而不是 `false`：先 `AnyConnectOptions` + `ConnectOptions::connect()`（包 timeout）
  拿到真实错误，再建池。`pool.acquire()` 会把连不上的错误伪装成 `PoolTimedOut`。

## 6. Wire 兼容

- REST 请求/响应 **snake_case**，MCP JSON-RPC **camelCase**，两者都和 Java 逐字段对齐——前端是原样复制
  过来的，字段改名等于直接改坏前端。
- 数据源字段叫 `connection_url`，界面/文档/表结构里**不允许出现 `jdbc` 字样**；输入侧可以宽容接受粘进来的
  `jdbc:` 前缀，但存储和返回都不带。
- 登录：`POST /v1/auth/login`，体 `{"type":"local","name":…,"password":…}`，返回 `{"token":…}`。
  `Authorization: Bearer` 同时接受 JWT 和 `sk_` 前缀的 API Key。

**有意偏离 Java 的地方，不要"顺手改回去"**（都是用户拍板过的）：不接 Elasticsearch（关系表 + `LIKE`
替代，检索输出形状不变）；不接 LDAP（走该分支返回 `AUTH004` 是预期）；`SqlAnalyzer` 是词法分析；
主题派生的数据范围返回**排序后**的 id（Java 用 `HashSet`，顺序不可复现）；索引目标不算被引用表；
连接测试返回原因而非裸 boolean；未发布期间迁移只有一个 `0001_init`。

## 7. 验证怎么做

**单元测试**：放在各模块 `#[cfg(test)]` 里，名字写成英文陈述句（`a_doris_row_of_…_is_still_read`）。
涉及知识库的用 `tempfile` 建临时 SQLite；涉及远程数据源的在 `live.rs` 测试里起了一个假的 MySQL 协议
服务端，直接喂 wire packet。

**集成测试**：`crates/dati-app/tests/*_slice.rs` 用 `Router` 直接驱动，不起真端口、不 spawn 进程。
断言精确 JSON 与错误码字符串；涉及 `HashMap` 输出的先排序，否则随机失败。

**端到端冒烟（连假 FE）**：要验证连接器改动时，写一个一次性 Node 假服务端（放 `D:/tmp` 下，用完删），
它必须：回 `COM_PING`（0x0E，池取连接前会发）；对非恒定 `SET` 回 1105；对 `COM_STMT_PREPARE` 回 1105；
在每个元数据结果集里放一个 TINYINT + DECIMAL + DATE 列；可配置每条查询的人工延迟。然后：

```bash
node fake-doris.mjs > fake.log 2>&1 &                      # FAKE_PORT=33060 STRICT_SET=1 SLOW_MS=1500
PORT=18085 DATI_DB_URL="sqlite:D:/tmp/xxx/db/e2e.db?mode=rwc" D:/cargo-target/debug/dati.exe &
# 登录 → test-connection(type=DORIS) → 建数据源 → /schemas → /schemas/{s}/tables → POST /tables/batch
# 用 fake.log 数 COM_QUERY 的条数，证明"一次动作 = 常数次查询"，并确认没有 COM_STMT_PREPARE
```

Windows 从 Git Bash 打 HTTP 的注意：请求体只用 ASCII（中文会被本地代码页搞坏），路径用正斜杠
`D:/…`；杀进程 `taskkill //F //PID <pid>`；按端口找 PID 用
`netstat -ano | grep <port>`，只杀自己起的，别碰用户 8085 上的服务。

**收尾清单**：临时进程全部终止、`D:/tmp` 下的草稿目录删掉、`git status` 里没有多余文件（`db/` 已
gitignore）、三条门禁命令都过。给用户交付时说明"哪些是离线证明过的、哪些还需要他在真实 FE 上复测"。

## 8. 前端

`frontend/` 是从 Java 仓库原样复制的 Vue 3 + Element Plus SPA（`frontend/src` 114 个文件），
`docs/user-guide/` 是 VitePress 文档（中/英），两者都不要为绕过后端问题去改。页面报错先当后端问题查。

- `crates/dati-app/build.rs` 会在 `cargo build` 时自动跑 pnpm；失败只 `cargo:warning`，不中断编译。
- VitePress 在 pnpm workspace 之外，所以根目录必须有 `node_modules`（build.rs 建 junction）和
  `{"type":"module"}` 的根 `package.json`。**不要"顺手清理"这两个。**
- 静态资源经 rust-embed 进二进制：`/` 是 SPA（history fallback 到 `index.html`），`/docs` 是文档站。
- 前端 axios 超时 15 s —— 任何面向控制台的接口都要在这个预算内完成。

## 9. 加一个功能的标准路径

1. 在 `https://github.com/yimindev/dati` 找到对应 Java 实现：业务代码在
   `backend/core/src/main/java/com/dati/<模块>/{domain,repository,server}`（模块是 `datasource`、
   `semantic`、`mcp`、`auth`、`permission`、`db`、`common`），`domain` 是规则、`repository` 是 SQL、
   `server/controller` 是接口形状；测试在镜像的 `src/test/java` 下，里面的断言就是我们要抄的行为。
   **Java 仓库里没有 DDL 脚本**，表形状看 `repository/po` 实体反推，我们的三份
   `migrations/*/0001_init.sql` 是手工对齐出来的。抄它的错误串、守卫顺序、分页形状。
   Java 仓库根目录还有一份它自己的 `AGENTS.md`，写它的风格约定（"English in code, Chinese in
   docs"、构造注入、`IdResponse`/`PageResponse` 响应形状），与本文件不冲突，可作参照。
2. `model/` 加结构 → `repo/` 加 SQL → `service/` 加规则 → `dati-app/src/api/routes/` 接线 →
   三个方言的 `migrations/*/0001_init.sql` 同步改表。
3. 补 `#[cfg(test)]` 单测 + 一个 `tests/*_slice.rs` 场景；必要时按 §7 做端到端冒烟。
4. 跑 §2 的命令，全绿再交。

Java 侧尚未移植的接口按需补，不要一次铺开；MCP 的 `tools/{tool_id}/test` 目前就还没接。
