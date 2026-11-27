//! content repo 契约一致性:**同一批断言对内存与 PG 的两个 repo 各跑一遍**,钉死行为 parity。
//! 覆盖 content(create/get/get_many/update/set_status/soft_delete/list/metadata upsert)与
//! object(create/get/list_by_content/get_by_key/(backend,key) Conflict/soft_delete/metadata upsert),
//! 以及**作用域隔离**(`ContentScope::Tenant` / `ContentScope::Personal`):跨租户取单条 → NotFound、
//! 列表只回当前租户、个人内容与租户内容互不可见、nil 租户被拒、新对象 key 带作用域前缀。
//! "内存绿不保证 PG 绿"的漂移,全靠这套契约抓。
//!
//! 只断言**顺序/相对/可见性**,绝不断言绝对时间戳(PG `now()` ≠ 内存 `now_utc()`)—— 同 widget/idm 的规则。
//! 内存入口:默认 `cargo test` 就跑(零 DB)。PG 入口:`just test-pg`(需 DATABASE_URL 连 content role + 跑着的 pg)。
//! PG 侧仓储用**非 owner、NOBYPASSRLS** 的测试运行时账号(`content_rt`)连,RLS 真的生效(见 `pg` 模块)。

use std::sync::Arc;

use bytes::Bytes;
use content::{
    Content, ContentError, ContentMetadata, ContentRepo, ContentScope, ContentService,
    ContentStatus, InMemoryObjectStore, NewContent, NewObject, ObjectMetadata, ObjectRepo,
    ObjectStatus, PrepareUploadInput, UploadContentInput, DEFAULT_TENANT_ID,
};
use time::OffsetDateTime;
use uuid::Uuid;

/// 随机租户作用域(非 nil,构造必过)。
fn tenant() -> ContentScope {
    ContentScope::tenant(Uuid::now_v7()).expect("非 nil 租户")
}

fn new_content(owner: Uuid, name: &str) -> NewContent {
    NewContent {
        owner_id: owner,
        owner_type: None,
        name: Some(name.to_owned()),
        description: None,
        document_type: None,
        derivation_type: None,
    }
}

fn new_object(content_id: Uuid, key: &str) -> NewObject {
    NewObject {
        content_id,
        storage_backend_name: "memory".into(),
        storage_class: None,
        object_key: key.to_owned(),
        file_name: None,
        object_type: None,
    }
}

fn content_meta(content_id: Uuid, size: i64) -> ContentMetadata {
    let now = OffsetDateTime::now_utc();
    ContentMetadata {
        content_id,
        tags: vec![],
        file_size: Some(size),
        file_name: None,
        mime_type: None,
        checksum: None,
        checksum_algorithm: None,
        metadata: serde_json::json!({}),
        created_at: now,
        updated_at: now,
    }
}

fn object_meta(object_id: Uuid, size: i64) -> ObjectMetadata {
    let now = OffsetDateTime::now_utc();
    ObjectMetadata {
        object_id,
        size_bytes: Some(size),
        mime_type: None,
        etag: None,
        metadata: serde_json::json!({}),
        created_at: now,
        updated_at: now,
    }
}

fn is_not_found<T>(r: Result<T, ContentError>) -> bool {
    matches!(r, Err(ContentError::NotFound))
}

/// 契约唯一真相源。两个 repo 协作(content → object),内存与 PG 都调它。
/// PG 有 FK(object/content_metadata → content、object_metadata → object),故必须先建 content 再建 object/元数据。
async fn content_repo_contract(contents: &dyn ContentRepo, objects: &dyn ObjectRepo) {
    let scope = tenant();
    let owner = Uuid::now_v7();
    let now = OffsetDateTime::now_utc();

    // ── content:create(by 审计透传)+ get 往返 ──
    let c = contents
        .create(
            scope,
            NewContent {
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
    let got = contents.get(scope, c.id).await.unwrap();
    assert_eq!(got.name.as_deref(), Some("doc"));
    assert_eq!(got.tenant_id, scope.tenant_id());
    assert_eq!(got.derivation_type.as_deref(), Some("original"));

    // get 不存在 → NotFound
    assert!(is_not_found(contents.get(scope, Uuid::now_v7()).await));

    // ── 第二条 content + get_many(命中 / 缺席 / 空集)──
    let c2 = contents
        .create(scope, new_content(owner, "two"), None) // 审计 by=None 也可建
        .await
        .unwrap();
    assert_eq!(
        contents
            .get_many(scope, &[c.id, c2.id])
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        contents
            .get_many(scope, &[c.id, Uuid::now_v7()])
            .await
            .unwrap()
            .len(),
        1
    ); // 不存在的缺席
    assert!(contents.get_many(scope, &[]).await.unwrap().is_empty()); // 空集 → 空

    // ── list(scope, owner):2 条,id desc(c2 较新 → 居首)──
    let listed = contents.list(scope, owner).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, c2.id); // v7 单列全序,desc → 最新在前
                                     // 不同 owner 隔离
    assert!(contents
        .list(scope, Uuid::now_v7())
        .await
        .unwrap()
        .is_empty());

    // ── update(PUT 全量替换可编辑字段)──
    let updated = Content {
        name: Some("doc2".into()),
        description: None,
        ..got.clone()
    };
    let u = contents
        .update(scope, &updated, Some("sys".into()))
        .await
        .unwrap();
    assert_eq!(u.name.as_deref(), Some("doc2"));
    assert_eq!(u.description, None);

    // ── content_metadata upsert 幂等(二次 set 覆盖,后者胜)──
    contents
        .set_metadata(
            scope,
            ContentMetadata {
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
            },
        )
        .await
        .unwrap();
    contents
        .set_metadata(
            scope,
            ContentMetadata {
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
            },
        )
        .await
        .unwrap();
    let m = contents.get_metadata(scope, c.id).await.unwrap();
    assert_eq!(m.file_size, Some(20)); // 覆盖
    assert_eq!(m.tags, vec!["y".to_string(), "z".to_string()]);
    assert_eq!(m.mime_type, None);
    assert_eq!(m.metadata, serde_json::json!({"k": "v2"}));
    // 无元数据的内容 → NotFound
    assert!(is_not_found(contents.get_metadata(scope, c2.id).await));
    // 给不存在的内容写元数据 → NotFound(先验父行在本作用域存活)
    assert!(is_not_found(
        contents
            .set_metadata(scope, content_meta(Uuid::now_v7(), 1))
            .await
    ));
    // get_metadata_many:命中 c、缺席 c2
    let mm = contents
        .get_metadata_many(scope, &[c.id, c2.id])
        .await
        .unwrap();
    assert_eq!(mm.len(), 1);
    assert!(mm.contains_key(&c.id));

    // ── object:create + get 往返 + version=1/Created ──
    let key_a = format!("{}/a", c.id);
    let key_b = format!("{}/b", c.id);
    let o1 = objects
        .create(
            scope,
            NewObject {
                file_name: Some("a".into()),
                ..new_object(c.id, &key_a)
            },
            Some("sys".into()),
        )
        .await
        .unwrap();
    assert_eq!(o1.status, ObjectStatus::Created);
    assert_eq!(o1.version, 1);
    let o2 = objects
        .create(scope, new_object(c.id, &key_b), None)
        .await
        .unwrap();
    assert_eq!(objects.get(scope, o1.id).await.unwrap().object_key, key_a);
    assert!(is_not_found(objects.get(scope, Uuid::now_v7()).await));

    // ── (storage_backend_name, object_key) 存活唯一 → Conflict ──
    assert!(matches!(
        objects.create(scope, new_object(c.id, &key_a), None).await,
        Err(ContentError::Conflict(_))
    ));

    // ── list_by_content 排序(id asc = 建序:o1 先于 o2)──
    let objs = objects.list_by_content(scope, c.id).await.unwrap();
    assert_eq!(objs.len(), 2);
    assert_eq!(objs[0].id, o1.id);
    assert_eq!(objs[1].id, o2.id);

    // ── get_by_key(复合唯一查)──
    assert_eq!(
        objects
            .get_by_key(scope, &key_a, "memory")
            .await
            .unwrap()
            .id,
        o1.id
    );
    assert!(is_not_found(
        objects.get_by_key(scope, "nope", "memory").await
    ));

    // ── set_status object → Uploaded ──
    objects
        .set_status(scope, o1.id, ObjectStatus::Uploaded, Some("sys".into()))
        .await
        .unwrap();
    assert_eq!(
        objects.get(scope, o1.id).await.unwrap().status,
        ObjectStatus::Uploaded
    );

    // ── object_metadata upsert 幂等(后者胜)──
    objects
        .set_metadata(
            scope,
            ObjectMetadata {
                object_id: o1.id,
                size_bytes: Some(5),
                mime_type: Some("text/plain".into()),
                etag: Some("e1".into()),
                metadata: serde_json::json!({}),
                created_at: now,
                updated_at: now,
            },
        )
        .await
        .unwrap();
    objects
        .set_metadata(
            scope,
            ObjectMetadata {
                object_id: o1.id,
                size_bytes: Some(7),
                mime_type: None,
                etag: Some("e2".into()),
                metadata: serde_json::json!({"a": 1}),
                created_at: now,
                updated_at: now,
            },
        )
        .await
        .unwrap();
    let om = objects.get_metadata(scope, o1.id).await.unwrap();
    assert_eq!(om.size_bytes, Some(7));
    assert_eq!(om.etag.as_deref(), Some("e2"));
    assert!(is_not_found(objects.get_metadata(scope, o2.id).await));
    // 给不存在的对象写元数据 → NotFound
    assert!(is_not_found(
        objects
            .set_metadata(scope, object_meta(Uuid::now_v7(), 1))
            .await
    ));

    // ── content set_status → Uploaded ──
    contents
        .set_status(scope, c.id, ContentStatus::Uploaded, None)
        .await
        .unwrap();
    assert_eq!(
        contents.get(scope, c.id).await.unwrap().status,
        ContentStatus::Uploaded
    );

    // ── object soft_delete:get 消失 + (backend,key) 释放可复用 + 二次删 NotFound ──
    objects.soft_delete(scope, o1.id, None).await.unwrap();
    assert!(is_not_found(objects.get(scope, o1.id).await));
    // 软删后同 key 可复用(部分唯一索引 where deleted_at is null)
    let o3 = objects
        .create(scope, new_object(c.id, &key_a), None)
        .await
        .unwrap();
    assert_ne!(o3.id, o1.id);
    assert!(is_not_found(objects.soft_delete(scope, o1.id, None).await));

    // ── content soft_delete:get 消失 + get_many 过滤 + 二次删 NotFound ──
    contents
        .soft_delete(scope, c.id, Some("sys".into()))
        .await
        .unwrap();
    assert!(is_not_found(contents.get(scope, c.id).await));
    assert_eq!(
        contents
            .get_many(scope, &[c.id, c2.id])
            .await
            .unwrap()
            .len(),
        1
    ); // 软删后只剩 c2
    assert!(is_not_found(contents.soft_delete(scope, c.id, None).await));
}

/// 在 `scope` 下铺一条 content + 元数据 + object + 对象元数据,返回 (content_id, object_id, key)。
async fn seed_one(
    contents: &dyn ContentRepo,
    objects: &dyn ObjectRepo,
    scope: ContentScope,
    owner: Uuid,
) -> (Uuid, Uuid, String) {
    let c = contents
        .create(scope, new_content(owner, "seed"), None)
        .await
        .unwrap();
    contents
        .set_metadata(scope, content_meta(c.id, 3))
        .await
        .unwrap();
    let key = format!("seed/{}/{}", c.id, Uuid::now_v7());
    let o = objects
        .create(scope, new_object(c.id, &key), None)
        .await
        .unwrap();
    objects
        .set_metadata(scope, object_meta(o.id, 3))
        .await
        .unwrap();
    (c.id, o.id, key)
}

/// `other` 作用域对 (cid, oid, key) **全不可见**:取单条 / 写 / 删 → NotFound,批量 / 列表 → 空。
async fn assert_invisible(
    contents: &dyn ContentRepo,
    objects: &dyn ObjectRepo,
    other: ContentScope,
    owner: Uuid,
    (cid, oid, key): (Uuid, Uuid, &str),
) {
    assert!(is_not_found(contents.get(other, cid).await));
    assert!(contents.get_many(other, &[cid]).await.unwrap().is_empty());
    assert!(contents.list(other, owner).await.unwrap().is_empty());
    assert!(is_not_found(contents.get_metadata(other, cid).await));
    assert!(contents
        .get_metadata_many(other, &[cid])
        .await
        .unwrap()
        .is_empty());
    assert!(is_not_found(
        contents.set_metadata(other, content_meta(cid, 99)).await
    ));
    let fake = Content {
        id: cid,
        tenant_id: other.tenant_id(),
        owner_id: owner,
        owner_type: None,
        name: Some("hijack".into()),
        description: None,
        document_type: None,
        status: ContentStatus::Created,
        derivation_type: None,
        created_at: OffsetDateTime::now_utc(),
        updated_at: OffsetDateTime::now_utc(),
    };
    assert!(is_not_found(contents.update(other, &fake, None).await));
    assert!(is_not_found(
        contents
            .set_status(other, cid, ContentStatus::Archived, None)
            .await
    ));
    assert!(is_not_found(contents.soft_delete(other, cid, None).await));

    assert!(is_not_found(objects.get(other, oid).await));
    assert!(objects
        .list_by_content(other, cid)
        .await
        .unwrap()
        .is_empty());
    assert!(is_not_found(objects.get_by_key(other, key, "memory").await));
    assert!(is_not_found(objects.get_metadata(other, oid).await));
    assert!(is_not_found(
        objects.set_metadata(other, object_meta(oid, 99)).await
    ));
    assert!(is_not_found(
        objects
            .set_status(other, oid, ObjectStatus::Failed, None)
            .await
    ));
    assert!(is_not_found(objects.soft_delete(other, oid, None).await));
}

/// 越权尝试之后,属主作用域看到的一切原样未动。
async fn assert_untouched(
    contents: &dyn ContentRepo,
    objects: &dyn ObjectRepo,
    owner_scope: ContentScope,
    (cid, oid): (Uuid, Uuid),
) {
    let c = contents.get(owner_scope, cid).await.unwrap();
    assert_eq!(c.name.as_deref(), Some("seed"));
    assert_eq!(c.status, ContentStatus::Created);
    assert_eq!(c.tenant_id, owner_scope.tenant_id());
    assert_eq!(
        contents
            .get_metadata(owner_scope, cid)
            .await
            .unwrap()
            .file_size,
        Some(3)
    );
    let o = objects.get(owner_scope, oid).await.unwrap();
    assert_eq!(o.status, ObjectStatus::Created);
    assert_eq!(
        objects
            .get_metadata(owner_scope, oid)
            .await
            .unwrap()
            .size_bytes,
        Some(3)
    );
}

/// 跨租户:租户 B 按 ID 取 / 改 / 删租户 A 的内容与对象 → NotFound;A 的数据原样。
async fn get_other_tenant_content_is_not_found_contract(
    contents: &dyn ContentRepo,
    objects: &dyn ObjectRepo,
) {
    let (a, b) = (tenant(), tenant());
    let owner = Uuid::now_v7();
    let (cid, oid, key) = seed_one(contents, objects, a, owner).await;
    assert_invisible(contents, objects, b, owner, (cid, oid, &key)).await;
    assert_untouched(contents, objects, a, (cid, oid)).await;
}

/// 列表只回当前租户(同一 owner 在两个租户里各有内容)。
async fn list_only_current_tenant_contract(contents: &dyn ContentRepo, objects: &dyn ObjectRepo) {
    let (a, b) = (tenant(), tenant());
    let owner = Uuid::now_v7();
    let (a1, _, _) = seed_one(contents, objects, a, owner).await;
    let (a2, _, _) = seed_one(contents, objects, a, owner).await;
    let (b1, _, _) = seed_one(contents, objects, b, owner).await;

    let la = contents.list(a, owner).await.unwrap();
    assert_eq!(
        la.iter().map(|c| c.id).collect::<Vec<_>>(),
        vec![a2, a1],
        "A 只看到自己的两条(id desc)"
    );
    assert!(la.iter().all(|c| c.tenant_id == a.tenant_id()));
    let lb = contents.list(b, owner).await.unwrap();
    assert_eq!(lb.iter().map(|c| c.id).collect::<Vec<_>>(), vec![b1]);
    // get_many 混传两个租户的 id → 只回当前租户的
    assert_eq!(contents.get_many(a, &[a1, a2, b1]).await.unwrap().len(), 2);
    // 个人作用域下同一 owner 什么都没有
    assert!(contents
        .list(ContentScope::Personal, owner)
        .await
        .unwrap()
        .is_empty());
}

/// 个人内容(tenant_id = NULL)对任何租户作用域(含默认租户)都不可见。
async fn personal_scope_invisible_to_tenant_scope_contract(
    contents: &dyn ContentRepo,
    objects: &dyn ObjectRepo,
) {
    let owner = Uuid::now_v7();
    let p = ContentScope::Personal;
    let (cid, oid, key) = seed_one(contents, objects, p, owner).await;
    assert_eq!(contents.get(p, cid).await.unwrap().tenant_id, None);
    // 个人列表按 owner 过滤:别人的个人内容不在我的列表里
    assert_eq!(contents.list(p, owner).await.unwrap().len(), 1);
    assert!(contents.list(p, Uuid::now_v7()).await.unwrap().is_empty());

    let default = ContentScope::tenant(DEFAULT_TENANT_ID).unwrap();
    for t in [tenant(), default] {
        assert_invisible(contents, objects, t, owner, (cid, oid, &key)).await;
    }
    assert_untouched(contents, objects, p, (cid, oid)).await;
}

/// 租户内容对个人作用域不可见。
async fn tenant_scope_invisible_to_personal_scope_contract(
    contents: &dyn ContentRepo,
    objects: &dyn ObjectRepo,
) {
    let owner = Uuid::now_v7();
    let t = tenant();
    let (cid, oid, key) = seed_one(contents, objects, t, owner).await;
    assert_invisible(
        contents,
        objects,
        ContentScope::Personal,
        owner,
        (cid, oid, &key),
    )
    .await;
    assert_untouched(contents, objects, t, (cid, oid)).await;
}

fn is_invalid<T>(r: Result<T, ContentError>) -> bool {
    matches!(r, Err(ContentError::InvalidInput(_)))
}

/// nil UUID 永远不是合法租户:构造拒、仓储入口也拒(防直接写 `ContentScope::Tenant(Uuid::nil())`)。
async fn nil_tenant_rejected_contract(contents: &dyn ContentRepo, objects: &dyn ObjectRepo) {
    assert!(is_invalid(ContentScope::tenant(Uuid::nil())));
    assert!(ContentScope::tenant(DEFAULT_TENANT_ID).is_ok());

    let nil = ContentScope::Tenant(Uuid::nil());
    let owner = Uuid::now_v7();
    assert!(is_invalid(
        contents.create(nil, new_content(owner, "x"), None).await
    ));
    assert!(is_invalid(contents.get(nil, Uuid::now_v7()).await));
    assert!(is_invalid(contents.list(nil, owner).await));
    assert!(is_invalid(contents.get_many(nil, &[Uuid::now_v7()]).await));
    assert!(is_invalid(objects.get(nil, Uuid::now_v7()).await));
    // 合法租户建的内容,拿 nil 作用域也碰不到(报 InvalidInput,而不是"看见")
    let c = contents
        .create(tenant(), new_content(owner, "ok"), None)
        .await
        .unwrap();
    assert!(is_invalid(
        objects
            .create(nil, new_object(c.id, &format!("nil/{}", c.id)), None)
            .await
    ));
    assert!(is_invalid(contents.soft_delete(nil, c.id, None).await));
}

/// `ObjectRepo::create` 要求父 content 在**同一作用域**存活:别的租户 / 个人作用域 / 不存在 / 已软删 → `NotFound`
/// (不能借 FK 不受 RLS 约束,把对象挂到别的租户的 content 下;也不该撞 FK 报 500)。
/// **只对 PG 跑**:内存 `InMemoryObjectRepo` 拿不到 content 表,不验父行(见 `ObjectRepo::create` 的文档)。
#[cfg_attr(not(feature = "pg-conformance"), allow(dead_code))]
async fn object_create_requires_parent_in_scope_contract(
    contents: &dyn ContentRepo,
    objects: &dyn ObjectRepo,
) {
    let (a, b) = (tenant(), tenant());
    let owner = Uuid::now_v7();
    let c = contents
        .create(a, new_content(owner, "parent"), None)
        .await
        .unwrap();
    // 别的租户 / 个人作用域:父行不可见 → NotFound,且没写进任何行
    assert!(is_not_found(
        objects
            .create(b, new_object(c.id, &format!("b/{}", c.id)), None)
            .await
    ));
    assert!(is_not_found(
        objects
            .create(
                ContentScope::Personal,
                new_object(c.id, &format!("p/{}", c.id)),
                None
            )
            .await
    ));
    assert!(objects.list_by_content(b, c.id).await.unwrap().is_empty());
    assert!(objects
        .list_by_content(ContentScope::Personal, c.id)
        .await
        .unwrap()
        .is_empty());
    // 不存在的父行 → NotFound(不是 FK 违例的 Internal)
    assert!(is_not_found(
        objects
            .create(a, new_object(Uuid::now_v7(), "nowhere"), None)
            .await
    ));
    // 已软删的父行 → NotFound
    let gone = contents
        .create(a, new_content(owner, "gone"), None)
        .await
        .unwrap();
    contents.soft_delete(a, gone.id, None).await.unwrap();
    assert!(is_not_found(
        objects
            .create(a, new_object(gone.id, &format!("a/{}", gone.id)), None)
            .await
    ));
    // 同作用域照常建
    let o = objects
        .create(a, new_object(c.id, &format!("a/{}", c.id)), None)
        .await
        .unwrap();
    assert_eq!(o.content_id, c.id);
}

fn upload_input(owner: Uuid, object_key: Option<String>) -> UploadContentInput {
    UploadContentInput {
        owner_id: owner,
        owner_type: None,
        name: Some("doc".into()),
        description: None,
        document_type: None,
        object_key,
        file_name: Some("doc.txt".into()),
        mime_type: Some("text/plain".into()),
        tags: vec![],
        custom_metadata: None,
        data: Bytes::from_static(b"bytes"),
    }
}

/// 断言 key = `{prefix}/{content_id}/{uuid}`。
fn assert_key(key: &str, prefix: &str, content_id: Uuid) {
    let head = format!("{prefix}/{content_id}/");
    let rest = key
        .strip_prefix(&head)
        .unwrap_or_else(|| panic!("key {key} 应以 {head} 开头"));
    assert!(Uuid::parse_str(rest).is_ok(), "key 尾段应是 uuid: {key}");
}

/// 服务 mint 的对象 key 带作用域前缀:租户 `tenants/{tenant_id}/{content_id}/{uuid}`、
/// 个人 `personal/{owner_id}/{content_id}/{uuid}`;调用方显式传的、本作用域前缀下的 key 原样保留。
async fn object_key_has_tenant_prefix_contract(
    contents: Arc<dyn ContentRepo>,
    objects: Arc<dyn ObjectRepo>,
) {
    let svc = ContentService::new(
        contents,
        objects,
        Arc::new(InMemoryObjectStore::new()),
        "memory",
    );
    let owner = Uuid::now_v7();
    let t = Uuid::now_v7();
    let ts = ContentScope::tenant(t).unwrap();

    let up = svc
        .upload_content(ts, upload_input(owner, None), None)
        .await
        .unwrap();
    assert_key(
        &up.object.object_key,
        &format!("tenants/{t}"),
        up.content.id,
    );
    assert_eq!(up.content.tenant_id, Some(t));
    // 字节在本作用域可读,换个租户 → NotFound
    assert_eq!(
        &svc.download_content(ts, up.content.id).await.unwrap()[..],
        b"bytes"
    );
    assert!(is_not_found(
        svc.download_content(tenant(), up.content.id).await
    ));

    let pu = svc
        .upload_content(ContentScope::Personal, upload_input(owner, None), None)
        .await
        .unwrap();
    assert_key(
        &pu.object.object_key,
        &format!("personal/{owner}"),
        pu.content.id,
    );
    assert_eq!(pu.content.tenant_id, None);

    let pr = svc
        .prepare_upload(ts, prepare_input(owner, None), None)
        .await
        .unwrap();
    assert_key(
        &pr.object.object_key,
        &format!("tenants/{t}"),
        pr.content.id,
    );

    // 调用方给的、本作用域前缀下的 key 原样保留(不再加前缀)
    let custom = format!("tenants/{t}/custom/{}", Uuid::now_v7());
    let cu = svc
        .upload_content(ts, upload_input(owner, Some(custom.clone())), None)
        .await
        .unwrap();
    assert_eq!(cu.object.object_key, custom);
}

fn prepare_input(owner: Uuid, object_key: Option<String>) -> PrepareUploadInput {
    PrepareUploadInput {
        owner_id: owner,
        owner_type: None,
        name: None,
        description: None,
        document_type: None,
        object_key,
        file_name: None,
        mime_type: None,
        tags: vec![],
        custom_metadata: None,
    }
}

/// 调用方显式给的 object_key 必须在本作用域前缀下(租户 `tenants/{tenant_id}/`、个人 `personal/{owner_id}/`):
/// 租户 A 传 `tenants/{B}/x`、个人传别人的 `personal/{other}/x`、无前缀、`..` 越级、前缀后为空 → `InvalidInput`,
/// 且被拒的请求**不留任何行**(校验在建行之前);一步上传和两步上传 prepare 同一规则。合法前缀照常可用。
async fn explicit_object_key_must_have_scope_prefix_contract(
    contents: Arc<dyn ContentRepo>,
    objects: Arc<dyn ObjectRepo>,
) {
    let svc = ContentService::new(
        contents.clone(),
        objects,
        Arc::new(InMemoryObjectStore::new()),
        "memory",
    );
    let owner = Uuid::now_v7();
    let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
    let sa = ContentScope::tenant(a).unwrap();
    let other_owner = Uuid::now_v7();

    let bad_for_a = [
        format!("tenants/{b}/x"),                // 别的租户的前缀
        format!("tenants/{a}"),                  // 少了结尾 /,不算在前缀下
        format!("tenants/{a}/"),                 // 前缀后为空
        format!("tenants/{a}/../{b}/x"),         // .. 越级到 B
        format!("tenants/{a}/x\\..\\..\\{b}/y"), // 反斜杠 .. 也拒
        format!("personal/{owner}/x"),           // 个人前缀不能在租户作用域用
        "x".to_owned(),                          // 无前缀
    ];
    for key in &bad_for_a {
        assert!(
            is_invalid(
                svc.upload_content(sa, upload_input(owner, Some(key.clone())), None)
                    .await
            ),
            "upload: 租户 A 用 {key} 应被拒"
        );
        assert!(
            is_invalid(
                svc.prepare_upload(sa, prepare_input(owner, Some(key.clone())), None)
                    .await
            ),
            "prepare: 租户 A 用 {key} 应被拒"
        );
    }
    let bad_for_personal = [
        format!("personal/{other_owner}/x"), // 别人的个人前缀
        format!("tenants/{a}/x"),            // 租户前缀不能在个人作用域用
    ];
    for key in &bad_for_personal {
        assert!(
            is_invalid(
                svc.upload_content(
                    ContentScope::Personal,
                    upload_input(owner, Some(key.clone())),
                    None
                )
                .await
            ),
            "upload: 个人作用域用 {key} 应被拒"
        );
        assert!(
            is_invalid(
                svc.prepare_upload(
                    ContentScope::Personal,
                    prepare_input(owner, Some(key.clone())),
                    None
                )
                .await
            ),
            "prepare: 个人作用域用 {key} 应被拒"
        );
    }
    // 被拒的请求一行都没建(校验先于建 content 行)
    assert!(contents.list(sa, owner).await.unwrap().is_empty());
    assert!(contents
        .list(ContentScope::Personal, owner)
        .await
        .unwrap()
        .is_empty());

    // 合法前缀:原样用
    let ka = format!("tenants/{a}/mine/{}", Uuid::now_v7());
    let up = svc
        .upload_content(sa, upload_input(owner, Some(ka.clone())), None)
        .await
        .unwrap();
    assert_eq!(up.object.object_key, ka);
    let ka2 = format!("tenants/{a}/two-step/{}", Uuid::now_v7());
    let pr = svc
        .prepare_upload(sa, prepare_input(owner, Some(ka2.clone())), None)
        .await
        .unwrap();
    assert_eq!(pr.object.object_key, ka2);
    let kp = format!("personal/{owner}/avatar/{}", Uuid::now_v7());
    let pu = svc
        .upload_content(
            ContentScope::Personal,
            upload_input(owner, Some(kp.clone())),
            None,
        )
        .await
        .unwrap();
    assert_eq!(pu.object.object_key, kp);
}

// ── 入口 1:内存(零 DB,默认 cargo test 就编译+跑)──
#[tokio::test]
async fn memory_satisfies_content_contract() {
    use content::{InMemoryContentRepo, InMemoryObjectRepo};
    content_repo_contract(&InMemoryContentRepo::new(), &InMemoryObjectRepo::new()).await;
}

/// 内存侧的作用域隔离契约(与 `pg` 模块同名同断言)。
mod memory {
    use super::*;
    use content::{InMemoryContentRepo, InMemoryObjectRepo};

    fn repos() -> (InMemoryContentRepo, InMemoryObjectRepo) {
        (InMemoryContentRepo::new(), InMemoryObjectRepo::new())
    }

    #[tokio::test]
    async fn get_other_tenant_content_is_not_found() {
        let (c, o) = repos();
        get_other_tenant_content_is_not_found_contract(&c, &o).await;
    }

    #[tokio::test]
    async fn list_only_current_tenant() {
        let (c, o) = repos();
        list_only_current_tenant_contract(&c, &o).await;
    }

    #[tokio::test]
    async fn personal_scope_invisible_to_tenant_scope() {
        let (c, o) = repos();
        personal_scope_invisible_to_tenant_scope_contract(&c, &o).await;
    }

    #[tokio::test]
    async fn tenant_scope_invisible_to_personal_scope() {
        let (c, o) = repos();
        tenant_scope_invisible_to_personal_scope_contract(&c, &o).await;
    }

    #[tokio::test]
    async fn nil_tenant_rejected() {
        let (c, o) = repos();
        nil_tenant_rejected_contract(&c, &o).await;
    }

    #[tokio::test]
    async fn object_key_has_tenant_prefix() {
        object_key_has_tenant_prefix_contract(
            Arc::new(InMemoryContentRepo::new()),
            Arc::new(InMemoryObjectRepo::new()),
        )
        .await;
    }

    #[tokio::test]
    async fn explicit_object_key_must_have_scope_prefix() {
        explicit_object_key_must_have_scope_prefix_contract(
            Arc::new(InMemoryContentRepo::new()),
            Arc::new(InMemoryObjectRepo::new()),
        )
        .await;
    }
}

// ── 入口 2:PG(需 --features pg-conformance + 连 content role 的 pg)──
// bootstrap 内联在本文件(不走 #[path] include):sqlx::migrate! 相对 content crate 的 CARGO_MANIFEST_DIR 解析,稳。
#[cfg(feature = "pg-conformance")]
mod pg {
    use super::*;
    use content::{PgContentRepo, PgObjectRepo};
    use sqlx::migrate::Migrator;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use sqlx::{AssertSqlSafe, Executor, PgPool};

    /// 编译期内嵌 content crate 的 migrations/(相对 content crate 的 CARGO_MANIFEST_DIR)。
    static CONTENT_MIGRATOR: Migrator = sqlx::migrate!("./migrations");

    /// 测试专用的运行时账号:NOLOGIN、NOBYPASSRLS、非 owner,只有 DML。连接账号(content)经
    /// `SET ROLE` 切过去 —— RLS 按 current_user 判定,所以仓储真的跑在 RLS 之下。
    /// 不依赖 baserust 的账号拆分(库自包含);角色是集群级的,建一次后续复用(幂等)。
    const RT_ROLE: &str = "content_rt";

    /// 两个连接池:`owner` = 跑迁移、拥有表的连接账号;`rt` = 每条连接 SET ROLE content_rt。
    struct Pools {
        #[allow(dead_code)] // 留着:池活到测试结束
        owner: PgPool,
        rt: PgPool,
    }

    /// 连临时库(连接账号 = owner)并建 content schema(不跑迁移)。
    async fn owner_pool(opts: PgPoolOptions, conn: PgConnectOptions) -> PgPool {
        let owner = opts.connect_with(conn).await.expect("连测试库");
        owner
            .execute("create schema if not exists content")
            .await
            .expect("建 content schema");
        owner
    }

    /// 确保集群级角色 content_rt 存在、连接账号可 SET ROLE 过去。需要连接账号有 CREATEROLE
    /// (测试库一次性授予,见 justfile 注释 / CI)。并发测试会同时撞建角色 / 授成员关系 → 重试即可
    /// (下一轮看到已存在就跳过)。
    async fn ensure_runtime_role(owner: &PgPool) {
        let ddl = format!(
            "do $$ begin
                 if not exists (select 1 from pg_roles where rolname = '{RT_ROLE}') then
                     create role {RT_ROLE} nologin nosuperuser nobypassrls noinherit;
                 end if;
                 if not pg_has_role(current_user, '{RT_ROLE}', 'SET') then
                     execute format('grant {RT_ROLE} to %I with set true, inherit false', current_user);
                 end if;
             end $$"
        );
        let mut last = None;
        for _ in 0..20 {
            match owner.execute(AssertSqlSafe(ddl.clone())).await {
                Ok(_) => return,
                Err(e) => last = Some(e),
            }
        }
        panic!("建测试运行时账号 {RT_ROLE} 失败(连接账号需 CREATEROLE): {last:?}");
    }

    /// 给运行时账号授本库 content schema 的 USAGE + 全部表 DML(每个临时库各授一次),并开出它的连接池。
    async fn runtime_pool(owner: &PgPool, conn: PgConnectOptions) -> PgPool {
        ensure_runtime_role(owner).await;
        owner
            .execute(AssertSqlSafe(format!(
                "grant usage on schema content to {RT_ROLE};
                 grant select, insert, update, delete on all tables in schema content to {RT_ROLE};"
            )))
            .await
            .expect("授运行时账号 DML");
        PgPoolOptions::new()
            .max_connections(4)
            .after_connect(|c, _| {
                Box::pin(async move {
                    c.execute(AssertSqlSafe(format!("set role {RT_ROLE}")))
                        .await?;
                    Ok(())
                })
            })
            .connect_with(conn)
            .await
            .expect("连运行时账号池")
    }

    /// #[sqlx::test] 的干净临时库:建 schema + 跑全部迁移 + 开 owner / 运行时两个池。
    async fn bootstrap(opts: PgPoolOptions, conn: PgConnectOptions) -> Pools {
        let owner = owner_pool(opts, conn.clone()).await;
        CONTENT_MIGRATOR.run(&owner).await.expect("跑 content 迁移");
        let rt = runtime_pool(&owner, conn).await;
        Pools { owner, rt }
    }

    fn repos(p: &Pools) -> (PgContentRepo, PgObjectRepo) {
        (
            PgContentRepo::new(p.rt.clone()),
            PgObjectRepo::new(p.rt.clone()),
        )
    }

    /// 在 `tx` 里设事务级租户上下文(直接 SQL 用例用)。
    async fn set_tenant(tx: &mut sqlx::PgConnection, tenant: Uuid) {
        sqlx::query("select set_config('app.tenant_id', $1, true)")
            .bind(tenant.to_string())
            .execute(tx)
            .await
            .unwrap();
    }

    fn sqlstate(err: &sqlx::Error) -> Option<String> {
        err.as_database_error()
            .and_then(|d| d.code())
            .map(|c| c.into_owned())
    }

    #[sqlx::test(migrations = false)]
    async fn pg_satisfies_content_contract(opts: PgPoolOptions, conn: PgConnectOptions) {
        let p = bootstrap(opts, conn).await;
        let (c, o) = repos(&p);
        content_repo_contract(&c, &o).await;
    }

    #[sqlx::test(migrations = false)]
    async fn get_other_tenant_content_is_not_found(opts: PgPoolOptions, conn: PgConnectOptions) {
        let p = bootstrap(opts, conn).await;
        let (c, o) = repos(&p);
        get_other_tenant_content_is_not_found_contract(&c, &o).await;
    }

    #[sqlx::test(migrations = false)]
    async fn list_only_current_tenant(opts: PgPoolOptions, conn: PgConnectOptions) {
        let p = bootstrap(opts, conn).await;
        let (c, o) = repos(&p);
        list_only_current_tenant_contract(&c, &o).await;
    }

    #[sqlx::test(migrations = false)]
    async fn personal_scope_invisible_to_tenant_scope(opts: PgPoolOptions, conn: PgConnectOptions) {
        let p = bootstrap(opts, conn).await;
        let (c, o) = repos(&p);
        personal_scope_invisible_to_tenant_scope_contract(&c, &o).await;
    }

    #[sqlx::test(migrations = false)]
    async fn tenant_scope_invisible_to_personal_scope(opts: PgPoolOptions, conn: PgConnectOptions) {
        let p = bootstrap(opts, conn).await;
        let (c, o) = repos(&p);
        tenant_scope_invisible_to_personal_scope_contract(&c, &o).await;
    }

    #[sqlx::test(migrations = false)]
    async fn nil_tenant_rejected(opts: PgPoolOptions, conn: PgConnectOptions) {
        let p = bootstrap(opts, conn).await;
        let (c, o) = repos(&p);
        nil_tenant_rejected_contract(&c, &o).await;
        // 纵深:绕过仓储直接写 nil 租户 → 表上的 CHECK 拒(设上 nil 让 RLS 放行,只剩 CHECK 把关)
        let mut tx = p.rt.begin().await.unwrap();
        set_tenant(&mut tx, Uuid::nil()).await;
        let err = sqlx::query("insert into content (id, tenant_id, owner_id) values ($1, $2, $3)")
            .bind(Uuid::now_v7())
            .bind(Uuid::nil())
            .bind(Uuid::now_v7())
            .execute(&mut *tx)
            .await
            .expect_err("nil 租户必须被 CHECK 拒");
        assert_eq!(
            sqlstate(&err).as_deref(),
            Some("23514"),
            "应是 check_violation: {err}"
        );
    }

    #[sqlx::test(migrations = false)]
    async fn object_create_requires_parent_in_scope(opts: PgPoolOptions, conn: PgConnectOptions) {
        let p = bootstrap(opts, conn).await;
        let (c, o) = repos(&p);
        object_create_requires_parent_in_scope_contract(&c, &o).await;
    }

    #[sqlx::test(migrations = false)]
    async fn object_key_has_tenant_prefix(opts: PgPoolOptions, conn: PgConnectOptions) {
        let p = bootstrap(opts, conn).await;
        object_key_has_tenant_prefix_contract(
            Arc::new(PgContentRepo::new(p.rt.clone())),
            Arc::new(PgObjectRepo::new(p.rt.clone())),
        )
        .await;
    }

    #[sqlx::test(migrations = false)]
    async fn explicit_object_key_must_have_scope_prefix(
        opts: PgPoolOptions,
        conn: PgConnectOptions,
    ) {
        let p = bootstrap(opts, conn).await;
        explicit_object_key_must_have_scope_prefix_contract(
            Arc::new(PgContentRepo::new(p.rt.clone())),
            Arc::new(PgObjectRepo::new(p.rt.clone())),
        )
        .await;
    }

    /// 五张有 tenant_id 的表。
    const TABLES: [&str; 5] = [
        "content",
        "content_metadata",
        "object",
        "object_metadata",
        "content_derived",
    ];

    async fn count(conn: &mut sqlx::PgConnection, table: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(AssertSqlSafe(format!("select count(*) from {table}")))
            .fetch_one(conn)
            .await
            .unwrap()
    }

    /// `select count(*) from {table} where {cond}`,cond 里一个 `$1` 绑 uuid。
    async fn count_where(conn: &mut sqlx::PgConnection, table: &str, cond: &str, v: Uuid) -> i64 {
        sqlx::query_scalar::<_, i64>(AssertSqlSafe(format!(
            "select count(*) from {table} where {cond}"
        )))
        .bind(v)
        .fetch_one(conn)
        .await
        .unwrap()
    }

    /// [`seed_with_derived`] 铺完后每个租户在各表的行数(content 2 = 原件 + 派生件)。
    const PER_TENANT_ROWS: [(&str, i64); 5] = [
        ("content", 2),
        ("content_metadata", 1),
        ("object", 1),
        ("object_metadata", 1),
        ("content_derived", 1),
    ];

    /// [`seed_one`] + 一条派生件 content + 一行 content_derived(仓储没有派生接口,直接 SQL 在本租户上下文里写)。
    /// 返回 (原件 content_id, 派生件 content_id)。
    async fn seed_with_derived(
        p: &Pools,
        c: &PgContentRepo,
        o: &PgObjectRepo,
        scope: ContentScope,
        owner: Uuid,
    ) -> (Uuid, Uuid) {
        let (parent, _, _) = seed_one(c, o, scope, owner).await;
        let child = c
            .create(scope, new_content(owner, "derived"), None)
            .await
            .unwrap()
            .id;
        let mut tx = p.rt.begin().await.unwrap();
        set_tenant(&mut tx, scope.tenant_id().unwrap()).await;
        sqlx::query(
            "insert into content_derived (parent_id, content_id, variant, derivation_type, tenant_id)
             values ($1, $2, 'thumb', 'resize', $3)",
        )
        .bind(parent)
        .bind(child)
        .bind(scope.tenant_id())
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        (parent, child)
    }

    /// 单连接的运行时账号池,连上就留**会话级**残留(`set …`,不是 `set local`:事务结束也不清),
    /// 模拟连接池里一条被别的代码 / 中间件弄脏的连接。
    async fn residual_pool(conn: PgConnectOptions, residual: String) -> PgPool {
        PgPoolOptions::new()
            .max_connections(1)
            .after_connect(move |c, _| {
                let sql = format!("set role {RT_ROLE}; {residual}");
                Box::pin(async move {
                    c.execute(AssertSqlSafe(sql)).await?;
                    Ok(())
                })
            })
            .connect_with(conn)
            .await
            .expect("连带残留的运行时账号池")
    }

    async fn content_ids(conn: &mut sqlx::PgConnection) -> Vec<Uuid> {
        sqlx::query_scalar("select id from content order by id")
            .fetch_all(conn)
            .await
            .unwrap()
    }

    /// 连接上有会话级残留(`app.personal_scope = 'on'` + `app.tenant_id = B`)时,仓储的 `scoped_tx` 开的事务里
    /// 用**不带作用域条件**的 SQL 查,RLS 仍只放行本作用域的行:租户 A 看不到个人行 / B 的行,个人作用域看不到 B 的行。
    /// (修复前 `scoped_tx` 只设本作用域的变量,另一个残留会让 RLS 的另一支也放行。)
    #[sqlx::test(migrations = false)]
    async fn scoped_tx_ignores_session_residual_of_other_scope(
        opts: PgPoolOptions,
        conn: PgConnectOptions,
    ) {
        use content::__pg_testing::scoped_tx;
        let p = bootstrap(opts, conn.clone()).await;
        let (c, o) = repos(&p);
        let (a, b) = (tenant(), tenant());
        let (ta, tb) = (a.tenant_id().unwrap(), b.tenant_id().unwrap());
        let owner = Uuid::now_v7();
        let (ca, _, _) = seed_one(&c, &o, a, owner).await;
        let (cb, _, _) = seed_one(&c, &o, b, owner).await;
        let (cp, _, _) = seed_one(&c, &o, ContentScope::Personal, owner).await;

        let dirty = residual_pool(
            conn,
            format!("set app.personal_scope = 'on'; set app.tenant_id = '{tb}'"),
        )
        .await;
        // 前提:残留真的在这条连接上(不经 scoped_tx 直接查 → 看得见 B + 个人)
        {
            let mut x = dirty.acquire().await.unwrap();
            let mut want = vec![cb, cp];
            want.sort();
            assert_eq!(content_ids(&mut x).await, want, "残留应已生效");
        }

        // 租户 A:只见 A(四张有数据的表都只 1 行,且没有非 A 的行)
        let mut tx = scoped_tx(&dirty, a).await.unwrap();
        assert_eq!(content_ids(&mut tx).await, vec![ca], "租户 A 只应见 A 的行");
        for table in &TABLES[..4] {
            assert_eq!(
                count(&mut tx, table).await,
                1,
                "{table}: 租户 A 只应见 1 行"
            );
            assert_eq!(
                count_where(&mut tx, table, "tenant_id is distinct from $1", ta).await,
                0,
                "{table}: 租户 A 不应见个人 / B 的行"
            );
        }
        tx.rollback().await.unwrap();

        // 个人:只见个人行
        let mut tx = scoped_tx(&dirty, ContentScope::Personal).await.unwrap();
        assert_eq!(
            content_ids(&mut tx).await,
            vec![cp],
            "个人作用域只应见个人行"
        );
        for table in &TABLES[..4] {
            assert_eq!(
                count(&mut tx, table).await,
                1,
                "{table}: 个人作用域只应见 1 行"
            );
            assert_eq!(
                sqlx::query_scalar::<_, i64>(AssertSqlSafe(format!(
                    "select count(*) from {table} where tenant_id is not null"
                )))
                .fetch_one(&mut *tx)
                .await
                .unwrap(),
                0,
                "{table}: 个人作用域不应见任何租户行"
            );
        }
        tx.rollback().await.unwrap();

        // 仓储方法走同一个 scoped_tx:在脏连接上照常只回本作用域
        let ca_repo = PgContentRepo::new(dirty.clone());
        assert_eq!(
            ca_repo
                .list(a, owner)
                .await
                .unwrap()
                .iter()
                .map(|x| x.id)
                .collect::<Vec<_>>(),
            vec![ca]
        );
        assert!(is_not_found(ca_repo.get(a, cp).await));
        assert!(is_not_found(ca_repo.get(ContentScope::Personal, cb).await));
    }

    /// 建对象的父行检查与并发软删的竞态:软删事务已改 deleted_at、未提交时并发 `objects.create`,
    /// 父行检查的 `FOR SHARE` 必须**等**软删提交,然后重查到 deleted_at 非空 → NotFound,库里不留挂在已删内容下的对象。
    /// (修复前检查不加锁,插入时的 FK 检查只拿 FOR KEY SHARE,不和软删的 FOR NO KEY UPDATE 冲突 → 不等、直接建成。)
    #[sqlx::test(migrations = false)]
    async fn object_create_waits_for_concurrent_parent_soft_delete(
        opts: PgPoolOptions,
        conn: PgConnectOptions,
    ) {
        let p = bootstrap(opts, conn).await;
        let (c, _) = repos(&p);
        let a = tenant();
        let ta = a.tenant_id().unwrap();
        let owner = Uuid::now_v7();
        let parent = c
            .create(a, new_content(owner, "parent"), None)
            .await
            .unwrap();

        // 并发软删:改了 deleted_at、不提交(持有该行的 FOR NO KEY UPDATE 锁)
        let mut del = p.rt.begin().await.unwrap();
        set_tenant(&mut del, ta).await;
        let n = sqlx::query("update content set deleted_at = now() where id = $1")
            .bind(parent.id)
            .execute(&mut *del)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(n, 1);

        let objs = PgObjectRepo::new(p.rt.clone());
        let key = format!("tenants/{ta}/{}/x", parent.id);
        let pid = parent.id;
        let create = tokio::spawn(async move { objs.create(a, new_object(pid, &key), None).await });

        // 等 create 卡在行锁上(本测试库里出现一个 wait_event_type = 'Lock' 的会话)
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut waited = false;
        while std::time::Instant::now() < deadline && !create.is_finished() {
            let n: i64 = sqlx::query_scalar(
                "select count(*) from pg_stat_activity
                  where datname = current_database() and wait_event_type = 'Lock'",
            )
            .fetch_one(&p.owner)
            .await
            .unwrap();
            if n > 0 {
                waited = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        if !waited {
            del.rollback().await.unwrap();
            panic!(
                "objects.create 应等并发软删提交(父行检查要加 FOR SHARE),实际没等: {:?}",
                create.await.unwrap()
            );
        }
        del.commit().await.unwrap();
        let res = create.await.unwrap();
        assert!(
            matches!(res, Err(ContentError::NotFound)),
            "软删提交后应重查到已删 → NotFound,实际 {res:?}"
        );

        // 库里没有挂在已删内容下的对象
        let mut tx = p.rt.begin().await.unwrap();
        set_tenant(&mut tx, ta).await;
        assert_eq!(
            count_where(&mut tx, "object", "content_id = $1", pid).await,
            0
        );
        tx.rollback().await.unwrap();
    }

    /// 直接 SQL:运行时账号不设 app.tenant_id / app.personal_scope → 每张表 0 行、写不进;
    /// 设了才看得到本作用域的行。顺带审计:运行时账号非 owner、无 BYPASSRLS,五张表都 ENABLE + FORCE RLS 且有策略。
    #[sqlx::test(migrations = false)]
    async fn query_without_tenant_setting_returns_nothing(
        opts: PgPoolOptions,
        conn: PgConnectOptions,
    ) {
        let p = bootstrap(opts, conn).await;
        let (c, o) = repos(&p);
        let t = tenant();
        let tid = t.tenant_id().unwrap();
        let b = tenant();
        let tid_b = b.tenant_id().unwrap();
        let owner = Uuid::now_v7();
        // 两个租户各铺一整套(content_derived 也铺),外加一套个人内容
        let seeded_a = seed_with_derived(&p, &c, &o, t, owner).await;
        seed_with_derived(&p, &c, &o, b, owner).await;
        seed_one(&c, &o, ContentScope::Personal, owner).await;

        let mut rt = p.rt.acquire().await.unwrap();
        // 账号审计
        let (who, bypass): (String, bool) = sqlx::query_as(
            "select current_user::text, rolbypassrls from pg_roles where rolname = current_user",
        )
        .fetch_one(&mut *rt)
        .await
        .unwrap();
        assert_eq!(who, RT_ROLE);
        assert!(!bypass, "运行时账号不得 BYPASSRLS");
        for table in TABLES {
            let (owner_name, rls, force, policies): (String, bool, bool, i64) = sqlx::query_as(
                "select pg_get_userbyid(c.relowner)::text, c.relrowsecurity, c.relforcerowsecurity,
                        (select count(*) from pg_policies p
                          where p.schemaname = 'content' and p.tablename = c.relname)
                   from pg_class c join pg_namespace n on n.oid = c.relnamespace
                  where n.nspname = 'content' and c.relname = $1",
            )
            .bind(table)
            .fetch_one(&mut *rt)
            .await
            .unwrap();
            assert_ne!(owner_name, RT_ROLE, "{table}: 运行时账号不得是 owner");
            assert!(rls && force, "{table}: 必须 ENABLE + FORCE RLS");
            assert!(policies >= 1, "{table}: 必须有策略");
        }

        // 不设任何作用域:读 0 行
        for table in TABLES {
            assert_eq!(count(&mut rt, table).await, 0, "{table} 不设租户应 0 行");
        }
        // 不设任何作用域:写不进(WITH CHECK)
        let err = sqlx::query("insert into content (id, tenant_id, owner_id) values ($1, $2, $3)")
            .bind(Uuid::now_v7())
            .bind(tid)
            .bind(owner)
            .execute(&mut *rt)
            .await
            .expect_err("不设租户不许写");
        assert_eq!(
            sqlstate(&err).as_deref(),
            Some("42501"),
            "应是 RLS 拒绝: {err}"
        );

        // 设了租户 A:用**不带 tenant_id 条件**的 SQL 查,每张表都只见 A 自己那套
        // (content 2 行 = 原件 + 派生件,其余各 1 行);B 的行、个人行一行都看不见 —— 只靠 RLS 隔开。
        let mut tx = sqlx::Acquire::begin(&mut *rt).await.unwrap();
        set_tenant(&mut tx, tid).await;
        for (table, n) in PER_TENANT_ROWS {
            assert_eq!(
                count(&mut tx, table).await,
                n,
                "{table} 设租户 A 后应只见 A 的 {n} 行"
            );
            assert_eq!(
                count_where(&mut tx, table, "tenant_id is distinct from $1", tid).await,
                0,
                "{table}: A 的上下文里不该出现任何非 A 的行(B / 个人)"
            );
        }
        tx.commit().await.unwrap();
        // B 的数据确实在库里(换到 B 的上下文,同样的 SQL 看得见),上面的 0 不是因为没铺数据
        let mut tx = sqlx::Acquire::begin(&mut *rt).await.unwrap();
        set_tenant(&mut tx, tid_b).await;
        for (table, n) in PER_TENANT_ROWS {
            assert_eq!(
                count_where(&mut tx, table, "tenant_id = $1", tid_b).await,
                n,
                "{table}: B 的上下文应看到 B 的 {n} 行"
            );
        }
        tx.commit().await.unwrap();
        // 在 A 的上下文里写 tenant_id = B → WITH CHECK 拒(42501):插 content / content_derived、把 A 的行改成 B
        let (ca, ca_child) = (seeded_a.0, seeded_a.1);
        for (sql, binds) in [
            (
                "insert into content (id, tenant_id, owner_id) values ($1, $2, $3)",
                vec![Uuid::now_v7(), tid_b, owner],
            ),
            (
                "insert into content_derived (parent_id, content_id, variant, derivation_type, tenant_id)
                 values ($1, $2, 'x', 'y', $3)",
                vec![ca, ca_child, tid_b],
            ),
            (
                "update content set tenant_id = $2 where id = $1",
                vec![ca, tid_b],
            ),
        ] {
            let mut tx = sqlx::Acquire::begin(&mut *rt).await.unwrap();
            set_tenant(&mut tx, tid).await;
            let mut q = sqlx::query(sql);
            for b in binds {
                q = q.bind(b);
            }
            let err = q
                .execute(&mut *tx)
                .await
                .expect_err("A 的上下文里写 B 的 tenant_id 必须被拒");
            assert_eq!(
                sqlstate(&err).as_deref(),
                Some("42501"),
                "应是 RLS WITH CHECK 拒绝: {sql}: {err}"
            );
            tx.rollback().await.unwrap();
        }
        // 事务级 set_config:提交后不残留(同一连接再查 → 0 行)
        assert_eq!(
            count(&mut rt, "content").await,
            0,
            "事务结束后租户上下文不残留"
        );

        let mut tx = sqlx::Acquire::begin(&mut *rt).await.unwrap();
        sqlx::query("select set_config('app.personal_scope', 'on', true)")
            .execute(&mut *tx)
            .await
            .unwrap();
        assert_eq!(
            count(&mut tx, "content").await,
            1,
            "个人作用域只见个人那 1 行"
        );
        tx.commit().await.unwrap();
        assert_eq!(
            count(&mut rt, "content").await,
            0,
            "事务结束后个人作用域不残留"
        );
    }

    /// 迁移:旧库里所有 tenant_id(nil 或旧 API 收下的任意客户端值)全部归入默认租户(子表从 content 回填);
    /// 迁移后用默认租户作用域能读回旧数据,旧 key 不搬家。
    #[sqlx::test(migrations = false)]
    async fn legacy_nil_rows_migrated_to_default_tenant(
        opts: PgPoolOptions,
        conn: PgConnectOptions,
    ) {
        let owner = owner_pool(opts, conn.clone()).await;
        // 只跑到多租户之前的版本(0001 + 0002),铺旧形状的数据
        CONTENT_MIGRATOR.run_to(2, &owner).await.expect("跑到 0002");
        let (legacy, child, other) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let (legacy_obj, other_obj) = (Uuid::now_v7(), Uuid::now_v7());
        let other_tenant = Uuid::now_v7();
        let user = Uuid::now_v7();
        for (id, tenant) in [
            (legacy, Uuid::nil()),
            (child, Uuid::nil()),
            (other, other_tenant),
        ] {
            sqlx::query(
                "insert into content (id, tenant_id, owner_id, name, status)
                 values ($1, $2, $3, 'old', 'uploaded')",
            )
            .bind(id)
            .bind(tenant)
            .bind(user)
            .execute(&owner)
            .await
            .unwrap();
        }
        sqlx::query("insert into content_metadata (content_id, file_size) values ($1, 42)")
            .bind(legacy)
            .execute(&owner)
            .await
            .unwrap();
        for (oid, cid) in [(legacy_obj, legacy), (other_obj, other)] {
            sqlx::query(
                "insert into object (id, content_id, storage_backend_name, object_key, status)
                 values ($1, $2, 'memory', $3, 'uploaded')",
            )
            .bind(oid)
            .bind(cid)
            .bind(format!("{cid}/old"))
            .execute(&owner)
            .await
            .unwrap();
        }
        sqlx::query("insert into object_metadata (object_id, size_bytes) values ($1, 42)")
            .bind(legacy_obj)
            .execute(&owner)
            .await
            .unwrap();
        sqlx::query(
            "insert into content_derived (parent_id, content_id, variant, derivation_type)
             values ($1, $2, 'thumb', 'resize')",
        )
        .bind(legacy)
        .bind(child)
        .execute(&owner)
        .await
        .unwrap();

        // 跑剩下的迁移(多租户)
        CONTENT_MIGRATOR.run(&owner).await.expect("跑多租户迁移");
        let rt = runtime_pool(&owner, conn).await;
        let p = Pools { owner, rt };
        let (c, o) = repos(&p);

        // 旧 nil 行 → 默认租户可读(content / metadata / object / object_metadata)
        let d = ContentScope::tenant(DEFAULT_TENANT_ID).unwrap();
        let got = c.get(d, legacy).await.expect("旧内容归入默认租户");
        assert_eq!(got.tenant_id, Some(DEFAULT_TENANT_ID));
        assert_eq!(c.get_metadata(d, legacy).await.unwrap().file_size, Some(42));
        let objs = o.list_by_content(d, legacy).await.unwrap();
        assert_eq!(
            objs.iter().map(|x| x.id).collect::<Vec<_>>(),
            vec![legacy_obj]
        );
        assert_eq!(
            o.get_metadata(d, legacy_obj).await.unwrap().size_bytes,
            Some(42)
        );
        assert_eq!(
            c.list(d, user).await.unwrap().len(),
            3,
            "legacy + child + 旧的任意租户行"
        );
        // 旧 key 不搬家,照旧能按原 key 查到
        assert_eq!(
            o.get_by_key(d, &format!("{legacy}/old"), "memory")
                .await
                .unwrap()
                .id,
            legacy_obj
        );
        // 旧 API 收客户端传的任意 tenant_id:非 nil 的旧行也一律归入默认租户,原值作用域下看不到
        assert_eq!(
            c.get(d, other).await.unwrap().tenant_id,
            Some(DEFAULT_TENANT_ID)
        );
        assert_eq!(o.get(d, other_obj).await.unwrap().content_id, other);
        let x = ContentScope::tenant(other_tenant).unwrap();
        assert!(is_not_found(c.get(x, other).await));
        assert!(is_not_found(o.get(x, other_obj).await));

        // 直接 SQL:content_derived 也回填到默认租户;任何表都不再有 nil 行
        let mut tx = p.rt.begin().await.unwrap();
        set_tenant(&mut tx, DEFAULT_TENANT_ID).await;
        let derived: Vec<Option<Uuid>> =
            sqlx::query_scalar("select tenant_id from content_derived")
                .fetch_all(&mut *tx)
                .await
                .unwrap();
        assert_eq!(derived, vec![Some(DEFAULT_TENANT_ID)]);
        tx.rollback().await.unwrap();
        let mut tx = p.rt.begin().await.unwrap();
        set_tenant(&mut tx, Uuid::nil()).await;
        for table in TABLES {
            assert_eq!(
                count(&mut tx, table).await,
                0,
                "{table} 不应再有 nil 租户行"
            );
        }
        tx.rollback().await.unwrap();
    }
}
