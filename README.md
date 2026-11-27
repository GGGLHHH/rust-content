# content

自包含的**内容 / 媒体领域与服务库(零 HTTP)**:`ContentService` + 可替换的仓储 / 对象存储 / 时钟端口 + 自己的 `ContentError`。
作为库嵌进应用使用,HTTP 边界(路由、DTO、校验、状态码)归消费方,生产用的 S3 / minio 存储也由消费方注入。

> 骨架和 **rust-idm** 一样(trait 端口 + 内存 / Postgres 实现 + builder 建服务 + 领域自己的错误);从 Go 版 content 服务移植。

## 里面有什么

- **领域**:`ContentService`(内容增删改查 / 一次性上传 / 两步上传 / 下载与预览 / 元数据 / 状态流转)+ 仓储端口
  `ContentRepo` / `ObjectRepo`(内存和 Postgres 两种实现,sqlx + sea-query)。
- **对象存储端口**:`ObjectStore`(`upload` / `download` / `delete` / `object_meta`,以及给浏览器直传直下的
  `upload_url` / `download_url` / `preview_url`),地位相当于 idm 的令牌端口:默认 `InMemoryObjectStore` 零外部依赖,
  生产由应用经 builder 注入 minio / S3。内容是整块的 `bytes::Bytes`(先缓冲,流式以后再做)。
- **时钟端口**:`Clock` / `SystemClock`(注入固定时钟,确定性地测时间戳)。
- **领域类型和错误**:`Content` / `Object` / `ContentMetadata` / `ObjectMetadata` / `UploadOutcome`(纯数据,不直接上线)
  + 强类型的 `ContentStatus` / `ObjectStatus` 生命周期,`ContentError` 八种:`NotFound` / `NotReady` / `InvalidState` /
  `InvalidStatus` / `InvalidInput` / `Conflict` / `Storage` / `Internal`。HTTP 状态码、错误码和响应格式由消费方经
  `From<ContentError> for AppError` 决定。
- **作用域(多租户)**:service / repo 每个方法第一个业务参数是 `ContentScope`:
  - `ContentScope::Tenant(id)`(用 `ContentScope::tenant(id)` 构造;nil UUID → `InvalidInput`):只看得到这个租户的行;
  - `ContentScope::Personal`:个人内容(如头像,不属于任何租户),库里 `tenant_id` 存 NULL,`Content.tenant_id = None`。
  两种作用域互不可见;按 ID 取别的作用域的内容一律 `NotFound`,列表只回当前作用域、按 owner 过滤。
  默认租户常量 `DEFAULT_TENANT_ID` = `00000000-0000-0000-0000-000000000001`(和 rust-idm / baserust 同值)。
- **对象 key**:没传 `object_key` 时服务按作用域生成 —— 租户 `tenants/{tenant_id}/{content_id}/{uuid}`,
  个人 `personal/{owner_id}/{content_id}/{uuid}`;调用方传的 key 原样保留,已有对象的 key 不搬家。
- **Postgres 隔离**:每个仓储方法一个事务,第一条语句 `select set_config('app.tenant_id', $1, true)`(个人内容设
  `app.personal_scope = 'on'`),SQL 里同时带租户条件;五张表都 ENABLE + FORCE RLS(迁移 `0003`),
  不设上下文时读不到也写不进。运行时账号要用**非 owner、NOBYPASSRLS** 的(owner 只跑迁移)。
- **迁移**:在 `migrations/`,拷进应用的 `migrations/content` 跑。`0002`(派生内容)只有表结构和一个不透明的
  `DerivedContent` 占位,派生逻辑以后再做。`0003`(多租户)给子表加 `tenant_id`、把旧行的租户(nil 或旧接口收下的任意值)一律回填成默认租户、
  加非 nil CHECK、`(tenant_id, …)` 索引和 RLS 策略。
- **一致性(有意的取舍)**:`upload_content` 先建数据库行再推字节,没有跨仓储事务:存储上传失败可能留下孤儿行;
  同步后端元数据的步骤失败不致命。见 `service.rs` / `repo/mod.rs`。

## 怎么用

```toml
[dependencies]
content = { git = "https://github.com/GGGLHHH/rust-content", tag = "v0.4.0" }
```

实现仓储端口(或直接用自带的 `InMemory*` / `Pg*`),注入一个 `ObjectStore`,建 `ContentService` 放进 `AppState`
(`Clone` 很便宜),在自己的 handler 里调用:

```rust
use std::sync::Arc;
use content::{ContentService, InMemoryContentRepo, InMemoryObjectRepo, InMemoryObjectStore};

let svc = ContentService::new(
    Arc::new(InMemoryContentRepo::new()),
    Arc::new(InMemoryObjectRepo::new()),
    Arc::new(InMemoryObjectStore::new()),   // 生产:换成自己的 minio / S3 实现
    "memory",                               // 写进 object 行的存储后端名
);
// 也可以用 builder 换掉单个端口(自定义存储 / 后端名 / 测试时钟):
let svc = ContentService::builder(contents, objects)
    .store(my_s3_store)      // 必填:不设 build() 会 panic(接线错误,启动时就暴露)
    .backend_name("s3")
    .build();

let scope = ContentScope::tenant(current_tenant_id)?;                   // 或 ContentScope::Personal(头像等)
let out = svc.upload_content(scope, upload_input, Some(actor)).await?;  // -> UploadOutcome { content, object }
let bytes = svc.download_content(scope, out.content.id).await?;         // -> Bytes(上传完成前是 NotReady)
```

## 服务方法

`ContentService`:`create_content` · `get_content` · `update_content` · `delete_content`(软删)· `list_content` ·
`upload_content` · `prepare_upload` · `confirm_upload`(浏览器直传的两步上传)· `download_content` · `preview_content` ·
`download_url` · `preview_url` · `set_content_metadata` · `get_content_metadata` · `get_objects` · `set_content_status` ·
`set_object_status`。第一个参数是 `scope: ContentScope`,其后是领域的 `*Input` 结构(写方法另带 `by: Option<String>`
做审计),返回领域数据;路由、鉴权(谁能用哪个租户 / 谁能改个人内容)、响应格式归消费方。

## 版本变更

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

**已知影响(升级时一次性)**:`0003` 的回填是普通 `UPDATE`,会触发各表的 `set_updated_at` 触发器 ——
升级前已有的行(`content`、`content_metadata`、`object`、`object_metadata`、`content_derived`)的 `updated_at`
都会变成**跑迁移的那一刻**,原来的「最后修改时间」丢失(`created_at` 不受影响)。迁移已在已有库上执行过,不再改它;
依赖 `updated_at` 做增量同步 / 排序的消费方,升级后第一次会把全部旧行当成刚改过。

## 测试

```sh
just test       # 单元测试 + 内存仓储的一致性契约(不要数据库)
just test-pg    # + 内存↔Postgres 仓储对拍(要有在跑的 pg,content role,需 CREATEDB + CREATEROLE:测试会建
                # NOBYPASSRLS 的运行时账号 content_rt,经 SET ROLE 跑仓储,RLS 真生效)。CREATEROLE 只在一次性的
                # 测试库 / 测试容器上授,别授给开发库(默认 5821)的 content 账号;用 PG_PORT 指向测试容器
just lint       # rustfmt 检查 + clippy -D warnings(和 CI 一样)
```

工具、lint 配置和约定与 baserust 一致,见 `AGENTS.md`。

## 许可

MIT,见 [LICENSE](LICENSE)。
