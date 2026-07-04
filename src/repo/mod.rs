//! content 仓储端口:`ContentRepo`(content + content_metadata)、`ObjectRepo`(object + object_metadata)。
//! 沿"内容聚合 vs 对象聚合"的自然边界把 Go 的单体 `Repository` 拆两个端口,镜像 idm 拆
//! UserRepo/SessionRepo/RoleRepo。范式同 widget:trait 端口 + 内存/PG 实现分文件,service 依赖 trait。
//!
//! 写操作收 `by: Option<String>`(审计主体,app 传);读自动过滤软删;一律返 `ContentError`。
//! `NewContent`/`NewObject` 是仓储输入(无 id/时间戳;仓储 mint `Uuid::now_v7()` + DB now())。
//!
//! **SCAFFOLD-CORE 无跨 repo 事务**(transactions skill:原子单元=一个 trait 方法)。`upload_content`
//! 把建 content 行、建 object 行、推字节、翻状态作为**各自独立**的调用编排(见 service.rs),不跨两个
//! 仓储包一个 tx。中途失败的孤儿行风险是 Go 已记录的行为,v0.1 接受(见 service.rs 注释)。

mod memory;
mod postgres;

use std::collections::HashMap;

use async_trait::async_trait;
use sea_query::Iden;
use uuid::Uuid;

use crate::error::ContentError;
use crate::status::{ContentStatus, ObjectStatus};
use crate::types::{Content, ContentMetadata, Object, ObjectMetadata};

pub use memory::{InMemoryContentRepo, InMemoryObjectRepo};
pub use postgres::{PgContentRepo, PgObjectRepo};

/// 建内容的仓储输入(无 id/时间戳/状态:仓储 mint id、置 status=Created)。
#[derive(Debug, Clone)]
pub struct NewContent {
    pub tenant_id: Uuid,
    pub owner_id: Uuid,
    pub owner_type: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub document_type: Option<String>,
    pub derivation_type: Option<String>,
}

/// 建对象的仓储输入(无 id/时间戳:仓储 mint id、置 version=1、status=Created)。
#[derive(Debug, Clone)]
pub struct NewObject {
    pub content_id: Uuid,
    pub storage_backend_name: String,
    pub storage_class: Option<String>,
    pub object_key: String,
    pub file_name: Option<String>,
    pub object_type: Option<String>,
}

// ── sea-query 表/列标识 ──
// 表名经枚举级 `#[iden = "..."]` 钉死(枚举名另取以避开领域类型 Content/Object 的命名冲突);
// 列变量按 snake_case 渲染(`TenantId` -> "tenant_id")。`Table` 变量渲染为表名。
// content_derived 的 Iden 不在此 —— SCAFFOLD-CORE 无查询碰它(避免 dead_code),只有迁移与 stub 类型。
#[derive(Iden)]
#[iden = "content"]
pub(crate) enum ContentTable {
    Table,
    Id,
    TenantId,
    OwnerId,
    OwnerType,
    Name,
    Description,
    DocumentType,
    Status,
    DerivationType,
    CreatedBy,
    CreatedAt,
    UpdatedBy,
    UpdatedAt,
    DeletedAt,
}
#[derive(Iden)]
#[iden = "content_metadata"]
pub(crate) enum ContentMetaTable {
    Table,
    ContentId,
    Tags,
    FileSize,
    FileName,
    MimeType,
    Checksum,
    ChecksumAlgorithm,
    Metadata,
    CreatedAt,
    UpdatedAt,
}
#[derive(Iden)]
#[iden = "object"]
pub(crate) enum ObjectTable {
    Table,
    Id,
    ContentId,
    StorageBackendName,
    StorageClass,
    ObjectKey,
    FileName,
    Version,
    ObjectType,
    Status,
    CreatedBy,
    CreatedAt,
    UpdatedBy,
    UpdatedAt,
    DeletedAt,
}
#[derive(Iden)]
#[iden = "object_metadata"]
pub(crate) enum ObjectMetaTable {
    Table,
    ObjectId,
    SizeBytes,
    MimeType,
    Etag,
    Metadata,
    CreatedAt,
    UpdatedAt,
}

/// 内容仓储端口(content + content_metadata)。
#[async_trait]
pub trait ContentRepo: Send + Sync {
    /// 建内容行(status=Created)。
    async fn create(&self, c: NewContent, by: Option<String>) -> Result<Content, ContentError>;

    /// 按 id 查存活内容。不存在 / 已软删 → `NotFound`。
    async fn get(&self, id: Uuid) -> Result<Content, ContentError>;

    /// 按 id **批量**查存活内容(N+1 防护 / 跨模块富化的根原语)。查不到的 id 不在结果里。
    async fn get_many(&self, ids: &[Uuid]) -> Result<Vec<Content>, ContentError>;

    /// **全量更新**可编辑字段(PUT 语义)。已软删 → `NotFound`。
    async fn update(&self, c: &Content, by: Option<String>) -> Result<Content, ContentError>;

    /// 置内容状态。已软删/不存在 → `NotFound`。
    async fn set_status(
        &self,
        id: Uuid,
        status: ContentStatus,
        by: Option<String>,
    ) -> Result<(), ContentError>;

    /// 软删内容。已删/不存在 → `NotFound`。
    async fn soft_delete(&self, id: Uuid, by: Option<String>) -> Result<(), ContentError>;

    /// 列某 (owner_id, tenant_id) 的存活内容(按 id desc)。
    async fn list(&self, owner_id: Uuid, tenant_id: Uuid) -> Result<Vec<Content>, ContentError>;

    /// upsert content_metadata(PK=content_id,ON CONFLICT DO UPDATE)。传入时间戳由仓储覆盖。
    async fn set_metadata(&self, m: ContentMetadata) -> Result<(), ContentError>;

    /// 查 content_metadata。不存在 → `NotFound`。
    async fn get_metadata(&self, content_id: Uuid) -> Result<ContentMetadata, ContentError>;

    /// 批量查 content_metadata(富化用)。查不到的 id 不在 map 里。
    async fn get_metadata_many(
        &self,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, ContentMetadata>, ContentError>;
}

/// 对象仓储端口(object + object_metadata)。
#[async_trait]
pub trait ObjectRepo: Send + Sync {
    /// 建对象行(version=1,status=Created)。撞存活 (storage_backend_name, object_key) → `Conflict`。
    async fn create(&self, o: NewObject, by: Option<String>) -> Result<Object, ContentError>;

    /// 按 id 查存活对象。不存在 / 已软删 → `NotFound`。
    async fn get(&self, id: Uuid) -> Result<Object, ContentError>;

    /// 列某 content 的存活对象(按 id asc = 建序)。
    async fn list_by_content(&self, content_id: Uuid) -> Result<Vec<Object>, ContentError>;

    /// 按 (object_key, backend) 复合唯一查存活对象。不存在 → `NotFound`。
    async fn get_by_key(&self, object_key: &str, backend: &str) -> Result<Object, ContentError>;

    /// 置对象状态。已软删/不存在 → `NotFound`。
    async fn set_status(
        &self,
        id: Uuid,
        status: ObjectStatus,
        by: Option<String>,
    ) -> Result<(), ContentError>;

    /// 软删对象。已删/不存在 → `NotFound`。
    async fn soft_delete(&self, id: Uuid, by: Option<String>) -> Result<(), ContentError>;

    /// upsert object_metadata(PK=object_id)。传入时间戳由仓储覆盖。
    async fn set_metadata(&self, m: ObjectMetadata) -> Result<(), ContentError>;

    /// 查 object_metadata。不存在 → `NotFound`。
    async fn get_metadata(&self, object_id: Uuid) -> Result<ObjectMetadata, ContentError>;
}
