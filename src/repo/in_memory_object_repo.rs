//! 对象仓储的内存实现。镜像 PG 的软删过滤、(storage_backend_name, object_key) 存活唯一、**作用域过滤**。
//! 不验父 content(拿不到 content 表),见 [`ObjectRepo::create`] 的文档。时间戳一律用调用方传入的值(同 PG)。

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use time::OffsetDateTime;
use uuid::Uuid;

use super::object_repo::{NewObject, ObjectLocation, ObjectRepo};
use super::scoped_metadata::ScopedMetadata;
use crate::content_error::ContentError;
use crate::content_scope::ContentScope;
use crate::object_status::ObjectStatus;
use crate::types::{Object, ObjectMetadata};

/// 内存内部行:公开实体 + 不暴露的 `tenant_id` / `deleted_at`。
#[derive(Debug)]
struct StoredObject {
    object: Object,
    tenant_id: Option<Uuid>,
    deleted_at: Option<OffsetDateTime>,
}

impl StoredObject {
    /// 本作用域可见的存活行。
    fn alive_in(&self, scope: ContentScope) -> bool {
        self.deleted_at.is_none() && scope.contains(self.tenant_id)
    }
}

/// [`ObjectRepo`] 的内存实现(进程内 `HashMap`,重启即丢)。
///
/// 内部锁中毒(别的线程持锁时 panic 过)后,各方法 panic,不去碰中毒锁里的数据。
#[derive(Debug, Default)]
pub struct InMemoryObjectRepo {
    objects: Mutex<HashMap<Uuid, StoredObject>>,
    metadata: Mutex<HashMap<Uuid, ScopedMetadata<ObjectMetadata>>>,
}

impl InMemoryObjectRepo {
    /// 空仓储。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 改一条本作用域的存活行;找不到 → NotFound。
    fn mutate_alive<T>(
        &self,
        scope: ContentScope,
        id: Uuid,
        mutate: impl FnOnce(&mut StoredObject) -> T,
    ) -> Result<T, ContentError> {
        scope.validate()?;
        let mut objects = self
            .objects
            .lock()
            .expect("InMemoryObjectRepo 的锁不应中毒");
        match objects.get_mut(&id) {
            Some(row) if row.alive_in(scope) => Ok(mutate(row)),
            _ => Err(ContentError::NotFound),
        }
    }

    /// 测试用:不论作用域 / 是否软删,取库里的原始行(公开实体 + `deleted_at`)。
    #[cfg(test)]
    pub(crate) fn raw_row(&self, id: Uuid) -> Option<(Object, Option<OffsetDateTime>)> {
        self.objects
            .lock()
            .expect("InMemoryObjectRepo 的锁不应中毒")
            .get(&id)
            .map(|row| (row.object.clone(), row.deleted_at))
    }
}

#[async_trait]
impl ObjectRepo for InMemoryObjectRepo {
    async fn create(
        &self,
        scope: ContentScope,
        object: NewObject,
        _by: Option<&str>,
    ) -> Result<Object, ContentError> {
        scope.validate()?;
        let mut objects = self
            .objects
            .lock()
            .expect("InMemoryObjectRepo 的锁不应中毒");
        // 存活唯一 (storage_backend_name, object_key):镜像部分唯一索引(**跨作用域**全局唯一,同 PG)。
        let duplicate = objects.values().any(|row| {
            row.deleted_at.is_none()
                && row.object.storage_backend_name == object.storage_backend_name
                && row.object.object_key == object.object_key
        });
        if duplicate {
            return Err(ContentError::Conflict(
                "object_key already exists for storage backend".to_owned(),
            ));
        }
        let created = Object::new(Uuid::now_v7(), object);
        let row = StoredObject {
            object: created.clone(),
            tenant_id: scope.tenant_id(),
            deleted_at: None,
        };
        objects.insert(created.id, row);
        Ok(created)
    }

    async fn get(&self, scope: ContentScope, id: Uuid) -> Result<Object, ContentError> {
        scope.validate()?;
        self.objects
            .lock()
            .expect("InMemoryObjectRepo 的锁不应中毒")
            .get(&id)
            .filter(|row| row.alive_in(scope))
            .map(|row| row.object.clone())
            .ok_or(ContentError::NotFound)
    }

    async fn list_by_content(
        &self,
        scope: ContentScope,
        content_id: Uuid,
    ) -> Result<Vec<Object>, ContentError> {
        scope.validate()?;
        let objects = self
            .objects
            .lock()
            .expect("InMemoryObjectRepo 的锁不应中毒");
        let mut listed: Vec<Object> = objects
            .values()
            .filter(|row| row.alive_in(scope) && row.object.content_id == content_id)
            .map(|row| row.object.clone())
            .collect();
        // id asc = 建序
        listed.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(listed)
    }

    async fn get_by_location(
        &self,
        scope: ContentScope,
        location: &ObjectLocation,
    ) -> Result<Object, ContentError> {
        scope.validate()?;
        self.objects
            .lock()
            .expect("InMemoryObjectRepo 的锁不应中毒")
            .values()
            .find(|row| {
                row.alive_in(scope)
                    && row.object.object_key == location.object_key
                    && row.object.storage_backend_name == location.storage_backend_name
            })
            .map(|row| row.object.clone())
            .ok_or(ContentError::NotFound)
    }

    async fn set_status(
        &self,
        scope: ContentScope,
        id: Uuid,
        status: ObjectStatus,
        at: OffsetDateTime,
        _by: Option<&str>,
    ) -> Result<(), ContentError> {
        self.mutate_alive(scope, id, |row| {
            row.object.status = status;
            row.object.updated_at = at;
        })
    }

    async fn soft_delete(
        &self,
        scope: ContentScope,
        id: Uuid,
        at: OffsetDateTime,
        _by: Option<&str>,
    ) -> Result<(), ContentError> {
        self.mutate_alive(scope, id, |row| {
            row.deleted_at = Some(at);
            row.object.updated_at = at;
        })
    }

    async fn set_metadata(
        &self,
        scope: ContentScope,
        metadata: ObjectMetadata,
    ) -> Result<(), ContentError> {
        // 先验父对象在本作用域存活(镜像 PG)。
        self.mutate_alive(scope, metadata.object_id, |_| ())?;
        // upsert:时间戳用传入值;已有行保留原 created_at(镜像 PG)。
        let mut store = self
            .metadata
            .lock()
            .expect("InMemoryObjectRepo 的锁不应中毒");
        let created_at = store
            .get(&metadata.object_id)
            .map_or(metadata.created_at, |existing| existing.metadata.created_at);
        let row = ScopedMetadata {
            tenant_id: scope.tenant_id(),
            metadata: ObjectMetadata {
                created_at,
                ..metadata
            },
        };
        store.insert(row.metadata.object_id, row);
        Ok(())
    }

    async fn get_metadata(
        &self,
        scope: ContentScope,
        object_id: Uuid,
    ) -> Result<ObjectMetadata, ContentError> {
        scope.validate()?;
        self.metadata
            .lock()
            .expect("InMemoryObjectRepo 的锁不应中毒")
            .get(&object_id)
            .filter(|row| row.visible_in(scope))
            .map(|row| row.metadata.clone())
            .ok_or(ContentError::NotFound)
    }
}
