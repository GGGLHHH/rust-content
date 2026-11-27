//! 内容仓储的 Postgres 实现 —— sea-query 构建 + sqlx 执行(app 注入的 pool,search_path=content)。
//! 每个方法一个事务、先设作用域上下文再查,见 [`pg_support`](super::pg_support)。
//! 镜像 idm 的 sea-query 习语:RETURNING 取建后实体、`ON CONFLICT DO UPDATE` upsert 元数据、
//! `IN (...)` 批量、`AND deleted_at IS NULL` 软删过滤。
//! 时间戳一律显式写调用方传入的值(INSERT 写 created_at / updated_at,UPDATE 写 updated_at),不靠 DB 默认值;
//! `set_updated_at_utc` 触发器在仓储事务里不覆盖显式值(迁移 0004 + [`pg_support::scoped_tx`](super::pg_support::scoped_tx))。

use std::collections::HashMap;

use async_trait::async_trait;
use sea_query::{Expr, ExprTrait, OnConflict, Order, PostgresQueryBuilder, Query};
use sea_query_sqlx::SqlxBinder;
use sqlx::postgres::{PgQueryResult, PgRow};
use sqlx::{AssertSqlSafe, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use super::content_repo::{ContentRepo, NewContent};
use super::pg_support;
use super::tables::{ContentMetadataTable, ContentTable};
use crate::content_error::ContentError;
use crate::content_scope::ContentScope;
use crate::content_status::ContentStatus;
use crate::types::{Content, ContentMetadata};

/// content 表查询行(status 取 String,转实体时解析成枚举;deleted_at / 审计列不查)。
#[derive(sqlx::FromRow)]
struct ContentRecord {
    id: Uuid,
    tenant_id: Option<Uuid>,
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

impl ContentRecord {
    fn into_entity(self) -> Result<Content, ContentError> {
        Ok(Content {
            id: self.id,
            tenant_id: self.tenant_id,
            owner_id: self.owner_id,
            owner_type: self.owner_type,
            name: self.name,
            description: self.description,
            document_type: self.document_type,
            status: self.status.parse()?,
            derivation_type: self.derivation_type,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// content 表的 SELECT/RETURNING 公共列。
fn content_columns() -> [ContentTable; 11] {
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

/// content_metadata 的 SELECT 列(tenant_id 不上公开类型)。
fn content_metadata_columns() -> [ContentMetadataTable; 10] {
    [
        ContentMetadataTable::ContentId,
        ContentMetadataTable::Tags,
        ContentMetadataTable::FileSize,
        ContentMetadataTable::FileName,
        ContentMetadataTable::MimeType,
        ContentMetadataTable::Checksum,
        ContentMetadataTable::ChecksumAlgorithm,
        ContentMetadataTable::Metadata,
        ContentMetadataTable::CreatedAt,
        ContentMetadataTable::UpdatedAt,
    ]
}

/// [`ContentRepo`] 的 Postgres 实现。`Clone` 只复制连接池句柄。
///
/// 连接账号要用**非 owner、NOBYPASSRLS** 的运行时账号(owner 只跑迁移),RLS 才真的生效;
/// search_path 指向 content 所在 schema。
#[derive(Clone, Debug)]
pub struct PgContentRepo {
    pool: PgPool,
}

impl PgContentRepo {
    /// 用应用注入的连接池建仓储。
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ContentRepo for PgContentRepo {
    #[tracing::instrument(name = "pg.content.create", skip_all)]
    async fn create(
        &self,
        scope: ContentScope,
        content: NewContent,
        by: Option<&str>,
    ) -> Result<Content, ContentError> {
        let mut tx = pg_support::scoped_tx(&self.pool, scope).await?;
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
                ContentTable::CreatedAt,
                ContentTable::UpdatedBy,
                ContentTable::UpdatedAt,
            ])
            .values_panic([
                id.into(),
                scope.tenant_id().into(),
                content.owner_id.into(),
                content.owner_type.into(),
                content.name.into(),
                content.description.into(),
                content.document_type.into(),
                ContentStatus::Created.as_str().to_owned().into(),
                content.derivation_type.into(),
                by.map(str::to_owned).into(),
                content.created_at.into(),
                by.map(str::to_owned).into(),
                content.created_at.into(),
            ])
            .returning(Query::returning().columns(content_columns()))
            .build_sqlx(PostgresQueryBuilder);
        let row: ContentRecord = sqlx::query_as_with(AssertSqlSafe(sql), values)
            .fetch_one(&mut *tx)
            .await
            .map_err(pg_support::internal)?;
        tx.commit().await.map_err(pg_support::internal)?;
        row.into_entity()
    }

    #[tracing::instrument(name = "pg.content.get", skip_all)]
    async fn get(&self, scope: ContentScope, id: Uuid) -> Result<Content, ContentError> {
        let mut tx = pg_support::scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::select()
            .columns(content_columns())
            .from(ContentTable::Table)
            .and_where(Expr::col(ContentTable::Id).eq(id))
            .and_where(pg_support::scope_condition(ContentTable::TenantId, scope))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let row: Option<ContentRecord> = sqlx::query_as_with(AssertSqlSafe(sql), values)
            .fetch_optional(&mut *tx)
            .await
            .map_err(pg_support::internal)?;
        tx.commit().await.map_err(pg_support::internal)?;
        row.ok_or(ContentError::NotFound)?.into_entity()
    }

    #[tracing::instrument(name = "pg.content.get_many", skip_all)]
    async fn get_many(
        &self,
        scope: ContentScope,
        ids: &[Uuid],
    ) -> Result<Vec<Content>, ContentError> {
        scope.validate()?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut tx = pg_support::scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::select()
            .columns(content_columns())
            .from(ContentTable::Table)
            .and_where(Expr::col(ContentTable::Id).is_in(ids.iter().copied()))
            .and_where(pg_support::scope_condition(ContentTable::TenantId, scope))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let rows: Vec<ContentRecord> = sqlx::query_as_with(AssertSqlSafe(sql), values)
            .fetch_all(&mut *tx)
            .await
            .map_err(pg_support::internal)?;
        tx.commit().await.map_err(pg_support::internal)?;
        rows.into_iter().map(ContentRecord::into_entity).collect()
    }

    #[tracing::instrument(name = "pg.content.update", skip_all)]
    async fn update(
        &self,
        scope: ContentScope,
        content: &Content,
        at: OffsetDateTime,
        by: Option<&str>,
    ) -> Result<Content, ContentError> {
        let mut tx = pg_support::scoped_tx(&self.pool, scope).await?;
        // PUT 全量替换可编辑字段;tenant/owner/status/derivation 不动。
        let (sql, values) = Query::update()
            .table(ContentTable::Table)
            .value(ContentTable::OwnerType, content.owner_type.clone())
            .value(ContentTable::Name, content.name.clone())
            .value(ContentTable::Description, content.description.clone())
            .value(ContentTable::DocumentType, content.document_type.clone())
            .value(ContentTable::UpdatedBy, by.map(str::to_owned))
            .value(ContentTable::UpdatedAt, at)
            .and_where(Expr::col(ContentTable::Id).eq(content.id))
            .and_where(pg_support::scope_condition(ContentTable::TenantId, scope))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .returning(Query::returning().columns(content_columns()))
            .build_sqlx(PostgresQueryBuilder);
        let row: Option<ContentRecord> = sqlx::query_as_with(AssertSqlSafe(sql), values)
            .fetch_optional(&mut *tx)
            .await
            .map_err(pg_support::internal)?;
        tx.commit().await.map_err(pg_support::internal)?;
        row.ok_or(ContentError::NotFound)?.into_entity()
    }

    #[tracing::instrument(name = "pg.content.set_status", skip_all)]
    async fn set_status(
        &self,
        scope: ContentScope,
        id: Uuid,
        status: ContentStatus,
        at: OffsetDateTime,
        by: Option<&str>,
    ) -> Result<(), ContentError> {
        let mut tx = pg_support::scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::update()
            .table(ContentTable::Table)
            .value(ContentTable::Status, status.as_str().to_owned())
            .value(ContentTable::UpdatedBy, by.map(str::to_owned))
            .value(ContentTable::UpdatedAt, at)
            .and_where(Expr::col(ContentTable::Id).eq(id))
            .and_where(pg_support::scope_condition(ContentTable::TenantId, scope))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let result: PgQueryResult = sqlx::query_with(AssertSqlSafe(sql), values)
            .execute(&mut *tx)
            .await
            .map_err(pg_support::internal)?;
        tx.commit().await.map_err(pg_support::internal)?;
        if result.rows_affected() == 0 {
            return Err(ContentError::NotFound);
        }
        Ok(())
    }

    #[tracing::instrument(name = "pg.content.soft_delete", skip_all)]
    async fn soft_delete(
        &self,
        scope: ContentScope,
        id: Uuid,
        at: OffsetDateTime,
        by: Option<&str>,
    ) -> Result<(), ContentError> {
        let mut tx = pg_support::scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::update()
            .table(ContentTable::Table)
            .value(ContentTable::DeletedAt, at)
            .value(ContentTable::UpdatedBy, by.map(str::to_owned))
            .value(ContentTable::UpdatedAt, at)
            .and_where(Expr::col(ContentTable::Id).eq(id))
            .and_where(pg_support::scope_condition(ContentTable::TenantId, scope))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let result: PgQueryResult = sqlx::query_with(AssertSqlSafe(sql), values)
            .execute(&mut *tx)
            .await
            .map_err(pg_support::internal)?;
        tx.commit().await.map_err(pg_support::internal)?;
        if result.rows_affected() == 0 {
            return Err(ContentError::NotFound);
        }
        Ok(())
    }

    #[tracing::instrument(name = "pg.content.list", skip_all)]
    async fn list(
        &self,
        scope: ContentScope,
        owner_id: Uuid,
    ) -> Result<Vec<Content>, ContentError> {
        let mut tx = pg_support::scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::select()
            .columns(content_columns())
            .from(ContentTable::Table)
            .and_where(pg_support::scope_condition(ContentTable::TenantId, scope))
            .and_where(Expr::col(ContentTable::OwnerId).eq(owner_id))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .order_by(ContentTable::Id, Order::Desc)
            .build_sqlx(PostgresQueryBuilder);
        let rows: Vec<ContentRecord> = sqlx::query_as_with(AssertSqlSafe(sql), values)
            .fetch_all(&mut *tx)
            .await
            .map_err(pg_support::internal)?;
        tx.commit().await.map_err(pg_support::internal)?;
        rows.into_iter().map(ContentRecord::into_entity).collect()
    }

    #[tracing::instrument(name = "pg.content.set_metadata", skip_all)]
    async fn set_metadata(
        &self,
        scope: ContentScope,
        metadata: ContentMetadata,
    ) -> Result<(), ContentError> {
        let mut tx = pg_support::scoped_tx(&self.pool, scope).await?;
        // 先验父行在本作用域存活:别的作用域 / 不存在 → NotFound(而不是撞 FK 或 RLS 报 500)。
        let (sql, values) = Query::select()
            .expr(Expr::val(1))
            .from(ContentTable::Table)
            .and_where(Expr::col(ContentTable::Id).eq(metadata.content_id))
            .and_where(pg_support::scope_condition(ContentTable::TenantId, scope))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let parent: Option<PgRow> = sqlx::query_with(AssertSqlSafe(sql), values)
            .fetch_optional(&mut *tx)
            .await
            .map_err(pg_support::internal)?;
        parent.ok_or(ContentError::NotFound)?;
        // upsert:PK=content_id,ON CONFLICT DO UPDATE。时间戳写传入值:冲突时只更新 updated_at,
        // created_at 保留已有行的(和内存实现同一语义)。
        let (sql, values) = Query::insert()
            .into_table(ContentMetadataTable::Table)
            .columns([
                ContentMetadataTable::ContentId,
                ContentMetadataTable::TenantId,
                ContentMetadataTable::Tags,
                ContentMetadataTable::FileSize,
                ContentMetadataTable::FileName,
                ContentMetadataTable::MimeType,
                ContentMetadataTable::Checksum,
                ContentMetadataTable::ChecksumAlgorithm,
                ContentMetadataTable::Metadata,
                ContentMetadataTable::CreatedAt,
                ContentMetadataTable::UpdatedAt,
            ])
            .values_panic([
                metadata.content_id.into(),
                scope.tenant_id().into(),
                metadata.tags.into(),
                metadata.file_size.into(),
                metadata.file_name.into(),
                metadata.mime_type.into(),
                metadata.checksum.into(),
                metadata.checksum_algorithm.into(),
                metadata.metadata.into(),
                metadata.created_at.into(),
                metadata.updated_at.into(),
            ])
            .on_conflict(
                OnConflict::column(ContentMetadataTable::ContentId)
                    .update_columns([
                        ContentMetadataTable::Tags,
                        ContentMetadataTable::FileSize,
                        ContentMetadataTable::FileName,
                        ContentMetadataTable::MimeType,
                        ContentMetadataTable::Checksum,
                        ContentMetadataTable::ChecksumAlgorithm,
                        ContentMetadataTable::Metadata,
                        ContentMetadataTable::UpdatedAt,
                    ])
                    .to_owned(),
            )
            .build_sqlx(PostgresQueryBuilder);
        sqlx::query_with(AssertSqlSafe(sql), values)
            .execute(&mut *tx)
            .await
            .map_err(pg_support::internal)?;
        tx.commit().await.map_err(pg_support::internal)?;
        Ok(())
    }

    #[tracing::instrument(name = "pg.content.get_metadata", skip_all)]
    async fn get_metadata(
        &self,
        scope: ContentScope,
        content_id: Uuid,
    ) -> Result<ContentMetadata, ContentError> {
        let mut tx = pg_support::scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::select()
            .columns(content_metadata_columns())
            .from(ContentMetadataTable::Table)
            .and_where(Expr::col(ContentMetadataTable::ContentId).eq(content_id))
            .and_where(pg_support::scope_condition(
                ContentMetadataTable::TenantId,
                scope,
            ))
            .build_sqlx(PostgresQueryBuilder);
        let row: Option<ContentMetadata> = sqlx::query_as_with(AssertSqlSafe(sql), values)
            .fetch_optional(&mut *tx)
            .await
            .map_err(pg_support::internal)?;
        tx.commit().await.map_err(pg_support::internal)?;
        row.ok_or(ContentError::NotFound)
    }

    #[tracing::instrument(name = "pg.content.get_metadata_many", skip_all)]
    async fn get_metadata_many(
        &self,
        scope: ContentScope,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, ContentMetadata>, ContentError> {
        scope.validate()?;
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut tx = pg_support::scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::select()
            .columns(content_metadata_columns())
            .from(ContentMetadataTable::Table)
            .and_where(Expr::col(ContentMetadataTable::ContentId).is_in(ids.iter().copied()))
            .and_where(pg_support::scope_condition(
                ContentMetadataTable::TenantId,
                scope,
            ))
            .build_sqlx(PostgresQueryBuilder);
        let rows: Vec<ContentMetadata> = sqlx::query_as_with(AssertSqlSafe(sql), values)
            .fetch_all(&mut *tx)
            .await
            .map_err(pg_support::internal)?;
        tx.commit().await.map_err(pg_support::internal)?;
        Ok(rows
            .into_iter()
            .map(|metadata| (metadata.content_id, metadata))
            .collect())
    }
}
