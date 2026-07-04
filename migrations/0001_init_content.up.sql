-- content schema 第一版:content + content_metadata + object + object_metadata(SCAFFOLD-CORE 四表)。
-- 无 schema 前缀:靠连接的 search_path 落位(对齐 idm/app 的写法,本库不关心落哪个 schema)。
-- 时间统一 UTC timestamptz;updated_at 由本 schema **自有**的触发器函数维护。
-- id 由仓储在 Rust 侧 mint(Uuid::now_v7),故列无 DEFAULT —— 不依赖 uuid-ossp 扩展。

-- updated_at 自动维护函数:content schema **自建一份**(自包含,不跨 schema 复用别人的)。
create or replace function set_updated_at_utc()
returns trigger as $$
begin
    new.updated_at = (now() at time zone 'utc');
    return new;
end;
$$ language plpgsql;

-- ── content:内容主体 + 审计 + 软删 ──
create table content (
    id              uuid        primary key,
    tenant_id       uuid        not null,
    owner_id        uuid        not null,
    owner_type      text,
    name            text,
    description     text,
    document_type   text,
    status          text        not null default 'created',           -- 生命周期状态(VARCHAR 边界,服务侧 typed)
    derivation_type text,                                             -- 'original' | 'derived'(保留;无 derived 逻辑)
    created_by      text,
    created_at      timestamptz not null default (now() at time zone 'utc'),
    updated_by      text,
    updated_at      timestamptz not null default (now() at time zone 'utc'),
    deleted_at      timestamptz
);
-- 存活 + keyset(id v7 单列全序)翻页索引
create index content_alive_id_idx on content (id desc) where deleted_at is null;
-- list(owner_id, tenant_id) 的存活过滤索引
create index content_owner_tenant_alive_idx on content (owner_id, tenant_id) where deleted_at is null;
create trigger content_set_updated_at
    before update on content for each row execute function set_updated_at_utc();

-- ── content_metadata:1:1 挂 content(PK=content_id,upsert)──
-- tags TEXT[] / metadata JSONB 设 NOT NULL DEFAULT,使 FromRow 直解 Vec<String>/serde_json::Value(免 Option 包裹)。
create table content_metadata (
    content_id         uuid        primary key references content (id) on delete cascade,
    tags               text[]      not null default '{}',
    file_size          bigint,
    file_name          text,
    mime_type          text,
    checksum           text,
    checksum_algorithm text,
    metadata           jsonb       not null default '{}',
    created_at         timestamptz not null default (now() at time zone 'utc'),
    updated_at         timestamptz not null default (now() at time zone 'utc')
);
create trigger content_metadata_set_updated_at
    before update on content_metadata for each row execute function set_updated_at_utc();

-- ── object:存储后端里的一份字节,挂 content(多对一)+ 审计 + 软删 ──
create table object (
    id                   uuid        primary key,
    content_id           uuid        not null references content (id) on delete cascade,
    storage_backend_name text        not null,                        -- 哪个后端(memory/minio/s3…)
    storage_class        text,
    object_key           text        not null,                        -- 后端内的 key
    file_name            text,
    version              integer     not null default 1,              -- v0.1 钉死 1(多版本 DEFER)
    object_type          text,
    status               text        not null default 'created',
    created_by           text,
    created_at           timestamptz not null default (now() at time zone 'utc'),
    updated_by           text,
    updated_at           timestamptz not null default (now() at time zone 'utc'),
    deleted_at           timestamptz
);
create index object_content_alive_idx on object (content_id) where deleted_at is null;
-- (storage_backend_name, object_key) 在**存活行内唯一**(软删感知,对齐 baserust widget 的部分唯一索引约定):
-- 违例由仓储下钻成 ContentError::Conflict 而非 500。软删后同 key 可复用。
create unique index object_backend_key_alive_uidx
    on object (storage_backend_name, object_key) where deleted_at is null;
create trigger object_set_updated_at
    before update on object for each row execute function set_updated_at_utc();

-- ── object_metadata:1:1 挂 object(PK=object_id,upsert)──
create table object_metadata (
    object_id  uuid        primary key references object (id) on delete cascade,
    size_bytes bigint,
    mime_type  text,
    etag       text,
    metadata   jsonb       not null default '{}',
    created_at timestamptz not null default (now() at time zone 'utc'),
    updated_at timestamptz not null default (now() at time zone 'utc')
);
create trigger object_metadata_set_updated_at
    before update on object_metadata for each row execute function set_updated_at_utc();
