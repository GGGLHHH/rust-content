//! 库内默认的对象存储:进程内内存后端(脚手架零 DB 跑通)。生产 minio/S3 由 app 注入。

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::anyhow;
use async_trait::async_trait;
use bytes::Bytes;

use crate::content_error::ContentError;
use crate::object_store::{ObjectStore, StoredObjectMetadata, UploadRequest};

/// 进程内内存后端。不支持预签名 URL(三个 URL 方法都回 `Ok(None)`)。
///
/// 内部锁中毒(别的线程持锁时 panic 过)后,各方法 panic,不去碰中毒锁里的数据。
#[derive(Debug, Default)]
pub struct InMemoryObjectStore {
    blobs: Mutex<HashMap<String, Blob>>,
}

#[derive(Debug)]
struct Blob {
    data: Bytes,
    mime_type: Option<String>,
}

impl InMemoryObjectStore {
    /// 空存储。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ObjectStore for InMemoryObjectStore {
    async fn upload(&self, request: UploadRequest, data: Bytes) -> Result<(), ContentError> {
        let blob = Blob {
            data,
            mime_type: request.mime_type,
        };
        self.blobs
            .lock()
            .expect("InMemoryObjectStore 的锁不应中毒")
            .insert(request.object_key, blob);
        Ok(())
    }

    async fn download(&self, object_key: &str) -> Result<Bytes, ContentError> {
        self.blobs
            .lock()
            .expect("InMemoryObjectStore 的锁不应中毒")
            .get(object_key)
            .map(|blob| blob.data.clone())
            .ok_or_else(|| ContentError::Storage(anyhow!("object not found in store")))
    }

    async fn delete(&self, object_key: &str) -> Result<(), ContentError> {
        self.blobs
            .lock()
            .expect("InMemoryObjectStore 的锁不应中毒")
            .remove(object_key);
        Ok(())
    }

    async fn metadata(&self, object_key: &str) -> Result<StoredObjectMetadata, ContentError> {
        let blobs = self.blobs.lock().expect("InMemoryObjectStore 的锁不应中毒");
        let Some(blob) = blobs.get(object_key) else {
            return Err(ContentError::Storage(anyhow!("object not found in store")));
        };
        let mut metadata = StoredObjectMetadata::new(object_key.to_owned(), blob.data.len() as i64);
        metadata.content_type = blob.mime_type.clone();
        Ok(metadata)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn text_request(key: &str) -> UploadRequest {
        let mut request = UploadRequest::new(key.to_owned());
        request.mime_type = Some("text/plain".to_owned());
        request.file_name = Some("hi.txt".to_owned());
        request
    }

    /// 往返:upload → metadata(size/mime)→ download(字节一致)→ delete(后续不可达、幂等)。
    #[tokio::test]
    async fn in_memory_store_round_trip() {
        let store = InMemoryObjectStore::new();
        let key = "c/o";
        store
            .upload(text_request(key), Bytes::from_static(b"hello world"))
            .await
            .unwrap();

        let metadata = store.metadata(key).await.unwrap();
        assert_eq!(metadata.size, 11);
        assert_eq!(metadata.content_type.as_deref(), Some("text/plain"));

        let got = store.download(key).await.unwrap();
        assert_eq!(&got[..], b"hello world");

        store.delete(key).await.unwrap();
        assert!(store.download(key).await.is_err(), "删后不可达");
        assert!(store.delete(key).await.is_ok(), "delete 幂等");
    }

    /// URL 是可选能力:memory 后端不支持 presign → `Ok(None)`(非错误;调用方回退字节代理)。
    #[tokio::test]
    async fn presign_urls_are_none() {
        let store = InMemoryObjectStore::new();
        assert!(store.upload_url("k", None).await.unwrap().is_none());
        assert!(store
            .download_url("k", Some("f.txt"))
            .await
            .unwrap()
            .is_none());
        assert!(store.preview_url("k").await.unwrap().is_none());
    }

    /// 只覆写 URL 方法的后端:钉住 `Arc<T>` / `Box<T>` 的转发实现**也转发有默认实现的方法**
    /// (漏转发会退回默认的 `Ok(None)`)。
    struct PresignStore(InMemoryObjectStore);

    #[async_trait]
    impl ObjectStore for PresignStore {
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
                "put/{object_key}?{}",
                mime_type.unwrap_or("-")
            )))
        }
        async fn download_url(
            &self,
            object_key: &str,
            download_filename: Option<&str>,
        ) -> Result<Option<String>, ContentError> {
            Ok(Some(format!(
                "get/{object_key}?{}",
                download_filename.unwrap_or("-")
            )))
        }
        async fn preview_url(&self, object_key: &str) -> Result<Option<String>, ContentError> {
            Ok(Some(format!("inline/{object_key}")))
        }
    }

    async fn assert_forwards_everything(store: &impl ObjectStore) {
        store
            .upload(text_request("k"), Bytes::from_static(b"abc"))
            .await
            .unwrap();
        assert_eq!(&store.download("k").await.unwrap()[..], b"abc");
        assert_eq!(store.metadata("k").await.unwrap().size, 3);
        assert_eq!(
            store
                .upload_url("k", Some("text/plain"))
                .await
                .unwrap()
                .as_deref(),
            Some("put/k?text/plain"),
            "upload_url 必须转发到内层实现,而不是退回默认的 None"
        );
        assert_eq!(
            store
                .download_url("k", Some("a.txt"))
                .await
                .unwrap()
                .as_deref(),
            Some("get/k?a.txt")
        );
        assert_eq!(
            store.preview_url("k").await.unwrap().as_deref(),
            Some("inline/k")
        );
        store.delete("k").await.unwrap();
        assert!(store.download("k").await.is_err(), "delete 必须转发");
    }

    #[tokio::test]
    async fn arc_dyn_store_forwards_every_method() {
        let store: Arc<dyn ObjectStore> = Arc::new(PresignStore(InMemoryObjectStore::new()));
        assert_forwards_everything(&store).await;
    }

    #[tokio::test]
    async fn box_dyn_store_forwards_every_method() {
        let store: Box<dyn ObjectStore> = Box::new(PresignStore(InMemoryObjectStore::new()));
        assert_forwards_everything(&store).await;
    }
}
