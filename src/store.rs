//! 存储后端端口(blob store)—— content 之于媒体,类比 idm 的 token 签验端口:
//! **默认实现在库内**(`InMemoryObjectStore`,零 DB/零外部依赖跑通),**生产 minio/S3 由 app 注入**
//! (baserust 在自己 infra/ 接 minio),经 builder 喂给服务。
//!
//! **字节而非流(v0.1 简化)**:Go 用 `io.Reader`/`io.ReadCloser`,这里端口收/发 owned `bytes::Bytes`
//! (缓冲整体)。理由:trait-object 流式 (`Box<dyn AsyncRead>`) 撑胖端口,而脚手架载荷小。流式是 DEFER 项,
//! 日后可在不破坏调用方的前提下加 `upload_stream`/`download_stream`。

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use time::OffsetDateTime;

use crate::error::ContentError;

/// 后端侧对象元数据(从后端读回 size/etag/content-type)。
#[derive(Clone, Debug)]
pub struct ObjectMeta {
    pub key: String,
    pub size: i64,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub updated_at: Option<OffsetDateTime>,
}

/// 上传时传给后端的参数(key + 内容类型 + 原始文件名,后者供 Content-Disposition)。
#[derive(Clone, Debug)]
pub struct UploadParams {
    pub object_key: String,
    pub mime_type: Option<String>,
    pub file_name: Option<String>,
}

/// 存储后端抽象。命名按后端,服务持其一(SCAFFOLD-CORE 单后端;多后端注册表 DEFER)。
/// 任何后端故障 → `ContentError::Storage`(key/backend 细节进日志、不进响应体)。
#[async_trait]
pub trait ObjectStore: Send + Sync {
    /// 推字节到后端(覆盖同 key)。
    async fn upload(&self, params: UploadParams, data: Bytes) -> Result<(), ContentError>;

    /// 取回整段字节(v0.1 缓冲;非流式)。
    async fn download(&self, object_key: &str) -> Result<Bytes, ContentError>;

    /// 删后端对象。幂等(不存在也 Ok)。
    async fn delete(&self, object_key: &str) -> Result<(), ContentError>;

    /// 读后端侧元数据(size/etag/content-type)。
    async fn object_meta(&self, object_key: &str) -> Result<ObjectMeta, ContentError>;

    // DEFER:get_upload_url / get_download_url / get_preview_url(预签名 URL)。
}

/// 库内默认实现:进程内内存后端(脚手架零 DB 跑通)。生产 minio/S3 由 app 注入。
#[derive(Default)]
pub struct InMemoryObjectStore {
    blobs: Mutex<HashMap<String, Blob>>,
}

struct Blob {
    data: Bytes,
    mime: Option<String>,
}

impl InMemoryObjectStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ObjectStore for InMemoryObjectStore {
    async fn upload(&self, params: UploadParams, data: Bytes) -> Result<(), ContentError> {
        self.blobs.lock().expect("锁未中毒").insert(
            params.object_key,
            Blob {
                data,
                mime: params.mime_type,
            },
        );
        Ok(())
    }

    async fn download(&self, object_key: &str) -> Result<Bytes, ContentError> {
        self.blobs
            .lock()
            .expect("锁未中毒")
            .get(object_key)
            .map(|b| b.data.clone())
            .ok_or_else(|| ContentError::Storage(anyhow::anyhow!("object not found in store")))
    }

    async fn delete(&self, object_key: &str) -> Result<(), ContentError> {
        self.blobs.lock().expect("锁未中毒").remove(object_key);
        Ok(())
    }

    async fn object_meta(&self, object_key: &str) -> Result<ObjectMeta, ContentError> {
        self.blobs
            .lock()
            .expect("锁未中毒")
            .get(object_key)
            .map(|b| ObjectMeta {
                key: object_key.to_owned(),
                size: b.data.len() as i64,
                content_type: b.mime.clone(),
                etag: None,
                updated_at: None,
            })
            .ok_or_else(|| ContentError::Storage(anyhow::anyhow!("object not found in store")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// InMemoryObjectStore 往返:upload → object_meta(size/mime)→ download(字节一致)→ delete(后续不可达)。
    #[tokio::test]
    async fn in_memory_store_round_trip() {
        let store = InMemoryObjectStore::new();
        let key = "c/o";
        store
            .upload(
                UploadParams {
                    object_key: key.to_owned(),
                    mime_type: Some("text/plain".to_owned()),
                    file_name: Some("hi.txt".to_owned()),
                },
                Bytes::from_static(b"hello world"),
            )
            .await
            .unwrap();

        let meta = store.object_meta(key).await.unwrap();
        assert_eq!(meta.size, 11);
        assert_eq!(meta.content_type.as_deref(), Some("text/plain"));

        let got = store.download(key).await.unwrap();
        assert_eq!(&got[..], b"hello world");

        store.delete(key).await.unwrap();
        assert!(store.download(key).await.is_err()); // 删后不可达
        assert!(store.delete(key).await.is_ok()); // delete 幂等
    }
}
