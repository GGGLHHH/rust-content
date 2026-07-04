//! content 服务的领域输入(纯数据:无 HTTP / 序列化 / 校验)。
//! HTTP 边界的反序列化 + 校验由消费方(app)做完,再 `.into()` 成这些结构传进 service。
//! 审计主体(created_by/updated_by)不在输入里 —— 由 service 方法的 `by: Option<String>` 参数单独传。

use bytes::Bytes;
use uuid::Uuid;

/// 创建内容(仅建 content 行,不碰对象/字节)。
#[derive(Debug, Clone)]
pub struct CreateContentInput {
    pub tenant_id: Uuid,
    pub owner_id: Uuid,
    pub owner_type: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub document_type: Option<String>,
    /// 不传则服务默认 "original"。
    pub derivation_type: Option<String>,
}

/// **全量更新**内容的可编辑字段(PUT 语义:都替换;非编辑字段 tenant/owner/status/derivation 不动)。
#[derive(Debug, Clone)]
pub struct UpdateContentInput {
    pub owner_type: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub document_type: Option<String>,
}

/// 一次性上传:建 content + object 行、推字节、同步元数据、翻状态(Go `UploadContent` 流)。
#[derive(Debug, Clone)]
pub struct UploadContentInput {
    pub tenant_id: Uuid,
    pub owner_id: Uuid,
    pub owner_type: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub document_type: Option<String>,
    /// 后端内的 key。`None` 则服务 mint 默认 `{content_id}/{uuid}`(不依赖 Go 的 objectkey 包)。
    pub object_key: Option<String>,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    pub tags: Vec<String>,
    /// 落 content_metadata.metadata 的自由表单 JSONB(`None` → `{}`)。
    pub custom_metadata: Option<serde_json::Value>,
    /// 要上传的字节(v0.1 缓冲整体;流式是 DEFER)。
    pub data: Bytes,
}

/// 设置内容元数据(全量替换,upsert)。
#[derive(Debug, Clone)]
pub struct SetContentMetadataInput {
    pub content_id: Uuid,
    pub tags: Vec<String>,
    pub file_size: Option<i64>,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    pub checksum: Option<String>,
    pub checksum_algorithm: Option<String>,
    pub metadata: serde_json::Value,
}
