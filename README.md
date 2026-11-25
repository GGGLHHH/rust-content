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
  + 强类型的 `ContentStatus` / `ObjectStatus` 生命周期,`ContentError` 七种:`NotFound` / `NotReady` / `InvalidState` /
  `InvalidStatus` / `Conflict` / `Storage` / `Internal`。HTTP 状态码、错误码和响应格式由消费方经
  `From<ContentError> for AppError` 决定。
- **迁移**:在 `migrations/`,拷进应用的 `migrations/content` 跑。`0002`(派生内容)只有表结构和一个不透明的
  `DerivedContent` 占位,派生逻辑以后再做。
- **一致性(有意的取舍)**:`upload_content` 先建数据库行再推字节,没有跨仓储事务:存储上传失败可能留下孤儿行;
  同步后端元数据的步骤失败不致命。见 `service.rs` / `repo/mod.rs`。

## 怎么用

```toml
[dependencies]
content = { git = "https://github.com/GGGLHHH/rust-content", tag = "v0.3.1" }
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

let out = svc.upload_content(upload_input, Some(actor)).await?; // -> UploadOutcome { content, object }
let bytes = svc.download_content(out.content.id).await?;        // -> Bytes(上传完成前是 NotReady)
```

## 服务方法

`ContentService`:`create_content` · `get_content` · `update_content` · `delete_content`(软删)· `list_content` ·
`upload_content` · `prepare_upload` · `confirm_upload`(浏览器直传的两步上传)· `download_content` · `preview_content` ·
`download_url` · `preview_url` · `set_content_metadata` · `get_content_metadata` · `get_objects` · `set_content_status` ·
`set_object_status`。参数是领域的 `*Input` 结构(写方法另带 `by: Option<String>` 做审计),返回领域数据;
路由、鉴权 / 租户隔离、响应格式归消费方。

## 测试

```sh
just test       # 单元测试 + 内存仓储的一致性契约(不要数据库)
just test-pg    # + 内存↔Postgres 仓储对拍(要有在跑的 pg;默认连 baserust 本地 compose 的 pg,content role)
just lint       # rustfmt 检查 + clippy -D warnings(和 CI 一样)
```

工具、lint 配置和约定与 baserust 一致,见 `AGENTS.md`。

## 许可

MIT,见 [LICENSE](LICENSE)。
