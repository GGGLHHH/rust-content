//! content —— 自包含的内容/媒体**领域/服务库**(零 HTTP)。
//!
//! 暴露:[`ContentService`](编排内容 CRUD / 一次性 upload / 两步上传 / download / 元数据 / 状态),可拔插仓储端口
//! ([`ContentRepo`] / [`ObjectRepo`],内存 / PG 实现),**存储后端端口**([`ObjectStore`],默认
//! [`InMemoryObjectStore`] —— 生产 minio/S3 由 app 注入),**时间端口**([`Clock`] / [`SystemClock`]),领域类型
//! ([`Content`] / [`Object`] / [`ContentMetadata`] / [`ObjectMetadata`] + 状态枚举),纯数据结果
//! ([`UploadOutcome`] 等),领域错误 [`ContentError`]。**不含任何 HTTP**:路由、DTO、校验、cookie、状态码、
//! OpenAPI、minio 接线全归消费方(app)—— app 在自己的 `features/` 建端点、注入 S3/minio 的 `ObjectStore`,并
//! `From<ContentError> for AppError` 接错误。
//!
//! 分层范式:`service → repo(trait + 内存 / PG 实现)→ 领域类型`,service 依赖 trait 而非实现
//! (端口是 `ContentService` 的类型参数;要运行时选实现就填 `Arc<dyn …>` / `Box<dyn …>`)。
//! 迁移表见 `migrations/`(消费方 copy 进自己的 migrations/content 跑,落哪个 schema 由 search_path 决定)。
//!
//! **作用域(多租户)**:service / repo 每个方法第一个业务参数是 [`ContentScope`](`Tenant(id)` | `Personal`),
//! 按 ID 取单条时别的作用域一律 `NotFound`;PG 实现每个方法一个事务、先 `set_config` 再查,表上另有 RLS 兜底。
//!
//! 派生内容(`content_derived`)以后再做:目前只有 `0002` 迁移建的表,没有对应的 Rust 类型和 service 逻辑。

// Google Rust 风格指南:必须 panic 时用 expect("原因") 说明不变量,不用 unwrap。
#![warn(clippy::unwrap_used)]

mod clock;
mod content_error;
mod content_scope;
mod content_service;
mod content_status;
mod in_memory_object_store;
mod input;
mod object_status;
mod object_store;
mod repo;
mod types;

pub use clock::{Clock, SystemClock};
pub use content_error::ContentError;
pub use content_scope::{ContentScope, DEFAULT_TENANT_ID};
pub use content_service::{
    ContentService, ContentServiceBuilder, PrepareOutcome, Preview, UploadOutcome,
};
pub use content_status::ContentStatus;
pub use in_memory_object_store::InMemoryObjectStore;
pub use input::{
    CreateContentInput, PrepareUploadInput, SetContentMetadataInput, UpdateContentInput,
    UploadContentInput,
};
pub use object_status::ObjectStatus;
pub use object_store::{ObjectStore, StoredObjectMetadata, UploadRequest};
pub use repo::{
    ContentRepo, InMemoryContentRepo, InMemoryObjectRepo, NewContent, NewObject, ObjectLocation,
    ObjectRepo, PgContentRepo, PgObjectRepo,
};
pub use types::{Content, ContentMetadata, Object, ObjectMetadata};

/// **仅测试用**(`--features pg-conformance` 才编译):PG 一致性测试直接调仓储内部的 `scoped_tx`,
/// 在它开的事务里跑不带作用域条件的 SQL,证明「只靠 RLS」也只放行本作用域的行。不是公开 API。
///
/// 指南「Do not access items marked `#[doc(hidden)]` that you don't own」:本模块只给本仓库自己的
/// `tests/content_repo_conformance.rs` 用(同一个 owner),下游不得引用。
#[cfg(feature = "pg-conformance")]
#[doc(hidden)]
pub mod __pg_testing {
    pub use crate::repo::scoped_tx;
}
