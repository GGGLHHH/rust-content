//! sea-query 表 / 列标识(只给 PG 实现用)。
//! 表名经枚举级 `#[iden = "..."]` 钉死(枚举名另取以避开领域类型 Content/Object 的命名冲突);
//! 列变量按 snake_case 渲染(`TenantId` -> "tenant_id")。`Table` 变量渲染为表名。
//! content_derived 的 Iden 不在此 —— 仓储没有查询碰它(避免 dead_code),目前只有迁移建的表。

use sea_query::Iden;

/// 表 `content`。
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

/// 表 `content_metadata`。
#[derive(Iden)]
#[iden = "content_metadata"]
pub(crate) enum ContentMetadataTable {
    Table,
    ContentId,
    TenantId,
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

/// 表 `object`。
#[derive(Iden)]
#[iden = "object"]
pub(crate) enum ObjectTable {
    Table,
    Id,
    TenantId,
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

/// 表 `object_metadata`。
#[derive(Iden)]
#[iden = "object_metadata"]
pub(crate) enum ObjectMetadataTable {
    Table,
    ObjectId,
    TenantId,
    SizeBytes,
    MimeType,
    Etag,
    Metadata,
    CreatedAt,
    UpdatedAt,
}
