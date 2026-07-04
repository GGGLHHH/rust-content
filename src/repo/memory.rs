//! content 仓储内存实现 —— 脚手架默认,无 DB 即可跑通 upload/download 全链路 + 写单测。
//! 镜像 PG 的软删过滤、(storage_backend_name, object_key) 存活唯一、status 边界,保 parity。

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{ContentRepo, NewContent, NewObject, ObjectRepo};
use crate::error::ContentError;
use crate::status::{ContentStatus, ObjectStatus};
use crate::types::{Content, ContentMetadata, Object, ObjectMetadata};

// ── 内容 ──

/// 内存内部行:比公开 `Content` 多 `deleted_at`(不暴露)。status 存 typed 枚举(内存不经字符串往返)。
#[derive(Clone)]
struct ContentRow {
    id: Uuid,
    tenant_id: Uuid,
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
}

pub struct InMemoryContentRepo {
    contents: Mutex<HashMap<Uuid, ContentRow>>,
    metadata: Mutex<HashMap<Uuid, ContentMetadata>>,
}

impl InMemoryContentRepo {
    pub fn new() -> Self {
        Self {
            contents: Mutex::new(HashMap::new()),
            metadata: Mutex::new(HashMap::new()),
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
    async fn create(&self, c: NewContent, _by: Option<String>) -> Result<Content, ContentError> {
        let now = OffsetDateTime::now_utc();
        let row = ContentRow {
            id: Uuid::now_v7(),
            tenant_id: c.tenant_id,
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

    async fn get(&self, id: Uuid) -> Result<Content, ContentError> {
        self.contents
            .lock()
            .expect("锁未中毒")
            .get(&id)
            .filter(|r| r.deleted_at.is_none())
            .map(ContentRow::to_entity)
            .ok_or(ContentError::NotFound)
    }

    async fn get_many(&self, ids: &[Uuid]) -> Result<Vec<Content>, ContentError> {
        let store = self.contents.lock().expect("锁未中毒");
        // 镜像 PG:只返存活行,查不到的 id 直接缺席(不报错)。
        Ok(ids
            .iter()
            .filter_map(|id| store.get(id))
            .filter(|r| r.deleted_at.is_none())
            .map(ContentRow::to_entity)
            .collect())
    }

    async fn update(&self, c: &Content, _by: Option<String>) -> Result<Content, ContentError> {
        let mut store = self.contents.lock().expect("锁未中毒");
        match store.get_mut(&c.id) {
            Some(r) if r.deleted_at.is_none() => {
                // PUT 全量替换可编辑字段;tenant/owner/status/derivation 不动。
                r.owner_type = c.owner_type.clone();
                r.name = c.name.clone();
                r.description = c.description.clone();
                r.document_type = c.document_type.clone();
                r.updated_at = OffsetDateTime::now_utc();
                Ok(r.to_entity())
            }
            _ => Err(ContentError::NotFound),
        }
    }

    async fn set_status(
        &self,
        id: Uuid,
        status: ContentStatus,
        _by: Option<String>,
    ) -> Result<(), ContentError> {
        let mut store = self.contents.lock().expect("锁未中毒");
        match store.get_mut(&id) {
            Some(r) if r.deleted_at.is_none() => {
                r.status = status;
                r.updated_at = OffsetDateTime::now_utc();
                Ok(())
            }
            _ => Err(ContentError::NotFound),
        }
    }

    async fn soft_delete(&self, id: Uuid, _by: Option<String>) -> Result<(), ContentError> {
        let mut store = self.contents.lock().expect("锁未中毒");
        match store.get_mut(&id) {
            Some(r) if r.deleted_at.is_none() => {
                r.deleted_at = Some(OffsetDateTime::now_utc());
                Ok(())
            }
            _ => Err(ContentError::NotFound),
        }
    }

    async fn list(&self, owner_id: Uuid, tenant_id: Uuid) -> Result<Vec<Content>, ContentError> {
        let store = self.contents.lock().expect("锁未中毒");
        let mut rows: Vec<&ContentRow> = store
            .values()
            .filter(|r| {
                r.deleted_at.is_none() && r.owner_id == owner_id && r.tenant_id == tenant_id
            })
            .collect();
        rows.sort_by(|a, b| b.id.cmp(&a.id)); // id desc(v7 单列全序 = 建序倒序)
        Ok(rows.into_iter().map(ContentRow::to_entity).collect())
    }

    async fn set_metadata(&self, m: ContentMetadata) -> Result<(), ContentError> {
        // upsert:覆盖时间戳为 now(传入时间戳忽略,镜像 PG 由默认/触发器维护)。
        let now = OffsetDateTime::now_utc();
        let mut store = self.metadata.lock().expect("锁未中毒");
        let created_at = store.get(&m.content_id).map_or(now, |e| e.created_at);
        store.insert(
            m.content_id,
            ContentMetadata {
                created_at,
                updated_at: now,
                ..m
            },
        );
        Ok(())
    }

    async fn get_metadata(&self, content_id: Uuid) -> Result<ContentMetadata, ContentError> {
        self.metadata
            .lock()
            .expect("锁未中毒")
            .get(&content_id)
            .cloned()
            .ok_or(ContentError::NotFound)
    }

    async fn get_metadata_many(
        &self,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, ContentMetadata>, ContentError> {
        let store = self.metadata.lock().expect("锁未中毒");
        Ok(ids
            .iter()
            .filter_map(|id| store.get(id).map(|m| (*id, m.clone())))
            .collect())
    }
}

// ── 对象 ──

#[derive(Clone)]
struct ObjectRow {
    id: Uuid,
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
}

pub struct InMemoryObjectRepo {
    objects: Mutex<HashMap<Uuid, ObjectRow>>,
    metadata: Mutex<HashMap<Uuid, ObjectMetadata>>,
}

impl InMemoryObjectRepo {
    pub fn new() -> Self {
        Self {
            objects: Mutex::new(HashMap::new()),
            metadata: Mutex::new(HashMap::new()),
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
    async fn create(&self, o: NewObject, _by: Option<String>) -> Result<Object, ContentError> {
        let now = OffsetDateTime::now_utc();
        let mut store = self.objects.lock().expect("锁未中毒");
        // 存活唯一 (storage_backend_name, object_key):镜像部分唯一索引。
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

    async fn get(&self, id: Uuid) -> Result<Object, ContentError> {
        self.objects
            .lock()
            .expect("锁未中毒")
            .get(&id)
            .filter(|r| r.deleted_at.is_none())
            .map(ObjectRow::to_entity)
            .ok_or(ContentError::NotFound)
    }

    async fn list_by_content(&self, content_id: Uuid) -> Result<Vec<Object>, ContentError> {
        let store = self.objects.lock().expect("锁未中毒");
        let mut rows: Vec<&ObjectRow> = store
            .values()
            .filter(|r| r.deleted_at.is_none() && r.content_id == content_id)
            .collect();
        rows.sort_by(|a, b| a.id.cmp(&b.id)); // id asc = 建序
        Ok(rows.into_iter().map(ObjectRow::to_entity).collect())
    }

    async fn get_by_key(&self, object_key: &str, backend: &str) -> Result<Object, ContentError> {
        self.objects
            .lock()
            .expect("锁未中毒")
            .values()
            .find(|r| {
                r.deleted_at.is_none()
                    && r.object_key == object_key
                    && r.storage_backend_name == backend
            })
            .map(ObjectRow::to_entity)
            .ok_or(ContentError::NotFound)
    }

    async fn set_status(
        &self,
        id: Uuid,
        status: ObjectStatus,
        _by: Option<String>,
    ) -> Result<(), ContentError> {
        let mut store = self.objects.lock().expect("锁未中毒");
        match store.get_mut(&id) {
            Some(r) if r.deleted_at.is_none() => {
                r.status = status;
                r.updated_at = OffsetDateTime::now_utc();
                Ok(())
            }
            _ => Err(ContentError::NotFound),
        }
    }

    async fn soft_delete(&self, id: Uuid, _by: Option<String>) -> Result<(), ContentError> {
        let mut store = self.objects.lock().expect("锁未中毒");
        match store.get_mut(&id) {
            Some(r) if r.deleted_at.is_none() => {
                r.deleted_at = Some(OffsetDateTime::now_utc());
                Ok(())
            }
            _ => Err(ContentError::NotFound),
        }
    }

    async fn set_metadata(&self, m: ObjectMetadata) -> Result<(), ContentError> {
        let now = OffsetDateTime::now_utc();
        let mut store = self.metadata.lock().expect("锁未中毒");
        let created_at = store.get(&m.object_id).map_or(now, |e| e.created_at);
        store.insert(
            m.object_id,
            ObjectMetadata {
                created_at,
                updated_at: now,
                ..m
            },
        );
        Ok(())
    }

    async fn get_metadata(&self, object_id: Uuid) -> Result<ObjectMetadata, ContentError> {
        self.metadata
            .lock()
            .expect("锁未中毒")
            .get(&object_id)
            .cloned()
            .ok_or(ContentError::NotFound)
    }
}
