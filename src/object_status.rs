//! 对象的生命周期状态(DB 边界 stringly-typed,服务侧 typed)。
//! 下载 / 预览的状态闸按内容状态把关(见 `ContentStatus`),对象级暂无流转守卫。

use std::fmt::{self, Display, Formatter};
use std::str::FromStr;

use crate::content_error::ContentError;

/// 对象状态(无 `Archived` —— 归档是内容级语义)。
/// `#[non_exhaustive]`:状态机还会长,消费方 `match` 要留通配分支。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ObjectStatus {
    /// 已建行,字节未上传。
    Created,
    /// 上传中。
    Uploading,
    /// 字节已落后端。
    Uploaded,
    /// 派生处理中(派生是 DEFER,状态值保留)。
    Processing,
    /// 派生处理完成。
    Processed,
    /// 上传 / 处理失败。
    Failed,
}

impl ObjectStatus {
    /// 与 DB VARCHAR 列双向映射的字符串(`FromStr` 的逆)。
    #[must_use]
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
}

// `fmt::Result` 带模块名:和 `std::result::Result` 同名(指南「Type names should be imported and used
// unqualified」的歧义例外)。
impl Display for ObjectStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 从 DB 字符串解析(`as_str` 的逆)。未知值 → [`ContentError::InvalidStatus`]。
impl FromStr for ObjectStatus {
    type Err = ContentError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
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

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [ObjectStatus; 6] = [
        ObjectStatus::Created,
        ObjectStatus::Uploading,
        ObjectStatus::Uploaded,
        ObjectStatus::Processing,
        ObjectStatus::Processed,
        ObjectStatus::Failed,
    ];

    /// `as_str` / `Display` / `FromStr` 三者一致、可往返;未知字符串 → `InvalidStatus`(带原值)。
    #[test]
    fn str_round_trip_and_unknown_is_invalid_status() {
        for status in ALL {
            assert_eq!(status.to_string(), status.as_str());
            let parsed: ObjectStatus = status.as_str().parse().unwrap();
            assert_eq!(parsed, status, "{status} 应能往返");
        }
        let unknown: Result<ObjectStatus, ContentError> = "deleted".parse();
        let err = unknown.unwrap_err();
        assert!(
            matches!(&err, ContentError::InvalidStatus(s) if s == "deleted"),
            "未知状态应报 InvalidStatus 并带原值,实际 {err:?}"
        );
    }
}
