//! 内容仓储端口 `ContentRepo`(content + content_metadata)及其仓储输入 `NewContent`。

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::content_error::ContentError;
use crate::content_scope::ContentScope;
use crate::content_status::ContentStatus;
use crate::types::{Content, ContentMetadata};

/// 建内容的仓储输入(无 id/状态:仓储生成 id、置 status=Created;租户取自 `scope`;
/// 时间戳取 `created_at`,由 service 经注入的时钟决定)。
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct NewContent {
    /// 所有者。
    pub owner_id: Uuid,
    /// 所有者种类。
    pub owner_type: Option<String>,
    /// 展示名。
    pub name: Option<String>,
    /// 描述。
    pub description: Option<String>,
    /// 文档类型。
    pub document_type: Option<String>,
    /// `"original"` | `"derived"`;仓储原样存(默认值由 service 填)。
    pub derivation_type: Option<String>,
    /// 建行时刻:新行的 `created_at` = `updated_at` = 它(仓储按传入值落库,不自己取时间)。
    pub created_at: OffsetDateTime,
}

impl NewContent {
    /// 只填所有者和建行时刻,其余可选字段为 `None`。
    #[must_use]
    pub fn new(owner_id: Uuid, created_at: OffsetDateTime) -> Self {
        Self {
            owner_id,
            owner_type: None,
            name: None,
            description: None,
            document_type: None,
            derivation_type: None,
            created_at,
        }
    }
}

/// 内容仓储端口(content + content_metadata)。每个方法第一个参数是作用域,见 [`ContentScope`]。
///
/// **时间戳由调用方给**(service 经注入的 [`Clock`](crate::Clock) 取):建行用 [`NewContent::created_at`],
/// 改 / 置状态 / 软删用参数 `at`,元数据用传入结构的时间戳。实现按传入值落库,不自己取时间。
#[async_trait]
pub trait ContentRepo: Send + Sync {
    /// 建内容行(status=Created,tenant_id 取自 `scope`,`created_at` = `updated_at` = `content.created_at`)。
    /// `by` = 审计主体。
    async fn create(
        &self,
        scope: ContentScope,
        content: NewContent,
        by: Option<&str>,
    ) -> Result<Content, ContentError>;

    /// 按 id 查本作用域的存活内容。不存在 / 已软删 / 别的作用域 → `NotFound`。
    async fn get(&self, scope: ContentScope, id: Uuid) -> Result<Content, ContentError>;

    /// 按 id **批量**查本作用域的存活内容(N+1 防护 / 跨模块富化的根原语)。查不到的 id 不在结果里,
    /// 顺序不保证。
    ///
    /// `ids` 收切片而不是 `impl IntoIterator`:泛型方法会让本 trait 失去 dyn 兼容性(指南「Generics versus
    /// `dyn Trait`」列的理由),而下游要用 `Arc<dyn ContentRepo>` 在运行时选实现。
    /// 返回 `Vec`:实现天然从数据库取回成 `Vec`(指南「Iterators vs. owned collections」的例外)。
    async fn get_many(
        &self,
        scope: ContentScope,
        ids: &[Uuid],
    ) -> Result<Vec<Content>, ContentError>;

    /// **全量更新**可编辑字段(PUT 语义;只用 `content.id` 和 owner_type / name / description /
    /// document_type,其余字段不动;`updated_at` = `at`)。已软删 / 别的作用域 → `NotFound`。
    async fn update(
        &self,
        scope: ContentScope,
        content: &Content,
        at: OffsetDateTime,
        by: Option<&str>,
    ) -> Result<Content, ContentError>;

    /// 置内容状态(`updated_at` = `at`)。已软删 / 不存在 / 别的作用域 → `NotFound`。
    async fn set_status(
        &self,
        scope: ContentScope,
        id: Uuid,
        status: ContentStatus,
        at: OffsetDateTime,
        by: Option<&str>,
    ) -> Result<(), ContentError>;

    /// 软删内容(`deleted_at` = `updated_at` = `at`)。已删 / 不存在 / 别的作用域 → `NotFound`。
    async fn soft_delete(
        &self,
        scope: ContentScope,
        id: Uuid,
        at: OffsetDateTime,
        by: Option<&str>,
    ) -> Result<(), ContentError>;

    /// 列本作用域里某 owner 的存活内容(按 id desc)。返回 `Vec`:同 [`ContentRepo::get_many`]。
    async fn list(&self, scope: ContentScope, owner_id: Uuid)
        -> Result<Vec<Content>, ContentError>;

    /// upsert content_metadata(PK=content_id,ON CONFLICT DO UPDATE)。时间戳按传入值落库:
    /// `updated_at` 取 `metadata.updated_at`;新插入的行 `created_at` 取 `metadata.created_at`,
    /// 已存在的行保留原 `created_at`。内容在本作用域不存在 / 已软删 → `NotFound`。
    async fn set_metadata(
        &self,
        scope: ContentScope,
        metadata: ContentMetadata,
    ) -> Result<(), ContentError>;

    /// 查 content_metadata。不存在 / 别的作用域 → `NotFound`。
    async fn get_metadata(
        &self,
        scope: ContentScope,
        content_id: Uuid,
    ) -> Result<ContentMetadata, ContentError>;

    /// 批量查 content_metadata(富化用)。查不到的 id 不在 map 里。返回 map:调用方按 id 随机取。
    /// `ids` 收切片的理由同 [`ContentRepo::get_many`]。
    async fn get_metadata_many(
        &self,
        scope: ContentScope,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, ContentMetadata>, ContentError>;
}

/// 转发实现:让 `Arc<dyn ContentRepo>` 能填 [`ContentService`](crate::ContentService) 的仓储类型参数
/// (下游在运行时二选一实现时用)。
#[async_trait]
impl<T: ContentRepo + ?Sized> ContentRepo for Arc<T> {
    async fn create(
        &self,
        scope: ContentScope,
        content: NewContent,
        by: Option<&str>,
    ) -> Result<Content, ContentError> {
        (**self).create(scope, content, by).await
    }

    async fn get(&self, scope: ContentScope, id: Uuid) -> Result<Content, ContentError> {
        (**self).get(scope, id).await
    }

    async fn get_many(
        &self,
        scope: ContentScope,
        ids: &[Uuid],
    ) -> Result<Vec<Content>, ContentError> {
        (**self).get_many(scope, ids).await
    }

    async fn update(
        &self,
        scope: ContentScope,
        content: &Content,
        at: OffsetDateTime,
        by: Option<&str>,
    ) -> Result<Content, ContentError> {
        (**self).update(scope, content, at, by).await
    }

    async fn set_status(
        &self,
        scope: ContentScope,
        id: Uuid,
        status: ContentStatus,
        at: OffsetDateTime,
        by: Option<&str>,
    ) -> Result<(), ContentError> {
        (**self).set_status(scope, id, status, at, by).await
    }

    async fn soft_delete(
        &self,
        scope: ContentScope,
        id: Uuid,
        at: OffsetDateTime,
        by: Option<&str>,
    ) -> Result<(), ContentError> {
        (**self).soft_delete(scope, id, at, by).await
    }

    async fn list(
        &self,
        scope: ContentScope,
        owner_id: Uuid,
    ) -> Result<Vec<Content>, ContentError> {
        (**self).list(scope, owner_id).await
    }

    async fn set_metadata(
        &self,
        scope: ContentScope,
        metadata: ContentMetadata,
    ) -> Result<(), ContentError> {
        (**self).set_metadata(scope, metadata).await
    }

    async fn get_metadata(
        &self,
        scope: ContentScope,
        content_id: Uuid,
    ) -> Result<ContentMetadata, ContentError> {
        (**self).get_metadata(scope, content_id).await
    }

    async fn get_metadata_many(
        &self,
        scope: ContentScope,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, ContentMetadata>, ContentError> {
        (**self).get_metadata_many(scope, ids).await
    }
}

/// 转发实现:同 `Arc<T>`,给独占所有权的 `Box<dyn ContentRepo>` 用。
#[async_trait]
impl<T: ContentRepo + ?Sized> ContentRepo for Box<T> {
    async fn create(
        &self,
        scope: ContentScope,
        content: NewContent,
        by: Option<&str>,
    ) -> Result<Content, ContentError> {
        (**self).create(scope, content, by).await
    }

    async fn get(&self, scope: ContentScope, id: Uuid) -> Result<Content, ContentError> {
        (**self).get(scope, id).await
    }

    async fn get_many(
        &self,
        scope: ContentScope,
        ids: &[Uuid],
    ) -> Result<Vec<Content>, ContentError> {
        (**self).get_many(scope, ids).await
    }

    async fn update(
        &self,
        scope: ContentScope,
        content: &Content,
        at: OffsetDateTime,
        by: Option<&str>,
    ) -> Result<Content, ContentError> {
        (**self).update(scope, content, at, by).await
    }

    async fn set_status(
        &self,
        scope: ContentScope,
        id: Uuid,
        status: ContentStatus,
        at: OffsetDateTime,
        by: Option<&str>,
    ) -> Result<(), ContentError> {
        (**self).set_status(scope, id, status, at, by).await
    }

    async fn soft_delete(
        &self,
        scope: ContentScope,
        id: Uuid,
        at: OffsetDateTime,
        by: Option<&str>,
    ) -> Result<(), ContentError> {
        (**self).soft_delete(scope, id, at, by).await
    }

    async fn list(
        &self,
        scope: ContentScope,
        owner_id: Uuid,
    ) -> Result<Vec<Content>, ContentError> {
        (**self).list(scope, owner_id).await
    }

    async fn set_metadata(
        &self,
        scope: ContentScope,
        metadata: ContentMetadata,
    ) -> Result<(), ContentError> {
        (**self).set_metadata(scope, metadata).await
    }

    async fn get_metadata(
        &self,
        scope: ContentScope,
        content_id: Uuid,
    ) -> Result<ContentMetadata, ContentError> {
        (**self).get_metadata(scope, content_id).await
    }

    async fn get_metadata_many(
        &self,
        scope: ContentScope,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, ContentMetadata>, ContentError> {
        (**self).get_metadata_many(scope, ids).await
    }
}
