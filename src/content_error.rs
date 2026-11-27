//! content 的领域错误类型(**零 HTTP**)。HTTP 状态码 / 机器码 / wire 形状一律由消费方(app)在
//! `From<ContentError> for AppError` 的边界决定 —— 本库只暴露"出了哪类错"。
//!
//! 把 Go 的 16 个 sentinel + 3 个上下文包装(ContentError/ObjectError/StorageError)收敛成一个精简枚举,
//! 按 app 将映射的 HTTP 语义分组:not-found→404、state→409、bad-status→400、storage/internal→500。
//! `Storage` 与 `Internal` 分开:app 可对后端错误(含 key/backend 上下文)单独落日志,但都不进响应体。
//!
//! 不实现 `Copy`(指南「Error types: Implement the Copy trait whenever possible」):
//! 消息变体带 `String`、`Storage` / `Internal` 带 `anyhow::Error`,都不是 `Copy`。

/// content 库所有操作的错误。`#[non_exhaustive]`:以后加变体不算破坏性变更,消费方 `match` 要留通配分支。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
#[must_use]
pub enum ContentError {
    /// 资源不存在 / 已软删 / 无可下载对象 / 属于别的作用域。
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

    /// 调用方给的入参不合法(如 nil 租户 —— nil UUID 永远不是合法租户)。消息可回传。
    #[error("invalid input: {0}")]
    InvalidInput(String),

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
