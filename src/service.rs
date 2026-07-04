//! content 业务。持 content/object 仓储端口 + ObjectStore blob 端口 + clock,编排 CRUD 与上传/下载。
//! 范式同分层 service:依赖 trait 而非实现,在此做编排/审计下传。
//! **入参是领域结构(`input` 模块),已由 app 在 HTTP 边界校验完**;审计主体经 `by: Option<String>` 传。

use std::sync::Arc;

use bytes::Bytes;
use uuid::Uuid;

use crate::clock::{Clock, SystemClock};
use crate::error::ContentError;
use crate::input::{
    CreateContentInput, PrepareUploadInput, SetContentMetadataInput, UpdateContentInput,
    UploadContentInput,
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

/// 展示用表示:字节 + **怎么展示它的说明书**。
/// 前端/HTTP 边界按 `metadata.mime_type` 决定渲染(<img>/<video>/pdf)与 Content-Type;
/// `download_content` 保持裸 Bytes(存盘语义,要元数据自己查)—— 两者的又一领域区别。
pub struct Preview {
    pub data: Bytes,
    /// 未同步过元数据 → `None`,调用方兜底(application/octet-stream)。
    pub metadata: Option<ContentMetadata>,
}

/// 两步上传第一步的结果:账已建、格已占、凭证已签(`None` = 后端不支持 → 调用方回退一步上传)。
pub struct PrepareOutcome {
    pub content: Content,
    pub object: Object,
    pub upload_url: Option<String>,
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

    /// 两步上传①:建 content 行 + object 行(皆 Created)+ **用户声明的元数据先写** +
    /// 签直传凭证。字节由调用方拿凭证 PUT 后端,传完调 [`Self::confirm_upload`] 销账。
    /// 一致性告知(同一步上传):行先于字节;拿了凭证不传 → 留 Created 孤儿行(清理 DEFER)。
    pub async fn prepare_upload(
        &self,
        input: PrepareUploadInput,
        by: Option<String>,
    ) -> Result<PrepareOutcome, ContentError> {
        // 1) content 行(同一步上传步骤 1)。
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
        // 2) object 行(同步骤 2)。
        let object_key = input
            .object_key
            .unwrap_or_else(|| format!("{}/{}", content.id, Uuid::now_v7()));
        let object = self
            .inner
            .objects
            .create(
                NewObject {
                    content_id: content.id,
                    storage_backend_name: self.inner.default_backend_name.clone(),
                    storage_class: None,
                    object_key: object_key.clone(),
                    file_name: input.file_name.clone(),
                    object_type: None,
                },
                by,
            )
            .await?;
        // 3) 用户声明的元数据 prepare 时即写(不等销账就可查);size/etag 留给 confirm 从后端补。
        let now = self.inner.clock.now();
        self.inner
            .contents
            .set_metadata(ContentMetadata {
                content_id: content.id,
                tags: input.tags,
                file_size: None,
                file_name: input.file_name,
                mime_type: input.mime_type.clone(),
                checksum: None,
                checksum_algorithm: None,
                metadata: input
                    .custom_metadata
                    .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new())),
                created_at: now,
                updated_at: now,
            })
            .await?;
        // 4) 签凭证(None 透传 = 回退一步上传的信号)。
        let upload_url = self
            .inner
            .store
            .upload_url(&object_key, input.mime_type.as_deref())
            .await?;
        Ok(PrepareOutcome {
            content,
            object,
            upload_url,
        })
    }

    /// 两步上传③(**不可省**,与 Go 原版 Optional 的刻意差异):本库读闸严格
    /// (Created 状态 preview/download → NotReady),不销账 = 传上去的字节永远读不出。
    /// **幂等**:已 Uploaded 再调直接返回(客户端网络重试安全)。
    pub async fn confirm_upload(
        &self,
        content_id: Uuid,
        by: Option<String>,
    ) -> Result<Content, ContentError> {
        let content = self.inner.contents.get(content_id).await?;
        // 幂等 early-return 覆盖**全部已过账状态**(同 can_download_content 的集合):
        // 只查 Uploaded 会让迟到的 confirm 重试把 Processed/Archived 回卷成 Uploaded —— 状态机倒车。
        if matches!(
            content.status,
            ContentStatus::Uploaded | ContentStatus::Processed | ContentStatus::Archived
        ) {
            return Ok(content);
        }
        // 找 prepare 占的格子。也接受 Uploaded:上次 confirm 在"object 已翻、content 未翻"
        // 之间崩掉,重试要能续走(整个方法幂等到崩溃恢复,不只幂等到成功重放)。
        let objects = self.inner.objects.list_by_content(content_id).await?;
        let target = objects
            .into_iter()
            .find(|o| matches!(o.status, ObjectStatus::Created | ObjectStatus::Uploaded))
            .ok_or(ContentError::NotFound)?;
        // 核对字节真的落桶了 —— 没传就来销账 → NotReady(app 映射 409)。
        let store_meta = self
            .inner
            .store
            .object_meta(&target.object_key)
            .await
            .map_err(|_| {
                ContentError::NotReady("bytes not found in backend, upload first".to_owned())
            })?;
        // 翻账(镜像一步上传步骤 4-8:object 致命 → 元数据非致命 → content 致命)。
        self.inner
            .objects
            .set_status(target.id, ObjectStatus::Uploaded, by.clone())
            .await?;
        let now = self.inner.clock.now();
        let _ = self
            .inner
            .objects
            .set_metadata(ObjectMetadata {
                object_id: target.id,
                size_bytes: Some(store_meta.size),
                mime_type: store_meta.content_type.clone(),
                etag: store_meta.etag.clone(),
                metadata: serde_json::Value::Object(serde_json::Map::new()),
                created_at: now,
                updated_at: now,
            })
            .await;
        // content_metadata 只补 size(prepare 写的 tags/mime/file_name/自由 jsonb 保留)。
        if let Ok(mut meta) = self.inner.contents.get_metadata(content_id).await {
            meta.file_size = Some(store_meta.size);
            meta.updated_at = now;
            let _ = self.inner.contents.set_metadata(meta).await;
        }
        self.inner
            .contents
            .set_status(content_id, ContentStatus::Uploaded, by)
            .await?;
        self.inner.contents.get(content_id).await
    }

    /// 下载内容主对象的字节(Go `DownloadContent` 流)。
    /// 内容不存在 → `NotFound`;状态不允许下载 → `NotReady`;无已上传对象 → `NotFound`。
    pub async fn download_content(&self, content_id: Uuid) -> Result<Bytes, ContentError> {
        let target = self.primary_object(content_id).await?;
        self.inner.store.download(&target.object_key).await
    }

    /// 预览内容(展示用):字节 + 元数据说明书。状态闸与对象选择**同 download**
    /// ("可展示"=="可下载",同一生命周期约束)。
    /// DEFER:派生编排落地后,这里优先取 variant="preview" 的派生对象(缩略图/poster),
    /// 签名不变、调用方零改动 —— 签名就是为那天设计的。
    pub async fn preview_content(&self, content_id: Uuid) -> Result<Preview, ContentError> {
        let target = self.primary_object(content_id).await?;
        let data = self.inner.store.download(&target.object_key).await?;
        // 元数据缺失(未同步)容忍为 None;其余错误上抛。
        let metadata = match self.inner.contents.get_metadata(content_id).await {
            Ok(m) => Some(m),
            Err(ContentError::NotFound) => None,
            Err(e) => return Err(e),
        };
        Ok(Preview { data, metadata })
    }

    /// 预签名预览 URL(inline)。`Ok(None)` = 后端不支持 → 调用方回退字节代理(`preview_content`)。
    pub async fn preview_url(&self, content_id: Uuid) -> Result<Option<String>, ContentError> {
        let target = self.primary_object(content_id).await?;
        self.inner.store.preview_url(&target.object_key).await
    }

    /// 预签名下载 URL(attachment)。`Ok(None)` 同上回退。
    /// filename **优先 content_metadata**(经 set_content_metadata 可编辑 —— 用户改名后签新名),
    /// 回退 object 行(建行时的原始名,之后不可变)。
    pub async fn download_url(&self, content_id: Uuid) -> Result<Option<String>, ContentError> {
        let target = self.primary_object(content_id).await?;
        let meta_name = match self.inner.contents.get_metadata(content_id).await {
            Ok(m) => m.file_name,
            Err(ContentError::NotFound) => None,
            Err(e) => return Err(e),
        };
        let filename = meta_name.or(target.file_name);
        self.inner
            .store
            .download_url(&target.object_key, filename.as_deref())
            .await
    }

    /// 状态闸 + 主对象选择(download / preview / URL 三路共用,一处收口):
    /// 内容存在 → 状态 ∈ {Uploaded, Processed, Archived} → 第一个 Uploaded 对象。
    async fn primary_object(&self, content_id: Uuid) -> Result<Object, ContentError> {
        let content = self.inner.contents.get(content_id).await?;
        can_download_content(content.status)?;
        let objects = self.inner.objects.list_by_content(content_id).await?;
        objects
            .into_iter()
            .find(|o| o.status == ObjectStatus::Uploaded)
            .ok_or(ContentError::NotFound)
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

    fn prepare_input() -> PrepareUploadInput {
        PrepareUploadInput {
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
    /// URL 是可选能力:memory 后端不支持 presign → Ok(None)(非错误;调用方回退字节代理)。
    #[tokio::test]
    async fn memory_store_presign_urls_are_none() {
        let store = InMemoryObjectStore::new();
        assert!(store.upload_url("k", None).await.unwrap().is_none());
        assert!(store
            .download_url("k", Some("f.txt"))
            .await
            .unwrap()
            .is_none());
        assert!(store.preview_url("k").await.unwrap().is_none());
    }

    /// preview = 字节 + 展示说明书:字节同 download,metadata 里 mime/file_name 可直接填 HTTP 头。
    #[tokio::test]
    async fn preview_returns_bytes_with_metadata() {
        let svc = svc();
        let out = svc
            .upload_content(upload_input(b"img-bytes"), Some("sys".into()))
            .await
            .unwrap();
        let p = svc.preview_content(out.content.id).await.unwrap();
        assert_eq!(&p.data[..], b"img-bytes");
        let meta = p.metadata.expect("upload 已同步元数据");
        assert_eq!(meta.mime_type.as_deref(), Some("text/plain"));
        assert_eq!(meta.file_name.as_deref(), Some("doc.txt"));
    }

    /// 状态闸与 download 同一把:未上传(created)→ NotReady;不存在 → NotFound。
    #[tokio::test]
    async fn preview_guards_match_download() {
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
            svc.preview_content(c.id).await,
            Err(ContentError::NotReady(_))
        ));
        assert!(matches!(
            svc.preview_content(Uuid::now_v7()).await,
            Err(ContentError::NotFound)
        ));
    }

    /// URL 方法:状态闸照常;memory 后端 → Ok(None)(回退代理的判别信号,非错误)。
    #[tokio::test]
    async fn presign_urls_none_on_memory_backend() {
        let svc = svc();
        let out = svc.upload_content(upload_input(b"x"), None).await.unwrap();
        assert!(svc.preview_url(out.content.id).await.unwrap().is_none());
        assert!(svc.download_url(out.content.id).await.unwrap().is_none());
        // 闸照常:不存在 → NotFound(不是 None)
        assert!(matches!(
            svc.preview_url(Uuid::now_v7()).await,
            Err(ContentError::NotFound)
        ));
    }

    /// 契约测试用 store:覆写 URL 方法(字节走内存实现)—— 钉住 Some 回传 + filename 透传。
    struct FixedUrlStore(InMemoryObjectStore);

    #[async_trait::async_trait]
    impl crate::store::ObjectStore for FixedUrlStore {
        async fn upload(&self, params: UploadParams, data: Bytes) -> Result<(), ContentError> {
            self.0.upload(params, data).await
        }
        async fn download(&self, object_key: &str) -> Result<Bytes, ContentError> {
            self.0.download(object_key).await
        }
        async fn delete(&self, object_key: &str) -> Result<(), ContentError> {
            self.0.delete(object_key).await
        }
        async fn object_meta(
            &self,
            object_key: &str,
        ) -> Result<crate::store::ObjectMeta, ContentError> {
            self.0.object_meta(object_key).await
        }
        async fn download_url(
            &self,
            object_key: &str,
            download_filename: Option<&str>,
        ) -> Result<Option<String>, ContentError> {
            Ok(Some(format!(
                "https://cdn.test/{object_key}?dl={}",
                download_filename.unwrap_or("-")
            )))
        }
        async fn preview_url(&self, object_key: &str) -> Result<Option<String>, ContentError> {
            Ok(Some(format!("https://cdn.test/{object_key}?inline")))
        }
    }

    /// URL 后端支持 presign 时:Some 原样回传;download filename **优先可编辑的 content_metadata**
    /// (用户改名后签新名),preview 无 filename 参与。
    #[tokio::test]
    async fn presign_urls_pass_through_and_prefer_metadata_filename() {
        let svc = ContentService::new(
            Arc::new(InMemoryContentRepo::new()),
            Arc::new(InMemoryObjectRepo::new()),
            Arc::new(FixedUrlStore(InMemoryObjectStore::new())),
            "cdn",
        );
        let out = svc.upload_content(upload_input(b"x"), None).await.unwrap();
        // 上传时的原始名先生效(metadata 由 upload 同步,file_name = doc.txt)
        let url = svc.download_url(out.content.id).await.unwrap().unwrap();
        assert!(url.contains("?dl=doc.txt"), "{url}");
        // 用户经 set_content_metadata 改名 → 下载签新名(metadata 是可编辑源,优先于 object 行)
        svc.set_content_metadata(SetContentMetadataInput {
            content_id: out.content.id,
            tags: vec![],
            file_size: None,
            file_name: Some("renamed.txt".to_owned()),
            mime_type: None,
            checksum: None,
            checksum_algorithm: None,
            metadata: serde_json::Value::Null,
        })
        .await
        .unwrap();
        let url = svc.download_url(out.content.id).await.unwrap().unwrap();
        assert!(url.contains("?dl=renamed.txt"), "{url}");
        // preview:inline,无 filename
        let url = svc.preview_url(out.content.id).await.unwrap().unwrap();
        assert!(url.ends_with("?inline"), "{url}");
    }

    /// metadata 未同步(直接驱动仓储造"已上传但无元数据")→ preview 容忍为 None,不报错。
    #[tokio::test]
    async fn preview_without_metadata_degrades_to_none() {
        let contents = Arc::new(InMemoryContentRepo::new());
        let objects = Arc::new(InMemoryObjectRepo::new());
        let store = Arc::new(InMemoryObjectStore::new());
        let svc = ContentService::new(contents.clone(), objects.clone(), store.clone(), "memory");
        // 手工铺状态:content 行 + object 行 + 字节,全程不写 content_metadata。
        let c = contents
            .create(
                NewContent {
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
        let o = objects
            .create(
                NewObject {
                    content_id: c.id,
                    storage_backend_name: "memory".to_owned(),
                    storage_class: None,
                    object_key: "k1".to_owned(),
                    file_name: None,
                    object_type: None,
                },
                None,
            )
            .await
            .unwrap();
        store
            .upload(
                UploadParams {
                    object_key: "k1".to_owned(),
                    mime_type: None,
                    file_name: None,
                },
                Bytes::from_static(b"raw"),
            )
            .await
            .unwrap();
        objects
            .set_status(o.id, ObjectStatus::Uploaded, None)
            .await
            .unwrap();
        contents
            .set_status(c.id, ContentStatus::Uploaded, None)
            .await
            .unwrap();

        let p = svc.preview_content(c.id).await.unwrap();
        assert_eq!(&p.data[..], b"raw");
        assert!(p.metadata.is_none(), "未同步过元数据应容忍为 None");
    }

    /// prepare(memory):账建了、格占了、凭证 None(回退信号);用户声明的元数据不等销账就可查。
    #[tokio::test]
    async fn prepare_creates_rows_and_metadata_without_url_on_memory() {
        let svc = svc();
        let out = svc.prepare_upload(prepare_input(), None).await.unwrap();
        assert_eq!(out.content.status, ContentStatus::Created);
        assert_eq!(out.object.status, ObjectStatus::Created);
        assert!(out.upload_url.is_none(), "memory 后端签不出凭证");
        let meta = svc.get_content_metadata(out.content.id).await.unwrap();
        assert_eq!(meta.tags, vec!["a".to_string()]);
        assert_eq!(meta.mime_type.as_deref(), Some("text/plain"));
        assert!(meta.file_size.is_none(), "size 留给 confirm 从后端补");
    }

    /// 后端支持时:凭证 Some 且 mime 已传给 store(签进凭证的前提)。
    #[tokio::test]
    async fn prepare_carries_upload_url_when_backend_supports() {
        let svc = ContentService::new(
            Arc::new(InMemoryContentRepo::new()),
            Arc::new(InMemoryObjectRepo::new()),
            Arc::new(UrlStore(InMemoryObjectStore::new())),
            "cdn",
        );
        let out = svc.prepare_upload(prepare_input(), None).await.unwrap();
        let url = out.upload_url.expect("该后端支持 presign");
        assert!(url.contains(&out.object.object_key), "{url}");
        assert!(url.contains("mime=text/plain"), "mime 应传到 store: {url}");
    }

    /// 契约测试用 store:upload_url 回显 key+mime,验证 prepare 把参数透传到端口。
    struct UrlStore(InMemoryObjectStore);

    #[async_trait::async_trait]
    impl crate::store::ObjectStore for UrlStore {
        async fn upload(&self, p: UploadParams, d: Bytes) -> Result<(), ContentError> {
            self.0.upload(p, d).await
        }
        async fn download(&self, k: &str) -> Result<Bytes, ContentError> {
            self.0.download(k).await
        }
        async fn delete(&self, k: &str) -> Result<(), ContentError> {
            self.0.delete(k).await
        }
        async fn object_meta(&self, k: &str) -> Result<crate::store::ObjectMeta, ContentError> {
            self.0.object_meta(k).await
        }
        async fn upload_url(
            &self,
            key: &str,
            mime: Option<&str>,
        ) -> Result<Option<String>, ContentError> {
            Ok(Some(format!(
                "https://cdn.test/put/{key}?mime={}",
                mime.unwrap_or("-")
            )))
        }
    }

    /// 两步全流程:prepare → 模拟客户端 PUT(直写 store)→ confirm → 可下载,size 已补。
    #[tokio::test]
    async fn prepare_then_confirm_round_trip() {
        let contents = Arc::new(InMemoryContentRepo::new());
        let objects = Arc::new(InMemoryObjectRepo::new());
        let store = Arc::new(InMemoryObjectStore::new());
        let svc = ContentService::new(contents, objects, store.clone(), "memory");
        let out = svc.prepare_upload(prepare_input(), None).await.unwrap();
        // 模拟第二步:客户端拿凭证 PUT(这里直写同一个 store)。
        store
            .upload(
                UploadParams {
                    object_key: out.object.object_key.clone(),
                    mime_type: Some("text/plain".to_owned()),
                    file_name: None,
                },
                Bytes::from_static(b"two-step bytes"),
            )
            .await
            .unwrap();
        let c = svc.confirm_upload(out.content.id, None).await.unwrap();
        assert_eq!(c.status, ContentStatus::Uploaded);
        let meta = svc.get_content_metadata(c.id).await.unwrap();
        assert_eq!(meta.file_size, Some(14), "size 由 confirm 从后端补");
        assert_eq!(meta.tags, vec!["a".to_string()], "prepare 写的 tags 不丢");
        let bytes = svc.download_content(c.id).await.unwrap();
        assert_eq!(&bytes[..], b"two-step bytes");
    }

    /// 没传字节就销账 → NotReady(app 映射 409);状态不动。
    #[tokio::test]
    async fn confirm_before_put_is_not_ready() {
        let svc = svc();
        let out = svc.prepare_upload(prepare_input(), None).await.unwrap();
        assert!(matches!(
            svc.confirm_upload(out.content.id, None).await,
            Err(ContentError::NotReady(_))
        ));
        assert_eq!(
            svc.get_content(out.content.id).await.unwrap().status,
            ContentStatus::Created
        );
    }

    /// confirm 幂等:重试(网络抖动)不报错、状态不变。
    #[tokio::test]
    async fn confirm_is_idempotent() {
        let contents = Arc::new(InMemoryContentRepo::new());
        let objects = Arc::new(InMemoryObjectRepo::new());
        let store = Arc::new(InMemoryObjectStore::new());
        let svc = ContentService::new(contents, objects, store.clone(), "memory");
        let out = svc.prepare_upload(prepare_input(), None).await.unwrap();
        store
            .upload(
                UploadParams {
                    object_key: out.object.object_key.clone(),
                    mime_type: None,
                    file_name: None,
                },
                Bytes::from_static(b"x"),
            )
            .await
            .unwrap();
        svc.confirm_upload(out.content.id, None).await.unwrap();
        let again = svc.confirm_upload(out.content.id, None).await.unwrap();
        assert_eq!(again.status, ContentStatus::Uploaded);
    }

    /// 崩溃恢复:上次 confirm 在"object 已翻、content 未翻"之间挂掉 → 重试要能续走。
    /// (直驱 repo 制造该状态;删掉 confirm 里 `| ObjectStatus::Uploaded` 这条匹配,本测试必红。)
    #[tokio::test]
    async fn confirm_resumes_after_crash_between_flips() {
        let contents = Arc::new(InMemoryContentRepo::new());
        let objects = Arc::new(InMemoryObjectRepo::new());
        let store = Arc::new(InMemoryObjectStore::new());
        let svc = ContentService::new(contents.clone(), objects.clone(), store.clone(), "memory");
        let out = svc.prepare_upload(prepare_input(), None).await.unwrap();
        store
            .upload(
                UploadParams {
                    object_key: out.object.object_key.clone(),
                    mime_type: None,
                    file_name: None,
                },
                Bytes::from_static(b"x"),
            )
            .await
            .unwrap();
        // 模拟半程崩溃:object 翻完、content 停在 Created。
        objects
            .set_status(out.object.id, ObjectStatus::Uploaded, None)
            .await
            .unwrap();
        let c = svc.confirm_upload(out.content.id, None).await.unwrap();
        assert_eq!(c.status, ContentStatus::Uploaded, "重试应续走完账");
    }

    /// 迟到的 confirm 重试不回卷状态:Processed/Archived 原样返回(状态机不倒车)。
    #[tokio::test]
    async fn confirm_never_rewinds_processed_or_archived() {
        let contents = Arc::new(InMemoryContentRepo::new());
        let objects = Arc::new(InMemoryObjectRepo::new());
        let store = Arc::new(InMemoryObjectStore::new());
        let svc = ContentService::new(contents.clone(), objects.clone(), store.clone(), "memory");
        let out = svc.prepare_upload(prepare_input(), None).await.unwrap();
        store
            .upload(
                UploadParams {
                    object_key: out.object.object_key.clone(),
                    mime_type: None,
                    file_name: None,
                },
                Bytes::from_static(b"x"),
            )
            .await
            .unwrap();
        svc.confirm_upload(out.content.id, None).await.unwrap();
        svc.set_content_status(out.content.id, ContentStatus::Archived, None)
            .await
            .unwrap();
        let c = svc.confirm_upload(out.content.id, None).await.unwrap();
        assert_eq!(
            c.status,
            ContentStatus::Archived,
            "confirm 重试不得回卷已推进的状态"
        );
    }

    /// 没有可销账对象(格子从未占/已删)→ NotFound。
    #[tokio::test]
    async fn confirm_without_object_is_not_found() {
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
            svc.confirm_upload(c.id, None).await,
            Err(ContentError::NotFound)
        ));
    }
}
