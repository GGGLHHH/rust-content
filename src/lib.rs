//! content —— 自包含的内容/媒体**领域/服务库**(零 HTTP)。
//!
//! 暴露:`ContentService`(编排内容 CRUD / 一次性 upload / download / 元数据 / 状态),可拔插仓储端口
//! (`ContentRepo`/`ObjectRepo`,内存/PG),**存储后端端口**(`ObjectStore`,默认 `InMemoryObjectStore`
//! —— 生产 minio/S3 由 app 注入),**时间端口**(`Clock`/`SystemClock`,测试可注入固定时钟),领域类型
//! (`Content`/`Object`/`ContentMetadata`/`ObjectMetadata` + 状态枚举),纯数据契约(`UploadOutcome`),
//! 领域错误 `ContentError`。**不含任何 HTTP**:路由、DTO、校验、cookie、状态码、OpenAPI、minio 接线全归
//! 消费方(app)—— app 在自己的 `features/` 建端点、注入 S3/minio 的 `ObjectStore`,并
//! `From<ContentError> for AppError` 接错误。
//!
//! 分层范式:`service → repo(trait + memory/postgres)→ 领域类型`,service 依赖 trait 而非实现。
//! 迁移表见 `migrations/`(消费方 copy 进自己的 migrations/content 跑,落哪个 schema 由 search_path 决定)。
//!
//! 派生内容(`content_derived`)是 **DEFER**:仅 ship `0002` 迁移 + 不透明 `DerivedContent` stub,无 service 逻辑。

mod clock;
mod error;
mod input;
mod repo;
mod service;
mod status;
mod store;
mod types;

pub use clock::{Clock, SystemClock};
pub use error::ContentError;
pub use input::{
    CreateContentInput, SetContentMetadataInput, UpdateContentInput, UploadContentInput,
};
pub use repo::{
    ContentRepo, InMemoryContentRepo, InMemoryObjectRepo, NewContent, NewObject, ObjectRepo,
    PgContentRepo, PgObjectRepo,
};
pub use service::{ContentService, ContentServiceBuilder, Preview, UploadOutcome};
pub use status::{ContentStatus, ObjectStatus};
pub use store::{InMemoryObjectStore, ObjectMeta, ObjectStore, UploadParams};
pub use types::{Content, ContentMetadata, DerivedContent, Object, ObjectMetadata};
