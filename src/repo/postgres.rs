//! content 仓储 Postgres 实现 —— sea-query 构建 + sqlx 执行(app 注入的 pool,search_path=content)。
//! 镜像 idm 的 sea-query 习语:RETURNING 取建后实体、`ON CONFLICT DO UPDATE` 两张元数据 upsert、
//! `IN (...)` 批量、`AND deleted_at IS NULL` 软删过滤、存活唯一索引违例 → `Conflict`、server-side now()/触发器。

use std::collections::HashMap;

use async_trait::async_trait;
use sea_query::{Expr, ExprTrait, OnConflict, Order, PostgresQueryBuilder, Query};
use sea_query_sqlx::SqlxBinder;
use sqlx::{AssertSqlSafe, PgPool, Postgres};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    ContentMetaTable, ContentRepo, ContentTable, NewContent, NewObject, ObjectMetaTable,
    ObjectRepo, ObjectTable,
};
use crate::error::ContentError;
use crate::status::{ContentStatus, ObjectStatus};
use crate::types::{Content, ContentMetadata, Object, ObjectMetadata};

/// 唯一冲突(撞存活唯一索引)→ `Conflict`;其它库错误 → `Internal`(原始进日志)。
fn map_unique(e: sqlx::Error, msg: &str) -> ContentError {
    if let sqlx::Error::Database(db) = &e {
        if db.is_unique_violation() {
            return ContentError::Conflict(msg.to_owned());
        }
    }
    ContentError::Internal(e.into())
}

// ── 内容 ──

/// content 表查询行(status 取 String,转实体时解析成枚举;无 deleted_at/审计于公开实体)。
#[derive(sqlx::FromRow)]
struct ContentDbRow {
    id: Uuid,
    tenant_id: Uuid,
    owner_id: Uuid,
    owner_type: Option<String>,
    name: Option<String>,
    description: Option<String>,
    document_type: Option<String>,
    status: String,
    derivation_type: Option<String>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl ContentDbRow {
    fn into_entity(self) -> Result<Content, ContentError> {
        Ok(Content {
            id: self.id,
            tenant_id: self.tenant_id,
            owner_id: self.owner_id,
            owner_type: self.owner_type,
            name: self.name,
            description: self.description,
            document_type: self.document_type,
            status: ContentStatus::from_db(&self.status)?,
            derivation_type: self.derivation_type,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// content 表的 SELECT/RETURNING 公共列。
fn content_cols() -> [ContentTable; 11] {
    [
        ContentTable::Id,
        ContentTable::TenantId,
        ContentTable::OwnerId,
        ContentTable::OwnerType,
        ContentTable::Name,
        ContentTable::Description,
        ContentTable::DocumentType,
        ContentTable::Status,
        ContentTable::DerivationType,
        ContentTable::CreatedAt,
        ContentTable::UpdatedAt,
    ]
}

/// content_metadata 的 SELECT 列。
fn content_meta_cols() -> [ContentMetaTable; 10] {
    [
        ContentMetaTable::ContentId,
        ContentMetaTable::Tags,
        ContentMetaTable::FileSize,
        ContentMetaTable::FileName,
        ContentMetaTable::MimeType,
        ContentMetaTable::Checksum,
        ContentMetaTable::ChecksumAlgorithm,
        ContentMetaTable::Metadata,
        ContentMetaTable::CreatedAt,
        ContentMetaTable::UpdatedAt,
    ]
}

pub struct PgContentRepo {
    pool: PgPool,
}
impl PgContentRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ContentRepo for PgContentRepo {
    async fn create(&self, c: NewContent, by: Option<String>) -> Result<Content, ContentError> {
        let id = Uuid::now_v7();
        let (sql, values) = Query::insert()
            .into_table(ContentTable::Table)
            .columns([
                ContentTable::Id,
                ContentTable::TenantId,
                ContentTable::OwnerId,
                ContentTable::OwnerType,
                ContentTable::Name,
                ContentTable::Description,
                ContentTable::DocumentType,
                ContentTable::Status,
                ContentTable::DerivationType,
                ContentTable::CreatedBy,
                ContentTable::UpdatedBy,
            ])
            .values_panic([
                id.into(),
                c.tenant_id.into(),
                c.owner_id.into(),
                c.owner_type.into(),
                c.name.into(),
                c.description.into(),
                c.document_type.into(),
                ContentStatus::Created.as_str().to_owned().into(),
                c.derivation_type.into(),
                by.clone().into(),
                by.into(),
            ])
            .returning(Query::returning().columns(content_cols()))
            .build_sqlx(PostgresQueryBuilder);
        let row = sqlx::query_as_with::<Postgres, ContentDbRow, _>(AssertSqlSafe(sql), values)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| ContentError::Internal(e.into()))?;
        row.into_entity()
    }

    async fn get(&self, id: Uuid) -> Result<Content, ContentError> {
        let (sql, values) = Query::select()
            .columns(content_cols())
            .from(ContentTable::Table)
            .and_where(Expr::col(ContentTable::Id).eq(id))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        sqlx::query_as_with::<Postgres, ContentDbRow, _>(AssertSqlSafe(sql), values)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ContentError::Internal(e.into()))?
            .ok_or(ContentError::NotFound)?
            .into_entity()
    }

    async fn get_many(&self, ids: &[Uuid]) -> Result<Vec<Content>, ContentError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let (sql, values) = Query::select()
            .columns(content_cols())
            .from(ContentTable::Table)
            .and_where(Expr::col(ContentTable::Id).is_in(ids.iter().copied()))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let rows = sqlx::query_as_with::<Postgres, ContentDbRow, _>(AssertSqlSafe(sql), values)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ContentError::Internal(e.into()))?;
        rows.into_iter().map(ContentDbRow::into_entity).collect()
    }

    async fn update(&self, c: &Content, by: Option<String>) -> Result<Content, ContentError> {
        // PUT 全量替换可编辑字段;tenant/owner/status/derivation 不动。
        let (sql, values) = Query::update()
            .table(ContentTable::Table)
            .value(ContentTable::OwnerType, c.owner_type.clone())
            .value(ContentTable::Name, c.name.clone())
            .value(ContentTable::Description, c.description.clone())
            .value(ContentTable::DocumentType, c.document_type.clone())
            .value(ContentTable::UpdatedBy, by)
            .and_where(Expr::col(ContentTable::Id).eq(c.id))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .returning(Query::returning().columns(content_cols()))
            .build_sqlx(PostgresQueryBuilder);
        sqlx::query_as_with::<Postgres, ContentDbRow, _>(AssertSqlSafe(sql), values)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ContentError::Internal(e.into()))?
            .ok_or(ContentError::NotFound)?
            .into_entity()
    }

    async fn set_status(
        &self,
        id: Uuid,
        status: ContentStatus,
        by: Option<String>,
    ) -> Result<(), ContentError> {
        let (sql, values) = Query::update()
            .table(ContentTable::Table)
            .value(ContentTable::Status, status.as_str().to_owned())
            .value(ContentTable::UpdatedBy, by)
            .and_where(Expr::col(ContentTable::Id).eq(id))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let res = sqlx::query_with::<Postgres, _>(AssertSqlSafe(sql), values)
            .execute(&self.pool)
            .await
            .map_err(|e| ContentError::Internal(e.into()))?;
        if res.rows_affected() == 0 {
            return Err(ContentError::NotFound);
        }
        Ok(())
    }

    async fn soft_delete(&self, id: Uuid, by: Option<String>) -> Result<(), ContentError> {
        let (sql, values) = Query::update()
            .table(ContentTable::Table)
            .value(ContentTable::DeletedAt, OffsetDateTime::now_utc())
            .value(ContentTable::UpdatedBy, by)
            .and_where(Expr::col(ContentTable::Id).eq(id))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let res = sqlx::query_with::<Postgres, _>(AssertSqlSafe(sql), values)
            .execute(&self.pool)
            .await
            .map_err(|e| ContentError::Internal(e.into()))?;
        if res.rows_affected() == 0 {
            return Err(ContentError::NotFound);
        }
        Ok(())
    }

    async fn list(&self, owner_id: Uuid, tenant_id: Uuid) -> Result<Vec<Content>, ContentError> {
        let (sql, values) = Query::select()
            .columns(content_cols())
            .from(ContentTable::Table)
            .and_where(Expr::col(ContentTable::OwnerId).eq(owner_id))
            .and_where(Expr::col(ContentTable::TenantId).eq(tenant_id))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .order_by(ContentTable::Id, Order::Desc)
            .build_sqlx(PostgresQueryBuilder);
        let rows = sqlx::query_as_with::<Postgres, ContentDbRow, _>(AssertSqlSafe(sql), values)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ContentError::Internal(e.into()))?;
        rows.into_iter().map(ContentDbRow::into_entity).collect()
    }

    async fn set_metadata(&self, m: ContentMetadata) -> Result<(), ContentError> {
        // upsert:PK=content_id,ON CONFLICT DO UPDATE。created_at/updated_at 由 DB 默认 + 触发器维护。
        let (sql, values) = Query::insert()
            .into_table(ContentMetaTable::Table)
            .columns([
                ContentMetaTable::ContentId,
                ContentMetaTable::Tags,
                ContentMetaTable::FileSize,
                ContentMetaTable::FileName,
                ContentMetaTable::MimeType,
                ContentMetaTable::Checksum,
                ContentMetaTable::ChecksumAlgorithm,
                ContentMetaTable::Metadata,
            ])
            .values_panic([
                m.content_id.into(),
                m.tags.into(),
                m.file_size.into(),
                m.file_name.into(),
                m.mime_type.into(),
                m.checksum.into(),
                m.checksum_algorithm.into(),
                m.metadata.into(),
            ])
            .on_conflict(
                OnConflict::column(ContentMetaTable::ContentId)
                    .update_columns([
                        ContentMetaTable::Tags,
                        ContentMetaTable::FileSize,
                        ContentMetaTable::FileName,
                        ContentMetaTable::MimeType,
                        ContentMetaTable::Checksum,
                        ContentMetaTable::ChecksumAlgorithm,
                        ContentMetaTable::Metadata,
                    ])
                    .to_owned(),
            )
            .build_sqlx(PostgresQueryBuilder);
        sqlx::query_with::<Postgres, _>(AssertSqlSafe(sql), values)
            .execute(&self.pool)
            .await
            .map_err(|e| ContentError::Internal(e.into()))?;
        Ok(())
    }

    async fn get_metadata(&self, content_id: Uuid) -> Result<ContentMetadata, ContentError> {
        let (sql, values) = Query::select()
            .columns(content_meta_cols())
            .from(ContentMetaTable::Table)
            .and_where(Expr::col(ContentMetaTable::ContentId).eq(content_id))
            .build_sqlx(PostgresQueryBuilder);
        sqlx::query_as_with::<Postgres, ContentMetadata, _>(AssertSqlSafe(sql), values)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ContentError::Internal(e.into()))?
            .ok_or(ContentError::NotFound)
    }

    async fn get_metadata_many(
        &self,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, ContentMetadata>, ContentError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let (sql, values) = Query::select()
            .columns(content_meta_cols())
            .from(ContentMetaTable::Table)
            .and_where(Expr::col(ContentMetaTable::ContentId).is_in(ids.iter().copied()))
            .build_sqlx(PostgresQueryBuilder);
        let rows = sqlx::query_as_with::<Postgres, ContentMetadata, _>(AssertSqlSafe(sql), values)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ContentError::Internal(e.into()))?;
        Ok(rows.into_iter().map(|m| (m.content_id, m)).collect())
    }
}

// ── 对象 ──

#[derive(sqlx::FromRow)]
struct ObjectDbRow {
    id: Uuid,
    content_id: Uuid,
    storage_backend_name: String,
    storage_class: Option<String>,
    object_key: String,
    file_name: Option<String>,
    version: i32,
    object_type: Option<String>,
    status: String,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl ObjectDbRow {
    fn into_entity(self) -> Result<Object, ContentError> {
        Ok(Object {
            id: self.id,
            content_id: self.content_id,
            storage_backend_name: self.storage_backend_name,
            storage_class: self.storage_class,
            object_key: self.object_key,
            file_name: self.file_name,
            version: self.version,
            object_type: self.object_type,
            status: ObjectStatus::from_db(&self.status)?,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// object 表的 SELECT/RETURNING 公共列。
fn object_cols() -> [ObjectTable; 11] {
    [
        ObjectTable::Id,
        ObjectTable::ContentId,
        ObjectTable::StorageBackendName,
        ObjectTable::StorageClass,
        ObjectTable::ObjectKey,
        ObjectTable::FileName,
        ObjectTable::Version,
        ObjectTable::ObjectType,
        ObjectTable::Status,
        ObjectTable::CreatedAt,
        ObjectTable::UpdatedAt,
    ]
}

pub struct PgObjectRepo {
    pool: PgPool,
}
impl PgObjectRepo {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ObjectRepo for PgObjectRepo {
    async fn create(&self, o: NewObject, by: Option<String>) -> Result<Object, ContentError> {
        let id = Uuid::now_v7();
        let (sql, values) = Query::insert()
            .into_table(ObjectTable::Table)
            .columns([
                ObjectTable::Id,
                ObjectTable::ContentId,
                ObjectTable::StorageBackendName,
                ObjectTable::StorageClass,
                ObjectTable::ObjectKey,
                ObjectTable::FileName,
                ObjectTable::Version,
                ObjectTable::ObjectType,
                ObjectTable::Status,
                ObjectTable::CreatedBy,
                ObjectTable::UpdatedBy,
            ])
            .values_panic([
                id.into(),
                o.content_id.into(),
                o.storage_backend_name.into(),
                o.storage_class.into(),
                o.object_key.into(),
                o.file_name.into(),
                1i32.into(),
                o.object_type.into(),
                ObjectStatus::Created.as_str().to_owned().into(),
                by.clone().into(),
                by.into(),
            ])
            .returning(Query::returning().columns(object_cols()))
            .build_sqlx(PostgresQueryBuilder);
        let row = sqlx::query_as_with::<Postgres, ObjectDbRow, _>(AssertSqlSafe(sql), values)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| map_unique(e, "object_key already exists for storage backend"))?;
        row.into_entity()
    }

    async fn get(&self, id: Uuid) -> Result<Object, ContentError> {
        let (sql, values) = Query::select()
            .columns(object_cols())
            .from(ObjectTable::Table)
            .and_where(Expr::col(ObjectTable::Id).eq(id))
            .and_where(Expr::col(ObjectTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        sqlx::query_as_with::<Postgres, ObjectDbRow, _>(AssertSqlSafe(sql), values)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ContentError::Internal(e.into()))?
            .ok_or(ContentError::NotFound)?
            .into_entity()
    }

    async fn list_by_content(&self, content_id: Uuid) -> Result<Vec<Object>, ContentError> {
        let (sql, values) = Query::select()
            .columns(object_cols())
            .from(ObjectTable::Table)
            .and_where(Expr::col(ObjectTable::ContentId).eq(content_id))
            .and_where(Expr::col(ObjectTable::DeletedAt).is_null())
            .order_by(ObjectTable::Id, Order::Asc)
            .build_sqlx(PostgresQueryBuilder);
        let rows = sqlx::query_as_with::<Postgres, ObjectDbRow, _>(AssertSqlSafe(sql), values)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ContentError::Internal(e.into()))?;
        rows.into_iter().map(ObjectDbRow::into_entity).collect()
    }

    async fn get_by_key(&self, object_key: &str, backend: &str) -> Result<Object, ContentError> {
        let (sql, values) = Query::select()
            .columns(object_cols())
            .from(ObjectTable::Table)
            .and_where(Expr::col(ObjectTable::ObjectKey).eq(object_key))
            .and_where(Expr::col(ObjectTable::StorageBackendName).eq(backend))
            .and_where(Expr::col(ObjectTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        sqlx::query_as_with::<Postgres, ObjectDbRow, _>(AssertSqlSafe(sql), values)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ContentError::Internal(e.into()))?
            .ok_or(ContentError::NotFound)?
            .into_entity()
    }

    async fn set_status(
        &self,
        id: Uuid,
        status: ObjectStatus,
        by: Option<String>,
    ) -> Result<(), ContentError> {
        let (sql, values) = Query::update()
            .table(ObjectTable::Table)
            .value(ObjectTable::Status, status.as_str().to_owned())
            .value(ObjectTable::UpdatedBy, by)
            .and_where(Expr::col(ObjectTable::Id).eq(id))
            .and_where(Expr::col(ObjectTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let res = sqlx::query_with::<Postgres, _>(AssertSqlSafe(sql), values)
            .execute(&self.pool)
            .await
            .map_err(|e| ContentError::Internal(e.into()))?;
        if res.rows_affected() == 0 {
            return Err(ContentError::NotFound);
        }
        Ok(())
    }

    async fn soft_delete(&self, id: Uuid, by: Option<String>) -> Result<(), ContentError> {
        let (sql, values) = Query::update()
            .table(ObjectTable::Table)
            .value(ObjectTable::DeletedAt, OffsetDateTime::now_utc())
            .value(ObjectTable::UpdatedBy, by)
            .and_where(Expr::col(ObjectTable::Id).eq(id))
            .and_where(Expr::col(ObjectTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let res = sqlx::query_with::<Postgres, _>(AssertSqlSafe(sql), values)
            .execute(&self.pool)
            .await
            .map_err(|e| ContentError::Internal(e.into()))?;
        if res.rows_affected() == 0 {
            return Err(ContentError::NotFound);
        }
        Ok(())
    }

    async fn set_metadata(&self, m: ObjectMetadata) -> Result<(), ContentError> {
        let (sql, values) = Query::insert()
            .into_table(ObjectMetaTable::Table)
            .columns([
                ObjectMetaTable::ObjectId,
                ObjectMetaTable::SizeBytes,
                ObjectMetaTable::MimeType,
                ObjectMetaTable::Etag,
                ObjectMetaTable::Metadata,
            ])
            .values_panic([
                m.object_id.into(),
                m.size_bytes.into(),
                m.mime_type.into(),
                m.etag.into(),
                m.metadata.into(),
            ])
            .on_conflict(
                OnConflict::column(ObjectMetaTable::ObjectId)
                    .update_columns([
                        ObjectMetaTable::SizeBytes,
                        ObjectMetaTable::MimeType,
                        ObjectMetaTable::Etag,
                        ObjectMetaTable::Metadata,
                    ])
                    .to_owned(),
            )
            .build_sqlx(PostgresQueryBuilder);
        sqlx::query_with::<Postgres, _>(AssertSqlSafe(sql), values)
            .execute(&self.pool)
            .await
            .map_err(|e| ContentError::Internal(e.into()))?;
        Ok(())
    }

    async fn get_metadata(&self, object_id: Uuid) -> Result<ObjectMetadata, ContentError> {
        let (sql, values) = Query::select()
            .columns([
                ObjectMetaTable::ObjectId,
                ObjectMetaTable::SizeBytes,
                ObjectMetaTable::MimeType,
                ObjectMetaTable::Etag,
                ObjectMetaTable::Metadata,
                ObjectMetaTable::CreatedAt,
                ObjectMetaTable::UpdatedAt,
            ])
            .from(ObjectMetaTable::Table)
            .and_where(Expr::col(ObjectMetaTable::ObjectId).eq(object_id))
            .build_sqlx(PostgresQueryBuilder);
        sqlx::query_as_with::<Postgres, ObjectMetadata, _>(AssertSqlSafe(sql), values)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ContentError::Internal(e.into()))?
            .ok_or(ContentError::NotFound)
    }
}
