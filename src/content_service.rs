//! content 业务:[`ContentService`] 持 content/object 仓储端口 + ObjectStore blob 端口 + 时钟,编排 CRUD 与上传/下载。
//! 范式同分层 service:依赖 trait 而非实现,在此做编排/审计下传。
//! **入参是领域结构(`input` 模块),已由 app 在 HTTP 边界校验完**;审计主体经 `by: Option<&str>` 传。
//!
//! **作用域**:每个公开方法第一个业务参数是 `scope: ContentScope`(租户 / 个人),原样下传仓储 ——
//! 按 ID 取单条时别的作用域的内容一律 `NotFound`。新对象 key 也按作用域生成(见 [`mint_object_key`])。
//!
//! **端口是类型参数而不是 `Arc<dyn …>`**(指南「Generics versus `dyn Trait`」):服务独占它的端口
//! (指南「Avoid `Rc` and `Arc`」)。下游要在运行时二选一实现时,用端口 trait 给 `Arc<T>` / `Box<T>`
//! 的转发实现,写成 `ContentService<Arc<dyn ContentRepo>, Arc<dyn ObjectRepo>, Arc<dyn ObjectStore>>`。
//! 四个类型参数(其中时钟有默认值)是端口个数决定的,不再合并。
//!
//! **时间戳由服务定**:每个写用例经注入的 [`Clock`] 取一次 now(一个用例里的多次写共用它),传给仓储
//! (`NewContent` / `NewObject` 的 `created_at`、`update` / `set_status` / `soft_delete` 的 `at`、元数据结构的时间戳);
//! 仓储按传入值落库。注入固定时钟,读回来的时间戳就是它。

use bytes::Bytes;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::clock::{Clock, SystemClock};
use crate::content_error::ContentError;
use crate::content_scope::ContentScope;
use crate::content_status::ContentStatus;
use crate::input::{
    CreateContentInput, PrepareUploadInput, SetContentMetadataInput, UpdateContentInput,
    UploadContentInput,
};
use crate::object_status::ObjectStatus;
use crate::object_store::{ObjectStore, UploadRequest};
use crate::repo::{ContentRepo, NewContent, NewObject, ObjectRepo};
use crate::types::{Content, ContentMetadata, Object, ObjectMetadata};

/// 一次性上传的结果(**纯数据**):内容 + 其主对象。app 映射成自己的对外响应 DTO。
/// (类比 idm 的 `AuthOutcome`。)
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct UploadOutcome {
    /// 已是 `Uploaded` 的内容。
    pub content: Content,
    /// 已是 `Uploaded` 的主对象。
    pub object: Object,
}

/// 展示用表示:字节 + **怎么展示它的说明书**。
/// 前端/HTTP 边界按 `metadata.mime_type` 决定渲染(`<img>` / `<video>` / pdf)与 Content-Type;
/// `download_content` 保持裸 Bytes(存盘语义,要元数据自己查)—— 两者的又一领域区别。
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Preview {
    /// 主对象的字节。
    pub data: Bytes,
    /// 未同步过元数据 → `None`,调用方兜底(application/octet-stream)。
    pub metadata: Option<ContentMetadata>,
}

/// 两步上传第一步的结果:账已建、格已占、凭证已签。
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct PrepareOutcome {
    /// 新建的内容(`Created`)。
    pub content: Content,
    /// 占好位置的对象(`Created`),客户端往它的 `object_key` 直传。
    pub object: Object,
    /// 直传凭证;`None` = 后端不支持 → 调用方回退一步上传。
    pub upload_url: Option<String>,
}

/// 默认后端名(配合库内 `InMemoryObjectStore`)。
const DEFAULT_BACKEND_NAME: &str = "memory";

/// 服务生成的对象 key(调用方没给 `object_key` 时):
/// 租户内容 `tenants/{tenant_id}/{content_id}/{uuid}`,个人内容 `personal/{owner_id}/{content_id}/{uuid}`。
/// 前缀让存储后端按租户 / 个人分目录(便于按前缀清理、配额、审计);已有对象的 key 存在库里,不搬家。
fn mint_object_key(scope: ContentScope, content: &Content) -> String {
    let leaf = Uuid::now_v7();
    let prefix = object_key_prefix(scope, content.owner_id);
    format!("{prefix}{}/{leaf}", content.id)
}

/// 本作用域的对象 key 前缀(带结尾 `/`):租户 `tenants/{tenant_id}/`,个人 `personal/{owner_id}/`。
/// 生成和校验调用方显式给的 key 共用这一处。
fn object_key_prefix(scope: ContentScope, owner_id: Uuid) -> String {
    match scope {
        ContentScope::Tenant(tenant_id) => format!("tenants/{tenant_id}/"),
        ContentScope::Personal => format!("personal/{owner_id}/"),
    }
}

/// 调用方显式给的 `object_key`:必须落在本作用域前缀下([`object_key_prefix`]),前缀后不能为空,
/// 也不能有 `..` 段(按 `/` 和 `\\` 切;防文件系统类后端把 `tenants/{A}/../{B}/x` 解析出前缀)。
/// 否则 → `InvalidInput`。不校验的话,租户 A 传 `tenants/{B}/…` 就能把字节写进 / 覆盖 B 的目录。
fn check_object_key(scope: ContentScope, owner_id: Uuid, key: &str) -> Result<(), ContentError> {
    let prefix = object_key_prefix(scope, owner_id);
    let rest = key.strip_prefix(prefix.as_str()).ok_or_else(|| {
        ContentError::InvalidInput(format!("object_key must start with \"{prefix}\""))
    })?;
    if rest.is_empty() || rest.split(['/', '\\']).any(|segment| segment == "..") {
        return Err(ContentError::InvalidInput(format!(
            "object_key must be a non-empty path under \"{prefix}\" without \"..\" segments"
        )));
    }
    Ok(())
}

/// 空的自由表单 JSONB(`{}`)。
fn empty_json_object() -> Value {
    Value::Object(Map::new())
}

/// 内容服务。
///
/// 类型参数:`C` 内容仓储、`O` 对象仓储、`S` 对象存储、`K` 时钟(clock;`C` 已被内容仓储占用,默认
/// [`SystemClock`])。端口都是独占持有的;`Clone` 在各端口都能 `Clone` 时可用 —— 端口填 `Arc<…>` 时
/// 克隆就很便宜,适合放进 app 的共享状态。
///
/// # Examples
///
/// ```
/// use bytes::Bytes;
/// use content::{
///     ContentScope, ContentService, InMemoryContentRepo, InMemoryObjectRepo, InMemoryObjectStore,
///     UploadContentInput,
/// };
/// use uuid::Uuid;
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> Result<(), content::ContentError> {
/// let service = ContentService::new(
///     InMemoryContentRepo::new(),
///     InMemoryObjectRepo::new(),
///     InMemoryObjectStore::new(), // 生产:换成自己的 minio / S3 实现
///     "memory",
/// );
/// let scope = ContentScope::try_new_tenant(Uuid::now_v7())?;
/// let mut input = UploadContentInput::new(Uuid::now_v7(), Bytes::from_static(b"hello"));
/// input.file_name = Some("hello.txt".to_owned());
/// let outcome = service.upload_content(scope, input, Some("alice")).await?;
/// let bytes = service.download_content(scope, outcome.content.id).await?;
/// assert_eq!(&bytes[..], b"hello");
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct ContentService<C, O, S, K = SystemClock> {
    contents: C,
    objects: O,
    store: S,
    backend_name: String,
    clock: K,
}

impl<C: ContentRepo, O: ObjectRepo, S: ObjectStore> ContentService<C, O, S> {
    /// **便捷构造**:给齐两个仓储 + 一个 ObjectStore + 写进对象行的后端名(系统时钟)。
    /// 想换时钟 → 用 [`ContentService::builder`]。
    #[must_use]
    pub fn new(contents: C, objects: O, store: S, backend_name: impl Into<String>) -> Self {
        Self::builder(contents, objects, store)
            .backend_name(backend_name)
            .build()
    }

    /// **builder**:三个端口必填,其余取默认(`SystemClock` / 后端名 `"memory"`),再按需覆盖。
    /// ObjectStore 没有安全的默认值,所以和仓储一样在这里给出。
    pub fn builder(contents: C, objects: O, store: S) -> ContentServiceBuilder<C, O, S> {
        ContentServiceBuilder {
            contents,
            objects,
            store,
            backend_name: DEFAULT_BACKEND_NAME.to_owned(),
            clock: SystemClock,
        }
    }
}

impl<C: ContentRepo, O: ObjectRepo, S: ObjectStore, K: Clock> ContentService<C, O, S, K> {
    /// 建内容(仅 content 行,status=Created)。`by` = 审计主体。
    ///
    /// # Errors
    ///
    /// 仓储错误原样上抛(nil 租户 → `InvalidInput`,库错误 → `Internal`)。
    pub async fn create_content(
        &self,
        scope: ContentScope,
        input: CreateContentInput,
        by: Option<&str>,
    ) -> Result<Content, ContentError> {
        let new_content = NewContent {
            owner_id: input.owner_id,
            owner_type: input.owner_type,
            name: input.name,
            description: input.description,
            document_type: input.document_type,
            derivation_type: input
                .derivation_type
                .or_else(|| Some("original".to_owned())),
            created_at: self.clock.now(),
        };
        self.contents.create(scope, new_content, by).await
    }

    /// 查内容。(owner 级鉴权仍归 app 授权层。)
    ///
    /// # Errors
    ///
    /// 不存在 / 已软删 / 别的作用域 → `NotFound`。
    pub async fn get_content(
        &self,
        scope: ContentScope,
        id: Uuid,
    ) -> Result<Content, ContentError> {
        self.contents.get(scope, id).await
    }

    /// **全量更新**内容可编辑字段(PUT)。先查存活再替换。
    ///
    /// # Errors
    ///
    /// 不存在 / 已软删 / 别的作用域 → `NotFound`。
    pub async fn update_content(
        &self,
        scope: ContentScope,
        id: Uuid,
        input: UpdateContentInput,
        by: Option<&str>,
    ) -> Result<Content, ContentError> {
        let existing = self.contents.get(scope, id).await?;
        let updated = Content {
            owner_type: input.owner_type,
            name: input.name,
            description: input.description,
            document_type: input.document_type,
            ..existing
        };
        self.contents
            .update(scope, &updated, self.clock.now(), by)
            .await
    }

    /// 软删内容。对象的清理是 DEFER(暂不级联软删 object/字节)。
    ///
    /// # Errors
    ///
    /// 已删 / 不存在 / 别的作用域 → `NotFound`。
    pub async fn delete_content(
        &self,
        scope: ContentScope,
        id: Uuid,
        by: Option<&str>,
    ) -> Result<(), ContentError> {
        self.contents
            .soft_delete(scope, id, self.clock.now(), by)
            .await
    }

    /// 列本作用域里某 owner 的存活内容(按 id desc)。个人作用域同样按 owner 过滤(只回自己的)。
    /// 返回 `Vec`:仓储从数据库取回的就是 `Vec`,原样交出(指南「Iterators vs. owned collections」的例外)。
    ///
    /// # Errors
    ///
    /// 仓储错误原样上抛。
    pub async fn list_content(
        &self,
        scope: ContentScope,
        owner_id: Uuid,
    ) -> Result<Vec<Content>, ContentError> {
        self.contents.list(scope, owner_id).await
    }

    /// 一次性上传(Go `UploadContent` 流):建 content + object 行 → 推字节 → 翻状态 → 同步元数据。
    ///
    /// **顺序与容错(对齐 Go 版)**:行先于字节;步骤 5–7(取后端元数据 / 写 object_metadata /
    /// 写 content_metadata)**非致命**(失败仅忽略,对象已是 Uploaded);步骤 4(object→Uploaded)与
    /// 步骤 8(content→Uploaded)致命。
    ///
    /// **一致性告知(有意接受,无跨 repo 事务,见 repo 模块文档)**:content 行与 object 行先于存储写建立;
    /// 若步骤 3 的 `ObjectStore::upload` 在行已存在后失败,则留下**孤儿行**(无补偿删除);若步骤 3 成功而
    /// 4/8 失败,则字节滞留。原子单元是单个 trait 方法,不在此跨两个仓储包一个 tx。
    ///
    /// # Errors
    ///
    /// 显式给的 `object_key` 不在本作用域前缀下 → `InvalidInput`(不建行);
    /// key 撞存活对象 → `Conflict`;存储失败 → `Storage`;其余仓储错误原样上抛。
    pub async fn upload_content(
        &self,
        scope: ContentScope,
        input: UploadContentInput,
        by: Option<&str>,
    ) -> Result<UploadOutcome, ContentError> {
        // 步骤 0:显式给的 object_key 必须在本作用域前缀下(建行之前校验,拒了不留孤儿行)。
        if let Some(key) = &input.object_key {
            check_object_key(scope, input.owner_id, key)?;
        }
        // 本用例的所有写共用一个 now(行、状态、元数据的时间戳一致)。
        let now = self.clock.now();
        // 步骤 1:建 content 行(status=Created)。
        let new_content = NewContent {
            owner_id: input.owner_id,
            owner_type: input.owner_type,
            name: input.name,
            description: input.description,
            document_type: input.document_type,
            derivation_type: Some("original".to_owned()),
            created_at: now,
        };
        let content = self.contents.create(scope, new_content, by).await?;

        // 步骤 2:定 object_key(None → 按作用域生成,见 mint_object_key)+ 建 object 行(默认后端、version=1、Created)。
        let object_key = match input.object_key {
            Some(key) => key,
            None => mint_object_key(scope, &content),
        };
        let new_object = NewObject {
            content_id: content.id,
            storage_backend_name: self.backend_name.clone(),
            storage_class: None,
            object_key: object_key.clone(),
            file_name: input.file_name.clone(),
            object_type: None,
            created_at: now,
        };
        let object = self.objects.create(scope, new_object, by).await?;

        // 步骤 3:推字节到后端。【孤儿行注意】行已建,此步失败则留孤儿(无补偿删除,有意接受)。
        let byte_len = input.data.len() as i64;
        let request = UploadRequest {
            object_key: object_key.clone(),
            mime_type: input.mime_type.clone(),
            file_name: input.file_name.clone(),
        };
        self.store.upload(request, input.data).await?;

        // 步骤 4(致命):object → Uploaded。
        self.objects
            .set_status(scope, object.id, ObjectStatus::Uploaded, now, by)
            .await?;

        // 步骤 5(非致命):读后端元数据(size/etag/content-type)。
        let stored = self.store.metadata(&object_key).await.ok();
        let size = stored.as_ref().map_or(byte_len, |stored| stored.size);
        let mime_type = stored
            .as_ref()
            .and_then(|stored| stored.content_type.clone())
            .or(input.mime_type);

        // 步骤 6(非致命):写 object_metadata(失败仅忽略)。
        let object_metadata = ObjectMetadata {
            object_id: object.id,
            size_bytes: Some(size),
            mime_type: mime_type.clone(),
            etag: stored.and_then(|stored| stored.etag),
            metadata: empty_json_object(),
            created_at: now,
            updated_at: now,
        };
        let _ = self.objects.set_metadata(scope, object_metadata).await;

        // 步骤 7(非致命):写 content_metadata(失败仅忽略)。
        let content_metadata = ContentMetadata {
            content_id: content.id,
            tags: input.tags,
            file_size: Some(size),
            file_name: input.file_name,
            mime_type,
            checksum: None,
            checksum_algorithm: None,
            metadata: input.custom_metadata.unwrap_or_else(empty_json_object),
            created_at: now,
            updated_at: now,
        };
        let _ = self.contents.set_metadata(scope, content_metadata).await;

        // 步骤 8(致命):content → Uploaded(原始内容终态)。
        self.contents
            .set_status(scope, content.id, ContentStatus::Uploaded, now, by)
            .await?;

        // 回读最终态实体(status 已翻 Uploaded),组装结果。
        let content = self.contents.get(scope, content.id).await?;
        let object = self.objects.get(scope, object.id).await?;
        Ok(UploadOutcome { content, object })
    }

    /// 两步上传①:建 content 行 + object 行(皆 Created)+ **用户声明的元数据先写** +
    /// 签直传凭证。字节由调用方拿凭证 PUT 后端,传完调 [`Self::confirm_upload`] 销账。
    /// 一致性告知(同一步上传):行先于字节;拿了凭证不传 → 留 Created 孤儿行(清理 DEFER)。
    ///
    /// # Errors
    ///
    /// 同 [`Self::upload_content`];签凭证真失败 → `Storage`(后端不支持不是错误,`upload_url = None`)。
    pub async fn prepare_upload(
        &self,
        scope: ContentScope,
        input: PrepareUploadInput,
        by: Option<&str>,
    ) -> Result<PrepareOutcome, ContentError> {
        // 0) 显式给的 object_key 必须在本作用域前缀下(同一步上传步骤 0)。
        if let Some(key) = &input.object_key {
            check_object_key(scope, input.owner_id, key)?;
        }
        let now = self.clock.now();
        // 1) content 行(同一步上传步骤 1)。
        let new_content = NewContent {
            owner_id: input.owner_id,
            owner_type: input.owner_type,
            name: input.name,
            description: input.description,
            document_type: input.document_type,
            derivation_type: Some("original".to_owned()),
            created_at: now,
        };
        let content = self.contents.create(scope, new_content, by).await?;
        // 2) object 行(同步骤 2)。
        let object_key = match input.object_key {
            Some(key) => key,
            None => mint_object_key(scope, &content),
        };
        let new_object = NewObject {
            content_id: content.id,
            storage_backend_name: self.backend_name.clone(),
            storage_class: None,
            object_key: object_key.clone(),
            file_name: input.file_name.clone(),
            object_type: None,
            created_at: now,
        };
        let object = self.objects.create(scope, new_object, by).await?;
        // 3) 用户声明的元数据 prepare 时即写(不等销账就可查);size/etag 留给 confirm 从后端补。
        let content_metadata = ContentMetadata {
            content_id: content.id,
            tags: input.tags,
            file_size: None,
            file_name: input.file_name,
            mime_type: input.mime_type.clone(),
            checksum: None,
            checksum_algorithm: None,
            metadata: input.custom_metadata.unwrap_or_else(empty_json_object),
            created_at: now,
            updated_at: now,
        };
        self.contents.set_metadata(scope, content_metadata).await?;
        // 4) 签凭证(None 透传 = 回退一步上传的信号)。
        let upload_url = self
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
    /// **幂等**:已过账(Uploaded / Processed / Archived)再调直接返回(客户端网络重试安全)。
    ///
    /// # Errors
    ///
    /// 内容不存在 / 没有可销账的对象 → `NotFound`;字节还没传到后端 → `NotReady`。
    pub async fn confirm_upload(
        &self,
        scope: ContentScope,
        content_id: Uuid,
        by: Option<&str>,
    ) -> Result<Content, ContentError> {
        let content = self.contents.get(scope, content_id).await?;
        // 幂等 early-return 覆盖**全部已过账状态**(= 可下载的集合):
        // 只查 Uploaded 会让迟到的 confirm 重试把 Processed/Archived 回卷成 Uploaded —— 状态机倒车。
        if content.status.is_downloadable() {
            return Ok(content);
        }
        // 找 prepare 占的格子。也接受 Uploaded:上次 confirm 在"object 已翻、content 未翻"
        // 之间崩掉,重试要能续走(整个方法幂等到崩溃恢复,不只幂等到成功重放)。
        let objects = self.objects.list_by_content(scope, content_id).await?;
        let target = objects
            .into_iter()
            .find(|object| {
                matches!(
                    object.status,
                    ObjectStatus::Created | ObjectStatus::Uploaded
                )
            })
            .ok_or(ContentError::NotFound)?;
        // 核对字节真的落桶了 —— 没传就来销账 → NotReady(app 映射 409)。
        let stored = self.store.metadata(&target.object_key).await.map_err(|_| {
            ContentError::NotReady("bytes not found in backend, upload first".to_owned())
        })?;
        // 翻账(镜像一步上传步骤 4-8:object 致命 → 元数据非致命 → content 致命),共用一个 now。
        let now = self.clock.now();
        self.objects
            .set_status(scope, target.id, ObjectStatus::Uploaded, now, by)
            .await?;
        let object_metadata = ObjectMetadata {
            object_id: target.id,
            size_bytes: Some(stored.size),
            mime_type: stored.content_type,
            etag: stored.etag,
            metadata: empty_json_object(),
            created_at: now,
            updated_at: now,
        };
        let _ = self.objects.set_metadata(scope, object_metadata).await;
        // content_metadata 只补 size(prepare 写的 tags/mime/file_name/自由 jsonb 保留)。
        if let Ok(mut metadata) = self.contents.get_metadata(scope, content_id).await {
            metadata.file_size = Some(stored.size);
            metadata.updated_at = now;
            let _ = self.contents.set_metadata(scope, metadata).await;
        }
        self.contents
            .set_status(scope, content_id, ContentStatus::Uploaded, now, by)
            .await?;
        self.contents.get(scope, content_id).await
    }

    /// 下载内容主对象的字节(Go `DownloadContent` 流)。
    ///
    /// # Errors
    ///
    /// 内容不存在 → `NotFound`;状态不允许下载 → `NotReady`;无已上传对象 → `NotFound`;
    /// 后端取不到 → `Storage`。
    pub async fn download_content(
        &self,
        scope: ContentScope,
        content_id: Uuid,
    ) -> Result<Bytes, ContentError> {
        let target = self.primary_object(scope, content_id).await?;
        self.store.download(&target.object_key).await
    }

    /// 预览内容(展示用):字节 + 元数据说明书。状态闸与对象选择**同 download**
    /// ("可展示"=="可下载",同一生命周期约束)。元数据缺失(未同步)容忍为 `None`。
    /// DEFER:派生编排落地后,这里优先取 variant="preview" 的派生对象(缩略图/poster),
    /// 签名不变、调用方零改动 —— 签名就是为那天设计的。
    ///
    /// # Errors
    ///
    /// 同 [`Self::download_content`];读元数据的其它错误也上抛。
    pub async fn preview_content(
        &self,
        scope: ContentScope,
        content_id: Uuid,
    ) -> Result<Preview, ContentError> {
        let target = self.primary_object(scope, content_id).await?;
        let data = self.store.download(&target.object_key).await?;
        let metadata = match self.contents.get_metadata(scope, content_id).await {
            Ok(metadata) => Some(metadata),
            Err(ContentError::NotFound) => None,
            Err(error) => return Err(error),
        };
        Ok(Preview { data, metadata })
    }

    /// 预签名预览 URL(inline)。`Ok(None)` = 后端不支持 → 调用方回退字节代理(`preview_content`)。
    ///
    /// # Errors
    ///
    /// 状态闸同 [`Self::download_content`];签名真失败 → `Storage`。
    pub async fn preview_url(
        &self,
        scope: ContentScope,
        content_id: Uuid,
    ) -> Result<Option<String>, ContentError> {
        let target = self.primary_object(scope, content_id).await?;
        self.store.preview_url(&target.object_key).await
    }

    /// 预签名下载 URL(attachment)。`Ok(None)` 同上回退。
    /// filename **优先 content_metadata**(经 set_content_metadata 可编辑 —— 用户改名后签新名),
    /// 回退 object 行(建行时的原始名,之后不可变)。
    ///
    /// # Errors
    ///
    /// 同 [`Self::preview_url`]。
    pub async fn download_url(
        &self,
        scope: ContentScope,
        content_id: Uuid,
    ) -> Result<Option<String>, ContentError> {
        let target = self.primary_object(scope, content_id).await?;
        let metadata_file_name = match self.contents.get_metadata(scope, content_id).await {
            Ok(metadata) => metadata.file_name,
            Err(ContentError::NotFound) => None,
            Err(error) => return Err(error),
        };
        let file_name = metadata_file_name.or(target.file_name);
        self.store
            .download_url(&target.object_key, file_name.as_deref())
            .await
    }

    /// 状态闸 + 主对象选择(download / preview / URL 三路共用,一处收口):
    /// 内容存在 → 状态 ∈ {Uploaded, Processed, Archived} → 第一个 Uploaded 对象。
    async fn primary_object(
        &self,
        scope: ContentScope,
        content_id: Uuid,
    ) -> Result<Object, ContentError> {
        let content = self.contents.get(scope, content_id).await?;
        content.status.check_downloadable()?;
        let objects = self.objects.list_by_content(scope, content_id).await?;
        objects
            .into_iter()
            .find(|object| object.status == ObjectStatus::Uploaded)
            .ok_or(ContentError::NotFound)
    }

    /// 设置内容元数据(全量替换,upsert)。
    ///
    /// # Errors
    ///
    /// 内容不存在 / 别的作用域 → `NotFound`。
    pub async fn set_content_metadata(
        &self,
        scope: ContentScope,
        input: SetContentMetadataInput,
    ) -> Result<(), ContentError> {
        self.contents.get(scope, input.content_id).await?; // 验存在
        let now = self.clock.now();
        let metadata = ContentMetadata {
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
        };
        self.contents.set_metadata(scope, metadata).await
    }

    /// 查内容元数据。
    ///
    /// # Errors
    ///
    /// 不存在 / 别的作用域 → `NotFound`。
    pub async fn get_content_metadata(
        &self,
        scope: ContentScope,
        content_id: Uuid,
    ) -> Result<ContentMetadata, ContentError> {
        self.contents.get_metadata(scope, content_id).await
    }

    /// 列某内容的存活对象(按建序)。返回 `Vec` 的理由同 [`Self::list_content`]。
    ///
    /// # Errors
    ///
    /// 仓储错误原样上抛(别的作用域的内容 → 空列表)。
    pub async fn get_objects(
        &self,
        scope: ContentScope,
        content_id: Uuid,
    ) -> Result<Vec<Object>, ContentError> {
        self.objects.list_by_content(scope, content_id).await
    }

    /// 显式置内容状态(异步工作流钩子)。状态已 typed(类型即合法),故为对仓储 set_status 的薄包装。
    ///
    /// # Errors
    ///
    /// 已软删 / 不存在 / 别的作用域 → `NotFound`。
    pub async fn set_content_status(
        &self,
        scope: ContentScope,
        id: Uuid,
        status: ContentStatus,
        by: Option<&str>,
    ) -> Result<(), ContentError> {
        self.contents
            .set_status(scope, id, status, self.clock.now(), by)
            .await
    }

    /// 显式置对象状态(异步工作流钩子)。同上,薄包装。
    ///
    /// # Errors
    ///
    /// 已软删 / 不存在 / 别的作用域 → `NotFound`。
    pub async fn set_object_status(
        &self,
        scope: ContentScope,
        id: Uuid,
        status: ObjectStatus,
        by: Option<&str>,
    ) -> Result<(), ContentError> {
        self.objects
            .set_status(scope, id, status, self.clock.now(), by)
            .await
    }
}

/// [`ContentService`] 的 builder —— 三个端口已给齐,只覆盖要改的默认值。见 [`ContentService::builder`]。
/// 默认:时钟 `SystemClock`、后端名 `"memory"`。
#[derive(Clone, Debug)]
#[must_use = "builder 不调 build() 什么也不会发生"]
pub struct ContentServiceBuilder<C, O, S, K = SystemClock> {
    contents: C,
    objects: O,
    store: S,
    backend_name: String,
    clock: K,
}

impl<C: ContentRepo, O: ObjectRepo, S: ObjectStore, K: Clock> ContentServiceBuilder<C, O, S, K> {
    /// 设存储后端名(写进 object.storage_backend_name;默认 `"memory"`)。
    pub fn backend_name(mut self, name: impl Into<String>) -> Self {
        self.backend_name = name.into();
        self
    }

    /// 替换时间端口(默认 `SystemClock`)。服务每次写都经它取 now 交给仓储,仓储按传入值落库:
    /// content / object 行和两张元数据表读回来的时间戳都来自这里注入的时钟(测试注入固定时钟即可断言绝对值)。
    pub fn clock<NewClock: Clock>(
        self,
        clock: NewClock,
    ) -> ContentServiceBuilder<C, O, S, NewClock> {
        ContentServiceBuilder {
            contents: self.contents,
            objects: self.objects,
            store: self.store,
            backend_name: self.backend_name,
            clock,
        }
    }

    /// 组装。
    #[must_use]
    pub fn build(self) -> ContentService<C, O, S, K> {
        ContentService {
            contents: self.contents,
            objects: self.objects,
            store: self.store,
            backend_name: self.backend_name,
            clock: self.clock,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use time::macros::datetime;
    use time::OffsetDateTime;

    use super::*;
    use crate::content_scope::DEFAULT_TENANT_ID;
    use crate::in_memory_object_store::InMemoryObjectStore;
    use crate::object_store::StoredObjectMetadata;
    use crate::repo::{InMemoryContentRepo, InMemoryObjectRepo, ObjectLocation};

    /// 单测默认作用域:默认租户。
    const SCOPE: ContentScope = ContentScope::Tenant(DEFAULT_TENANT_ID);

    type MemoryService =
        ContentService<InMemoryContentRepo, InMemoryObjectRepo, InMemoryObjectStore>;

    /// 下游运行时选实现时的写法:四个端口都是 `Arc<dyn …>`。
    type DynService = ContentService<
        Arc<dyn ContentRepo>,
        Arc<dyn ObjectRepo>,
        Arc<dyn ObjectStore>,
        Arc<dyn Clock>,
    >;

    /// 共享端口的服务:测试要绕过服务直驱仓储 / 存储造状态时用。
    type SharedService =
        ContentService<Arc<InMemoryContentRepo>, Arc<InMemoryObjectRepo>, Arc<InMemoryObjectStore>>;

    fn service() -> MemoryService {
        ContentService::new(
            InMemoryContentRepo::new(),
            InMemoryObjectRepo::new(),
            InMemoryObjectStore::new(),
            "memory",
        )
    }

    /// 服务 + 它的三个端口(各持一份 `Arc`)。
    struct Shared {
        service: SharedService,
        contents: Arc<InMemoryContentRepo>,
        objects: Arc<InMemoryObjectRepo>,
        store: Arc<InMemoryObjectStore>,
    }

    fn shared() -> Shared {
        let contents = Arc::new(InMemoryContentRepo::new());
        let objects = Arc::new(InMemoryObjectRepo::new());
        let store = Arc::new(InMemoryObjectStore::new());
        let service = ContentService::new(
            Arc::clone(&contents),
            Arc::clone(&objects),
            Arc::clone(&store),
            "memory",
        );
        Shared {
            service,
            contents,
            objects,
            store,
        }
    }

    fn upload_input(data: &'static [u8]) -> UploadContentInput {
        let mut input = UploadContentInput::new(Uuid::now_v7(), Bytes::from_static(data));
        input.name = Some("doc".to_owned());
        input.file_name = Some("doc.txt".to_owned());
        input.mime_type = Some("text/plain".to_owned());
        input.tags = vec!["a".to_owned()];
        input
    }

    fn prepare_input() -> PrepareUploadInput {
        let mut input = PrepareUploadInput::new(Uuid::now_v7());
        input.name = Some("doc".to_owned());
        input.file_name = Some("doc.txt".to_owned());
        input.mime_type = Some("text/plain".to_owned());
        input.tags = vec!["a".to_owned()];
        input
    }

    /// 模拟客户端拿凭证直传:直接往存储写 `key` 的字节。
    async fn put_bytes(store: &InMemoryObjectStore, key: &str, data: &'static [u8]) {
        store
            .upload(UploadRequest::new(key.to_owned()), Bytes::from_static(data))
            .await
            .unwrap();
    }

    /// 上传→下载往返:upload 后 content/object 皆 Uploaded,download 取回原字节;元数据已同步。
    #[tokio::test]
    async fn upload_then_download_round_trip() {
        let service = service();
        let out = service
            .upload_content(SCOPE, upload_input(b"hello content"), Some("sys"))
            .await
            .unwrap();
        assert_eq!(out.content.status, ContentStatus::Uploaded);
        assert_eq!(out.object.status, ObjectStatus::Uploaded);

        let bytes = service
            .download_content(SCOPE, out.content.id)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"hello content");

        let metadata = service
            .get_content_metadata(SCOPE, out.content.id)
            .await
            .unwrap();
        assert_eq!(metadata.file_size, Some(13), "size 由步骤 7 从后端同步");
        assert_eq!(metadata.tags, vec!["a".to_owned()], "tags 透传");
    }

    /// 新建(未上传)内容不可下载 → `NotReady`(状态守卫)。
    #[tokio::test]
    async fn download_before_upload_is_not_ready() {
        let service = service();
        let content = service
            .create_content(SCOPE, CreateContentInput::new(Uuid::now_v7()), None)
            .await
            .unwrap();
        assert_eq!(
            content.derivation_type.as_deref(),
            Some("original"),
            "没给 derivation_type 时服务默认 original"
        );
        assert!(matches!(
            service.download_content(SCOPE, content.id).await,
            Err(ContentError::NotReady(_))
        ));
    }

    /// preview = 字节 + 展示说明书:字节同 download,metadata 里 mime/file_name 可直接填 HTTP 头。
    #[tokio::test]
    async fn preview_returns_bytes_with_metadata() {
        let service = service();
        let out = service
            .upload_content(SCOPE, upload_input(b"img-bytes"), Some("sys"))
            .await
            .unwrap();
        let preview = service
            .preview_content(SCOPE, out.content.id)
            .await
            .unwrap();
        assert_eq!(&preview.data[..], b"img-bytes");
        let metadata = preview.metadata.expect("upload 已同步元数据");
        assert_eq!(metadata.mime_type.as_deref(), Some("text/plain"));
        assert_eq!(metadata.file_name.as_deref(), Some("doc.txt"));
    }

    /// 状态闸与 download 同一把:未上传(created)→ NotReady;不存在 → NotFound。
    #[tokio::test]
    async fn preview_guards_match_download() {
        let service = service();
        let content = service
            .create_content(SCOPE, CreateContentInput::new(Uuid::now_v7()), None)
            .await
            .unwrap();
        assert!(matches!(
            service.preview_content(SCOPE, content.id).await,
            Err(ContentError::NotReady(_))
        ));
        assert!(matches!(
            service.preview_content(SCOPE, Uuid::now_v7()).await,
            Err(ContentError::NotFound)
        ));
    }

    /// URL 方法:状态闸照常;memory 后端 → Ok(None)(回退代理的判别信号,非错误)。
    #[tokio::test]
    async fn presign_urls_none_on_memory_backend() {
        let service = service();
        let out = service
            .upload_content(SCOPE, upload_input(b"x"), None)
            .await
            .unwrap();
        assert!(service
            .preview_url(SCOPE, out.content.id)
            .await
            .unwrap()
            .is_none());
        assert!(service
            .download_url(SCOPE, out.content.id)
            .await
            .unwrap()
            .is_none());
        assert!(
            matches!(
                service.preview_url(SCOPE, Uuid::now_v7()).await,
                Err(ContentError::NotFound)
            ),
            "闸照常:不存在 → NotFound(不是 None)"
        );
    }

    /// 测试用 store:覆写 URL 方法(字节走内存实现)—— 钉住 Some 回传 + filename / mime 透传。
    struct UrlStore(InMemoryObjectStore);

    #[async_trait]
    impl ObjectStore for UrlStore {
        async fn upload(&self, request: UploadRequest, data: Bytes) -> Result<(), ContentError> {
            self.0.upload(request, data).await
        }
        async fn download(&self, object_key: &str) -> Result<Bytes, ContentError> {
            self.0.download(object_key).await
        }
        async fn delete(&self, object_key: &str) -> Result<(), ContentError> {
            self.0.delete(object_key).await
        }
        async fn metadata(&self, object_key: &str) -> Result<StoredObjectMetadata, ContentError> {
            self.0.metadata(object_key).await
        }
        async fn upload_url(
            &self,
            object_key: &str,
            mime_type: Option<&str>,
        ) -> Result<Option<String>, ContentError> {
            Ok(Some(format!(
                "https://cdn.test/put/{object_key}?mime={}",
                mime_type.unwrap_or("-")
            )))
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

    fn url_service() -> ContentService<InMemoryContentRepo, InMemoryObjectRepo, UrlStore> {
        ContentService::new(
            InMemoryContentRepo::new(),
            InMemoryObjectRepo::new(),
            UrlStore(InMemoryObjectStore::new()),
            "cdn",
        )
    }

    /// URL 后端支持 presign 时:Some 原样回传;download filename **优先可编辑的 content_metadata**
    /// (用户改名后签新名),preview 无 filename 参与。
    #[tokio::test]
    async fn presign_urls_pass_through_and_prefer_metadata_filename() {
        let service = url_service();
        let out = service
            .upload_content(SCOPE, upload_input(b"x"), None)
            .await
            .unwrap();
        let url = service
            .download_url(SCOPE, out.content.id)
            .await
            .unwrap()
            .unwrap();
        assert!(url.contains("?dl=doc.txt"), "上传时的原始名先生效: {url}");

        let mut rename = SetContentMetadataInput::new(out.content.id);
        rename.file_name = Some("renamed.txt".to_owned());
        service.set_content_metadata(SCOPE, rename).await.unwrap();
        let url = service
            .download_url(SCOPE, out.content.id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            url.contains("?dl=renamed.txt"),
            "metadata 是可编辑源,优先于 object 行: {url}"
        );

        let url = service
            .preview_url(SCOPE, out.content.id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            url.ends_with("?inline"),
            "preview 是 inline、无 filename: {url}"
        );
    }

    /// metadata 未同步(直接驱动仓储造"已上传但无元数据")→ preview 容忍为 None,不报错。
    #[tokio::test]
    async fn preview_without_metadata_degrades_to_none() {
        let Shared {
            service,
            contents,
            objects,
            store,
        } = shared();
        // 手工铺状态:content 行 + object 行 + 字节,全程不写 content_metadata。
        let now = OffsetDateTime::now_utc();
        let content = contents
            .create(SCOPE, NewContent::new(Uuid::now_v7(), now), None)
            .await
            .unwrap();
        let location = ObjectLocation {
            storage_backend_name: "memory".to_owned(),
            object_key: "k1".to_owned(),
        };
        let object = objects
            .create(SCOPE, NewObject::new(content.id, location, now), None)
            .await
            .unwrap();
        put_bytes(&store, "k1", b"raw").await;
        objects
            .set_status(SCOPE, object.id, ObjectStatus::Uploaded, now, None)
            .await
            .unwrap();
        contents
            .set_status(SCOPE, content.id, ContentStatus::Uploaded, now, None)
            .await
            .unwrap();

        let preview = service.preview_content(SCOPE, content.id).await.unwrap();
        assert_eq!(&preview.data[..], b"raw");
        assert!(preview.metadata.is_none(), "未同步过元数据应容忍为 None");
    }

    /// prepare(memory):账建了、格占了、凭证 None(回退信号);用户声明的元数据不等销账就可查。
    #[tokio::test]
    async fn prepare_creates_rows_and_metadata_without_url_on_memory() {
        let service = service();
        let out = service
            .prepare_upload(SCOPE, prepare_input(), None)
            .await
            .unwrap();
        assert_eq!(out.content.status, ContentStatus::Created);
        assert_eq!(out.object.status, ObjectStatus::Created);
        assert!(out.upload_url.is_none(), "memory 后端签不出凭证");
        let metadata = service
            .get_content_metadata(SCOPE, out.content.id)
            .await
            .unwrap();
        assert_eq!(metadata.tags, vec!["a".to_owned()]);
        assert_eq!(metadata.mime_type.as_deref(), Some("text/plain"));
        assert!(metadata.file_size.is_none(), "size 留给 confirm 从后端补");
    }

    /// 后端支持时:凭证 Some 且 mime 已传给 store(签进凭证的前提)。
    #[tokio::test]
    async fn prepare_carries_upload_url_when_backend_supports() {
        let service = url_service();
        let out = service
            .prepare_upload(SCOPE, prepare_input(), None)
            .await
            .unwrap();
        let url = out.upload_url.expect("该后端支持 presign");
        assert!(url.contains(&out.object.object_key), "{url}");
        assert!(url.contains("mime=text/plain"), "mime 应传到 store: {url}");
    }

    /// 两步全流程:prepare → 模拟客户端 PUT(直写 store)→ confirm → 可下载,size 已补。
    #[tokio::test]
    async fn prepare_then_confirm_round_trip() {
        let Shared { service, store, .. } = shared();
        let out = service
            .prepare_upload(SCOPE, prepare_input(), None)
            .await
            .unwrap();
        put_bytes(&store, &out.object.object_key, b"two-step bytes").await;
        let content = service
            .confirm_upload(SCOPE, out.content.id, None)
            .await
            .unwrap();
        assert_eq!(content.status, ContentStatus::Uploaded);
        let metadata = service
            .get_content_metadata(SCOPE, content.id)
            .await
            .unwrap();
        assert_eq!(metadata.file_size, Some(14), "size 由 confirm 从后端补");
        assert_eq!(
            metadata.tags,
            vec!["a".to_owned()],
            "prepare 写的 tags 不丢"
        );
        let bytes = service.download_content(SCOPE, content.id).await.unwrap();
        assert_eq!(&bytes[..], b"two-step bytes");
    }

    /// 没传字节就销账 → NotReady(app 映射 409);状态不动。
    #[tokio::test]
    async fn confirm_before_put_is_not_ready() {
        let service = service();
        let out = service
            .prepare_upload(SCOPE, prepare_input(), None)
            .await
            .unwrap();
        assert!(matches!(
            service.confirm_upload(SCOPE, out.content.id, None).await,
            Err(ContentError::NotReady(_))
        ));
        assert_eq!(
            service
                .get_content(SCOPE, out.content.id)
                .await
                .unwrap()
                .status,
            ContentStatus::Created
        );
    }

    /// confirm 幂等:重试(网络抖动)不报错、状态不变。
    #[tokio::test]
    async fn confirm_is_idempotent() {
        let Shared { service, store, .. } = shared();
        let out = service
            .prepare_upload(SCOPE, prepare_input(), None)
            .await
            .unwrap();
        put_bytes(&store, &out.object.object_key, b"x").await;
        service
            .confirm_upload(SCOPE, out.content.id, None)
            .await
            .unwrap();
        let again = service
            .confirm_upload(SCOPE, out.content.id, None)
            .await
            .unwrap();
        assert_eq!(again.status, ContentStatus::Uploaded);
    }

    /// 崩溃恢复:上次 confirm 在"object 已翻、content 未翻"之间挂掉 → 重试要能续走。
    /// (直驱 repo 制造该状态;删掉 confirm 里 `| ObjectStatus::Uploaded` 这条匹配,本测试必红。)
    #[tokio::test]
    async fn confirm_resumes_after_crash_between_flips() {
        let Shared {
            service,
            objects,
            store,
            ..
        } = shared();
        let out = service
            .prepare_upload(SCOPE, prepare_input(), None)
            .await
            .unwrap();
        put_bytes(&store, &out.object.object_key, b"x").await;
        // 模拟半程崩溃:object 翻完、content 停在 Created。
        objects
            .set_status(
                SCOPE,
                out.object.id,
                ObjectStatus::Uploaded,
                OffsetDateTime::now_utc(),
                None,
            )
            .await
            .unwrap();
        let content = service
            .confirm_upload(SCOPE, out.content.id, None)
            .await
            .unwrap();
        assert_eq!(content.status, ContentStatus::Uploaded, "重试应续走完账");
    }

    /// 迟到的 confirm 重试不回卷状态:Processed/Archived 原样返回(状态机不倒车)。
    #[tokio::test]
    async fn confirm_never_rewinds_processed_or_archived() {
        let Shared { service, store, .. } = shared();
        let out = service
            .prepare_upload(SCOPE, prepare_input(), None)
            .await
            .unwrap();
        put_bytes(&store, &out.object.object_key, b"x").await;
        service
            .confirm_upload(SCOPE, out.content.id, None)
            .await
            .unwrap();
        service
            .set_content_status(SCOPE, out.content.id, ContentStatus::Archived, None)
            .await
            .unwrap();
        let content = service
            .confirm_upload(SCOPE, out.content.id, None)
            .await
            .unwrap();
        assert_eq!(
            content.status,
            ContentStatus::Archived,
            "confirm 重试不得回卷已推进的状态"
        );
    }

    /// 没有可销账对象(格子从未占/已删)→ NotFound。
    #[tokio::test]
    async fn confirm_without_object_is_not_found() {
        let service = service();
        let content = service
            .create_content(SCOPE, CreateContentInput::new(Uuid::now_v7()), None)
            .await
            .unwrap();
        assert!(matches!(
            service.confirm_upload(SCOPE, content.id, None).await,
            Err(ContentError::NotFound)
        ));
    }

    /// 跨租户:A 上传的内容,B 作用域经 service 的每个按 ID 方法都 NotFound;A 的状态不受影响。
    #[tokio::test]
    async fn cross_tenant_service_calls_are_not_found() {
        let service = service();
        let a = ContentScope::try_new_tenant(Uuid::now_v7()).unwrap();
        let b = ContentScope::try_new_tenant(Uuid::now_v7()).unwrap();
        let out = service
            .upload_content(a, upload_input(b"a-bytes"), None)
            .await
            .unwrap();
        let id = out.content.id;
        let not_found =
            |result: Result<(), ContentError>| matches!(result, Err(ContentError::NotFound));
        assert!(not_found(service.get_content(b, id).await.map(drop)));
        assert!(not_found(service.download_content(b, id).await.map(drop)));
        assert!(not_found(service.preview_content(b, id).await.map(drop)));
        assert!(not_found(service.preview_url(b, id).await.map(drop)));
        assert!(not_found(service.download_url(b, id).await.map(drop)));
        assert!(not_found(
            service.get_content_metadata(b, id).await.map(drop)
        ));
        assert!(not_found(
            service.confirm_upload(b, id, None).await.map(drop)
        ));
        let hijack = UpdateContentInput {
            name: Some("hijack".into()),
            ..UpdateContentInput::default()
        };
        assert!(not_found(
            service.update_content(b, id, hijack, None).await.map(drop)
        ));
        assert!(not_found(
            service
                .set_content_metadata(b, SetContentMetadataInput::new(id))
                .await
        ));
        assert!(not_found(
            service
                .set_content_status(b, id, ContentStatus::Archived, None)
                .await
        ));
        assert!(not_found(
            service
                .set_object_status(b, out.object.id, ObjectStatus::Failed, None)
                .await
        ));
        assert!(not_found(service.delete_content(b, id, None).await));
        assert!(service.get_objects(b, id).await.unwrap().is_empty());
        assert!(service
            .list_content(b, out.content.owner_id)
            .await
            .unwrap()
            .is_empty());
        // A 原样:还在、名字没被改、可下载
        let content = service.get_content(a, id).await.unwrap();
        assert_eq!(content.name.as_deref(), Some("doc"));
        assert_eq!(content.status, ContentStatus::Uploaded);
        assert_eq!(
            &service.download_content(a, id).await.unwrap()[..],
            b"a-bytes"
        );
    }

    /// 个人作用域:list 按 owner 过滤;租户作用域看不到个人内容;tenant_id = None。
    #[tokio::test]
    async fn personal_scope_lists_by_owner_and_hides_from_tenants() {
        let service = service();
        let personal = ContentScope::Personal;
        let mine = upload_input(b"me");
        let me = mine.owner_id;
        let out = service.upload_content(personal, mine, None).await.unwrap();
        service
            .upload_content(personal, upload_input(b"other"), None)
            .await
            .unwrap();
        assert_eq!(out.content.tenant_id, None);
        let listed = service.list_content(personal, me).await.unwrap();
        let listed_ids: Vec<Uuid> = listed.iter().map(|content| content.id).collect();
        assert_eq!(listed_ids, vec![out.content.id]);
        assert!(matches!(
            service.get_content(SCOPE, out.content.id).await,
            Err(ContentError::NotFound)
        ));
        assert!(service.list_content(SCOPE, me).await.unwrap().is_empty());
    }

    /// nil 租户:构造即拒;直接写 `Tenant(nil)` 调 service → 仓储入口拒(InvalidInput)。
    #[tokio::test]
    async fn nil_tenant_scope_is_invalid_input() {
        assert!(matches!(
            ContentScope::try_new_tenant(Uuid::nil()),
            Err(ContentError::InvalidInput(_))
        ));
        let service = service();
        assert!(matches!(
            service
                .upload_content(ContentScope::Tenant(Uuid::nil()), upload_input(b"x"), None)
                .await,
            Err(ContentError::InvalidInput(_))
        ));
    }

    /// 下游在运行时二选一实现:端口填 `Arc<dyn …>`(经端口 trait 的转发实现)照常工作,
    /// 存储的 presign 方法也转发到具体后端(不会退回默认的 None)。
    #[tokio::test]
    async fn service_over_arc_dyn_ports_works() {
        let contents: Arc<dyn ContentRepo> = Arc::new(InMemoryContentRepo::new());
        let objects: Arc<dyn ObjectRepo> = Arc::new(InMemoryObjectRepo::new());
        let store: Arc<dyn ObjectStore> = Arc::new(UrlStore(InMemoryObjectStore::new()));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let service: DynService = ContentService::builder(contents, objects, store)
            .backend_name("cdn")
            .clock(clock)
            .build();
        let cloned = service.clone();
        let out = cloned
            .upload_content(SCOPE, upload_input(b"dyn"), None)
            .await
            .unwrap();
        assert_eq!(out.object.storage_backend_name, "cdn");
        assert_eq!(
            &service
                .download_content(SCOPE, out.content.id)
                .await
                .unwrap()[..],
            b"dyn"
        );
        let url = service.preview_url(SCOPE, out.content.id).await.unwrap();
        assert!(
            url.is_some_and(|url| url.ends_with("?inline")),
            "Arc<dyn ObjectStore> 必须把 preview_url 转发给具体后端"
        );
    }

    /// 独占所有权的运行时选择:端口填 `Box<dyn …>`。
    #[tokio::test]
    async fn service_over_box_dyn_ports_works() {
        let service: ContentService<
            Box<dyn ContentRepo>,
            Box<dyn ObjectRepo>,
            Box<dyn ObjectStore>,
        > = ContentService::new(
            Box::new(InMemoryContentRepo::new()),
            Box::new(InMemoryObjectRepo::new()),
            Box::new(InMemoryObjectStore::new()),
            "memory",
        );
        let out = service
            .upload_content(SCOPE, upload_input(b"box"), None)
            .await
            .unwrap();
        assert_eq!(
            &service
                .download_content(SCOPE, out.content.id)
                .await
                .unwrap()[..],
            b"box"
        );
    }

    /// 数调用次数的时钟:证明 builder 注入的时钟真的被服务用上。
    #[derive(Default)]
    struct CountingClock(AtomicUsize);

    impl Clock for CountingClock {
        fn now(&self) -> OffsetDateTime {
            self.0.fetch_add(1, Ordering::SeqCst);
            OffsetDateTime::UNIX_EPOCH
        }
    }

    #[tokio::test]
    async fn builder_clock_is_used_by_service() {
        let clock = Arc::new(CountingClock::default());
        let service = ContentService::builder(
            InMemoryContentRepo::new(),
            InMemoryObjectRepo::new(),
            InMemoryObjectStore::new(),
        )
        .clock(Arc::clone(&clock))
        .build();
        service
            .upload_content(SCOPE, upload_input(b"t"), None)
            .await
            .unwrap();
        assert!(
            clock.0.load(Ordering::SeqCst) > 0,
            "upload_content 应经注入的时钟取 now"
        );
    }

    /// 手动拨的时钟:测试里按步骤换时刻,证明每一步落库的时间戳都来自注入的时钟。
    struct ManualClock(Mutex<OffsetDateTime>);

    impl ManualClock {
        fn new(at: OffsetDateTime) -> Self {
            Self(Mutex::new(at))
        }

        fn set(&self, at: OffsetDateTime) {
            *self.0.lock().expect("ManualClock 的锁不应中毒") = at;
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> OffsetDateTime {
            *self.0.lock().expect("ManualClock 的锁不应中毒")
        }
    }

    /// 服务 + 共享的内存端口 + 手动时钟(测试要绕过服务读仓储里的原始行)。
    struct Clocked {
        service: ContentService<
            Arc<InMemoryContentRepo>,
            Arc<InMemoryObjectRepo>,
            Arc<InMemoryObjectStore>,
            Arc<ManualClock>,
        >,
        contents: Arc<InMemoryContentRepo>,
        objects: Arc<InMemoryObjectRepo>,
        store: Arc<InMemoryObjectStore>,
        clock: Arc<ManualClock>,
    }

    fn clocked(start: OffsetDateTime) -> Clocked {
        let contents = Arc::new(InMemoryContentRepo::new());
        let objects = Arc::new(InMemoryObjectRepo::new());
        let store = Arc::new(InMemoryObjectStore::new());
        let clock = Arc::new(ManualClock::new(start));
        let service = ContentService::builder(
            Arc::clone(&contents),
            Arc::clone(&objects),
            Arc::clone(&store),
        )
        .clock(Arc::clone(&clock))
        .build();
        Clocked {
            service,
            contents,
            objects,
            store,
            clock,
        }
    }

    const T1: OffsetDateTime = datetime!(2030-01-02 03:04:05.123456 UTC);
    const T2: OffsetDateTime = datetime!(2030-01-03 03:04:05.5 UTC);
    const T3: OffsetDateTime = datetime!(2030-01-04 00:00 UTC);
    const T4: OffsetDateTime = datetime!(2030-01-05 00:00 UTC);
    const T5: OffsetDateTime = datetime!(2030-01-06 00:00 UTC);

    /// 一次性上传 → 改 → 改元数据 → 置状态 → 软删:每一步读回的时间戳都等于那一刻注入的时钟值;
    /// created_at 只在建行 / 首次写元数据时定下,之后不变。
    #[tokio::test]
    async fn upload_flow_timestamps_come_from_injected_clock() {
        let Clocked {
            service,
            contents,
            objects,
            clock,
            ..
        } = clocked(T1);
        let out = service
            .upload_content(SCOPE, upload_input(b"clock"), Some("sys"))
            .await
            .unwrap();
        let (content_id, object_id) = (out.content.id, out.object.id);
        assert_eq!(
            (out.content.created_at, out.content.updated_at),
            (T1, T1),
            "上传:content 行的时间戳 = 时钟"
        );
        assert_eq!(
            (out.object.created_at, out.object.updated_at),
            (T1, T1),
            "上传:object 行的时间戳 = 时钟"
        );
        let metadata = service
            .get_content_metadata(SCOPE, content_id)
            .await
            .unwrap();
        assert_eq!((metadata.created_at, metadata.updated_at), (T1, T1));
        let object_metadata = objects.get_metadata(SCOPE, object_id).await.unwrap();
        assert_eq!(
            (object_metadata.created_at, object_metadata.updated_at),
            (T1, T1)
        );

        clock.set(T2);
        let rename = UpdateContentInput {
            name: Some("renamed".to_owned()),
            ..UpdateContentInput::default()
        };
        let updated = service
            .update_content(SCOPE, content_id, rename, None)
            .await
            .unwrap();
        assert_eq!((updated.created_at, updated.updated_at), (T1, T2), "改");

        clock.set(T3);
        let mut tags = SetContentMetadataInput::new(content_id);
        tags.tags = vec!["t".to_owned()];
        service.set_content_metadata(SCOPE, tags).await.unwrap();
        let metadata = service
            .get_content_metadata(SCOPE, content_id)
            .await
            .unwrap();
        assert_eq!(
            (metadata.created_at, metadata.updated_at),
            (T1, T3),
            "改元数据:保留首次的 created_at"
        );

        clock.set(T4);
        service
            .set_content_status(SCOPE, content_id, ContentStatus::Processed, None)
            .await
            .unwrap();
        service
            .set_object_status(SCOPE, object_id, ObjectStatus::Failed, None)
            .await
            .unwrap();
        let content = service.get_content(SCOPE, content_id).await.unwrap();
        assert_eq!((content.created_at, content.updated_at), (T1, T4), "置状态");
        let object = objects.get(SCOPE, object_id).await.unwrap();
        assert_eq!(
            (object.created_at, object.updated_at),
            (T1, T4),
            "置对象状态"
        );

        clock.set(T5);
        service
            .delete_content(SCOPE, content_id, None)
            .await
            .unwrap();
        let (deleted, deleted_at) = contents.raw_row(content_id).expect("软删只打标记,行还在");
        assert_eq!(deleted_at, Some(T5), "软删:deleted_at = 时钟");
        assert_eq!((deleted.created_at, deleted.updated_at), (T1, T5));
    }

    /// 两步上传:prepare 时刻建行 + 写元数据,confirm 时刻翻状态 + 补元数据。
    #[tokio::test]
    async fn prepare_confirm_timestamps_come_from_injected_clock() {
        let Clocked {
            service,
            objects,
            store,
            clock,
            ..
        } = clocked(T1);
        let out = service
            .prepare_upload(SCOPE, prepare_input(), None)
            .await
            .unwrap();
        assert_eq!((out.content.created_at, out.content.updated_at), (T1, T1));
        assert_eq!((out.object.created_at, out.object.updated_at), (T1, T1));
        put_bytes(&store, &out.object.object_key, b"two-step").await;

        clock.set(T2);
        let content = service
            .confirm_upload(SCOPE, out.content.id, None)
            .await
            .unwrap();
        assert_eq!((content.created_at, content.updated_at), (T1, T2));
        let object = objects.get(SCOPE, out.object.id).await.unwrap();
        assert_eq!((object.created_at, object.updated_at), (T1, T2));
        let metadata = service
            .get_content_metadata(SCOPE, out.content.id)
            .await
            .unwrap();
        assert_eq!(
            (metadata.created_at, metadata.updated_at),
            (T1, T2),
            "prepare 写、confirm 补:created_at 是 prepare 的"
        );
        let object_metadata = objects.get_metadata(SCOPE, out.object.id).await.unwrap();
        assert_eq!(
            (object_metadata.created_at, object_metadata.updated_at),
            (T2, T2),
            "object_metadata 在 confirm 时才第一次写"
        );
    }

    /// 软删对象:`deleted_at` 和 `updated_at` 都是传入的 `at`(仓储直驱;服务目前不单独删对象)。
    #[tokio::test]
    async fn object_soft_delete_uses_given_time() {
        let Clocked { objects, .. } = clocked(T1);
        let location = ObjectLocation {
            storage_backend_name: "memory".to_owned(),
            object_key: "k".to_owned(),
        };
        let object = objects
            .create(SCOPE, NewObject::new(Uuid::now_v7(), location, T1), None)
            .await
            .unwrap();
        objects
            .soft_delete(SCOPE, object.id, T2, None)
            .await
            .unwrap();
        let (deleted, deleted_at) = objects.raw_row(object.id).expect("软删只打标记,行还在");
        assert_eq!(deleted_at, Some(T2));
        assert_eq!((deleted.created_at, deleted.updated_at), (T1, T2));
    }

    /// 生成的 key 用内容自己的 owner / id:租户 `tenants/{t}/{content_id}/{uuid}`、个人 `personal/{owner}/…`。
    #[test]
    fn minted_object_key_uses_scope_prefix_and_content_ids() {
        let owner = Uuid::now_v7();
        let tenant = Uuid::now_v7();
        let content = Content::new(
            Uuid::now_v7(),
            ContentScope::Tenant(tenant),
            NewContent::new(owner, OffsetDateTime::UNIX_EPOCH),
        );
        let tenant_key = mint_object_key(ContentScope::Tenant(tenant), &content);
        let tenant_head = format!("tenants/{tenant}/{}/", content.id);
        assert!(tenant_key.starts_with(&tenant_head), "{tenant_key}");
        let personal_key = mint_object_key(ContentScope::Personal, &content);
        let personal_head = format!("personal/{owner}/{}/", content.id);
        assert!(personal_key.starts_with(&personal_head), "{personal_key}");
    }
}
