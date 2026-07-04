//! content 的领域错误类型(**零 HTTP**)。HTTP 状态码 / 机器码 / wire 形状一律由消费方(app)在
//! `From<ContentError> for AppError` 的边界决定 —— 本库只暴露"出了哪类错"。
//!
//! 把 Go 的 16 个 sentinel + 3 个上下文包装(ContentError/ObjectError/StorageError)收敛成一个精简枚举,
//! 按 app 将映射的 HTTP 语义分组:not-found→404、state→409、bad-status→400、storage/internal→500。
//! `Storage` 与 `Internal` 分开:app 可对后端错误(含 key/backend 上下文)单独落日志,但都不进响应体。

#[derive(Debug, thiserror::Error)]
pub enum ContentError {
    /// 资源不存在 / 已软删 / 无可下载对象。
    /// (ErrContentNotFound / ErrObjectNotFound / ErrNoObjectsFound / ErrNoUploadedObjects → 404)
    #[error("resource not found")]
    NotFound,

    /// 资源尚未就绪(状态不允许该操作,如未上传完不能下载)。
    /// (ErrContentNotReady / ErrObjectNotReady / ErrParentNotReady → 409)
    #[error("not ready: {0}")]
    NotReady(String),

    /// 非法状态流转(如对已上传内容再次上传)。
    /// (ErrInvalidUploadState / ErrContentBeingProcessed → 409)
    #[error("invalid state: {0}")]
    InvalidState(String),

    /// 无法识别的状态字符串(DB 边界解析失败)。
    /// (ErrInvalidContentStatus / ErrInvalidObjectStatus → 400)
    #[error("invalid status: {0}")]
    InvalidStatus(String),

    /// 资源冲突:存活唯一索引 (storage_backend_name, object_key) 违例。消息写给用户、可回传。
    #[error("conflict: {0}")]
    Conflict(String),

    /// 存储后端错误(上传/下载/取元数据失败、后端不可用)。
    /// 原始细节(含 key/backend)交 app 落日志,**绝不进响应体**。
    /// (ErrUploadFailed / ErrDownloadFailed / ErrStorageBackendNotFound / ErrNoStorageBackend → 500)
    #[error("storage backend error")]
    Storage(#[source] anyhow::Error),

    /// 兜底:任何 anyhow 错误(DB / IO / 依赖)。原始细节交 app 落日志、绝不进响应体。
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
