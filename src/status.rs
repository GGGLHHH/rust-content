//! 内容/对象的生命周期状态(DB 边界 stringly-typed,服务侧 typed)。
//!
//! `Deleted` 变体**故意丢弃**(Go 标其 deprecated):软删走 `deleted_at`,status 列停在最后操作态,
//! 镜像 idm "软删经 deleted_at、status 不动"。
//!
//! 状态流转守卫(`can_*`)以自由函数落本模块,返回 `Result<(), ContentError>`。SCAFFOLD-CORE 只保
//! 下载/上传四个守卫;`can_create_derived` / `can_delete(force)` / 完整 uploading→processing 机是 DEFER。

use crate::error::ContentError;

/// 内容状态。`Uploaded`=原始内容终态;`Processed`=派生内容终态(派生为 DEFER,但状态值保留)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentStatus {
    Created,
    Uploading,
    Uploaded,
    Processing,
    Processed,
    Failed,
    Archived,
}

impl ContentStatus {
    /// 与 DB VARCHAR 列双向映射的字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            ContentStatus::Created => "created",
            ContentStatus::Uploading => "uploading",
            ContentStatus::Uploaded => "uploaded",
            ContentStatus::Processing => "processing",
            ContentStatus::Processed => "processed",
            ContentStatus::Failed => "failed",
            ContentStatus::Archived => "archived",
        }
    }

    /// 从 DB 字符串解析。未知值 → `InvalidStatus`(400)。
    pub fn from_db(s: &str) -> Result<Self, ContentError> {
        match s {
            "created" => Ok(ContentStatus::Created),
            "uploading" => Ok(ContentStatus::Uploading),
            "uploaded" => Ok(ContentStatus::Uploaded),
            "processing" => Ok(ContentStatus::Processing),
            "processed" => Ok(ContentStatus::Processed),
            "failed" => Ok(ContentStatus::Failed),
            "archived" => Ok(ContentStatus::Archived),
            other => Err(ContentError::InvalidStatus(other.to_owned())),
        }
    }
}

/// 对象状态(无 `Archived` —— 归档是内容级语义)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectStatus {
    Created,
    Uploading,
    Uploaded,
    Processing,
    Processed,
    Failed,
}

impl ObjectStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ObjectStatus::Created => "created",
            ObjectStatus::Uploading => "uploading",
            ObjectStatus::Uploaded => "uploaded",
            ObjectStatus::Processing => "processing",
            ObjectStatus::Processed => "processed",
            ObjectStatus::Failed => "failed",
        }
    }

    pub fn from_db(s: &str) -> Result<Self, ContentError> {
        match s {
            "created" => Ok(ObjectStatus::Created),
            "uploading" => Ok(ObjectStatus::Uploading),
            "uploaded" => Ok(ObjectStatus::Uploaded),
            "processing" => Ok(ObjectStatus::Processing),
            "processed" => Ok(ObjectStatus::Processed),
            "failed" => Ok(ObjectStatus::Failed),
            other => Err(ContentError::InvalidStatus(other.to_owned())),
        }
    }
}

// ── 流转守卫(SCAFFOLD-CORE 子集):允许 → Ok;否则 NotReady/InvalidState ──

/// 内容可下载?允许 {Uploaded, Processed, Archived};否则 `NotReady`。
pub(crate) fn can_download_content(status: ContentStatus) -> Result<(), ContentError> {
    match status {
        ContentStatus::Uploaded | ContentStatus::Processed | ContentStatus::Archived => Ok(()),
        other => Err(ContentError::NotReady(format!(
            "content not downloadable (status: {})",
            other.as_str()
        ))),
    }
}

/// 对象可下载?允许 {Uploaded, Processed};否则 `NotReady`。
#[allow(dead_code)] // download_content 按内容状态把关;对象级守卫供 DEFER 的多对象/派生路径复用。
pub(crate) fn can_download_object(status: ObjectStatus) -> Result<(), ContentError> {
    match status {
        ObjectStatus::Uploaded | ObjectStatus::Processed => Ok(()),
        other => Err(ContentError::NotReady(format!(
            "object not downloadable (status: {})",
            other.as_str()
        ))),
    }
}

/// 内容可上传?允许 {Created, Failed};否则 `InvalidState`。
#[allow(dead_code)] // upload_content 总从新建 Created 行起,守卫此刻平凡;wired 供 DEFER 的异步上传路径复用。
pub(crate) fn can_upload_content(status: ContentStatus) -> Result<(), ContentError> {
    match status {
        ContentStatus::Created | ContentStatus::Failed => Ok(()),
        other => Err(ContentError::InvalidState(format!(
            "content not in an uploadable state (status: {})",
            other.as_str()
        ))),
    }
}

/// 对象可上传?允许 {Created, Failed};否则 `InvalidState`。
#[allow(dead_code)] // 同上:DEFER 的异步/重传路径复用。
pub(crate) fn can_upload_object(status: ObjectStatus) -> Result<(), ContentError> {
    match status {
        ObjectStatus::Created | ObjectStatus::Failed => Ok(()),
        other => Err(ContentError::InvalidState(format!(
            "object not in an uploadable state (status: {})",
            other.as_str()
        ))),
    }
}
