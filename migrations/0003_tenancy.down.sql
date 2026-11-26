-- 回退 0003_tenancy:关 RLS、删策略 / 约束 / 索引 / 子表 tenant_id 列,content.tenant_id 恢复 not null。
-- 个人内容(tenant_id IS NULL)回退成 nil(多租户之前的单租户写法);默认租户的行保持默认租户 UUID
-- (分不出哪些原来是 nil,且它在旧代码看来只是一个普通租户值)。

drop policy if exists tenant_isolation on content_derived;
drop policy if exists tenant_isolation on object_metadata;
drop policy if exists tenant_isolation on object;
drop policy if exists tenant_isolation on content_metadata;
drop policy if exists tenant_isolation on content;
alter table content_derived  no force row level security;
alter table content_derived  disable row level security;
alter table object_metadata  no force row level security;
alter table object_metadata  disable row level security;
alter table object           no force row level security;
alter table object           disable row level security;
alter table content_metadata no force row level security;
alter table content_metadata disable row level security;
alter table content          no force row level security;
alter table content          disable row level security;

drop index if exists content_derived_tenant_parent_alive_idx;
drop index if exists object_metadata_tenant_idx;
drop index if exists object_tenant_content_alive_idx;
drop index if exists content_metadata_tenant_idx;
drop index if exists content_tenant_owner_alive_idx;
create index if not exists content_owner_tenant_alive_idx on content (owner_id, tenant_id) where deleted_at is null;

alter table content_derived  drop constraint if exists content_derived_tenant_not_nil;
alter table object_metadata  drop constraint if exists object_metadata_tenant_not_nil;
alter table object           drop constraint if exists object_tenant_not_nil;
alter table content_metadata drop constraint if exists content_metadata_tenant_not_nil;
alter table content          drop constraint if exists content_tenant_not_nil;

alter table content_derived  drop column if exists tenant_id;
alter table object_metadata  drop column if exists tenant_id;
alter table object           drop column if exists tenant_id;
alter table content_metadata drop column if exists tenant_id;

update content set tenant_id = '00000000-0000-0000-0000-000000000000' where tenant_id is null;
alter table content alter column tenant_id set not null;
