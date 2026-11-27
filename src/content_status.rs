//! 内容的生命周期状态(DB 边界 stringly-typed,服务侧 typed)+ 内容级流转守卫。
//!
//! `Deleted` 变体**故意丢弃**(Go 标其 deprecated):软删走 `deleted_at`,status 列停在最后操作态,
//! 镜像 idm "软删经 deleted_at、status 不动"。
//!
//! 守卫是 `pub(crate)` 方法,返回 `Result<(), ContentError>`。目前只有服务真正用到的下载守卫;
//! 上传守卫、`can_create_derived` / `can_delete(force)` / 完整 uploading→processing 机等用到时再加。

use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use crate::content_error::ContentError;

/// 内容状态。`Uploaded` = 原始内容终态;`Processed` = 派生内容终态(派生为 DEFER,但状态值保留)。
/// `#[non_exhaustive]`:状态机还会长(见模块文档),消费方 `match` 要留通配分支。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ContentStatus {
    /// 已建行,字节未上传。
    Created,
    /// 上传中。
    Uploading,
    /// 原始内容的终态:字节已落后端并销账。
    Uploaded,
    /// 派生处理中(派生是 DEFER,状态值保留)。
    Processing,
    /// 派生内容的终态。
    Processed,
    /// 上传 / 处理失败。
    Failed,
    /// 已归档(仍可下载)。
    Archived,
}

impl ContentStatus {
    /// 与 DB VARCHAR 列双向映射的字符串(`FromStr` 的逆)。
    #[must_use]
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

    /// 是否可下载 / 可展示:{Uploaded, Processed, Archived}。也是 `confirm_upload` 判"已过账"的集合。
    pub(crate) fn is_downloadable(self) -> bool {
        match self {
            ContentStatus::Uploaded | ContentStatus::Processed | ContentStatus::Archived => true,
            ContentStatus::Created
            | ContentStatus::Uploading
            | ContentStatus::Processing
            | ContentStatus::Failed => false,
        }
    }

    /// 流转守卫:可下载 → `Ok`;否则 `NotReady`。
    pub(crate) fn check_downloadable(self) -> Result<(), ContentError> {
        if self.is_downloadable() {
            return Ok(());
        }
        Err(ContentError::NotReady(format!(
            "content not downloadable (status: {self})"
        )))
    }
}

// `fmt::Result` 带模块名:和 `std::result::Result` 同名(指南「Type names should be imported and used
// unqualified」的歧义例外)。
impl Display for ContentStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 从 DB 字符串解析(`as_str` 的逆)。未知值 → [`ContentError::InvalidStatus`]。
impl FromStr for ContentStatus {
    type Err = ContentError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
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

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [ContentStatus; 7] = [
        ContentStatus::Created,
        ContentStatus::Uploading,
        ContentStatus::Uploaded,
        ContentStatus::Processing,
        ContentStatus::Processed,
        ContentStatus::Failed,
        ContentStatus::Archived,
    ];

    /// `as_str` / `Display` / `FromStr` 三者一致、可往返;未知字符串 → `InvalidStatus`(带原值)。
    #[test]
    fn str_round_trip_and_unknown_is_invalid_status() {
        for status in ALL {
            assert_eq!(status.to_string(), status.as_str());
            let parsed: ContentStatus = status.as_str().parse().unwrap();
            assert_eq!(parsed, status, "{status} 应能往返");
        }
        let unknown: Result<ContentStatus, ContentError> = "deleted".parse();
        let err = unknown.unwrap_err();
        assert!(
            matches!(&err, ContentError::InvalidStatus(s) if s == "deleted"),
            "未知状态应报 InvalidStatus 并带原值,实际 {err:?}"
        );
    }
}
