# DatI（dati-rs）

Agent 数据库语义网关。这是 Java 版 DatI（`https://github.com/yimindev/dati`，Spring Boot 3.5 + Vue 3）的 Rust 实现：
后端换成 **axum + sqlx**，前端原样复用，两者编译进 **一个可执行文件**——同一个进程同时提供管理后台、
REST API、用户文档站和 MCP 端点。

它做的事情：把一批真实数据源（MySQL / PostgreSQL / SQLite / Doris）的表和字段登记进自己的**知识库**，
在知识库上维护主题（subject）、业务口径（term）和字段取值，再把选定范围打包成一个 **MCP 服务**发布出去，
让 Agent 用 `search_tables_and_terms` / `get_table_schema` / `execute_sql` 这些工具来问数。

---

## 快速开始

```bash
# 构建（首次会自动用 pnpm 构建前端与文档，产出 frontend/dist 供 rust-embed 嵌入）
cargo build --release
# 产物：target/release/dati(.exe)；本机若把 target 指到别处，用 CARGO_TARGET_DIR=D:/cargo-target

# 运行（默认监听 8085，知识库用 ./db/dati.db）
./target/release/dati
```

打开 <http://localhost:8085> 即管理后台，<http://localhost:8085/docs> 是用户文档。
首次启动会自动创建管理员账号：用户名 `admin`，密码取 `ADMIN_PASSWORD`，未设置时为 `admin123`。
登录后 `Authorization: Bearer <token>` 用于所有 `/v1/*` 接口。

```bash
# 登录拿 token
curl -X POST http://localhost:8085/v1/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"type":"local","name":"admin","password":"admin123"}'
```

没有 pnpm / 没有网络也能编译：前端构建失败只降级成一条 `cargo:warning`，仍然得到一个可用的 API 二进制；
`DATI_SKIP_FRONTEND=1` 可以显式跳过前端构建。

## 配置

全部来自环境变量，没有配置文件。

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `PORT` | `8085` | 监听端口，绑定 `0.0.0.0` |
| `DATI_DB_URL` | `sqlite:db/dati.db?mode=rwc` | 知识库地址，支持 `sqlite:` / `mysql:` / `postgres:`；scheme 决定方言与迁移脚本 |
| `JWT_SECRET` | 内置示例值 | HS256 密钥，**至少 32 字节**，否则启动即报错 |
| `JWT_EXPIRATION_SECONDS` | `604800` | token 有效期（默认 7 天） |
| `ADMIN_USERS` | `admin` | 首次启动创建的管理员用户名，逗号分隔 |
| `ADMIN_PASSWORD` | `admin123` | 上述管理员的初始密码 |
| `MCP_ENDPOINT_BASE_URL` | 空 | 非空时把 MCP 地址拼成绝对 URL 返回给客户端 |
| `MCP_ALLOWED_ORIGINS` | 空 | 逗号分隔的 Origin 白名单 |
| `MCP_STATS_ENABLED` | `true` | MCP 调用统计开关 |
| `MCP_STATS_BATCH_SIZE` | `100` | 单次落库的批量条数 |
| `MCP_STATS_FLUSH_INTERVAL_SECONDS` | `10` | 统计刷盘间隔 |
| `MCP_STATS_RETENTION_DAYS` | `15` | 明细保留天数，过期由后台任务清理 |
| `COLUMN_VALUE_SAMPLE_LIMIT` | `1000` | 字段取值抽取的取样上限 |
| `COLUMN_VALUE_LENGTH_LIMIT` | `256` | 单个取值入库长度上限 |

日志走 `tracing`，默认过滤 `dati=info,dati_core=info,dati_app=info`，可用 `RUST_LOG` 覆盖。

## 知识库与数据源

- **知识库**（存 DatI 自己的元数据）：SQLite（默认，零依赖）/ MySQL / PostgreSQL。启动时按方言执行
  `migrations/<dialect>/0001_init.sql`，共 20 张表，已应用的版本记在 `schema_migration` 表里，重复启动幂等。
- **数据源**（被管理的真实库）：MySQL、PostgreSQL、SQLite、Apache Doris。Doris 走 MySQL 协议（默认端口
  9030），地址写成 `mysql://host:9030/db` 并把类型选为 **Apache Doris**——类型字段决定驱动路径，仅靠 URL
  推断是不够的。
- 登记流程：数据源 → 选 schema → 列出实时表 → 勾选批量加入 → 表字段随一次查询一起落库 → 可选地做字段取值抽取。

## REST API

统一前缀 `/v1`，请求与响应 JSON 都是 **snake_case**（与 Java 版逐字段对齐，前端未改动）。
失败时是 Java 那套错误体，HTTP 状态码由错误码决定：

```json
{ "code": "DS007", "message": "Table not found: ghost", "timestamp": 1790748642879 }
```

错误码前缀：`CM` 通用、`DS` 数据源、`SM` 语义层、`MS` MCP 服务、`AUTH` 认证、`PM` 权限、`VAL` 字段校验。
成功响应不套信封。

| 分组 | 路径 |
| --- | --- |
| 认证 | `/v1/auth/login`、`/v1/auth/register`、`/v1/auth/me`、`/v1/auth/api-keys[/{id}]` |
| 用户 | `/v1/users/search` |
| 数据源 | `/v1/data-sources`、`/v1/data-sources/test-connection`、`/v1/data-sources/{id}`、`.../schemas`、`.../schemas/{schema}/tables` |
| 表与字段 | `/v1/data-sources/{id}/tables[?keyword=]`、`.../tables/batch`、`.../tables/added-names`、`.../tables/{table_id}`、`.../columns[/sync]`、`.../columns/{column_id}[/values][/values/extract]` |
| 语义层 | `/v1/subjects[/{id}][/tables][/available-tables]`、`/v1/subjects/{id}/terms`、`/v1/terms/{id}[/relations]` |
| MCP 服务 | `/v1/mcp-services[/{id}]`、`.../data-scope`、`.../tools[/{tool_id}]`、`.../tools/detect-annotations`、`.../prompts[/{prompt_id}]`、`.../publish`、`.../enable`、`.../disable`、`.../diff`、`.../snapshots`、`.../rollback`、`.../stats`、`.../invocation-logs` |
| 模板 | `/v1/template/preview`、`/v1/template/extract` |
| 权限 | `/v1/acls/{resource_type}/{resource_id}[/{principal_type}/{principal_id}]` |
| 系统 | `/v1/system/config` |

鉴权方式两种，都放在 `Authorization: Bearer …` 里：登录得到的 JWT，或以 `sk_` 前缀开头的 API Key。

## MCP 端点

服务发布后是一个**无状态** JSON-RPC 端点：`POST /{service_code}/mcp`。

```bash
curl -X POST http://localhost:8085/sales/mcp \
  -H 'Authorization: Bearer sk_xxx' \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'
```

实现的方法：`initialize`、`notifications/initialized`、`ping`、`tools/list`、`tools/call`、
`prompts/list`、`prompts/get`。这里的 JSON 保持 MCP 规范的 **camelCase**。

内置工具（可在服务里逐个启停）：

| 工具名 | 类型 | 作用 |
| --- | --- | --- |
| `search_tables_and_terms` | `SEARCH_METADATA` | 按关键词检索表与业务口径 |
| `get_table_schema` | `GET_TABLE_INFO` | 取一张表的字段与口径 |
| `list_tables_and_terms` | `LIST_TABLES` | 列出服务范围内的表和主题 |
| `execute_sql` | `EXECUTE_SQL` | 在数据源上执行 SQL，受 SQL 策略（允许的操作、最大行数）约束 |
| `update_table_metadata` | `UPDATE_TABLE_INFO` | 回写表描述/别名 |
| `update_column_metadata` | `UPDATE_COLUMN_INFO` | 回写字段描述/口径 |
| `upsert_business_term` | `UPSERT_TERM` | 新增或更新业务口径 |

`PARAMETERIZED_SQL` 是模板型工具，由参数化 SQL 生成，不对应固定工具名。调用统计（次数、耗时、成败）
按天聚合并保留明细，见 `.../stats` 与 `.../invocation-logs`。

## 项目结构

```
crates/dati-core/   领域模型、repo（SQL）、service（业务与校验）、连接器、认证、MCP 协议——不依赖 axum
crates/dati-app/    axum 路由与提取器、内嵌前端、build.rs、集成测试（tests/*_slice.rs）；产出 dati 可执行文件
migrations/         {sqlite,mysql,postgres}/0001_init.sql
frontend/           从 Java 仓库原样复制的 Vue 3 + Element Plus 管理后台
docs/user-guide/    VitePress 用户文档（中/英），构建后挂在 /docs
db/                 默认 SQLite 知识库所在位置（已 gitignore）
```

`dati-core` 内部按 `model → repo → service` 分层：`model` 只放数据结构和 wire 形状，`repo` 只放 SQL 和
行映射，`service` 放业务规则与错误码，HTTP 相关的形状全部留在 `dati-app`。

## 开发

```bash
cargo fmt --all
cargo clippy --all-targets --workspace -- -D warnings
cargo test --workspace          # 300+ 单元测试 + 5 个 slice 集成测试，全程离线
cargo build --release           # 单文件产物
```

只改后端、不想等前端时可以跳过前端构建：

```bash
DATI_SKIP_FRONTEND=1 cargo build
```

前端单独开发：

```bash
cd frontend && pnpm install && pnpm dev          # vite dev server
cd frontend && pnpm docs:dev                     # VitePress 文档预览（5174）
cd frontend && pnpm test                         # vitest
```

工具链：edition 2024（rustc ≥ 1.85，本仓库开发用 1.97）、sqlx 0.8.6、axum 0.8.9、tokio 1.53。
测试不依赖任何外部数据库：涉及知识库的用例在临时目录建 SQLite，涉及远程数据源的用例用内置的
MySQL 协议假服务。

更细的约定、常见坑和验证流程写在 [AGENTS.md](AGENTS.md)。

## 已知边界

- **Doris** 需要单独一条驱动路径：FE 拒绝 sqlx 连接后的会话设置，也可能关掉服务端 prepared statement，
  且 `sqlx::any` 无法把 MySQL 的 TINYINT/DECIMAL/DATE 列转成 `AnyRow`。因此 Doris 用 typed MySQL 池、
  文本协议、逐列解码。相同类型洞在**以 `Any` 打开的真实 MySQL** 上仍存在，尚未收口。
- **不接 Elasticsearch**。Java 版的语义检索、业务口径、字段取值抽样存在 ES + IK 分词里，这里全部改存知识
  库表并用 `LIKE`/别名匹配实现，`search_tables_and_terms` 的输出形状保持不变。
- **不接 LDAP 登录**。Java 的 LDAP 分支有意不移植，走到该分支返回 `AUTH004` 是预期行为。
- SQL 分析是**词法**实现（切语句 + 识别首个关键字 + 表关键字操作数校验），不是完整 parser；解析不出来
  的语句归为 `OTHER` 且 `statementTypes` 为空，与 Java 的 parse-failure 路径一致。

## 许可

Apache-2.0。
