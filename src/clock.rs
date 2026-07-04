//! 时间端口(可拔插)。生产用系统时钟;测试注入固定时钟可**确定性**验证 created_at/updated_at、
//! 过期等时间相关逻辑,无需 sleep 真实时间。解耦理由是**具体需求**(测试时间相关逻辑),不是"时间是外部的"。
//! 逐字镜像 idm 的 clock.rs。

use time::OffsetDateTime;

/// 当前时刻来源。service 经它取 now(填元数据时间戳、判生命周期),不直接调 `OffsetDateTime::now_utc()`。
pub trait Clock: Send + Sync {
    fn now(&self) -> OffsetDateTime;
}

/// 生产实现:系统 UTC 时钟。
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}
