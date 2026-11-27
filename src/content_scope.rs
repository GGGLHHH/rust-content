//! 内容的作用域(多租户):`ContentScope` + 默认租户常量。

use uuid::Uuid;

use crate::content_error::ContentError;

/// 默认租户的固定 UUID(和 rust-idm、baserust 里的同名常量同值)。单租户部署的全部内容都在它名下;
/// 迁移把旧数据里的 nil 租户也回填成它。
pub const DEFAULT_TENANT_ID: Uuid = Uuid::from_u128(1);

/// 内容的作用域 —— service / repo 每个方法的第一个业务参数,决定"看得见哪些行"。
///
/// - `Tenant(id)`:租户内容,只看得到 `tenant_id = id` 的行。**nil 不是合法租户**:用
///   [`ContentScope::try_new_tenant`] 构造(nil → `InvalidInput`);直接写 `Tenant(Uuid::nil())` 的,
///   仓储入口同样拒。
/// - `Personal`:个人内容(如头像,不属于任何租户),库里 `tenant_id` 存 **NULL**;只看得到 NULL 行。
///
/// 两种作用域互相不可见。个人内容的"谁能读写"由消费方在 app 层判断(本库只按 owner 过滤列表)。
/// `#[non_exhaustive]`:以后可能加作用域种类(如平台级),消费方 `match` 要留通配分支。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ContentScope {
    /// 某个租户的内容(`tenant_id = id`)。
    Tenant(Uuid),
    /// 个人内容(`tenant_id IS NULL`)。
    Personal,
}

impl ContentScope {
    /// 构造租户作用域。
    ///
    /// # Errors
    ///
    /// `id` 是 nil UUID → [`ContentError::InvalidInput`]。
    pub fn try_new_tenant(id: Uuid) -> Result<Self, ContentError> {
        let scope = ContentScope::Tenant(id);
        scope.validate()?;
        Ok(scope)
    }

    /// 行上应存的 `tenant_id`:租户 → `Some(id)`,个人 → `None`(NULL)。
    #[must_use]
    pub fn tenant_id(self) -> Option<Uuid> {
        match self {
            ContentScope::Tenant(id) => Some(id),
            ContentScope::Personal => None,
        }
    }

    /// 一行(其 `tenant_id` 列为 `tenant_id`)是否属于本作用域:租户 ↔ 同一 id,个人 ↔ NULL。
    pub(crate) fn contains(self, tenant_id: Option<Uuid>) -> bool {
        self.tenant_id() == tenant_id
    }

    /// 校验作用域本身合法(nil 租户 → `InvalidInput`)。仓储每个方法入口都调。
    pub(crate) fn validate(self) -> Result<(), ContentError> {
        match self {
            ContentScope::Tenant(id) if id.is_nil() => Err(ContentError::InvalidInput(
                "tenant id must not be nil".to_owned(),
            )),
            ContentScope::Tenant(_) | ContentScope::Personal => Ok(()),
        }
    }
}
