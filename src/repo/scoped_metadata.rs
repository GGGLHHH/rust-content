//! 内存仓储里的元数据行:元数据本体 + 所属租户(PG 里是元数据表的 `tenant_id` 列)。

use uuid::Uuid;

use crate::content_scope::ContentScope;

/// 带所属租户的元数据行(个人内容 = `None`)。
#[derive(Debug)]
pub(super) struct ScopedMetadata<M> {
    pub(super) tenant_id: Option<Uuid>,
    pub(super) metadata: M,
}

impl<M> ScopedMetadata<M> {
    /// 本作用域看得见这一行吗。
    pub(super) fn visible_in(&self, scope: ContentScope) -> bool {
        scope.contains(self.tenant_id)
    }
}
