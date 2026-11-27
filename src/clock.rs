//! 时间端口(可拔插)。生产用系统时钟;测试可注入固定时钟。
//!
//! **落库的时间戳由它决定**:service 每个写用例经它取一次 now,交给仓储(建行的 `created_at`、
//! 改 / 置状态 / 软删的 `at`、元数据的 `created_at` / `updated_at`),两种仓储实现都按传入值落库。
//! 所以注入固定时钟后,读回来的时间戳就是这个时钟给的值,可以断言绝对值。
//!
//! 解耦理由是**具体需求**(测试时间相关逻辑),不是"时间是外部的"。镜像 idm 的 clock.rs。

use std::sync::Arc;

use time::OffsetDateTime;

/// 当前时刻来源。service 经它取 now,不直接调 `OffsetDateTime::now_utc()`。
pub trait Clock: Send + Sync {
    /// 当前 UTC 时刻。
    fn now(&self) -> OffsetDateTime;
}

/// 转发实现:让 `Arc<dyn Clock>` 也能填 [`ContentService`](crate::ContentService) 的时钟类型参数
/// (下游要在运行时二选一实现时用)。
impl<T: Clock + ?Sized> Clock for Arc<T> {
    fn now(&self) -> OffsetDateTime {
        (**self).now()
    }
}

/// 转发实现:同 `Arc<T>`,给 `Box<dyn Clock>` 用。
impl<T: Clock + ?Sized> Clock for Box<T> {
    fn now(&self) -> OffsetDateTime {
        (**self).now()
    }
}

/// 生产实现:系统 UTC 时钟。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[allow(
    clippy::exhaustive_structs,
    reason = "条款例外(Apply #[non_exhaustive] to public structs with public fields):\
              无状态的系统时钟没有字段,也不会加字段;保留 `SystemClock` 字面量构造"
)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}
