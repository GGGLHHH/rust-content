//! PG 仓储共用的件:带作用域的事务、作用域 SQL 条件、库错误映射。
//!
//! **作用域(多租户)**:每个仓储方法都是一个事务 —— [`scoped_tx`] 开事务、**第一条语句**用事务级
//! `set_config(..., true)` 设上下文(租户:`app.tenant_id`;个人:`app.personal_scope = 'on'`;另一个变量同时清成 `''`),
//! 然后业务 SQL **同时**带作用域条件(`tenant_id = $1` / `tenant_id IS NULL`,走索引、也是内存实现的同一语义),
//! 最后 commit。表上的 RLS(迁移 0003,ENABLE + FORCE)是第二道防线:忘了带条件也串不了租户。
//! 不在借出连接时设会话级变量(以后加 PgBouncer 也不会串租户)。
//!
//! **时间戳**:同一条语句还设事务级 `app.explicit_updated_at = 'on'`。仓储的写都显式带 `updated_at`
//! (调用方经注入的时钟给的值);`set_updated_at_utc` 触发器(迁移 0004)看到这个开关就不再改写它 ——
//! 否则显式值恰好等于旧值时(同一个 now 先建行再翻状态、固定时钟),触发器分不清「没改」和「改成同一个值」,
//! 会把它换成 DB 的 `now()`。仓储之外的 SQL 不设这个开关,照旧由触发器维护。

use sea_query::{Expr, ExprTrait, IntoColumnRef};
use sqlx::{Error, PgPool, Postgres, Transaction};

use crate::content_error::ContentError;
use crate::content_scope::ContentScope;

/// 库错误 → `Internal`(原始进日志)。
pub(super) fn internal(error: Error) -> ContentError {
    ContentError::Internal(error.into())
}

/// 唯一冲突(撞存活唯一索引)→ `Conflict`(带 `message`);其它库错误 → `Internal`(原始进日志)。
pub(super) fn map_unique(error: Error, message: &str) -> ContentError {
    if let Error::Database(database_error) = &error {
        if database_error.is_unique_violation() {
            return ContentError::Conflict(message.to_owned());
        }
    }
    internal(error)
}

/// 开一个带作用域的事务:校验作用域(nil 租户 → `InvalidInput`)→ `begin` →
/// 第一条语句 `select set_config(..., true)`(事务级,commit / rollback 后自动失效)。
/// 同一条语句另设 `app.explicit_updated_at = 'on'`:本事务里的写按传入的 `updated_at` 落库(见模块文档)。
///
/// **两个变量一起设**:本作用域的那个设上,另一个显式清成 `''`。RLS 策略是「租户分支 OR 个人分支」,
/// 连接上若有会话级残留(别的代码 / 连接池中间件 `set app.personal_scope = 'on'` 之类没清掉),只设本作用域的变量
/// 会让另一支也放行 —— 租户事务里看得到个人内容,反之亦然。清成 `''`:租户分支 `nullif('', '')::uuid` 是 NULL、
/// 个人分支要求 `= 'on'`,两支都不放行。
///
/// `pub` 只为 `pg-conformance` 测试经 `content::__pg_testing` 直接拿到它(所在模块私有,平时不对外)。
///
/// # Errors
///
/// nil 租户 → `InvalidInput`;连库 / 执行失败 → `Internal`。
pub async fn scoped_tx(
    pool: &PgPool,
    scope: ContentScope,
) -> Result<Transaction<'static, Postgres>, ContentError> {
    scope.validate()?;
    let mut tx = pool.begin().await.map_err(internal)?;
    let query = match scope {
        ContentScope::Tenant(id) => sqlx::query(
            "select set_config('app.tenant_id', $1, true), set_config('app.personal_scope', '', true), \
             set_config('app.explicit_updated_at', 'on', true)",
        )
        .bind(id.to_string()),
        ContentScope::Personal => sqlx::query(
            "select set_config('app.personal_scope', 'on', true), set_config('app.tenant_id', '', true), \
             set_config('app.explicit_updated_at', 'on', true)",
        ),
    };
    query.execute(&mut *tx).await.map_err(internal)?;
    Ok(tx)
}

/// SQL 里的作用域条件(和 RLS 同义,显式写出来走 `(tenant_id, …)` 索引)。
pub(super) fn scope_condition<C: IntoColumnRef>(column: C, scope: ContentScope) -> Expr {
    match scope.tenant_id() {
        Some(tenant_id) => Expr::col(column).eq(tenant_id),
        None => Expr::col(column).is_null(),
    }
}
