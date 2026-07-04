//! content 领域实体 —— 映射 SCAFFOLD-CORE 四表。
//! 审计字段(created_by/updated_by)与 `deleted_at` 是仓储内部细节,**不上**公开实体
//! (镜像 idm 的 `User` vs 内部 `UserRow`)。`status` 在域里 typed,仓储在 VARCHAR 边界互转。
//!
//! `ContentMetadata` / `ObjectMetadata` 直接 derive `sqlx::FromRow`(无 deleted_at/审计,字段与列一一对应);
//! `Content` / `Object` 因 `status` 是枚举,改由仓储内部 row 结构(status: String)转换,不在此 derive。

use time::OffsetDateTime;
use uuid::Uuid;

use crate::status::{ContentStatus, ObjectStatus};

/// 内容主体(表 `content`)。
#[derive(Clone, Debug)]
pub struct Content {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub owner_id: Uuid,
    pub owner_type: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub document_type: Option<String>,
    /// 生命周期状态(域里 typed;仓储映射到/自 VARCHAR)。
    pub status: ContentStatus,
    /// "original" | "derived"(保留;无 derived 逻辑也照存,廉价)。
    pub derivation_type: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// 内容自由表单元数据(表 `content_metadata`,1:1 挂 content,PK=content_id,upsert)。
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ContentMetadata {
    pub content_id: Uuid,
    pub tags: Vec<String>,
    pub file_size: Option<i64>,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    pub checksum: Option<String>,
    pub checksum_algorithm: Option<String>,
    /// 自由表单 JSONB(serde_json 仅此处用,如 idm 仅 JWT 用 serde)。
    pub metadata: serde_json::Value,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// 存储后端里的一份字节(表 `object`)。
#[derive(Clone, Debug)]
pub struct Object {
    pub id: Uuid,
    pub content_id: Uuid,
    pub storage_backend_name: String,
    pub storage_class: Option<String>,
    pub object_key: String,
    pub file_name: Option<String>,
    /// SCAFFOLD-CORE 钉死 1(多版本 DEFER)。
    pub version: i32,
    pub object_type: Option<String>,
    pub status: ObjectStatus,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// 对象自由表单元数据(表 `object_metadata`,1:1 挂 object,PK=object_id,upsert)。
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ObjectMetadata {
    pub object_id: Uuid,
    pub size_bytes: Option<i64>,
    pub mime_type: Option<String>,
    pub etag: Option<String>,
    pub metadata: serde_json::Value,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// 派生内容(表 `content_derived`)—— **DEFER stub**。
/// 仅让 0002 迁移建的表有个对应公开类型;SCAFFOLD-CORE 无任何 service 方法触碰它。
/// 字段保持不透明占位;真正的派生编排(创建/上传/列举派生关系)留待后续版本。
#[derive(Clone, Debug)]
pub struct DerivedContent {
    pub parent_id: Uuid,
    pub content_id: Uuid,
    pub variant: String,
    pub derivation_type: String,
    pub derivation_params: serde_json::Value,
    pub processing_metadata: serde_json::Value,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}
