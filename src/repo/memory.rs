//! content 仓储内存实现 —— 脚手架默认,无 DB 即可跑通 upload/download 全链路 + 写单测。
//! 镜像 PG 的软删过滤、(storage_backend_name, object_key) 存活唯一、status 边界、**作用域过滤**,保 parity。
//! 内存没有 RLS:每一行都记着自己的 `tenant_id`(个人内容 = `None`),每个方法按 `scope` 过滤 —— 参数是唯一防线。

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{ContentRepo, NewContent, NewObject, ObjectRepo};
use crate::error::ContentError;
use crate::status::{ContentStatus, ObjectStatus};
use crate::types::{Content, ContentMetadata, ContentScope, Object, ObjectMetadata};

/// 行是否属于本作用域(租户 ↔ 同一 tenant_id;个人 ↔ tenant_id 为 None)。
fn in_scope(scope: ContentScope, tenant_id: Option<Uuid>) -> bool {
    scope.tenant_id() == tenant_id
}

// ── 内容 ──

/// 内存内部行:比公开 `Content` 多 `deleted_at`(不暴露)。status 存 typed 枚举(内存不经字符串往返)。
#[derive(Clone)]
struct ContentRow {
    id: Uuid,
    tenant_id: Option<Uuid>,
    owner_id: Uuid,
    owner_type: Option<String>,
    name: Option<String>,
    description: Option<String>,
    document_type: Option<String>,
    status: ContentStatus,
    derivation_type: Option<String>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    deleted_at: Option<OffsetDateTime>,
}

impl ContentRow {
    fn to_entity(&self) -> Content {
        Content {
            id: self.id,
            tenant_id: self.tenant_id,
            owner_id: self.owner_id,
            owner_type: self.owner_type.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            document_type: self.document_type.clone(),
            status: self.status,
            derivation_type: self.derivation_type.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }

    /// 本作用域可见的存活行。
    fn alive_in(&self, scope: ContentScope) -> bool {
        self.deleted_at.is_none() && in_scope(scope, self.tenant_id)
    }
}

/// 元数据行带上所属租户(PG 里是 content_metadata.tenant_id 列)。
type ScopedContentMeta = (Option<Uuid>, ContentMetadata);

pub struct InMemoryContentRepo {
    contents: Mutex<HashMap<Uuid, ContentRow>>,
    metadata: Mutex<HashMap<Uuid, ScopedContentMeta>>,
}

impl InMemoryContentRepo {
    pub fn new() -> Self {
        Self {
            contents: Mutex::new(HashMap::new()),
            metadata: Mutex::new(HashMap::new()),
        }
    }

    /// 改一条本作用域的存活行;找不到 → NotFound。
    fn mutate_alive<T>(
        &self,
        scope: ContentScope,
        id: Uuid,
        f: impl FnOnce(&mut ContentRow) -> T,
    ) -> Result<T, ContentError> {
        scope.validate()?;
        let mut store = self.contents.lock().expect("锁未中毒");
        match store.get_mut(&id) {
            Some(r) if r.alive_in(scope) => Ok(f(r)),
            _ => Err(ContentError::NotFound),
        }
    }
}

impl Default for InMemoryContentRepo {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ContentRepo for InMemoryContentRepo {
    async fn create(
        &self,
        scope: ContentScope,
        c: NewContent,
        _by: Option<String>,
    ) -> Result<Content, ContentError> {
        scope.validate()?;
        let now = OffsetDateTime::now_utc();
        let row = ContentRow {
            id: Uuid::now_v7(),
            tenant_id: scope.tenant_id(),
            owner_id: c.owner_id,
            owner_type: c.owner_type,
            name: c.name,
            description: c.description,
            document_type: c.document_type,
            status: ContentStatus::Created,
            derivation_type: c.derivation_type,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        let entity = row.to_entity();
        self.contents.lock().expect("锁未中毒").insert(row.id, row);
        Ok(entity)
    }

    async fn get(&self, scope: ContentScope, id: Uuid) -> Result<Content, ContentError> {
        scope.validate()?;
        self.contents
            .lock()
            .expect("锁未中毒")
            .get(&id)
            .filter(|r| r.alive_in(scope))
            .map(ContentRow::to_entity)
            .ok_or(ContentError::NotFound)
    }

    async fn get_many(
        &self,
        scope: ContentScope,
        ids: &[Uuid],
    ) -> Result<Vec<Content>, ContentError> {
        scope.validate()?;
        let store = self.contents.lock().expect("锁未中毒");
        // 镜像 PG:只返本作用域的存活行,查不到的 id 直接缺席(不报错)。
        Ok(ids
            .iter()
            .filter_map(|id| store.get(id))
            .filter(|r| r.alive_in(scope))
            .map(ContentRow::to_entity)
            .collect())
    }

    async fn update(
        &self,
        scope: ContentScope,
        c: &Content,
        _by: Option<String>,
    ) -> Result<Content, ContentError> {
        self.mutate_alive(scope, c.id, |r| {
            // PUT 全量替换可编辑字段;tenant/owner/status/derivation 不动。
            r.owner_type = c.owner_type.clone();
            r.name = c.name.clone();
            r.description = c.description.clone();
            r.document_type = c.document_type.clone();
            r.updated_at = OffsetDateTime::now_utc();
            r.to_entity()
        })
    }

    async fn set_status(
        &self,
        scope: ContentScope,
        id: Uuid,
        status: ContentStatus,
        _by: Option<String>,
    ) -> Result<(), ContentError> {
        self.mutate_alive(scope, id, |r| {
            r.status = status;
            r.updated_at = OffsetDateTime::now_utc();
        })
    }

    async fn soft_delete(
        &self,
        scope: ContentScope,
        id: Uuid,
        _by: Option<String>,
    ) -> Result<(), ContentError> {
        self.mutate_alive(scope, id, |r| {
            r.deleted_at = Some(OffsetDateTime::now_utc());
        })
    }

    async fn list(
        &self,
        scope: ContentScope,
        owner_id: Uuid,
    ) -> Result<Vec<Content>, ContentError> {
        scope.validate()?;
        let store = self.contents.lock().expect("锁未中毒");
        let mut rows: Vec<&ContentRow> = store
            .values()
            .filter(|r| r.alive_in(scope) && r.owner_id == owner_id)
            .collect();
        rows.sort_by(|a, b| b.id.cmp(&a.id)); // id desc(v7 单列全序 = 建序倒序)
        Ok(rows.into_iter().map(ContentRow::to_entity).collect())
    }

    async fn set_metadata(
        &self,
        scope: ContentScope,
        m: ContentMetadata,
    ) -> Result<(), ContentError> {
        // 先验父行在本作用域存活(镜像 PG:同一事务里先查 content)。
        self.mutate_alive(scope, m.content_id, |_| ())?;
        // upsert:覆盖时间戳为 now(传入时间戳忽略,镜像 PG 由默认/触发器维护)。
        let now = OffsetDateTime::now_utc();
        let mut store = self.metadata.lock().expect("锁未中毒");
        let created_at = store.get(&m.content_id).map_or(now, |(_, e)| e.created_at);
        store.insert(
            m.content_id,
            (
                scope.tenant_id(),
                ContentMetadata {
                    created_at,
                    updated_at: now,
                    ..m
                },
            ),
        );
        Ok(())
    }

    async fn get_metadata(
        &self,
        scope: ContentScope,
        content_id: Uuid,
    ) -> Result<ContentMetadata, ContentError> {
        scope.validate()?;
        self.metadata
            .lock()
            .expect("锁未中毒")
            .get(&content_id)
            .filter(|(t, _)| in_scope(scope, *t))
            .map(|(_, m)| m.clone())
            .ok_or(ContentError::NotFound)
    }

    async fn get_metadata_many(
        &self,
        scope: ContentScope,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, ContentMetadata>, ContentError> {
        scope.validate()?;
        let store = self.metadata.lock().expect("锁未中毒");
        Ok(ids
            .iter()
            .filter_map(|id| store.get(id).map(|e| (*id, e)))
            .filter(|(_, (t, _))| in_scope(scope, *t))
            .map(|(id, (_, m))| (id, m.clone()))
            .collect())
    }
}

// ── 对象 ──

#[derive(Clone)]
struct ObjectRow {
    id: Uuid,
    tenant_id: Option<Uuid>,
    content_id: Uuid,
    storage_backend_name: String,
    storage_class: Option<String>,
    object_key: String,
    file_name: Option<String>,
    version: i32,
    object_type: Option<String>,
    status: ObjectStatus,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    deleted_at: Option<OffsetDateTime>,
}

impl ObjectRow {
    fn to_entity(&self) -> Object {
        Object {
            id: self.id,
            content_id: self.content_id,
            storage_backend_name: self.storage_backend_name.clone(),
            storage_class: self.storage_class.clone(),
            object_key: self.object_key.clone(),
            file_name: self.file_name.clone(),
            version: self.version,
            object_type: self.object_type.clone(),
            status: self.status,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }

    fn alive_in(&self, scope: ContentScope) -> bool {
        self.deleted_at.is_none() && in_scope(scope, self.tenant_id)
    }
}

type ScopedObjectMeta = (Option<Uuid>, ObjectMetadata);

pub struct InMemoryObjectRepo {
    objects: Mutex<HashMap<Uuid, ObjectRow>>,
    metadata: Mutex<HashMap<Uuid, ScopedObjectMeta>>,
}

impl InMemoryObjectRepo {
    pub fn new() -> Self {
        Self {
            objects: Mutex::new(HashMap::new()),
            metadata: Mutex::new(HashMap::new()),
        }
    }

    fn mutate_alive<T>(
        &self,
        scope: ContentScope,
        id: Uuid,
        f: impl FnOnce(&mut ObjectRow) -> T,
    ) -> Result<T, ContentError> {
        scope.validate()?;
        let mut store = self.objects.lock().expect("锁未中毒");
        match store.get_mut(&id) {
            Some(r) if r.alive_in(scope) => Ok(f(r)),
            _ => Err(ContentError::NotFound),
        }
    }
}

impl Default for InMemoryObjectRepo {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ObjectRepo for InMemoryObjectRepo {
    async fn create(
        &self,
        scope: ContentScope,
        o: NewObject,
        _by: Option<String>,
    ) -> Result<Object, ContentError> {
        scope.validate()?;
        let now = OffsetDateTime::now_utc();
        let mut store = self.objects.lock().expect("锁未中毒");
        // 存活唯一 (storage_backend_name, object_key):镜像部分唯一索引(**跨作用域**全局唯一,同 PG)。
        let dup = store.values().any(|r| {
            r.deleted_at.is_none()
                && r.storage_backend_name == o.storage_backend_name
                && r.object_key == o.object_key
        });
        if dup {
            return Err(ContentError::Conflict(
                "object_key already exists for storage backend".to_owned(),
            ));
        }
        let row = ObjectRow {
            id: Uuid::now_v7(),
            tenant_id: scope.tenant_id(),
            content_id: o.content_id,
            storage_backend_name: o.storage_backend_name,
            storage_class: o.storage_class,
            object_key: o.object_key,
            file_name: o.file_name,
            version: 1,
            object_type: o.object_type,
            status: ObjectStatus::Created,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };
        let entity = row.to_entity();
        store.insert(row.id, row);
        Ok(entity)
    }

    async fn get(&self, scope: ContentScope, id: Uuid) -> Result<Object, ContentError> {
        scope.validate()?;
        self.objects
            .lock()
            .expect("锁未中毒")
            .get(&id)
            .filter(|r| r.alive_in(scope))
            .map(ObjectRow::to_entity)
            .ok_or(ContentError::NotFound)
    }

    async fn list_by_content(
        &self,
        scope: ContentScope,
        content_id: Uuid,
    ) -> Result<Vec<Object>, ContentError> {
        scope.validate()?;
        let store = self.objects.lock().expect("锁未中毒");
        let mut rows: Vec<&ObjectRow> = store
            .values()
            .filter(|r| r.alive_in(scope) && r.content_id == content_id)
            .collect();
        rows.sort_by(|a, b| a.id.cmp(&b.id)); // id asc = 建序
        Ok(rows.into_iter().map(ObjectRow::to_entity).collect())
    }

    async fn get_by_key(
        &self,
        scope: ContentScope,
        object_key: &str,
        backend: &str,
    ) -> Result<Object, ContentError> {
        scope.validate()?;
        self.objects
            .lock()
            .expect("锁未中毒")
            .values()
            .find(|r| {
                r.alive_in(scope) && r.object_key == object_key && r.storage_backend_name == backend
            })
            .map(ObjectRow::to_entity)
            .ok_or(ContentError::NotFound)
    }

    async fn set_status(
        &self,
        scope: ContentScope,
        id: Uuid,
        status: ObjectStatus,
        _by: Option<String>,
    ) -> Result<(), ContentError> {
        self.mutate_alive(scope, id, |r| {
            r.status = status;
            r.updated_at = OffsetDateTime::now_utc();
        })
    }

    async fn soft_delete(
        &self,
        scope: ContentScope,
        id: Uuid,
        _by: Option<String>,
    ) -> Result<(), ContentError> {
        self.mutate_alive(scope, id, |r| {
            r.deleted_at = Some(OffsetDateTime::now_utc());
        })
    }

    async fn set_metadata(
        &self,
        scope: ContentScope,
        m: ObjectMetadata,
    ) -> Result<(), ContentError> {
        // 先验父对象在本作用域存活(镜像 PG)。
        self.mutate_alive(scope, m.object_id, |_| ())?;
        let now = OffsetDateTime::now_utc();
        let mut store = self.metadata.lock().expect("锁未中毒");
        let created_at = store.get(&m.object_id).map_or(now, |(_, e)| e.created_at);
        store.insert(
            m.object_id,
            (
                scope.tenant_id(),
                ObjectMetadata {
                    created_at,
                    updated_at: now,
                    ..m
                },
            ),
        );
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
            .expect("锁未中毒")
            .get(&object_id)
            .filter(|(t, _)| in_scope(scope, *t))
            .map(|(_, m)| m.clone())
            .ok_or(ContentError::NotFound)
    }
}
