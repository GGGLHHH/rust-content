//! 存储后端端口(blob store)—— content 之于媒体,类比 idm 的 token 签验端口:
//! **默认实现在库内**(`InMemoryObjectStore`,零 DB/零外部依赖跑通),**生产 minio/S3 由 app 注入**
//! (baserust 在自己 infra/ 接 minio),经构造函数喂给服务。
//!
//! **字节而非流(有意的简化)**:Go 用 `io.Reader`/`io.ReadCloser`,这里端口收/发 owned `bytes::Bytes`
//! (缓冲整体)。理由:trait-object 流式 (`Box<dyn AsyncRead>`) 撑胖端口,而脚手架载荷小。流式是 DEFER 项,
//! 日后可在不破坏调用方的前提下加 `upload_stream`/`download_stream`。

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use time::OffsetDateTime;

use crate::content_error::ContentError;

/// 后端侧对象元数据(从后端读回 size/etag/content-type)。
/// 和仓储里的 [`ObjectMetadata`](crate::ObjectMetadata)(表 `object_metadata`)不是一回事。
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct StoredObjectMetadata {
    /// 后端内的 key。
    pub key: String,
    /// 字节数。
    pub size: i64,
    /// 后端记录的 Content-Type。
    pub content_type: Option<String>,
    /// 后端给的 ETag。
    pub etag: Option<String>,
    /// 后端记录的最后修改时刻。
    pub updated_at: Option<OffsetDateTime>,
}

impl StoredObjectMetadata {
    /// 只有 key 和大小,其余字段 `None`;实现方再按后端能读回的填。
    #[must_use]
    pub fn new(key: String, size: i64) -> Self {
        Self {
            key,
            size,
            content_type: None,
            etag: None,
            updated_at: None,
        }
    }
}

/// 上传时传给后端的参数(key + 内容类型 + 原始文件名,后者供 Content-Disposition)。
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct UploadRequest {
    /// 后端内的 key(同 key 覆盖)。
    pub object_key: String,
    /// 内容类型。
    pub mime_type: Option<String>,
    /// 原始文件名。
    pub file_name: Option<String>,
}

impl UploadRequest {
    /// 只有 key,内容类型和文件名为 `None`。
    #[must_use]
    pub fn new(object_key: String) -> Self {
        Self {
            object_key,
            mime_type: None,
            file_name: None,
        }
    }
}

/// 存储后端抽象。命名按后端,服务持其一(目前单后端;多后端注册表以后再做)。
/// 任何后端故障 → `ContentError::Storage`(key/backend 细节进日志、不进响应体)。
#[async_trait]
pub trait ObjectStore: Send + Sync {
    /// 推字节到后端(覆盖同 key)。
    async fn upload(&self, request: UploadRequest, data: Bytes) -> Result<(), ContentError>;

    /// 取回整段字节(缓冲整体;非流式)。
    async fn download(&self, object_key: &str) -> Result<Bytes, ContentError>;

    /// 删后端对象。幂等(不存在也 Ok)。
    async fn delete(&self, object_key: &str) -> Result<(), ContentError>;

    /// 读后端侧元数据(size/etag/content-type)。
    async fn metadata(&self, object_key: &str) -> Result<StoredObjectMetadata, ContentError>;

    /// 客户端直传用的预签名上传 URL(PUT)。`Ok(None)` = 后端不支持(**能力缺省,非错误**)。
    /// `mime_type` 给了应**签进凭证**(S3 签名含 Content-Type → 客户端 PUT 必须带同样的头,
    /// 类型声明跑不掉)—— 两步上传没有"写入前校验",这是唯一能提前钉住的约束。
    /// presign 真失败(网络/凭据)→ `Err(Storage)`,与"不支持"可判别。
    async fn upload_url(
        &self,
        _object_key: &str,
        _mime_type: Option<&str>,
    ) -> Result<Option<String>, ContentError> {
        Ok(None)
    }

    /// 预签名下载 URL,**attachment 语义**:带 `download_filename` 时实现方应把
    /// `Content-Disposition: attachment; filename="..."` 钉进签名(S3 ResponseContentDisposition)。
    async fn download_url(
        &self,
        _object_key: &str,
        _download_filename: Option<&str>,
    ) -> Result<Option<String>, ContentError> {
        Ok(None)
    }

    /// 预签名预览 URL,**inline 语义**(浏览器直接渲染)。与 download_url 的区别就在
    /// 签进 URL 的 disposition —— 领域区别,不是 HTTP 层装饰。
    async fn preview_url(&self, _object_key: &str) -> Result<Option<String>, ContentError> {
        Ok(None)
    }
}

/// 转发实现:让 `Arc<dyn ObjectStore>` 能填 [`ContentService`](crate::ContentService) 的存储类型参数
/// (下游在运行时二选一后端时用)。**每个方法都转发**,含有默认实现的三个 URL 方法 ——
/// 漏转发会让 `Arc<S3Store>` 退回默认的 `Ok(None)`。
#[async_trait]
impl<T: ObjectStore + ?Sized> ObjectStore for Arc<T> {
    async fn upload(&self, request: UploadRequest, data: Bytes) -> Result<(), ContentError> {
        (**self).upload(request, data).await
    }

    async fn download(&self, object_key: &str) -> Result<Bytes, ContentError> {
        (**self).download(object_key).await
    }

    async fn delete(&self, object_key: &str) -> Result<(), ContentError> {
        (**self).delete(object_key).await
    }

    async fn metadata(&self, object_key: &str) -> Result<StoredObjectMetadata, ContentError> {
        (**self).metadata(object_key).await
    }

    async fn upload_url(
        &self,
        object_key: &str,
        mime_type: Option<&str>,
    ) -> Result<Option<String>, ContentError> {
        (**self).upload_url(object_key, mime_type).await
    }

    async fn download_url(
        &self,
        object_key: &str,
        download_filename: Option<&str>,
    ) -> Result<Option<String>, ContentError> {
        (**self).download_url(object_key, download_filename).await
    }

    async fn preview_url(&self, object_key: &str) -> Result<Option<String>, ContentError> {
        (**self).preview_url(object_key).await
    }
}

/// 转发实现:同 `Arc<T>`,给 `Box<dyn ObjectStore>` 用。
#[async_trait]
impl<T: ObjectStore + ?Sized> ObjectStore for Box<T> {
    async fn upload(&self, request: UploadRequest, data: Bytes) -> Result<(), ContentError> {
        (**self).upload(request, data).await
    }

    async fn download(&self, object_key: &str) -> Result<Bytes, ContentError> {
        (**self).download(object_key).await
    }

    async fn delete(&self, object_key: &str) -> Result<(), ContentError> {
        (**self).delete(object_key).await
    }

    async fn metadata(&self, object_key: &str) -> Result<StoredObjectMetadata, ContentError> {
        (**self).metadata(object_key).await
    }

    async fn upload_url(
        &self,
        object_key: &str,
        mime_type: Option<&str>,
    ) -> Result<Option<String>, ContentError> {
        (**self).upload_url(object_key, mime_type).await
    }

    async fn download_url(
        &self,
        object_key: &str,
        download_filename: Option<&str>,
    ) -> Result<Option<String>, ContentError> {
        (**self).download_url(object_key, download_filename).await
    }

    async fn preview_url(&self, object_key: &str) -> Result<Option<String>, ContentError> {
        (**self).preview_url(object_key).await
    }
}
