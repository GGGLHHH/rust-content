//! content 仓储 Postgres 实现 —— sea-query 构建 + sqlx 执行(app 注入的 pool,search_path=content)。
//! 镜像 idm 的 sea-query 习语:RETURNING 取建后实体、`ON CONFLICT DO UPDATE` 两张元数据 upsert、
//! `IN (...)` 批量、`AND deleted_at IS NULL` 软删过滤、存活唯一索引违例 → `Conflict`、server-side now()/触发器。
//!
//! **作用域(多租户)**:每个方法都是一个事务 —— [`scoped_tx`] 开事务、**第一条语句**用事务级
//! `set_config(..., true)` 设上下文(租户:`app.tenant_id`;个人:`app.personal_scope = 'on'`),
//! 然后业务 SQL **同时**带作用域条件(`tenant_id = $1` / `tenant_id IS NULL`,走索引、也是内存实现的同一语义),
//! 最后 commit。表上的 RLS(迁移 0003,ENABLE + FORCE)是第二道防线:忘了带条件也串不了租户。
//! 不在借出连接时设会话级变量(以后加 PgBouncer 也不会串租户)。

use std::collections::HashMap;

use async_trait::async_trait;
use sea_query::{Expr, ExprTrait, IntoColumnRef, OnConflict, Order, PostgresQueryBuilder, Query};
use sea_query_sqlx::SqlxBinder;
use sqlx::{AssertSqlSafe, PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    ContentMetaTable, ContentRepo, ContentTable, NewContent, NewObject, ObjectMetaTable,
    ObjectRepo, ObjectTable,
};
use crate::error::ContentError;
use crate::status::{ContentStatus, ObjectStatus};
use crate::types::{Content, ContentMetadata, ContentScope, Object, ObjectMetadata};

/// 库错误 → `Internal`(原始进日志)。
fn internal(e: sqlx::Error) -> ContentError {
    ContentError::Internal(e.into())
}

/// 唯一冲突(撞存活唯一索引)→ `Conflict`;其它库错误 → `Internal`(原始进日志)。
fn map_unique(e: sqlx::Error, msg: &str) -> ContentError {
    if let sqlx::Error::Database(db) = &e {
        if db.is_unique_violation() {
            return ContentError::Conflict(msg.to_owned());
        }
    }
    internal(e)
}

/// 开一个带作用域的事务:校验作用域(nil 租户 → `InvalidInput`)→ `begin` →
/// 第一条语句 `select set_config(..., true)`(事务级,commit / rollback 后自动失效)。
async fn scoped_tx(
    pool: &PgPool,
    scope: ContentScope,
) -> Result<Transaction<'static, Postgres>, ContentError> {
    scope.validate()?;
    let mut tx = pool.begin().await.map_err(internal)?;
    let q = match scope {
        ContentScope::Tenant(id) => {
            sqlx::query("select set_config('app.tenant_id', $1, true)").bind(id.to_string())
        }
        ContentScope::Personal => {
            sqlx::query("select set_config('app.personal_scope', 'on', true)")
        }
    };
    q.execute(&mut *tx).await.map_err(internal)?;
    Ok(tx)
}

/// SQL 里的作用域条件(和 RLS 同义,显式写出来走 `(tenant_id, …)` 索引)。
fn scope_cond<C: IntoColumnRef>(col: C, scope: ContentScope) -> Expr {
    match scope.tenant_id() {
        Some(t) => Expr::col(col).eq(t),
        None => Expr::col(col).is_null(),
    }
}

// ── 内容 ──

/// content 表查询行(status 取 String,转实体时解析成枚举;无 deleted_at/审计于公开实体)。
#[derive(sqlx::FromRow)]
struct ContentDbRow {
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

/// content_metadata 的 SELECT 列(tenant_id 不上公开类型)。
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
    #[tracing::instrument(name = "pg.content.create", skip_all)]
    async fn create(
        &self,
        scope: ContentScope,
        c: NewContent,
        by: Option<String>,
    ) -> Result<Content, ContentError> {
        let mut tx = scoped_tx(&self.pool, scope).await?;
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
                scope.tenant_id().into(),
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
            .fetch_one(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        row.into_entity()
    }

    #[tracing::instrument(name = "pg.content.get", skip_all)]
    async fn get(&self, scope: ContentScope, id: Uuid) -> Result<Content, ContentError> {
        let mut tx = scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::select()
            .columns(content_cols())
            .from(ContentTable::Table)
            .and_where(Expr::col(ContentTable::Id).eq(id))
            .and_where(scope_cond(ContentTable::TenantId, scope))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let row = sqlx::query_as_with::<Postgres, ContentDbRow, _>(AssertSqlSafe(sql), values)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
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
        let mut tx = scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::select()
            .columns(content_cols())
            .from(ContentTable::Table)
            .and_where(Expr::col(ContentTable::Id).is_in(ids.iter().copied()))
            .and_where(scope_cond(ContentTable::TenantId, scope))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let rows = sqlx::query_as_with::<Postgres, ContentDbRow, _>(AssertSqlSafe(sql), values)
            .fetch_all(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        rows.into_iter().map(ContentDbRow::into_entity).collect()
    }

    #[tracing::instrument(name = "pg.content.update", skip_all)]
    async fn update(
        &self,
        scope: ContentScope,
        c: &Content,
        by: Option<String>,
    ) -> Result<Content, ContentError> {
        let mut tx = scoped_tx(&self.pool, scope).await?;
        // PUT 全量替换可编辑字段;tenant/owner/status/derivation 不动。
        let (sql, values) = Query::update()
            .table(ContentTable::Table)
            .value(ContentTable::OwnerType, c.owner_type.clone())
            .value(ContentTable::Name, c.name.clone())
            .value(ContentTable::Description, c.description.clone())
            .value(ContentTable::DocumentType, c.document_type.clone())
            .value(ContentTable::UpdatedBy, by)
            .and_where(Expr::col(ContentTable::Id).eq(c.id))
            .and_where(scope_cond(ContentTable::TenantId, scope))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .returning(Query::returning().columns(content_cols()))
            .build_sqlx(PostgresQueryBuilder);
        let row = sqlx::query_as_with::<Postgres, ContentDbRow, _>(AssertSqlSafe(sql), values)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        row.ok_or(ContentError::NotFound)?.into_entity()
    }

    #[tracing::instrument(name = "pg.content.set_status", skip_all)]
    async fn set_status(
        &self,
        scope: ContentScope,
        id: Uuid,
        status: ContentStatus,
        by: Option<String>,
    ) -> Result<(), ContentError> {
        let mut tx = scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::update()
            .table(ContentTable::Table)
            .value(ContentTable::Status, status.as_str().to_owned())
            .value(ContentTable::UpdatedBy, by)
            .and_where(Expr::col(ContentTable::Id).eq(id))
            .and_where(scope_cond(ContentTable::TenantId, scope))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let res = sqlx::query_with::<Postgres, _>(AssertSqlSafe(sql), values)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        if res.rows_affected() == 0 {
            return Err(ContentError::NotFound);
        }
        Ok(())
    }

    #[tracing::instrument(name = "pg.content.soft_delete", skip_all)]
    async fn soft_delete(
        &self,
        scope: ContentScope,
        id: Uuid,
        by: Option<String>,
    ) -> Result<(), ContentError> {
        let mut tx = scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::update()
            .table(ContentTable::Table)
            .value(ContentTable::DeletedAt, OffsetDateTime::now_utc())
            .value(ContentTable::UpdatedBy, by)
            .and_where(Expr::col(ContentTable::Id).eq(id))
            .and_where(scope_cond(ContentTable::TenantId, scope))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let res = sqlx::query_with::<Postgres, _>(AssertSqlSafe(sql), values)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        if res.rows_affected() == 0 {
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
        let mut tx = scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::select()
            .columns(content_cols())
            .from(ContentTable::Table)
            .and_where(scope_cond(ContentTable::TenantId, scope))
            .and_where(Expr::col(ContentTable::OwnerId).eq(owner_id))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .order_by(ContentTable::Id, Order::Desc)
            .build_sqlx(PostgresQueryBuilder);
        let rows = sqlx::query_as_with::<Postgres, ContentDbRow, _>(AssertSqlSafe(sql), values)
            .fetch_all(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        rows.into_iter().map(ContentDbRow::into_entity).collect()
    }

    #[tracing::instrument(name = "pg.content.set_metadata", skip_all)]
    async fn set_metadata(
        &self,
        scope: ContentScope,
        m: ContentMetadata,
    ) -> Result<(), ContentError> {
        let mut tx = scoped_tx(&self.pool, scope).await?;
        // 先验父行在本作用域存活:别的作用域 / 不存在 → NotFound(而不是撞 FK 或 RLS 报 500)。
        let (sql, values) = Query::select()
            .expr(Expr::val(1))
            .from(ContentTable::Table)
            .and_where(Expr::col(ContentTable::Id).eq(m.content_id))
            .and_where(scope_cond(ContentTable::TenantId, scope))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        sqlx::query_with::<Postgres, _>(AssertSqlSafe(sql), values)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?
            .ok_or(ContentError::NotFound)?;
        // upsert:PK=content_id,ON CONFLICT DO UPDATE。created_at/updated_at 由 DB 默认 + 触发器维护。
        let (sql, values) = Query::insert()
            .into_table(ContentMetaTable::Table)
            .columns([
                ContentMetaTable::ContentId,
                ContentMetaTable::TenantId,
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
                scope.tenant_id().into(),
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
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(())
    }

    #[tracing::instrument(name = "pg.content.get_metadata", skip_all)]
    async fn get_metadata(
        &self,
        scope: ContentScope,
        content_id: Uuid,
    ) -> Result<ContentMetadata, ContentError> {
        let mut tx = scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::select()
            .columns(content_meta_cols())
            .from(ContentMetaTable::Table)
            .and_where(Expr::col(ContentMetaTable::ContentId).eq(content_id))
            .and_where(scope_cond(ContentMetaTable::TenantId, scope))
            .build_sqlx(PostgresQueryBuilder);
        let row = sqlx::query_as_with::<Postgres, ContentMetadata, _>(AssertSqlSafe(sql), values)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
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
        let mut tx = scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::select()
            .columns(content_meta_cols())
            .from(ContentMetaTable::Table)
            .and_where(Expr::col(ContentMetaTable::ContentId).is_in(ids.iter().copied()))
            .and_where(scope_cond(ContentMetaTable::TenantId, scope))
            .build_sqlx(PostgresQueryBuilder);
        let rows = sqlx::query_as_with::<Postgres, ContentMetadata, _>(AssertSqlSafe(sql), values)
            .fetch_all(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
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
    #[tracing::instrument(name = "pg.object.create", skip_all)]
    async fn create(
        &self,
        scope: ContentScope,
        o: NewObject,
        by: Option<String>,
    ) -> Result<Object, ContentError> {
        let mut tx = scoped_tx(&self.pool, scope).await?;
        // 先验父 content 在本作用域存活(同 set_metadata):object.content_id 的 FK 检查不受 RLS 约束,
        // 不验就能把对象挂到别的租户 / 个人作用域的 content 下;不存在的父行也该是 NotFound 而不是 FK 报 500。
        let (sql, values) = Query::select()
            .expr(Expr::val(1))
            .from(ContentTable::Table)
            .and_where(Expr::col(ContentTable::Id).eq(o.content_id))
            .and_where(scope_cond(ContentTable::TenantId, scope))
            .and_where(Expr::col(ContentTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        sqlx::query_with::<Postgres, _>(AssertSqlSafe(sql), values)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?
            .ok_or(ContentError::NotFound)?;
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
                ObjectTable::UpdatedBy,
            ])
            .values_panic([
                id.into(),
                scope.tenant_id().into(),
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
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| map_unique(e, "object_key already exists for storage backend"))?;
        tx.commit().await.map_err(internal)?;
        row.into_entity()
    }

    #[tracing::instrument(name = "pg.object.get", skip_all)]
    async fn get(&self, scope: ContentScope, id: Uuid) -> Result<Object, ContentError> {
        let mut tx = scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::select()
            .columns(object_cols())
            .from(ObjectTable::Table)
            .and_where(Expr::col(ObjectTable::Id).eq(id))
            .and_where(scope_cond(ObjectTable::TenantId, scope))
            .and_where(Expr::col(ObjectTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let row = sqlx::query_as_with::<Postgres, ObjectDbRow, _>(AssertSqlSafe(sql), values)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        row.ok_or(ContentError::NotFound)?.into_entity()
    }

    #[tracing::instrument(name = "pg.object.list_by_content", skip_all)]
    async fn list_by_content(
        &self,
        scope: ContentScope,
        content_id: Uuid,
    ) -> Result<Vec<Object>, ContentError> {
        let mut tx = scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::select()
            .columns(object_cols())
            .from(ObjectTable::Table)
            .and_where(scope_cond(ObjectTable::TenantId, scope))
            .and_where(Expr::col(ObjectTable::ContentId).eq(content_id))
            .and_where(Expr::col(ObjectTable::DeletedAt).is_null())
            .order_by(ObjectTable::Id, Order::Asc)
            .build_sqlx(PostgresQueryBuilder);
        let rows = sqlx::query_as_with::<Postgres, ObjectDbRow, _>(AssertSqlSafe(sql), values)
            .fetch_all(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        rows.into_iter().map(ObjectDbRow::into_entity).collect()
    }

    #[tracing::instrument(name = "pg.object.get_by_key", skip_all)]
    async fn get_by_key(
        &self,
        scope: ContentScope,
        object_key: &str,
        backend: &str,
    ) -> Result<Object, ContentError> {
        let mut tx = scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::select()
            .columns(object_cols())
            .from(ObjectTable::Table)
            .and_where(Expr::col(ObjectTable::ObjectKey).eq(object_key))
            .and_where(Expr::col(ObjectTable::StorageBackendName).eq(backend))
            .and_where(scope_cond(ObjectTable::TenantId, scope))
            .and_where(Expr::col(ObjectTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let row = sqlx::query_as_with::<Postgres, ObjectDbRow, _>(AssertSqlSafe(sql), values)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        row.ok_or(ContentError::NotFound)?.into_entity()
    }

    #[tracing::instrument(name = "pg.object.set_status", skip_all)]
    async fn set_status(
        &self,
        scope: ContentScope,
        id: Uuid,
        status: ObjectStatus,
        by: Option<String>,
    ) -> Result<(), ContentError> {
        let mut tx = scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::update()
            .table(ObjectTable::Table)
            .value(ObjectTable::Status, status.as_str().to_owned())
            .value(ObjectTable::UpdatedBy, by)
            .and_where(Expr::col(ObjectTable::Id).eq(id))
            .and_where(scope_cond(ObjectTable::TenantId, scope))
            .and_where(Expr::col(ObjectTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let res = sqlx::query_with::<Postgres, _>(AssertSqlSafe(sql), values)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        if res.rows_affected() == 0 {
            return Err(ContentError::NotFound);
        }
        Ok(())
    }

    #[tracing::instrument(name = "pg.object.soft_delete", skip_all)]
    async fn soft_delete(
        &self,
        scope: ContentScope,
        id: Uuid,
        by: Option<String>,
    ) -> Result<(), ContentError> {
        let mut tx = scoped_tx(&self.pool, scope).await?;
        let (sql, values) = Query::update()
            .table(ObjectTable::Table)
            .value(ObjectTable::DeletedAt, OffsetDateTime::now_utc())
            .value(ObjectTable::UpdatedBy, by)
            .and_where(Expr::col(ObjectTable::Id).eq(id))
            .and_where(scope_cond(ObjectTable::TenantId, scope))
            .and_where(Expr::col(ObjectTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        let res = sqlx::query_with::<Postgres, _>(AssertSqlSafe(sql), values)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        if res.rows_affected() == 0 {
            return Err(ContentError::NotFound);
        }
        Ok(())
    }

    #[tracing::instrument(name = "pg.object.set_metadata", skip_all)]
    async fn set_metadata(
        &self,
        scope: ContentScope,
        m: ObjectMetadata,
    ) -> Result<(), ContentError> {
        let mut tx = scoped_tx(&self.pool, scope).await?;
        // 先验父对象在本作用域存活:别的作用域 / 不存在 → NotFound。
        let (sql, values) = Query::select()
            .expr(Expr::val(1))
            .from(ObjectTable::Table)
            .and_where(Expr::col(ObjectTable::Id).eq(m.object_id))
            .and_where(scope_cond(ObjectTable::TenantId, scope))
            .and_where(Expr::col(ObjectTable::DeletedAt).is_null())
            .build_sqlx(PostgresQueryBuilder);
        sqlx::query_with::<Postgres, _>(AssertSqlSafe(sql), values)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?
            .ok_or(ContentError::NotFound)?;
        let (sql, values) = Query::insert()
            .into_table(ObjectMetaTable::Table)
            .columns([
                ObjectMetaTable::ObjectId,
                ObjectMetaTable::TenantId,
                ObjectMetaTable::SizeBytes,
                ObjectMetaTable::MimeType,
                ObjectMetaTable::Etag,
                ObjectMetaTable::Metadata,
            ])
            .values_panic([
                m.object_id.into(),
                scope.tenant_id().into(),
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
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(())
    }

    #[tracing::instrument(name = "pg.object.get_metadata", skip_all)]
    async fn get_metadata(
        &self,
        scope: ContentScope,
        object_id: Uuid,
    ) -> Result<ObjectMetadata, ContentError> {
        let mut tx = scoped_tx(&self.pool, scope).await?;
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
            .and_where(scope_cond(ObjectMetaTable::TenantId, scope))
            .build_sqlx(PostgresQueryBuilder);
        let row = sqlx::query_as_with::<Postgres, ObjectMetadata, _>(AssertSqlSafe(sql), values)
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        row.ok_or(ContentError::NotFound)
    }
}
