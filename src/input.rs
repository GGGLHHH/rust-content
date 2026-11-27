//! content 服务的领域输入(纯数据:无 HTTP / 序列化 / 校验)。
//! HTTP 边界的反序列化 + 校验由消费方(app)做完,再组装成这些结构传进 service。
//! 审计主体(created_by/updated_by)不在输入里 —— 由 service 方法的 `by: Option<&str>` 参数单独传。
//! 租户也不在输入里 —— 由 service 方法的第一个参数 `scope: ContentScope` 给出。
//!
//! 有必填字段的输入是 `#[non_exhaustive]` + `new(必填字段)`(可选字段默认 `None` / 空,构造后直接改公开字段);
//! 全是可选字段的 [`UpdateContentInput`] 走选项结构体的路子(`Default` + 结构体更新语法)。

use bytes::Bytes;
use serde_json::{Map, Value};
use uuid::Uuid;

/// 创建内容(仅建 content 行,不碰对象/字节)。
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct CreateContentInput {
    /// 所有者。
    pub owner_id: Uuid,
    /// 所有者种类。
    pub owner_type: Option<String>,
    /// 展示名。
    pub name: Option<String>,
    /// 描述。
    pub description: Option<String>,
    /// 文档类型。
    pub document_type: Option<String>,
    /// 不传则服务默认 `"original"`。
    pub derivation_type: Option<String>,
}

impl CreateContentInput {
    /// 只填所有者,其余可选字段为 `None`。
    #[must_use]
    pub fn new(owner_id: Uuid) -> Self {
        Self {
            owner_id,
            owner_type: None,
            name: None,
            description: None,
            document_type: None,
            derivation_type: None,
        }
    }
}

/// **全量更新**内容的可编辑字段(PUT 语义:都替换;非编辑字段 tenant/owner/status/derivation 不动)。
/// `Default` = 全部清空。
#[derive(Clone, Debug, Default, PartialEq)]
#[allow(
    clippy::exhaustive_structs,
    reason = "条款例外(Apply #[non_exhaustive] to public structs with public fields):\
              选项结构体,要让调用方用 `UpdateContentInput { name, ..Default::default() }` 的结构体更新语法"
)]
pub struct UpdateContentInput {
    /// 所有者种类。
    pub owner_type: Option<String>,
    /// 展示名。
    pub name: Option<String>,
    /// 描述。
    pub description: Option<String>,
    /// 文档类型。
    pub document_type: Option<String>,
}

/// 一次性上传:建 content + object 行、推字节、同步元数据、翻状态(Go `UploadContent` 流)。
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct UploadContentInput {
    /// 所有者。
    pub owner_id: Uuid,
    /// 所有者种类。
    pub owner_type: Option<String>,
    /// 展示名。
    pub name: Option<String>,
    /// 描述。
    pub description: Option<String>,
    /// 文档类型。
    pub document_type: Option<String>,
    /// 后端内的 key。`None` 则服务按作用域生成:租户 `tenants/{tenant_id}/{content_id}/{uuid}`、
    /// 个人 `personal/{owner_id}/{content_id}/{uuid}`。给了就原样用,但必须以本作用域前缀开头
    /// (租户 `tenants/{tenant_id}/`、个人 `personal/{owner_id}/`),前缀后非空、不含 `..` 段,否则 `InvalidInput`。
    pub object_key: Option<String>,
    /// 原始文件名(写进对象行和元数据,供 Content-Disposition)。
    pub file_name: Option<String>,
    /// MIME 类型(后端读不回时用它)。
    pub mime_type: Option<String>,
    /// 标签。
    pub tags: Vec<String>,
    /// 落 content_metadata.metadata 的自由表单 JSONB(`None` → `{}`)。
    pub custom_metadata: Option<Value>,
    /// 要上传的字节(缓冲整体;流式以后再做)。
    pub data: Bytes,
}

impl UploadContentInput {
    /// 只填所有者和字节,其余可选字段为 `None` / 空。
    #[must_use]
    pub fn new(owner_id: Uuid, data: Bytes) -> Self {
        Self {
            owner_id,
            owner_type: None,
            name: None,
            description: None,
            document_type: None,
            object_key: None,
            file_name: None,
            mime_type: None,
            tags: Vec::new(),
            custom_metadata: None,
            data,
        }
    }
}

/// 两步上传第一步(prepare)的入参 —— `UploadContentInput` 减 `data`(字节由调用方直传后端)。
/// **不复用** UploadContentInput:`data` 没有合法占位值,别用空 Bytes 骗类型。
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct PrepareUploadInput {
    /// 所有者。
    pub owner_id: Uuid,
    /// 所有者种类。
    pub owner_type: Option<String>,
    /// 展示名。
    pub name: Option<String>,
    /// 描述。
    pub description: Option<String>,
    /// 文档类型。
    pub document_type: Option<String>,
    /// 后端内的 key。`None` 则服务按作用域生成;给了必须在本作用域前缀下
    /// (规则同 [`UploadContentInput::object_key`])。
    pub object_key: Option<String>,
    /// 原始文件名。
    pub file_name: Option<String>,
    /// MIME 类型(后端支持时签进直传凭证)。
    pub mime_type: Option<String>,
    /// 标签。
    pub tags: Vec<String>,
    /// 落 content_metadata.metadata 的自由表单 JSONB(`None` → `{}`)。
    pub custom_metadata: Option<Value>,
}

impl PrepareUploadInput {
    /// 只填所有者,其余可选字段为 `None` / 空。
    #[must_use]
    pub fn new(owner_id: Uuid) -> Self {
        Self {
            owner_id,
            owner_type: None,
            name: None,
            description: None,
            document_type: None,
            object_key: None,
            file_name: None,
            mime_type: None,
            tags: Vec::new(),
            custom_metadata: None,
        }
    }
}

/// 设置内容元数据(全量替换,upsert)。
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct SetContentMetadataInput {
    /// 目标内容。
    pub content_id: Uuid,
    /// 标签。
    pub tags: Vec<String>,
    /// 字节数。
    pub file_size: Option<i64>,
    /// 文件名(改它,之后签的下载 URL 用新名)。
    pub file_name: Option<String>,
    /// MIME 类型。
    pub mime_type: Option<String>,
    /// 校验和。
    pub checksum: Option<String>,
    /// 校验和算法。
    pub checksum_algorithm: Option<String>,
    /// 自由表单 JSONB。
    pub metadata: Value,
}

impl SetContentMetadataInput {
    /// 全空的元数据:无标签、可选字段 `None`、`metadata = {}`(整体替换时等于清空)。
    #[must_use]
    pub fn new(content_id: Uuid) -> Self {
        Self {
            content_id,
            tags: Vec::new(),
            file_size: None,
            file_name: None,
            mime_type: None,
            checksum: None,
            checksum_algorithm: None,
            metadata: Value::Object(Map::new()),
        }
    }
}
