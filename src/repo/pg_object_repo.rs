//! 对象仓储的 Postgres 实现 —— sea-query 构建 + sqlx 执行(app 注入的 pool,search_path=content)。
//! 每个方法一个事务、先设作用域上下文再查,见 [`pg_support`](super::pg_support)。
//! 镜像 idm 的 sea-query 习语:RETURNING 取建后实体、`ON CONFLICT DO UPDATE` upsert 元数据、
//! `IN (...)` 批量、`AND deleted_at IS NULL` 软删过滤。
//! 时间戳一律显式写调用方传入的值(同 [`PgContentRepo`](super::PgContentRepo)),不靠 DB 默认值 / 触发器。
//! 撞存活唯一索引 (storage_backend_name, object_key) → `Conflict`。

use async_trait::async_trait;
use sea_query::{Expr, ExprTrait, OnConflict, Order, PostgresQueryBuilder, Query};
use sea_query_sqlx::SqlxBinder;
use sqlx::postgres::{PgQueryResult, PgRow};
use sqlx::{AssertSqlSafe, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use super::object_repo::{NewObject, ObjectLocation, ObjectRepo};
use super::pg_support;
use super::tables::{ContentTable, ObjectMetadataTable, ObjectTable};
use crate::content_error::ContentError;
use crate::content_scope::ContentScope;
use crate::object_status::ObjectStatus;
use crate::types::{Object, ObjectMetadata};

/// object 表查询行(status 取 String,转实体时解析成枚举;tenant_id / deleted_at / 审计列不查)。
#[derive(sqlx::FromRow)]
struct ObjectRecord {
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

impl ObjectRecord {
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
            status: self.status.parse()?,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// object_metadata 的 SELECT 列(tenant_id 不上公开类型)。
fn object_metadata_columns() -> [ObjectMetadataTable; 7] {
    [
        ObjectMetadataTable::ObjectId,
        ObjectMetadataTable::SizeBytes,
        ObjectMetadataTable::MimeType,
        ObjectMetadataTable::Etag,
        ObjectMetadataTable::Metadata,
        ObjectMetadataTable::CreatedAt,
        ObjectMetadataTable::UpdatedAt,
    ]
}

/// object 表的 SELECT/RETURNING 公共列。
fn object_columns() -> [ObjectTable; 11] {
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

/// [`ObjectRepo`] 的 Postgres 实现。`Clone` 只复制连接池句柄。连接账号的要求同 [`PgContentRepo`](super::PgContentRepo)。
#[derive(Clone, Debug)]
pub struct PgObjectRepo {
    pool: PgPool,
}

impl PgObjectRepo {
    /// 用应用注入的连接池建仓储。
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ObjectRepo for PgObjectRepo {
    #[tracing::instrument(name = "pg.object.create", skip_all)]
    async fn create(
        &self,
        scope: ContentScope,
        object: NewObject,
        by: Option<&str>,
    ) -> Result<Object, ContentError> {
        let mut tx = pg_support::scoped_tx(&self.pool, scope).await?;
        // 先验父 content 在本作用域存活(同 set_metadata):object.content_id 的 FK 检查不受 RLS 约束,
        // 不验就能把对象挂到别的租户 / 个人作用域的 content 下;不存在的父行也该是 NotFound 而不是 FK 报 500。
        //
        // **`FOR SHARE` 锁住父行到本事务结束**,堵「检查通过 → 并发软删提交 → 插入」的竞态。
        // 软删是改非键列的 UPDATE,拿的是 `FOR NO KEY UPDATE` 行锁;插 object 时 FK 检查只拿 `FOR KEY SHARE`,
        // 和它不冲突,挡不住。`FOR SHARE` 是和 `FOR NO KEY UPDATE` 冲突的最弱一级:
        // 软删在先 → 这里等它提交,READ COMMITTED 下重查最新版本发现 deleted_at 非空 → 不返回行 → NotFound;
        // 这里在先 → 软删等本事务提交后再删(等价于「先建对象、后删内容」的串行顺序)。
        // 不用 `FOR NO KEY UPDATE`:它和自己冲突,同一 content 下并发建对象会被无谓串行化;`FOR SHARE` 之间互相兼容。
        let (sql, values) = Query::select()
            .expr(Expr::val(1))
            .from(ContentTable::Table)
            .and_where(Expr::col(ContentTable::Id).eq(object.content_id))
            .and_where(pg_support::scope_condition(ContentTable::TenantId, scope))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .lock_shared()
            .build_sqlx(PostgresQueryBuilder);
        let parent: Option<PgRow> = sqlx::query_with(AssertSqlSafe(sql), values)
            .fetch_optional(&mut *tx)
            .await
            .map_err(pg_support::internal)?;
        parent.ok_or(ContentError::NotFound)?;
        let id = Uuid::now_v7();
        let (sql, values) = Query::insert()
            .into_table(ObjectTable::Table)
            .columns([
                ObjectTable::Id,
                ObjectTable::TenantId,
                ObjectTable::ContentId,
                ObjectTable::StorageBackendName,
                ObjectTable::StorageClass,
                ObjectTable::ObjectKey,
                ObjectTable::FileName,
                ObjectTable::Version,
                ObjectTable::ObjectType,
                ObjectTable::Status,
                ObjectTable::CreatedBy,
                ObjectTable::CreatedAt,
                ObjectTable::UpdatedBy,
                ObjectTable::UpdatedAt,
            ])
            .values_panic([
                id.into(),
                scope.tenant_id().into(),
                object.content_id.into(),
                object.storage_backend_name.into(),
                object.storage_class.into(),
                object.object_key.into(),
                object.file_name.into(),
                1i32.into(),
                object.object_type.into(),
                ObjectStatus::Created.as_str().to_owned().into(),
                by.map(str::to_owned).into(),
                object.created_at.into(),
                by.map(str::to_owned).into(),
                object.created_at.into(),
            ])
            .returning(Query::returning().columns(object_columns()))
            .build_sqlx(PostgresQueryBuilder);
        let row: ObjectRecord = sqlx::query_as_with(AssertSqlSafe(sql), values)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| {
                pg_support::map_unique(error, "object_key already exists for storage backend")
            })?;
        tx.commit().await.map_err(pg_support::internal)?;
        row.into_entity()
    }

    #[tracing::instrument(name = "pg.object.get", skip_all)]
    async fn get(&self, scope: ContentScope, id: Uuid) -> Result<Object, ContentError> {
        let mut tx = pg_support::scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::select()
            .columns(object_columns())
            .from(ObjectTable::Table)
            .and_where(Expr::col(ObjectTable::Id).eq(id))
            .and_where(pg_support::scope_condition(ObjectTable::TenantId, scope))
            .and_where(Expr::col(ObjectTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let row: Option<ObjectRecord> = sqlx::query_as_with(AssertSqlSafe(sql), values)
            .fetch_optional(&mut *tx)
            .await
            .map_err(pg_support::internal)?;
        tx.commit().await.map_err(pg_support::internal)?;
        row.ok_or(ContentError::NotFound)?.into_entity()
    }

    #[tracing::instrument(name = "pg.object.list_by_content", skip_all)]
    async fn list_by_content(
        &self,
        scope: ContentScope,
        content_id: Uuid,
    ) -> Result<Vec<Object>, ContentError> {
        let mut tx = pg_support::scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::select()
            .columns(object_columns())
            .from(ObjectTable::Table)
            .and_where(pg_support::scope_condition(ObjectTable::TenantId, scope))
            .and_where(Expr::col(ObjectTable::ContentId).eq(content_id))
            .and_where(Expr::col(ObjectTable::DeletedAt).is_null())
            .order_by(ObjectTable::Id, Order::Asc)
            .build_sqlx(PostgresQueryBuilder);
        let rows: Vec<ObjectRecord> = sqlx::query_as_with(AssertSqlSafe(sql), values)
            .fetch_all(&mut *tx)
            .await
            .map_err(pg_support::internal)?;
        tx.commit().await.map_err(pg_support::internal)?;
        rows.into_iter().map(ObjectRecord::into_entity).collect()
    }

    #[tracing::instrument(name = "pg.object.get_by_location", skip_all)]
    async fn get_by_location(
        &self,
        scope: ContentScope,
        location: &ObjectLocation,
    ) -> Result<Object, ContentError> {
        let mut tx = pg_support::scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::select()
            .columns(object_columns())
            .from(ObjectTable::Table)
            .and_where(Expr::col(ObjectTable::ObjectKey).eq(location.object_key.as_str()))
            .and_where(
                Expr::col(ObjectTable::StorageBackendName)
                    .eq(location.storage_backend_name.as_str()),
            )
            .and_where(pg_support::scope_condition(ObjectTable::TenantId, scope))
            .and_where(Expr::col(ObjectTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let row: Option<ObjectRecord> = sqlx::query_as_with(AssertSqlSafe(sql), values)
            .fetch_optional(&mut *tx)
            .await
            .map_err(pg_support::internal)?;
        tx.commit().await.map_err(pg_support::internal)?;
        row.ok_or(ContentError::NotFound)?.into_entity()
    }

    #[tracing::instrument(name = "pg.object.set_status", skip_all)]
    async fn set_status(
        &self,
        scope: ContentScope,
        id: Uuid,
        status: ObjectStatus,
        at: OffsetDateTime,
        by: Option<&str>,
    ) -> Result<(), ContentError> {
        let mut tx = pg_support::scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::update()
            .table(ObjectTable::Table)
            .value(ObjectTable::Status, status.as_str().to_owned())
            .value(ObjectTable::UpdatedBy, by.map(str::to_owned))
            .value(ObjectTable::UpdatedAt, at)
            .and_where(Expr::col(ObjectTable::Id).eq(id))
            .and_where(pg_support::scope_condition(ObjectTable::TenantId, scope))
            .and_where(Expr::col(ObjectTable::DeletedAt).is_null())
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

    #[tracing::instrument(name = "pg.object.soft_delete", skip_all)]
    async fn soft_delete(
        &self,
        scope: ContentScope,
        id: Uuid,
        at: OffsetDateTime,
        by: Option<&str>,
    ) -> Result<(), ContentError> {
        let mut tx = pg_support::scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::update()
            .table(ObjectTable::Table)
            .value(ObjectTable::DeletedAt, at)
            .value(ObjectTable::UpdatedBy, by.map(str::to_owned))
            .value(ObjectTable::UpdatedAt, at)
            .and_where(Expr::col(ObjectTable::Id).eq(id))
            .and_where(pg_support::scope_condition(ObjectTable::TenantId, scope))
            .and_where(Expr::col(ObjectTable::DeletedAt).is_null())
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

    #[tracing::instrument(name = "pg.object.set_metadata", skip_all)]
    async fn set_metadata(
        &self,
        scope: ContentScope,
        metadata: ObjectMetadata,
    ) -> Result<(), ContentError> {
        let mut tx = pg_support::scoped_tx(&self.pool, scope).await?;
        // 先验父对象在本作用域存活:别的作用域 / 不存在 → NotFound。
        let (sql, values) = Query::select()
            .expr(Expr::val(1))
            .from(ObjectTable::Table)
            .and_where(Expr::col(ObjectTable::Id).eq(metadata.object_id))
            .and_where(pg_support::scope_condition(ObjectTable::TenantId, scope))
            .and_where(Expr::col(ObjectTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let parent: Option<PgRow> = sqlx::query_with(AssertSqlSafe(sql), values)
            .fetch_optional(&mut *tx)
            .await
            .map_err(pg_support::internal)?;
        parent.ok_or(ContentError::NotFound)?;
        let (sql, values) = Query::insert()
            .into_table(ObjectMetadataTable::Table)
            .columns([
                ObjectMetadataTable::ObjectId,
                ObjectMetadataTable::TenantId,
                ObjectMetadataTable::SizeBytes,
                ObjectMetadataTable::MimeType,
                ObjectMetadataTable::Etag,
                ObjectMetadataTable::Metadata,
                ObjectMetadataTable::CreatedAt,
                ObjectMetadataTable::UpdatedAt,
            ])
            .values_panic([
                metadata.object_id.into(),
                scope.tenant_id().into(),
                metadata.size_bytes.into(),
                metadata.mime_type.into(),
                metadata.etag.into(),
                metadata.metadata.into(),
                metadata.created_at.into(),
                metadata.updated_at.into(),
            ])
            // 冲突时只更新 updated_at,created_at 保留已有行的(同 content_metadata)。
            .on_conflict(
                OnConflict::column(ObjectMetadataTable::ObjectId)
                    .update_columns([
                        ObjectMetadataTable::SizeBytes,
                        ObjectMetadataTable::MimeType,
                        ObjectMetadataTable::Etag,
                        ObjectMetadataTable::Metadata,
                        ObjectMetadataTable::UpdatedAt,
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

    #[tracing::instrument(name = "pg.object.get_metadata", skip_all)]
    async fn get_metadata(
        &self,
        scope: ContentScope,
        object_id: Uuid,
    ) -> Result<ObjectMetadata, ContentError> {
        let mut tx = pg_support::scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::select()
            .columns(object_metadata_columns())
            .from(ObjectMetadataTable::Table)
            .and_where(Expr::col(ObjectMetadataTable::ObjectId).eq(object_id))
            .and_where(pg_support::scope_condition(
                ObjectMetadataTable::TenantId,
                scope,
            ))
            .build_sqlx(PostgresQueryBuilder);
        let row: Option<ObjectMetadata> = sqlx::query_as_with(AssertSqlSafe(sql), values)
            .fetch_optional(&mut *tx)
            .await
            .map_err(pg_support::internal)?;
        tx.commit().await.map_err(pg_support::internal)?;
        row.ok_or(ContentError::NotFound)
    }
}
