//! 对象仓储端口 `ObjectRepo`(object + object_metadata)及其仓储输入 `NewObject` / `ObjectLocation`。

use std::sync::Arc;

use async_trait::async_trait;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::content_error::ContentError;
use crate::content_scope::ContentScope;
use crate::object_status::ObjectStatus;
use crate::types::{Object, ObjectMetadata};

/// 对象在存储里的位置:后端名 + 后端内 key。二者合起来在存活对象里全局唯一(跨作用域)。
/// 用具名字段代替两个并排的 `&str` 参数,免得把后端名和 key 传反。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[allow(
    clippy::exhaustive_structs,
    reason = "条款例外(Apply #[non_exhaustive] to public structs with public fields):\
              描述既有本体 —— 位置就是「后端 + key」两段,不会再加字段;保留字面量构造"
)]
pub struct ObjectLocation {
    /// 存储后端名(对象行的 `storage_backend_name`)。
    pub storage_backend_name: String,
    /// 后端内的 key。
    pub object_key: String,
}

/// 建对象的仓储输入(无 id:仓储生成 id、置 version=1、status=Created;租户取自 `scope`;
/// 时间戳取 `created_at`,由 service 经注入的时钟决定)。
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct NewObject {
    /// 所属内容(须在同一作用域存活)。
    pub content_id: Uuid,
    /// 存储后端名。
    pub storage_backend_name: String,
    /// 存储级别。
    pub storage_class: Option<String>,
    /// 后端内的 key。
    pub object_key: String,
    /// 原始文件名。
    pub file_name: Option<String>,
    /// 对象种类。
    pub object_type: Option<String>,
    /// 建行时刻:新行的 `created_at` = `updated_at` = 它(仓储按传入值落库,不自己取时间)。
    pub created_at: OffsetDateTime,
}

impl NewObject {
    /// 只填所属内容、位置和建行时刻,其余可选字段为 `None`。
    #[must_use]
    pub fn new(content_id: Uuid, location: ObjectLocation, created_at: OffsetDateTime) -> Self {
        Self {
            content_id,
            storage_backend_name: location.storage_backend_name,
            storage_class: None,
            object_key: location.object_key,
            file_name: None,
            object_type: None,
            created_at,
        }
    }
}

/// 对象仓储端口(object + object_metadata)。每个方法第一个参数是作用域,见 [`ContentScope`]。
///
/// **时间戳由调用方给**(同 [`ContentRepo`](crate::ContentRepo)):建行用 [`NewObject::created_at`],
/// 置状态 / 软删用参数 `at`,元数据用传入结构的时间戳。实现按传入值落库,不自己取时间。
#[async_trait]
pub trait ObjectRepo: Send + Sync {
    /// 建对象行(version=1,status=Created,tenant_id 取自 `scope`,
    /// `created_at` = `updated_at` = `object.created_at`)。
    /// 撞存活 (storage_backend_name, object_key) → `Conflict`。
    ///
    /// **父 content 校验**:PG 实现在同一事务里先验 `object.content_id` 在本作用域存活(别的租户 / 个人作用域 /
    /// 不存在 / 已软删 → `NotFound`)—— FK 检查不受 RLS 约束,不验就能跨作用域挂对象。
    /// **内存实现不验**(`InMemoryObjectRepo` 与 content 仓储各管各的行,拿不到 content 表):调用方(service)
    /// 必须先在同一作用域取到 / 建好 content 再建对象。契约用例 `object_create_requires_parent_in_scope` 只对 PG 跑。
    async fn create(
        &self,
        scope: ContentScope,
        object: NewObject,
        by: Option<&str>,
    ) -> Result<Object, ContentError>;

    /// 按 id 查本作用域的存活对象。不存在 / 已软删 / 别的作用域 → `NotFound`。
    async fn get(&self, scope: ContentScope, id: Uuid) -> Result<Object, ContentError>;

    /// 列某 content 在本作用域的存活对象(按 id asc = 建序)。
    /// 返回 `Vec`:实现天然从数据库取回成 `Vec`(指南「Iterators vs. owned collections」的例外)。
    async fn list_by_content(
        &self,
        scope: ContentScope,
        content_id: Uuid,
    ) -> Result<Vec<Object>, ContentError>;

    /// 按 (storage_backend_name, object_key) 复合唯一查本作用域的存活对象。不存在 → `NotFound`。
    async fn get_by_location(
        &self,
        scope: ContentScope,
        location: &ObjectLocation,
    ) -> Result<Object, ContentError>;

    /// 置对象状态(`updated_at` = `at`)。已软删 / 不存在 / 别的作用域 → `NotFound`。
    async fn set_status(
        &self,
        scope: ContentScope,
        id: Uuid,
        status: ObjectStatus,
        at: OffsetDateTime,
        by: Option<&str>,
    ) -> Result<(), ContentError>;

    /// 软删对象(`deleted_at` = `updated_at` = `at`)。已删 / 不存在 / 别的作用域 → `NotFound`。
    async fn soft_delete(
        &self,
        scope: ContentScope,
        id: Uuid,
        at: OffsetDateTime,
        by: Option<&str>,
    ) -> Result<(), ContentError>;

    /// upsert object_metadata(PK=object_id)。时间戳按传入值落库:`updated_at` 取
    /// `metadata.updated_at`;新插入的行 `created_at` 取 `metadata.created_at`,已存在的行保留原 `created_at`。
    /// 对象在本作用域不存在 / 已软删 → `NotFound`。
    async fn set_metadata(
        &self,
        scope: ContentScope,
        metadata: ObjectMetadata,
    ) -> Result<(), ContentError>;

    /// 查 object_metadata。不存在 / 别的作用域 → `NotFound`。
    async fn get_metadata(
        &self,
        scope: ContentScope,
        object_id: Uuid,
    ) -> Result<ObjectMetadata, ContentError>;
}

/// 转发实现:让 `Arc<dyn ObjectRepo>` 能填 [`ContentService`](crate::ContentService) 的仓储类型参数
/// (下游在运行时二选一实现时用)。
#[async_trait]
impl<T: ObjectRepo + ?Sized> ObjectRepo for Arc<T> {
    async fn create(
        &self,
        scope: ContentScope,
        object: NewObject,
        by: Option<&str>,
    ) -> Result<Object, ContentError> {
        (**self).create(scope, object, by).await
    }

    async fn get(&self, scope: ContentScope, id: Uuid) -> Result<Object, ContentError> {
        (**self).get(scope, id).await
    }

    async fn list_by_content(
        &self,
        scope: ContentScope,
        content_id: Uuid,
    ) -> Result<Vec<Object>, ContentError> {
        (**self).list_by_content(scope, content_id).await
    }

    async fn get_by_location(
        &self,
        scope: ContentScope,
        location: &ObjectLocation,
    ) -> Result<Object, ContentError> {
        (**self).get_by_location(scope, location).await
    }

    async fn set_status(
        &self,
        scope: ContentScope,
        id: Uuid,
        status: ObjectStatus,
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

    async fn set_metadata(
        &self,
        scope: ContentScope,
        metadata: ObjectMetadata,
    ) -> Result<(), ContentError> {
        (**self).set_metadata(scope, metadata).await
    }

    async fn get_metadata(
        &self,
        scope: ContentScope,
        object_id: Uuid,
    ) -> Result<ObjectMetadata, ContentError> {
        (**self).get_metadata(scope, object_id).await
    }
}

/// 转发实现:同 `Arc<T>`,给独占所有权的 `Box<dyn ObjectRepo>` 用。
#[async_trait]
impl<T: ObjectRepo + ?Sized> ObjectRepo for Box<T> {
    async fn create(
        &self,
        scope: ContentScope,
        object: NewObject,
        by: Option<&str>,
    ) -> Result<Object, ContentError> {
        (**self).create(scope, object, by).await
    }

    async fn get(&self, scope: ContentScope, id: Uuid) -> Result<Object, ContentError> {
        (**self).get(scope, id).await
    }

    async fn list_by_content(
        &self,
        scope: ContentScope,
        content_id: Uuid,
    ) -> Result<Vec<Object>, ContentError> {
        (**self).list_by_content(scope, content_id).await
    }

    async fn get_by_location(
        &self,
        scope: ContentScope,
        location: &ObjectLocation,
    ) -> Result<Object, ContentError> {
        (**self).get_by_location(scope, location).await
    }

    async fn set_status(
        &self,
        scope: ContentScope,
        id: Uuid,
        status: ObjectStatus,
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

    async fn set_metadata(
        &self,
        scope: ContentScope,
        metadata: ObjectMetadata,
    ) -> Result<(), ContentError> {
        (**self).set_metadata(scope, metadata).await
    }

    async fn get_metadata(
        &self,
        scope: ContentScope,
        object_id: Uuid,
    ) -> Result<ObjectMetadata, ContentError> {
        (**self).get_metadata(scope, object_id).await
    }
}
