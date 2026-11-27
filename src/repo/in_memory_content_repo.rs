//! 内容仓储的内存实现 —— 脚手架默认,无 DB 即可跑通 upload/download 全链路 + 写单测。
//! 镜像 PG 的软删过滤、status 边界、**作用域过滤**,保 parity。
//! 内存没有 RLS:每一行都记着自己的 `tenant_id`(个人内容 = `None`),每个方法按 `scope` 过滤 —— 参数是唯一防线。
//! 时间戳一律用调用方传入的值(同 PG),不读系统时钟。

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use time::OffsetDateTime;
use uuid::Uuid;

use super::content_repo::{ContentRepo, NewContent};
use super::scoped_metadata::ScopedMetadata;
use crate::content_error::ContentError;
use crate::content_scope::ContentScope;
use crate::content_status::ContentStatus;
use crate::types::{Content, ContentMetadata};

/// 内存内部行:公开实体 + 不暴露的 `deleted_at`。status 存 typed 枚举(内存不经字符串往返)。
#[derive(Debug)]
struct StoredContent {
    content: Content,
    deleted_at: Option<OffsetDateTime>,
}

impl StoredContent {
    /// 本作用域可见的存活行。
    fn alive_in(&self, scope: ContentScope) -> bool {
        self.deleted_at.is_none() && scope.contains(self.content.tenant_id)
    }
}

/// [`ContentRepo`] 的内存实现(进程内 `HashMap`,重启即丢)。
///
/// 内部锁中毒(别的线程持锁时 panic 过)后,各方法 panic,不去碰中毒锁里的数据。
#[derive(Debug, Default)]
pub struct InMemoryContentRepo {
    contents: Mutex<HashMap<Uuid, StoredContent>>,
    metadata: Mutex<HashMap<Uuid, ScopedMetadata<ContentMetadata>>>,
}

impl InMemoryContentRepo {
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
        mutate: impl FnOnce(&mut StoredContent) -> T,
    ) -> Result<T, ContentError> {
        scope.validate()?;
        let mut contents = self
            .contents
            .lock()
            .expect("InMemoryContentRepo 的锁不应中毒");
        match contents.get_mut(&id) {
            Some(row) if row.alive_in(scope) => Ok(mutate(row)),
            _ => Err(ContentError::NotFound),
        }
    }

    /// 测试用:不论作用域 / 是否软删,取库里的原始行(公开实体 + `deleted_at`)。
    #[cfg(test)]
    pub(crate) fn raw_row(&self, id: Uuid) -> Option<(Content, Option<OffsetDateTime>)> {
        self.contents
            .lock()
            .expect("InMemoryContentRepo 的锁不应中毒")
            .get(&id)
            .map(|row| (row.content.clone(), row.deleted_at))
    }
}

#[async_trait]
impl ContentRepo for InMemoryContentRepo {
    async fn create(
        &self,
        scope: ContentScope,
        content: NewContent,
        _by: Option<&str>,
    ) -> Result<Content, ContentError> {
        scope.validate()?;
        let created = Content::new(Uuid::now_v7(), scope, content);
        let row = StoredContent {
            content: created.clone(),
            deleted_at: None,
        };
        self.contents
            .lock()
            .expect("InMemoryContentRepo 的锁不应中毒")
            .insert(created.id, row);
        Ok(created)
    }

    async fn get(&self, scope: ContentScope, id: Uuid) -> Result<Content, ContentError> {
        scope.validate()?;
        self.contents
            .lock()
            .expect("InMemoryContentRepo 的锁不应中毒")
            .get(&id)
            .filter(|row| row.alive_in(scope))
            .map(|row| row.content.clone())
            .ok_or(ContentError::NotFound)
    }

    async fn get_many(
        &self,
        scope: ContentScope,
        ids: &[Uuid],
    ) -> Result<Vec<Content>, ContentError> {
        scope.validate()?;
        let contents = self
            .contents
            .lock()
            .expect("InMemoryContentRepo 的锁不应中毒");
        // 镜像 PG:只返本作用域的存活行,查不到的 id 直接缺席(不报错)。
        Ok(ids
            .iter()
            .filter_map(|id| contents.get(id))
            .filter(|row| row.alive_in(scope))
            .map(|row| row.content.clone())
            .collect())
    }

    async fn update(
        &self,
        scope: ContentScope,
        content: &Content,
        at: OffsetDateTime,
        _by: Option<&str>,
    ) -> Result<Content, ContentError> {
        self.mutate_alive(scope, content.id, |row| {
            // PUT 全量替换可编辑字段;tenant/owner/status/derivation 不动。
            let stored = &mut row.content;
            stored.owner_type.clone_from(&content.owner_type);
            stored.name.clone_from(&content.name);
            stored.description.clone_from(&content.description);
            stored.document_type.clone_from(&content.document_type);
            stored.updated_at = at;
            stored.clone()
        })
    }

    async fn set_status(
        &self,
        scope: ContentScope,
        id: Uuid,
        status: ContentStatus,
        at: OffsetDateTime,
        _by: Option<&str>,
    ) -> Result<(), ContentError> {
        self.mutate_alive(scope, id, |row| {
            row.content.status = status;
            row.content.updated_at = at;
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
            row.content.updated_at = at;
        })
    }

    async fn list(
        &self,
        scope: ContentScope,
        owner_id: Uuid,
    ) -> Result<Vec<Content>, ContentError> {
        scope.validate()?;
        let contents = self
            .contents
            .lock()
            .expect("InMemoryContentRepo 的锁不应中毒");
        let mut listed: Vec<Content> = contents
            .values()
            .filter(|row| row.alive_in(scope) && row.content.owner_id == owner_id)
            .map(|row| row.content.clone())
            .collect();
        // id desc(v7 单列全序 = 建序倒序)
        listed.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(listed)
    }

    async fn set_metadata(
        &self,
        scope: ContentScope,
        metadata: ContentMetadata,
    ) -> Result<(), ContentError> {
        // 先验父行在本作用域存活(镜像 PG:同一事务里先查 content)。
        self.mutate_alive(scope, metadata.content_id, |_| ())?;
        // upsert:时间戳用传入值;已有行保留原 created_at(镜像 PG 的 ON CONFLICT 不更新 created_at)。
        let mut store = self
            .metadata
            .lock()
            .expect("InMemoryContentRepo 的锁不应中毒");
        let created_at = store
            .get(&metadata.content_id)
            .map_or(metadata.created_at, |existing| existing.metadata.created_at);
        let row = ScopedMetadata {
            tenant_id: scope.tenant_id(),
            metadata: ContentMetadata {
                created_at,
                ..metadata
            },
        };
        store.insert(row.metadata.content_id, row);
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
            .expect("InMemoryContentRepo 的锁不应中毒")
            .get(&content_id)
            .filter(|row| row.visible_in(scope))
            .map(|row| row.metadata.clone())
            .ok_or(ContentError::NotFound)
    }

    async fn get_metadata_many(
        &self,
        scope: ContentScope,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, ContentMetadata>, ContentError> {
        scope.validate()?;
        let store = self
            .metadata
            .lock()
            .expect("InMemoryContentRepo 的锁不应中毒");
        let mut found = HashMap::with_capacity(ids.len());
        for id in ids {
            if let Some(row) = store.get(id).filter(|row| row.visible_in(scope)) {
                found.insert(*id, row.metadata.clone());
            }
        }
        Ok(found)
    }
}
