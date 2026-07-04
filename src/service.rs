//! content 业务。持 content/object 仓储端口 + ObjectStore blob 端口 + clock,编排 CRUD 与上传/下载。
//! 范式同分层 service:依赖 trait 而非实现,在此做编排/审计下传。
//! **入参是领域结构(`input` 模块),已由 app 在 HTTP 边界校验完**;审计主体经 `by: Option<String>` 传。

use std::sync::Arc;

use bytes::Bytes;
use uuid::Uuid;

use crate::clock::{Clock, SystemClock};
use crate::error::ContentError;
use crate::input::{
    CreateContentInput, SetContentMetadataInput, UpdateContentInput, UploadContentInput,
};
use crate::repo::{ContentRepo, NewContent, NewObject, ObjectRepo};
use crate::status::{can_download_content, ContentStatus, ObjectStatus};
use crate::store::{ObjectStore, UploadParams};
use crate::types::{Content, ContentMetadata, Object, ObjectMetadata};

/// 一次性上传的结果(**纯数据**):内容 + 其主对象。app 映射成自己的对外响应 DTO。
/// (类比 idm 的 `AuthOutcome`。)
pub struct UploadOutcome {
    pub content: Content,
    pub object: Object,
}

/// 内容服务。`Clone` 廉价(全是 Arc),app 直接持有它(放进 `AppState`)。
#[derive(Clone)]
pub struct ContentService {
    inner: Arc<Inner>,
}

struct Inner {
    contents: Arc<dyn ContentRepo>,
    objects: Arc<dyn ObjectRepo>,
    store: Arc<dyn ObjectStore>,
    default_backend_name: String,
    clock: Arc<dyn Clock>,
}

/// 默认后端名(配合库内 `InMemoryObjectStore`)。
const DEFAULT_BACKEND_NAME: &str = "memory";

impl ContentService {
    /// **便捷构造**:给齐两个仓储 + 一个 ObjectStore + 后端名(系统时钟)。
    /// 想 override 时钟 → 用 [`ContentService::builder`]。
    pub fn new(
        contents: Arc<dyn ContentRepo>,
        objects: Arc<dyn ObjectRepo>,
        store: Arc<dyn ObjectStore>,
        backend_name: impl Into<String>,
    ) -> Self {
        Self::builder(contents, objects)
            .store(store)
            .backend_name(backend_name)
            .build()
    }

    /// **builder**:只设要 override 的端口,其余取默认(SystemClock / 后端名 "memory")。
    /// 仅 repos 无默认须显式传;ObjectStore **无安全默认** —— 必须经 `store(...)` 给出,否则 `build` panic
    /// (wiring 错误,启动期即暴露;镜像 idm 签验端口未设即 panic)。
    pub fn builder(
        contents: Arc<dyn ContentRepo>,
        objects: Arc<dyn ObjectRepo>,
    ) -> ContentServiceBuilder {
        ContentServiceBuilder {
            contents,
            objects,
            store: None,
            default_backend_name: DEFAULT_BACKEND_NAME.to_owned(),
            clock: Arc::new(SystemClock),
        }
    }

    /// 建内容(仅 content 行,status=Created)。`by` = 审计主体。
    pub async fn create_content(
        &self,
        input: CreateContentInput,
        by: Option<String>,
    ) -> Result<Content, ContentError> {
        self.inner
            .contents
            .create(
                NewContent {
                    tenant_id: input.tenant_id,
                    owner_id: input.owner_id,
                    owner_type: input.owner_type,
                    name: input.name,
                    description: input.description,
                    document_type: input.document_type,
                    derivation_type: input
                        .derivation_type
                        .or_else(|| Some("original".to_owned())),
                },
                by,
            )
            .await
    }

    /// 查内容。不存在 / 已软删 → `NotFound`。(**不**强制 tenant —— 鉴权归 app 授权层。)
    pub async fn get_content(&self, id: Uuid) -> Result<Content, ContentError> {
        self.inner.contents.get(id).await
    }

    /// **全量更新**内容可编辑字段(PUT)。先查存活再替换。已软删 → `NotFound`。
    pub async fn update_content(
        &self,
        id: Uuid,
        input: UpdateContentInput,
        by: Option<String>,
    ) -> Result<Content, ContentError> {
        let existing = self.inner.contents.get(id).await?;
        let updated = Content {
            owner_type: input.owner_type,
            name: input.name,
            description: input.description,
            document_type: input.document_type,
            ..existing
        };
        self.inner.contents.update(&updated, by).await
    }

    /// 软删内容。对象的清理是 DEFER(暂不级联软删 object/字节)。
    pub async fn delete_content(&self, id: Uuid, by: Option<String>) -> Result<(), ContentError> {
        self.inner.contents.soft_delete(id, by).await
    }

    /// 列某 (owner_id, tenant_id) 的存活内容(按 id desc)。
    pub async fn list_content(
        &self,
        owner_id: Uuid,
        tenant_id: Uuid,
    ) -> Result<Vec<Content>, ContentError> {
        self.inner.contents.list(owner_id, tenant_id).await
    }

    /// 一次性上传(Go `UploadContent` 流):建 content + object 行 → 推字节 → 翻状态 → 同步元数据。
    ///
    /// **顺序与容错(对齐 Go,见 spec §6)**:行先于字节;步骤 5–7(取后端元数据 / 写 object_metadata /
    /// 写 content_metadata)**非致命**(失败仅忽略,对象已是 Uploaded);步骤 4(object→Uploaded)与
    /// 步骤 8(content→Uploaded)致命。
    ///
    /// **一致性告知(v0.1 接受,无跨 repo 事务,见 repo/mod.rs)**:content 行与 object 行先于存储写建立;
    /// 若步骤 3 的 `ObjectStore::upload` 在行已存在后失败,则留下**孤儿行**(无补偿删除);若步骤 3 成功而
    /// 4/8 失败,则字节滞留。原子单元是单个 trait 方法,不在此跨两个仓储包一个 tx。
    pub async fn upload_content(
        &self,
        input: UploadContentInput,
        by: Option<String>,
    ) -> Result<UploadOutcome, ContentError> {
        // 步骤 1:建 content 行(status=Created)。
        let content = self
            .inner
            .contents
            .create(
                NewContent {
                    tenant_id: input.tenant_id,
                    owner_id: input.owner_id,
                    owner_type: input.owner_type,
                    name: input.name,
                    description: input.description,
                    document_type: input.document_type.clone(),
                    derivation_type: Some("original".to_owned()),
                },
                by.clone(),
            )
            .await?;

        // 步骤 2:定 object_key(None → 默认 `{content_id}/{uuid}`)+ 建 object 行(默认后端、version=1、Created)。
        let backend = self.inner.default_backend_name.clone();
        let object_key = input
            .object_key
            .clone()
            .unwrap_or_else(|| format!("{}/{}", content.id, Uuid::now_v7()));
        let object = self
            .inner
            .objects
            .create(
                NewObject {
                    content_id: content.id,
                    storage_backend_name: backend,
                    storage_class: None,
                    object_key: object_key.clone(),
                    file_name: input.file_name.clone(),
                    object_type: None,
                },
                by.clone(),
            )
            .await?;

        // 步骤 3:推字节到后端。【孤儿行注意】行已建,此步失败则留孤儿(无补偿删除,v0.1 接受)。
        let byte_len = input.data.len() as i64;
        self.inner
            .store
            .upload(
                UploadParams {
                    object_key: object_key.clone(),
                    mime_type: input.mime_type.clone(),
                    file_name: input.file_name.clone(),
                },
                input.data,
            )
            .await?;

        // 步骤 4(致命):object → Uploaded。
        self.inner
            .objects
            .set_status(object.id, ObjectStatus::Uploaded, by.clone())
            .await?;

        // 步骤 5(非致命):读后端元数据(size/etag/content-type)。
        let store_meta = self.inner.store.object_meta(&object_key).await.ok();
        let now = self.inner.clock.now();

        // 步骤 6(非致命):写 object_metadata(失败仅忽略)。
        let _ = self
            .inner
            .objects
            .set_metadata(ObjectMetadata {
                object_id: object.id,
                size_bytes: store_meta.as_ref().map(|m| m.size).or(Some(byte_len)),
                mime_type: store_meta
                    .as_ref()
                    .and_then(|m| m.content_type.clone())
                    .or_else(|| input.mime_type.clone()),
                etag: store_meta.as_ref().and_then(|m| m.etag.clone()),
                metadata: serde_json::Value::Object(serde_json::Map::new()),
                created_at: now,
                updated_at: now,
            })
            .await;

        // 步骤 7(非致命):写 content_metadata(失败仅忽略)。
        let _ = self
            .inner
            .contents
            .set_metadata(ContentMetadata {
                content_id: content.id,
                tags: input.tags,
                file_size: store_meta.as_ref().map(|m| m.size).or(Some(byte_len)),
                file_name: input.file_name,
                mime_type: store_meta
                    .as_ref()
                    .and_then(|m| m.content_type.clone())
                    .or(input.mime_type),
                checksum: None,
                checksum_algorithm: None,
                metadata: input
                    .custom_metadata
                    .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new())),
                created_at: now,
                updated_at: now,
            })
            .await;

        // 步骤 8(致命):content → Uploaded(原始内容终态)。
        self.inner
            .contents
            .set_status(content.id, ContentStatus::Uploaded, by)
            .await?;

        // 回读最终态实体(status 已翻 Uploaded),组装结果。
        let content = self.inner.contents.get(content.id).await?;
        let object = self.inner.objects.get(object.id).await?;
        Ok(UploadOutcome { content, object })
    }

    /// 下载内容主对象的字节(Go `DownloadContent` 流)。
    /// 内容不存在 → `NotFound`;状态不允许下载 → `NotReady`;无已上传对象 → `NotFound`。
    pub async fn download_content(&self, content_id: Uuid) -> Result<Bytes, ContentError> {
        // 1) 取内容(校验存在)。
        let content = self.inner.contents.get(content_id).await?;
        // 2) 状态守卫:仅 {Uploaded, Processed, Archived} 可下载。
        can_download_content(content.status)?;
        // 3) 取对象,挑第一个 status==Uploaded 的。
        let objects = self.inner.objects.list_by_content(content_id).await?;
        let target = objects
            .into_iter()
            .find(|o| o.status == ObjectStatus::Uploaded)
            .ok_or(ContentError::NotFound)?;
        // 4) 从后端取字节。
        self.inner.store.download(&target.object_key).await
    }

    /// 设置内容元数据(全量替换,upsert)。先验内容存在 → `NotFound`。
    pub async fn set_content_metadata(
        &self,
        input: SetContentMetadataInput,
    ) -> Result<(), ContentError> {
        self.inner.contents.get(input.content_id).await?; // 验存在
        let now = self.inner.clock.now();
        self.inner
            .contents
            .set_metadata(ContentMetadata {
                content_id: input.content_id,
                tags: input.tags,
                file_size: input.file_size,
                file_name: input.file_name,
                mime_type: input.mime_type,
                checksum: input.checksum,
                checksum_algorithm: input.checksum_algorithm,
                metadata: input.metadata,
                created_at: now,
                updated_at: now,
            })
            .await
    }

    /// 查内容元数据。不存在 → `NotFound`。
    pub async fn get_content_metadata(
        &self,
        content_id: Uuid,
    ) -> Result<ContentMetadata, ContentError> {
        self.inner.contents.get_metadata(content_id).await
    }

    /// 列某内容的对象(读侧 HTTP 层会用)。
    pub async fn get_objects(&self, content_id: Uuid) -> Result<Vec<Object>, ContentError> {
        self.inner.objects.list_by_content(content_id).await
    }

    /// 显式置内容状态(异步工作流钩子)。状态已 typed(类型即合法),故为对仓储 set_status 的薄包装。
    pub async fn set_content_status(
        &self,
        id: Uuid,
        status: ContentStatus,
        by: Option<String>,
    ) -> Result<(), ContentError> {
        self.inner.contents.set_status(id, status, by).await
    }

    /// 显式置对象状态(异步工作流钩子)。同上,薄包装。
    pub async fn set_object_status(
        &self,
        id: Uuid,
        status: ObjectStatus,
        by: Option<String>,
    ) -> Result<(), ContentError> {
        self.inner.objects.set_status(id, status, by).await
    }
}

/// [`ContentService`] 的 builder —— 只设要 override 的端口,其余取默认。见 [`ContentService::builder`]。
/// 默认:`clock`=SystemClock、后端名 "memory";ObjectStore 无默认(`build` 前必经 `store` 设)。
pub struct ContentServiceBuilder {
    contents: Arc<dyn ContentRepo>,
    objects: Arc<dyn ObjectRepo>,
    store: Option<Arc<dyn ObjectStore>>,
    default_backend_name: String,
    clock: Arc<dyn Clock>,
}

impl ContentServiceBuilder {
    /// 设 ObjectStore blob 端口(生产 minio/S3 由 app 注入;测试用 `InMemoryObjectStore`)。
    pub fn store(mut self, store: Arc<dyn ObjectStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// 设默认存储后端名(写进 object.storage_backend_name;默认 "memory")。
    pub fn backend_name(mut self, name: impl Into<String>) -> Self {
        self.default_backend_name = name.into();
        self
    }

    /// 替换时间端口(默认 `SystemClock`;测试注入固定时钟)。
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// 组装。未经 `store` 设 ObjectStore → panic(wiring 错误,启动期即暴露)。
    pub fn build(self) -> ContentService {
        let store = self
            .store
            .expect("ContentServiceBuilder::build: 须先调 store 设 ObjectStore blob 端口");
        ContentService {
            inner: Arc::new(Inner {
                contents: self.contents,
                objects: self.objects,
                store,
                default_backend_name: self.default_backend_name,
                clock: self.clock,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::{InMemoryContentRepo, InMemoryObjectRepo};
    use crate::store::InMemoryObjectStore;

    fn svc() -> ContentService {
        ContentService::new(
            Arc::new(InMemoryContentRepo::new()),
            Arc::new(InMemoryObjectRepo::new()),
            Arc::new(InMemoryObjectStore::new()),
            "memory",
        )
    }

    fn upload_input(data: &'static [u8]) -> UploadContentInput {
        UploadContentInput {
            tenant_id: Uuid::now_v7(),
            owner_id: Uuid::now_v7(),
            owner_type: None,
            name: Some("doc".to_owned()),
            description: None,
            document_type: None,
            object_key: None,
            file_name: Some("doc.txt".to_owned()),
            mime_type: Some("text/plain".to_owned()),
            tags: vec!["a".to_owned()],
            custom_metadata: None,
            data: Bytes::from_static(data),
        }
    }

    /// ObjectStore 无默认:builder 未设就 build → panic(wiring 错误启动期即暴露)。
    #[test]
    #[should_panic(expected = "store")]
    fn builder_without_store_panics() {
        let _ = ContentService::builder(
            Arc::new(InMemoryContentRepo::new()),
            Arc::new(InMemoryObjectRepo::new()),
        )
        .build();
    }

    /// 上传→下载往返:upload 后 content/object 皆 Uploaded,download 取回原字节;元数据已同步。
    #[tokio::test]
    async fn upload_then_download_round_trip() {
        let svc = svc();
        let out = svc
            .upload_content(upload_input(b"hello content"), Some("sys".into()))
            .await
            .unwrap();
        assert_eq!(out.content.status, ContentStatus::Uploaded);
        assert_eq!(out.object.status, ObjectStatus::Uploaded);

        let bytes = svc.download_content(out.content.id).await.unwrap();
        assert_eq!(&bytes[..], b"hello content");

        // content_metadata 已由步骤 7 同步(size 来自后端、tags 透传)。
        let meta = svc.get_content_metadata(out.content.id).await.unwrap();
        assert_eq!(meta.file_size, Some(13));
        assert_eq!(meta.tags, vec!["a".to_string()]);
    }

    /// 新建(未上传)内容不可下载 → `NotReady`(状态守卫)。
    #[tokio::test]
    async fn download_before_upload_is_not_ready() {
        let svc = svc();
        let c = svc
            .create_content(
                CreateContentInput {
                    tenant_id: Uuid::now_v7(),
                    owner_id: Uuid::now_v7(),
                    owner_type: None,
                    name: None,
                    description: None,
                    document_type: None,
                    derivation_type: None,
                },
                None,
            )
            .await
            .unwrap();
        assert!(matches!(
            svc.download_content(c.id).await,
            Err(ContentError::NotReady(_))
        ));
    }
}
