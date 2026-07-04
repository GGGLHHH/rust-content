//! content repo 契约一致性:**同一批断言对内存与 PG 的两个 repo 各跑一遍**,钉死行为 parity。
//! 覆盖 content(create/get/get_many/update/set_status/soft_delete/list/metadata upsert)与
//! object(create/get/list_by_content/get_by_key/(backend,key) Conflict/soft_delete/metadata upsert)。
//! "内存绿不保证 PG 绿"的漂移,全靠这套契约抓。
//!
//! 只断言**顺序/相对/可见性**,绝不断言绝对时间戳(PG `now()` ≠ 内存 `now_utc()`)—— 同 widget/idm 的规则。
//! 内存入口:默认 `cargo test` 就跑(零 DB)。PG 入口:`--features pg-conformance`(需连 content role 的 pg)。

use content::{
    Content, ContentError, ContentMetadata, ContentRepo, ContentStatus, NewContent, NewObject,
    ObjectMetadata, ObjectRepo, ObjectStatus,
};
use time::OffsetDateTime;
use uuid::Uuid;

/// 契约唯一真相源。两个 repo 协作(content → object),内存与 PG 都调它。
/// PG 有 FK(object/content_metadata → content、object_metadata → object),故必须先建 content 再建 object/元数据。
async fn content_repo_contract(contents: &dyn ContentRepo, objects: &dyn ObjectRepo) {
    let tenant = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let now = OffsetDateTime::now_utc();

    // ── content:create(by 审计透传)+ get 往返 ──
    let c = contents
        .create(
            NewContent {
                tenant_id: tenant,
                owner_id: owner,
                owner_type: Some("user".into()),
                name: Some("doc".into()),
                description: Some("d".into()),
                document_type: Some("pdf".into()),
                derivation_type: Some("original".into()),
            },
            Some("sys".into()),
        )
        .await
        .unwrap();
    assert_eq!(c.status, ContentStatus::Created);
    let got = contents.get(c.id).await.unwrap();
    assert_eq!(got.name.as_deref(), Some("doc"));
    assert_eq!(got.tenant_id, tenant);
    assert_eq!(got.derivation_type.as_deref(), Some("original"));

    // get 不存在 → NotFound
    assert!(matches!(
        contents.get(Uuid::now_v7()).await,
        Err(ContentError::NotFound)
    ));

    // ── 第二条 content + get_many(命中 / 缺席 / 空集)──
    let c2 = contents
        .create(
            NewContent {
                tenant_id: tenant,
                owner_id: owner,
                owner_type: None,
                name: Some("two".into()),
                description: None,
                document_type: None,
                derivation_type: None,
            },
            None, // 审计 by=None 也可建
        )
        .await
        .unwrap();
    assert_eq!(contents.get_many(&[c.id, c2.id]).await.unwrap().len(), 2);
    assert_eq!(
        contents
            .get_many(&[c.id, Uuid::now_v7()])
            .await
            .unwrap()
            .len(),
        1
    ); // 不存在的缺席
    assert!(contents.get_many(&[]).await.unwrap().is_empty()); // 空集 → 空

    // ── list(owner, tenant):2 条,id desc(c2 较新 → 居首)──
    let listed = contents.list(owner, tenant).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, c2.id); // v7 单列全序,desc → 最新在前
                                     // 不同 owner/tenant 隔离
    assert!(contents
        .list(Uuid::now_v7(), tenant)
        .await
        .unwrap()
        .is_empty());

    // ── update(PUT 全量替换可编辑字段)──
    let updated = Content {
        name: Some("doc2".into()),
        description: None,
        ..got.clone()
    };
    let u = contents.update(&updated, Some("sys".into())).await.unwrap();
    assert_eq!(u.name.as_deref(), Some("doc2"));
    assert_eq!(u.description, None);

    // ── content_metadata upsert 幂等(二次 set 覆盖,后者胜)──
    contents
        .set_metadata(ContentMetadata {
            content_id: c.id,
            tags: vec!["x".into()],
            file_size: Some(10),
            file_name: Some("f".into()),
            mime_type: Some("text/plain".into()),
            checksum: None,
            checksum_algorithm: None,
            metadata: serde_json::json!({"k": "v"}),
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    contents
        .set_metadata(ContentMetadata {
            content_id: c.id,
            tags: vec!["y".into(), "z".into()],
            file_size: Some(20),
            file_name: Some("f2".into()),
            mime_type: None,
            checksum: None,
            checksum_algorithm: None,
            metadata: serde_json::json!({"k": "v2"}),
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let m = contents.get_metadata(c.id).await.unwrap();
    assert_eq!(m.file_size, Some(20)); // 覆盖
    assert_eq!(m.tags, vec!["y".to_string(), "z".to_string()]);
    assert_eq!(m.mime_type, None);
    assert_eq!(m.metadata, serde_json::json!({"k": "v2"}));
    // 无元数据的内容 → NotFound
    assert!(matches!(
        contents.get_metadata(c2.id).await,
        Err(ContentError::NotFound)
    ));
    // get_metadata_many:命中 c、缺席 c2
    let mm = contents.get_metadata_many(&[c.id, c2.id]).await.unwrap();
    assert_eq!(mm.len(), 1);
    assert!(mm.contains_key(&c.id));

    // ── object:create + get 往返 + version=1/Created ──
    let key_a = format!("{}/a", c.id);
    let key_b = format!("{}/b", c.id);
    let o1 = objects
        .create(
            NewObject {
                content_id: c.id,
                storage_backend_name: "memory".into(),
                storage_class: None,
                object_key: key_a.clone(),
                file_name: Some("a".into()),
                object_type: None,
            },
            Some("sys".into()),
        )
        .await
        .unwrap();
    assert_eq!(o1.status, ObjectStatus::Created);
    assert_eq!(o1.version, 1);
    let o2 = objects
        .create(
            NewObject {
                content_id: c.id,
                storage_backend_name: "memory".into(),
                storage_class: None,
                object_key: key_b,
                file_name: None,
                object_type: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(objects.get(o1.id).await.unwrap().object_key, key_a);
    assert!(matches!(
        objects.get(Uuid::now_v7()).await,
        Err(ContentError::NotFound)
    ));

    // ── (storage_backend_name, object_key) 存活唯一 → Conflict ──
    assert!(matches!(
        objects
            .create(
                NewObject {
                    content_id: c.id,
                    storage_backend_name: "memory".into(),
                    storage_class: None,
                    object_key: key_a.clone(),
                    file_name: None,
                    object_type: None,
                },
                None,
            )
            .await,
        Err(ContentError::Conflict(_))
    ));

    // ── list_by_content 排序(id asc = 建序:o1 先于 o2)──
    let objs = objects.list_by_content(c.id).await.unwrap();
    assert_eq!(objs.len(), 2);
    assert_eq!(objs[0].id, o1.id);
    assert_eq!(objs[1].id, o2.id);

    // ── get_by_key(复合唯一查)──
    assert_eq!(
        objects.get_by_key(&key_a, "memory").await.unwrap().id,
        o1.id
    );
    assert!(matches!(
        objects.get_by_key("nope", "memory").await,
        Err(ContentError::NotFound)
    ));

    // ── set_status object → Uploaded ──
    objects
        .set_status(o1.id, ObjectStatus::Uploaded, Some("sys".into()))
        .await
        .unwrap();
    assert_eq!(
        objects.get(o1.id).await.unwrap().status,
        ObjectStatus::Uploaded
    );

    // ── object_metadata upsert 幂等(后者胜)──
    objects
        .set_metadata(ObjectMetadata {
            object_id: o1.id,
            size_bytes: Some(5),
            mime_type: Some("text/plain".into()),
            etag: Some("e1".into()),
            metadata: serde_json::json!({}),
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    objects
        .set_metadata(ObjectMetadata {
            object_id: o1.id,
            size_bytes: Some(7),
            mime_type: None,
            etag: Some("e2".into()),
            metadata: serde_json::json!({"a": 1}),
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap();
    let om = objects.get_metadata(o1.id).await.unwrap();
    assert_eq!(om.size_bytes, Some(7));
    assert_eq!(om.etag.as_deref(), Some("e2"));
    assert!(matches!(
        objects.get_metadata(o2.id).await,
        Err(ContentError::NotFound)
    ));

    // ── content set_status → Uploaded ──
    contents
        .set_status(c.id, ContentStatus::Uploaded, None)
        .await
        .unwrap();
    assert_eq!(
        contents.get(c.id).await.unwrap().status,
        ContentStatus::Uploaded
    );

    // ── object soft_delete:get 消失 + (backend,key) 释放可复用 + 二次删 NotFound ──
    objects.soft_delete(o1.id, None).await.unwrap();
    assert!(matches!(
        objects.get(o1.id).await,
        Err(ContentError::NotFound)
    ));
    let o3 = objects
        .create(
            NewObject {
                content_id: c.id,
                storage_backend_name: "memory".into(),
                storage_class: None,
                object_key: key_a, // 软删后同 key 可复用(部分唯一索引 where deleted_at is null)
                file_name: None,
                object_type: None,
            },
            None,
        )
        .await
        .unwrap();
    assert_ne!(o3.id, o1.id);
    assert!(matches!(
        objects.soft_delete(o1.id, None).await,
        Err(ContentError::NotFound)
    ));

    // ── content soft_delete:get 消失 + get_many 过滤 + 二次删 NotFound ──
    contents
        .soft_delete(c.id, Some("sys".into()))
        .await
        .unwrap();
    assert!(matches!(
        contents.get(c.id).await,
        Err(ContentError::NotFound)
    ));
    assert_eq!(contents.get_many(&[c.id, c2.id]).await.unwrap().len(), 1); // 软删后只剩 c2
    assert!(matches!(
        contents.soft_delete(c.id, None).await,
        Err(ContentError::NotFound)
    ));
}

// ── 入口 1:内存(零 DB,默认 cargo test 就编译+跑)──
#[tokio::test]
async fn memory_satisfies_content_contract() {
    use content::{InMemoryContentRepo, InMemoryObjectRepo};
    content_repo_contract(&InMemoryContentRepo::new(), &InMemoryObjectRepo::new()).await;
}

// ── 入口 2:PG(需 --features pg-conformance + 连 content role 的 pg)──
// bootstrap 内联在本文件(不走 #[path] include):sqlx::migrate! 相对 content crate 的 CARGO_MANIFEST_DIR 解析,稳。
#[cfg(feature = "pg-conformance")]
mod pg {
    use super::content_repo_contract;
    use sqlx::migrate::Migrator;

    /// 编译期内嵌 content crate 的 migrations/(相对 content crate 的 CARGO_MANIFEST_DIR)。
    static CONTENT_MIGRATOR: Migrator = sqlx::migrate!("./migrations");

    /// #[sqlx::test] 的干净临时库:建 content schema + 跑 content crate 自带 migrations。
    async fn bootstrap_content(pool: &sqlx::PgPool) -> anyhow::Result<()> {
        sqlx::query("create schema if not exists content")
            .execute(pool)
            .await?;
        CONTENT_MIGRATOR.run(pool).await?;
        Ok(())
    }

    #[sqlx::test(migrations = false)]
    async fn pg_satisfies_content_contract(pool: sqlx::PgPool) -> sqlx::Result<()> {
        bootstrap_content(&pool)
            .await
            .expect("bootstrap content schema + 跑 migrations");
        let contents = content::PgContentRepo::new(pool.clone());
        let objects = content::PgObjectRepo::new(pool);
        content_repo_contract(&contents, &objects).await;
        Ok(())
    }
}
