# content

自包含的**内容 / 媒体领域与服务库(零 HTTP)**:`ContentService` + 可替换的仓储 / 对象存储 / 时钟端口 + 自己的 `ContentError`。
作为库嵌进应用使用,HTTP 边界(路由、DTO、校验、状态码)归消费方,生产用的 S3 / minio 存储也由消费方注入。

> 骨架和 **rust-idm** 一样(trait 端口 + 内存 / Postgres 实现 + builder 建服务 + 领域自己的错误);从 Go 版 content 服务移植。

## 里面有什么

- **领域**:`ContentService`(内容增删改查 / 一次性上传 / 两步上传 / 下载与预览 / 元数据 / 状态流转)+ 仓储端口
  `ContentRepo` / `ObjectRepo`(内存和 Postgres 两种实现,sqlx + sea-query)。
- **对象存储端口**:`ObjectStore`(`upload` / `download` / `delete` / `metadata`,以及给浏览器直传直下的
  `upload_url` / `download_url` / `preview_url`),地位相当于 idm 的令牌端口:默认 `InMemoryObjectStore` 零外部依赖,
  生产由应用经 builder 注入 minio / S3。内容是整块的 `bytes::Bytes`(先缓冲,流式以后再做)。
- **时钟端口**:`Clock` / `SystemClock`。**落库的时间戳一律由它决定**:service 每个写用例经它取一次 now,交给仓储
  (建行的 `created_at`、改 / 置状态 / 软删的 `at`、元数据的 `created_at` / `updated_at`),内存和 PG 两种仓储都按传入值落库
  (PG 显式写时间列,不靠列默认值;`set_updated_at_utc()` 触发器在仓储事务里不覆盖显式值,见迁移 `0004`)。
  注入固定时钟后,`Content` / `Object` / 两张元数据表读回来的时间戳就是它给的值。
- **领域类型和错误**:`Content` / `Object` / `ContentMetadata` / `ObjectMetadata` / `UploadOutcome`(纯数据,不直接上线)
  + 强类型的 `ContentStatus` / `ObjectStatus` 生命周期,`ContentError` 八种(`#[non_exhaustive]`,`match` 要留通配分支):`NotFound` / `NotReady` / `InvalidState` /
  `InvalidStatus` / `InvalidInput` / `Conflict` / `Storage` / `Internal`。HTTP 状态码、错误码和响应格式由消费方经
  `From<ContentError> for AppError` 决定。
- **作用域(多租户)**:service / repo 每个方法第一个业务参数是 `ContentScope`:
  - `ContentScope::Tenant(id)`(用 `ContentScope::try_new_tenant(id)` 构造;nil UUID → `InvalidInput`):只看得到这个租户的行;
  - `ContentScope::Personal`:个人内容(如头像,不属于任何租户),库里 `tenant_id` 存 NULL,`Content.tenant_id = None`。
  两种作用域互不可见;按 ID 取别的作用域的内容一律 `NotFound`,列表只回当前作用域、按 owner 过滤。
  默认租户常量 `DEFAULT_TENANT_ID` = `00000000-0000-0000-0000-000000000001`(和 rust-idm / baserust 同值)。
- **对象 key**:没传 `object_key` 时服务按作用域生成 —— 租户 `tenants/{tenant_id}/{content_id}/{uuid}`,
  个人 `personal/{owner_id}/{content_id}/{uuid}`;调用方传的 key 原样保留,已有对象的 key 不搬家。
- **Postgres 隔离**:每个仓储方法一个事务,第一条语句 `select set_config('app.tenant_id', $1, true)`(个人内容设
  `app.personal_scope = 'on'`),SQL 里同时带租户条件;五张表都 ENABLE + FORCE RLS(迁移 `0003`),
  不设上下文时读不到也写不进。运行时账号要用**非 owner、NOBYPASSRLS** 的(owner 只跑迁移)。
- **迁移**:在 `migrations/`,拷进应用的 `migrations/content` 跑。`0002`(派生内容)只有表结构,Rust 侧没有对应类型,
  派生逻辑以后再做。`0003`(多租户)给子表加 `tenant_id`、把旧行的租户(nil 或旧接口收下的任意值)一律回填成默认租户、
  加非 nil CHECK、`(tenant_id, …)` 索引和 RLS 策略。`0004`(显式时间戳)改 `set_updated_at_utc()` 触发器函数:
  显式写入的 `updated_at` 不再被 `now()` 覆盖(见 0.5.0 版本变更)。
- **一致性(有意的取舍)**:`upload_content` 先建数据库行再推字节,没有跨仓储事务:存储上传失败可能留下孤儿行;
  同步后端元数据的步骤失败不致命。见 `content_service.rs` / `repo/mod.rs`。

## 怎么用

```toml
[dependencies]
content = { git = "https://github.com/GGGLHHH/rust-content", tag = "v0.5.0" }
```

> `v0.5.0` 这个 tag **发版后**才存在(目前已有的 tag 只到 `v0.3.1`);发版前要用当前代码,按提交钉住:
> `content = { git = "https://github.com/GGGLHHH/rust-content", rev = "<提交>" }`。

实现仓储端口(或直接用自带的 `InMemory*` / `Pg*`),注入一个 `ObjectStore`,建 `ContentService`,在自己的 handler 里调用。
端口是 `ContentService<C, O, S, K = SystemClock>` 的类型参数;要在运行时二选一实现(内存 / PG、内存 / S3),
就填 `Arc<dyn …>`(或独占的 `Box<dyn …>`)—— 三个端口 trait 和 `Clock` 都给 `Arc<T>` / `Box<T>` 做了转发实现:

```rust
use std::sync::Arc;
use content::{
    ContentRepo, ContentScope, ContentService, InMemoryContentRepo, InMemoryObjectRepo,
    InMemoryObjectStore, ObjectRepo, ObjectStore, UploadContentInput,
};

// 运行时选实现时,给服务类型起个别名;端口全是 Arc 时 Clone 很便宜,可直接放进 AppState。
type AppContentService = ContentService<Arc<dyn ContentRepo>, Arc<dyn ObjectRepo>, Arc<dyn ObjectStore>>;

let contents: Arc<dyn ContentRepo> = Arc::new(InMemoryContentRepo::new());
let objects: Arc<dyn ObjectRepo> = Arc::new(InMemoryObjectRepo::new());
let store: Arc<dyn ObjectStore> = Arc::new(InMemoryObjectStore::new()); // 生产:换成自己的 minio / S3 实现
let service: AppContentService = ContentService::new(contents, objects, store, "memory"); // 最后一个是写进 object 行的后端名
// 也可以用 builder 覆盖默认值(后端名 / 测试时钟);三个端口必填,不会再有「忘了设存储」的 panic:
// let service = ContentService::builder(contents, objects, store).backend_name("s3").clock(my_clock).build();

let scope = ContentScope::try_new_tenant(current_tenant_id)?;          // 或 ContentScope::Personal(头像等)
let mut input = UploadContentInput::new(owner_id, bytes);              // 必填字段走 new,可选字段直接赋值
input.file_name = Some("a.png".to_owned());
let out = service.upload_content(scope, input, Some(actor)).await?;    // actor: &str -> UploadOutcome { content, object }
let bytes = service.download_content(scope, out.content.id).await?;    // -> Bytes(上传完成前是 NotReady)
```

## 服务方法

`ContentService`:`create_content` · `get_content` · `update_content` · `delete_content`(软删)· `list_content` ·
`upload_content` · `prepare_upload` · `confirm_upload`(浏览器直传的两步上传)· `download_content` · `preview_content` ·
`download_url` · `preview_url` · `set_content_metadata` · `get_content_metadata` · `get_objects` · `set_content_status` ·
`set_object_status`。第一个参数是 `scope: ContentScope`,其后是领域的 `*Input` 结构(写方法另带 `by: Option<&str>`
做审计),返回领域数据;路由、鉴权(谁能用哪个租户 / 谁能改个人内容)、响应格式归消费方。

## 版本变更

### 0.5.0(按 Google Rust 风格指南修正 API + 时间戳改由注入的时钟决定;相对 0.4.x **破坏性**)

> **0.4.0 从未打 tag**:仓库里的 tag 只有 `v0.1.0` / `v0.2.0` / `v0.3.0` / `v0.3.1`。`Cargo.toml` 写着 0.4.0 的那段代码
> (多租户 + 复审修复)下游是按提交钉住用的 —— baserust 曾依赖 `rev = "fa3e600"`。所以「从 0.4.x 升级」指的是
> 从这个提交升级;从 `v0.3.1` 升上来要把下面 0.4.0 和 0.5.0 两节都做一遍。

除了**时间戳**,行为不变(作用域隔离、上传流程都没动),其余只改 API 形状。

**时间戳改由注入的时钟决定(行为变更)**:以前仓储自己取时间(内存取系统时钟,PG 靠列默认值 + `set_updated_at_utc()`
触发器),service 经 `Clock` 取的 now 写进元数据结构也会被忽略;现在仓储的写方法都收时间、按传入值落库,
service 每个写用例经 `Clock` 取一次 now 传下去。升级要做的:

1. 把 `migrations/0004_explicit_timestamps.*` 拷进应用的 `migrations/content` 跑。它只换 `set_updated_at_utc()` 的函数体:
   这次 UPDATE 改了 `updated_at`,或事务级开关 `app.explicit_updated_at = 'on'`(PG 仓储每个事务都设)时,保留显式写入的值;
   否则照旧置 `now()`(仓储之外的 SQL 不受影响)。**不跑 0004** 时 PG 的 `updated_at` 仍会被触发器改成 DB 时间
   (`created_at` 已按传入值落库)。
2. 自己实现仓储端口的,按下表改签名并按传入值写时间戳;直接调仓储的,补上时间参数。

逐条的旧写法 → 新写法:

| 旧 | 新 | 说明 |
|---|---|---|
| `ContentService`(内部 `Arc<dyn …>`) | `ContentService<C, O, S, K = SystemClock>` | 端口是类型参数;运行时选实现填 `Arc<dyn ContentRepo>` 等(端口 trait 有 `Arc<T>` / `Box<T>` 转发实现)。端口全是 `Arc` 时 `Clone` 仍然便宜 |
| `ContentService::new(Arc::new(r1), Arc::new(r2), Arc::new(s), name)` | 同名,参数是任意实现(`Arc<dyn …>` 也行) | |
| `ContentService::builder(c, o).store(s)…build()`(不设 store 会 panic) | `ContentService::builder(c, o, s)…build()` | store 改成必填参数,panic 路径没了;`.store()` 删除 |
| `.clock(Arc<dyn Clock>)` | `.clock(impl Clock)` | 换时钟会改 builder 的类型参数 |
| 写方法 `by: Option<String>` | `by: Option<&str>` | service 和两个仓储 trait 都改了;调用方 `.as_deref()` |
| `ContentScope::tenant(id)` | `ContentScope::try_new_tenant(id)` | 返回 `Result` 的构造函数叫 `try_new*` |
| `scope.tenant_id()`(`&self`) | 同名,`self` 按值 | `ContentScope: Copy`,调用写法不变 |
| `ContentStatus::from_db(s)` / `ObjectStatus::from_db(s)` | `let status: ContentStatus = s.parse()?`(`FromStr`) | 另加了 `Display`(= `as_str()`) |
| `ObjectMeta` / `ObjectStore::object_meta` | `StoredObjectMetadata` / `ObjectStore::metadata` | 去掉缩写,并和表 `object_metadata` 的 `ObjectMetadata` 区分开 |
| `UploadParams` / `ObjectStore::upload(params, data)` | `UploadRequest` / `ObjectStore::upload(request, data)` | |
| `NewContent` / `NewObject`(无时间戳) | 加字段 `created_at: OffsetDateTime`;`NewContent::new(owner_id, created_at)`、`NewObject::new(content_id, location, created_at)` | 新行 `created_at` = `updated_at` = 它;`Content::new(id, scope, input)` / `Object::new(id, input)` 的时间戳也取它 |
| `ContentRepo::update(scope, &content, by)` | `update(scope, &content, at, by)` | `updated_at` = `at` |
| `ContentRepo::set_status(scope, id, status, by)` / `ObjectRepo::set_status(…)` | `set_status(scope, id, status, at, by)` | `updated_at` = `at` |
| `ContentRepo::soft_delete(scope, id, by)` / `ObjectRepo::soft_delete(…)` | `soft_delete(scope, id, at, by)` | `deleted_at` = `updated_at` = `at` |
| `set_metadata(scope, metadata)`:传入的时间戳被忽略 | 签名不变,按传入值落库 | `updated_at` 取传入值;新插入的行 `created_at` 取传入值,已存在的行保留原 `created_at` |
| `ObjectRepo::get_by_key(scope, key, backend)` | `ObjectRepo::get_by_location(scope, &ObjectLocation { storage_backend_name, object_key })` | 两个并排 `&str` 改具名结构体;PG span 名随之改为 `pg.object.get_by_location` |
| `DerivedContent`(派生表的占位类型,无构造函数、无任何方法用到) | 删除 | 表 `content_derived` 和迁移 `0002` 不变;派生功能做的时候再按需要设计类型 |
| 公开结构体字面量构造 | `#[non_exhaustive]` + `new(必填字段)`,可选字段构造后直接赋值 | `Content::new` / `Object::new` / `ContentMetadata::new` / `ObjectMetadata::new` / `StoredObjectMetadata::new` / `UploadRequest::new` / `NewContent::new` / `NewObject::new` / `CreateContentInput::new` / `UploadContentInput::new` / `PrepareUploadInput::new` / `SetContentMetadataInput::new`。`UpdateContentInput` 例外:保留字面量 + `Default` |
| 枚举穷尽 `match` | `ContentError` / `ContentStatus` / `ObjectStatus` / `ContentScope` 都 `#[non_exhaustive]` | 库外 `match` 要加通配分支 |

另:`ContentError` 加了 `#[must_use]`;实体 / 输入 / 结果类型补了 `PartialEq`;`PgContentRepo` / `PgObjectRepo` 可 `Clone` / `Debug`;
内存实现和 `SystemClock` 补了 `Debug` / `Default`;`ContentService` / builder 在端口都实现时可 `Clone` / `Debug`。

依赖收紧(只影响靠 content 顺带开 feature 的下游):`tokio` 改为 dev-dependency(库代码不用它,运行时由消费方提供);
`sqlx` 关掉默认 feature,只开库代码用到的(`macros` / `migrate` 移到 dev-dependencies);`uuid` 去掉 `v4`、
`time` 去掉 `serde-well-known`。下游自己用到这些 feature 的,要在自己的 `Cargo.toml` 里声明(baserust 已自行声明)。

### 0.4.0(多租户;相对 0.3.x **破坏性**)

升级要做的:把 `migrations/0003_tenancy.*` 拷进应用的 `migrations/content` 跑(**运行时账号必须是非 owner、
NOBYPASSRLS 的**,否则 RLS 形同虚设);每个调用点补上 `ContentScope`。

| 变更 | 说明 |
|---|---|
| `ContentScope`(`Tenant(Uuid)` / `Personal`) | 新类型;`ContentService` **每个方法**和 `ContentRepo` / `ObjectRepo` 每个方法的第一个业务参数。构造用 `ContentScope::tenant(id)`(nil → `InvalidInput`) |
| `Content.tenant_id: Option<Uuid>` | 原来是 `Uuid`;`None` = 个人内容(库里 NULL)。`Object` 等领域类型不暴露租户(库里子表各带 `tenant_id` 列) |
| `*Input` 去掉 `tenant_id` 字段 | 租户只从 `ContentScope` 来,不再随输入结构传 |
| `ContentError::InvalidInput(String)` | 新变体(nil 租户等非法入参)。消费方的 `From<ContentError>` 要加一支 |
| 对象 key 格式 | 没传 `object_key` 时:租户 `tenants/{tenant_id}/{content_id}/{uuid}`,个人 `personal/{owner_id}/{content_id}/{uuid}`。显式传的 `object_key` 必须以本作用域前缀开头(租户 `tenants/{tenant_id}/`、个人 `personal/{owner_id}/`),前缀后非空、不含 `..` 段,否则 `InvalidInput`(`upload_content` / `prepare_upload` 都校验,拒了不建行)。已有对象的 key 不搬家 |
| 可见性 | 按 ID 取别的作用域的内容 → `NotFound`;两种作用域互不可见 |
| PG 仓储 | 每个方法一个事务,先 `set_config('app.tenant_id', …, true)`(个人内容设 `app.personal_scope`)再查;另一个变量同时清成 `''`,连接上的会话级残留不会让 RLS 的另一支放行 |
| 常量 `DEFAULT_TENANT_ID` | `00000000-0000-0000-0000-000000000001`(与 rust-idm / baserust 同值) |
| 迁移 `0003_tenancy` | 子表加 `tenant_id` 并回填;旧行的租户(nil 或旧接口收下的任意客户端值)一律回填成默认租户;非 nil CHECK、`(tenant_id, …)` 索引;五张表 ENABLE + FORCE RLS |

**已知影响(升级时一次性)**:`0003` 的回填是普通 `UPDATE`,会触发各表的 `set_updated_at_utc()` 触发器 ——
升级前已有的行(`content`、`content_metadata`、`object`、`object_metadata`、`content_derived`)的 `updated_at`
都会变成**跑迁移的那一刻**,原来的「最后修改时间」丢失(`created_at` 不受影响)。迁移已在已有库上执行过,不再改它;
依赖 `updated_at` 做增量同步 / 排序的消费方,升级后第一次会把全部旧行当成刚改过。

## 测试

```sh
just test       # 单元测试 + 内存仓储的一致性契约(不要数据库)
just test-pg    # + 内存↔Postgres 仓储对拍(要有在跑的 pg,content role,需 CREATEDB + CREATEROLE:测试会建
                # NOBYPASSRLS 的运行时账号 content_rt,经 SET ROLE 跑仓储,RLS 真生效)。CREATEROLE 只在一次性的
                # 测试库 / 测试容器上授,别授给开发库(默认 5821)的 content 账号;用 PG_PORT 指向测试容器
just lint       # rustfmt 检查(稳定版 + nightly 的 Google 风格三项)+ clippy -D warnings(和 CI 一样)
just fmt        # 稳定版 rustfmt + nightly rustfmt 的 Google 风格三项
```

`just fmt` / `just lint` 需要 **nightly rustfmt**(Google Rust 风格指南要求的 `imports_granularity=Module`、
`hex_literal_case=Lower`、`format_macro_matchers=true` 只有 nightly 支持;稳定版只认 `rustfmt.toml` 里的
`max_width = 100`)。没装就先:`rustup toolchain install nightly --profile minimal --component rustfmt`。
代码本身只用稳定版编译,nightly 只拿来格式化。

工具、lint 配置和约定与 baserust 一致,见 `AGENTS.md`。

## 许可

MIT,见 [LICENSE](LICENSE)。
