//! content 领域实体 —— 映射 content / content_metadata / object / object_metadata 四表。
//! 派生表 `content_derived`(迁移 0002)目前只有表结构,没有对应的 Rust 类型。
//! 审计字段(created_by/updated_by)与 `deleted_at` 是仓储内部细节,**不上**公开实体
//! (镜像 idm 的 `User` vs 内部 `UserRow`)。`status` 在域里 typed,仓储在 VARCHAR 边界互转。
//!
//! `ContentMetadata` / `ObjectMetadata` 直接 derive `sqlx::FromRow`(无 deleted_at/审计,字段与列一一对应);
//! `Content` / `Object` 因 `status` 是枚举,改由仓储内部 row 结构(status: String)转换,不在此 derive。
//!
//! 实体都 `#[non_exhaustive]`(表结构会随迁移长字段):库外不能用字面量构造,要自己实现仓储端口的,
//! 用各自的 `new` 建出"刚创建"的实体,再改公开字段。

use serde_json::{Map, Value};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::content_scope::ContentScope;
use crate::content_status::ContentStatus;
use crate::object_status::ObjectStatus;
use crate::repo::{NewContent, NewObject};

/// 内容主体(表 `content`)。
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Content {
    /// 主键(UUIDv7,按它排序 = 按创建先后)。
    pub id: Uuid,
    /// 所属租户;个人内容(`ContentScope::Personal`)为 `None`。
    pub tenant_id: Option<Uuid>,
    /// 所有者(用户等,由 `owner_type` 区分种类)。
    pub owner_id: Uuid,
    /// 所有者种类(如 `"user"`),调用方自定。
    pub owner_type: Option<String>,
    /// 展示名。
    pub name: Option<String>,
    /// 描述。
    pub description: Option<String>,
    /// 文档类型(调用方自定的分类)。
    pub document_type: Option<String>,
    /// 生命周期状态(域里 typed;仓储映射到/自 VARCHAR)。
    pub status: ContentStatus,
    /// `"original"` | `"derived"`(保留;无 derived 逻辑也照存,廉价)。
    pub derivation_type: Option<String>,
    /// 创建时刻(service 经注入的时钟取,仓储按传入值落库)。
    pub created_at: OffsetDateTime,
    /// 最后修改时刻(同上:每次写由 service 给出)。
    pub updated_at: OffsetDateTime,
}

impl Content {
    /// 刚建好的内容行:`status = Created`,`tenant_id` 取自 `scope`,
    /// `created_at = updated_at = input.created_at`。
    /// 自己实现 [`ContentRepo`](crate::ContentRepo) 的仓储用它组装 `create` 的返回值。
    #[must_use]
    pub fn new(id: Uuid, scope: ContentScope, input: NewContent) -> Self {
        Self {
            id,
            tenant_id: scope.tenant_id(),
            owner_id: input.owner_id,
            owner_type: input.owner_type,
            name: input.name,
            description: input.description,
            document_type: input.document_type,
            status: ContentStatus::Created,
            derivation_type: input.derivation_type,
            created_at: input.created_at,
            updated_at: input.created_at,
        }
    }
}

/// 内容自由表单元数据(表 `content_metadata`,1:1 挂 content,PK=content_id,upsert)。
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
#[non_exhaustive]
pub struct ContentMetadata {
    /// 所属内容。
    pub content_id: Uuid,
    /// 标签。
    pub tags: Vec<String>,
    /// 字节数(上传后从后端补)。
    pub file_size: Option<i64>,
    /// 原始文件名(可编辑;预签名下载 URL 优先用它)。
    pub file_name: Option<String>,
    /// MIME 类型(展示 / Content-Type 用)。
    pub mime_type: Option<String>,
    /// 校验和。
    pub checksum: Option<String>,
    /// 校验和算法(如 `"sha256"`)。
    pub checksum_algorithm: Option<String>,
    /// 自由表单 JSONB。
    pub metadata: Value,
    /// 创建时刻。仓储按传入值落库,但只用于新插入的行:已存在的行保留原值。
    pub created_at: OffsetDateTime,
    /// 最后修改时刻(仓储按传入值落库)。
    pub updated_at: OffsetDateTime,
}

impl ContentMetadata {
    /// 空元数据:无标签、可选字段全 `None`、`metadata = {}`,两个时间戳都是 `at`。
    /// 调用方再按需改公开字段。
    #[must_use]
    pub fn new(content_id: Uuid, at: OffsetDateTime) -> Self {
        Self {
            content_id,
            tags: Vec::new(),
            file_size: None,
            file_name: None,
            mime_type: None,
            checksum: None,
            checksum_algorithm: None,
            metadata: Value::Object(Map::new()),
            created_at: at,
            updated_at: at,
        }
    }
}

/// 存储后端里的一份字节(表 `object`)。
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Object {
    /// 主键(UUIDv7)。
    pub id: Uuid,
    /// 所属内容。
    pub content_id: Uuid,
    /// 存储后端名(和 `object_key` 一起在存活对象里全局唯一)。
    pub storage_backend_name: String,
    /// 存储级别(后端自定,如 S3 的 storage class)。
    pub storage_class: Option<String>,
    /// 后端内的 key。
    pub object_key: String,
    /// 建行时的原始文件名(之后不可变)。
    pub file_name: Option<String>,
    /// 版本号。目前恒为 1(多版本以后再做)。
    pub version: i32,
    /// 对象种类(调用方自定)。
    pub object_type: Option<String>,
    /// 生命周期状态。
    pub status: ObjectStatus,
    /// 创建时刻(service 经注入的时钟取,仓储按传入值落库)。
    pub created_at: OffsetDateTime,
    /// 最后修改时刻(同上:每次写由 service 给出)。
    pub updated_at: OffsetDateTime,
}

impl Object {
    /// 刚建好的对象行:`version = 1`、`status = Created`,`created_at = updated_at = input.created_at`。
    /// 自己实现 [`ObjectRepo`](crate::ObjectRepo) 的仓储用它组装 `create` 的返回值。
    #[must_use]
    pub fn new(id: Uuid, input: NewObject) -> Self {
        Self {
            id,
            content_id: input.content_id,
            storage_backend_name: input.storage_backend_name,
            storage_class: input.storage_class,
            object_key: input.object_key,
            file_name: input.file_name,
            version: 1,
            object_type: input.object_type,
            status: ObjectStatus::Created,
            created_at: input.created_at,
            updated_at: input.created_at,
        }
    }
}

/// 对象自由表单元数据(表 `object_metadata`,1:1 挂 object,PK=object_id,upsert)。
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
#[non_exhaustive]
pub struct ObjectMetadata {
    /// 所属对象。
    pub object_id: Uuid,
    /// 字节数(从后端读回)。
    pub size_bytes: Option<i64>,
    /// MIME 类型。
    pub mime_type: Option<String>,
    /// 后端给的 ETag。
    pub etag: Option<String>,
    /// 自由表单 JSONB。
    pub metadata: Value,
    /// 创建时刻。仓储按传入值落库,但只用于新插入的行:已存在的行保留原值。
    pub created_at: OffsetDateTime,
    /// 最后修改时刻(仓储按传入值落库)。
    pub updated_at: OffsetDateTime,
}

impl ObjectMetadata {
    /// 空元数据:可选字段全 `None`、`metadata = {}`,两个时间戳都是 `at`。调用方再按需改公开字段。
    #[must_use]
    pub fn new(object_id: Uuid, at: OffsetDateTime) -> Self {
        Self {
            object_id,
            size_bytes: None,
            mime_type: None,
            etag: None,
            metadata: Value::Object(Map::new()),
            created_at: at,
            updated_at: at,
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::input::{
        CreateContentInput, PrepareUploadInput, SetContentMetadataInput, UploadContentInput,
    };
    use crate::repo::ObjectLocation;

    /// `Content::new` / `Object::new` 组装的是"刚创建"的行:状态 Created、版本 1、两个时间戳相等、租户取自作用域。
    #[test]
    fn entity_constructors_build_freshly_created_rows() {
        let at = OffsetDateTime::UNIX_EPOCH;
        let owner = Uuid::now_v7();
        let tenant = Uuid::now_v7();
        let mut input = NewContent::new(owner, at);
        input.name = Some("n".to_owned());
        let content = Content::new(Uuid::now_v7(), ContentScope::Tenant(tenant), input);
        assert_eq!(content.tenant_id, Some(tenant));
        assert_eq!(content.owner_id, owner);
        assert_eq!(content.name.as_deref(), Some("n"));
        assert_eq!(content.status, ContentStatus::Created);
        assert_eq!((content.created_at, content.updated_at), (at, at));
        let personal = Content::new(
            Uuid::now_v7(),
            ContentScope::Personal,
            NewContent::new(owner, at),
        );
        assert_eq!(personal.tenant_id, None, "个人内容的 tenant_id 是 None");

        let location = ObjectLocation {
            storage_backend_name: "s3".to_owned(),
            object_key: "k".to_owned(),
        };
        let object = Object::new(Uuid::now_v7(), NewObject::new(content.id, location, at));
        assert_eq!(object.content_id, content.id);
        assert_eq!(object.storage_backend_name, "s3");
        assert_eq!(object.object_key, "k");
        assert_eq!(object.version, 1);
        assert_eq!(object.status, ObjectStatus::Created);
        assert_eq!((object.created_at, object.updated_at), (at, at));
    }

    /// 元数据 / 输入的 `new`:可选字段全空,自由表单 JSONB 是 `{}`(和服务里 "None → {}" 一致)。
    #[test]
    fn metadata_and_input_constructors_default_to_empty() {
        let at = OffsetDateTime::UNIX_EPOCH;
        let id = Uuid::now_v7();
        let empty = serde_json::json!({});

        let content_metadata = ContentMetadata::new(id, at);
        assert!(content_metadata.tags.is_empty());
        assert_eq!(content_metadata.file_size, None);
        assert_eq!(content_metadata.metadata, empty);
        let object_metadata = ObjectMetadata::new(id, at);
        assert_eq!(object_metadata.size_bytes, None);
        assert_eq!(object_metadata.metadata, empty);
        assert_eq!(SetContentMetadataInput::new(id).metadata, empty);

        assert_eq!(CreateContentInput::new(id).derivation_type, None);
        let upload = UploadContentInput::new(id, Bytes::from_static(b"x"));
        assert_eq!((upload.object_key, upload.custom_metadata), (None, None));
        assert!(PrepareUploadInput::new(id).tags.is_empty());
    }
}
