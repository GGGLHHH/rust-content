//! content 仓储:端口 `ContentRepo`(content + content_metadata)、`ObjectRepo`(object + object_metadata)
//! 及其内存 / Postgres 实现。本模块只放子模块声明和 re-export(指南「Don't implement APIs in non-leaf modules」)。
//!
//! 沿"内容聚合 vs 对象聚合"的自然边界把 Go 的单体 `Repository` 拆两个端口,镜像 idm 拆
//! UserRepo/SessionRepo/RoleRepo。范式同 widget:trait 端口 + 内存/PG 实现分文件,service 依赖 trait。
//!
//! 写操作收 `by: Option<&str>`(审计主体,app 传);读自动过滤软删;一律返 `ContentError`。
//!
//! **时间戳由调用方给**:写操作收时间(`NewContent` / `NewObject` 的 `created_at`、`update` / `set_status` /
//! `soft_delete` 的 `at`、元数据结构里的 `created_at` / `updated_at`),实现按传入值落库,不读系统时钟、
//! 不靠 DB `now()` / 默认值;service 经注入的 `Clock` 取这个值。PG 的 `set_updated_at_utc` 触发器在仓储事务里
//! 不覆盖显式写入的 `updated_at`(迁移 0004 + `pg_support::scoped_tx` 设的事务级开关)。
//!
//! **作用域**:每个方法第一个参数是 `scope: ContentScope`,只读写本作用域的行(`Tenant(id)` ↔ `tenant_id = id`,
//! `Personal` ↔ `tenant_id IS NULL`);别的作用域的行按"不存在"处理(取单条 / 写 / 删 → `NotFound`,批量 / 列表里缺席)。
//! nil 租户 → `InvalidInput`。内存实现没有 RLS,这个参数是它唯一的防线;PG 实现另有 RLS 兜底(见 `pg_support`)。
//! `NewContent`/`NewObject` 是仓储输入(无 id:仓储生成 `Uuid::now_v7()`;时间戳取输入里的 `created_at`)。
//!
//! **没有跨 repo 事务**(transactions skill:原子单元=一个 trait 方法)。`upload_content`
//! 把建 content 行、建 object 行、推字节、翻状态作为**各自独立**的调用编排(见 content_service.rs),不跨两个
//! 仓储包一个 tx。中途失败的孤儿行风险是 Go 版已记录的行为,有意接受(见 content_service.rs 注释)。
//!
//! 命名里的 `Repo`(repository)、`Pg`(PostgreSQL)保留缩写:落在指南「Avoid abbreviations」的例外
//! (Abbreviations that are unlikely to be confusing to people outside of your project),
//! 和 sqlx 的 `PgPool`、下游 baserust 的 `*Repo` 端口同一套叫法。

mod content_repo;
mod in_memory_content_repo;
mod in_memory_object_repo;
mod object_repo;
mod pg_content_repo;
mod pg_object_repo;
mod pg_support;
mod scoped_metadata;
mod tables;

pub use content_repo::{ContentRepo, NewContent};
pub use in_memory_content_repo::InMemoryContentRepo;
pub use in_memory_object_repo::InMemoryObjectRepo;
pub use object_repo::{NewObject, ObjectLocation, ObjectRepo};
pub use pg_content_repo::PgContentRepo;
pub use pg_object_repo::PgObjectRepo;
#[cfg(feature = "pg-conformance")]
pub use pg_support::scoped_tx;
