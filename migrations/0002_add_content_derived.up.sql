-- 派生内容(DEFER):独立迁移建表,SCAFFOLD-CORE **无任何 service 逻辑**碰它。
-- 仅为让 schema/迁移先到位 + types.rs 的 DerivedContent stub 有对应表;真正的派生编排留待后续。
-- 状态不在本表(track 在 content.status,避免重复),对齐 Go 注释。

-- set_updated_at_utc() 已由 0001 在本 schema 建好;此处 create or replace 一次保本迁移 up 自包含(幂等、无害)。
create or replace function set_updated_at_utc()
returns trigger as $$
begin
    new.updated_at = (now() at time zone 'utc');
    return new;
end;
$$ language plpgsql;

create table content_derived (
    parent_id           uuid        not null references content (id) on delete cascade,
    content_id          uuid        not null references content (id) on delete cascade,
    variant             text        not null,
    derivation_type     text        not null,
    derivation_params   jsonb       not null default '{}',
    processing_metadata jsonb       not null default '{}',
    created_at          timestamptz not null default (now() at time zone 'utc'),
    updated_at          timestamptz not null default (now() at time zone 'utc'),
    deleted_at          timestamptz
);
-- (parent_id, content_id) 软删感知唯一(不能直接做主键,因软删允许同对复用)。
create unique index content_derived_pair_alive_uidx
    on content_derived (parent_id, content_id) where deleted_at is null;
create index content_derived_parent_alive_idx on content_derived (parent_id) where deleted_at is null;
create index content_derived_variant_alive_idx on content_derived (variant) where deleted_at is null;
create trigger content_derived_set_updated_at
    before update on content_derived for each row execute function set_updated_at_utc();
