-- 多租户(单机):content 各表按租户隔离 + 个人内容(头像)特例。
-- 设计见 baserust-stack docs/superpowers/specs/2026-11-25-multi-tenant-design.md §4.4 / §4.7。
--
-- * 五张表都带 tenant_id(子表不靠子查询判归属):content(已有列)、content_metadata、object、object_metadata、content_derived。
-- * tenant_id 可空:NULL = 个人内容(ContentScope::Personal,不属于任何租户);nil UUID 永远不是合法租户(CHECK 拒)。
-- * 旧数据:content 里**所有**旧 tenant_id 回填成默认租户 00000000-0000-0000-0000-000000000001(nil,以及多租户之前
--   API 原样收下的任意客户端值 —— 那些值从来不是真实租户),子表从父行回填。
-- * RLS:ENABLE + FORCE(owner 也受约束);仓储每个方法在事务里先 set_config(..., true) 再查。
--   不设 app.tenant_id / app.personal_scope 时读不到、也写不进任何行。
-- 回填必须在开 RLS 之前做(FORCE 之后迁移账号自己也看不到行)。

-- ── 1) 加列 + 回填 ──
alter table content alter column tenant_id drop not null;
update content
   set tenant_id = '00000000-0000-0000-0000-000000000001'
 where tenant_id is not null;

alter table content_metadata add column tenant_id uuid;
update content_metadata m set tenant_id = c.tenant_id from content c where c.id = m.content_id;

alter table object add column tenant_id uuid;
update object o set tenant_id = c.tenant_id from content c where c.id = o.content_id;

alter table object_metadata add column tenant_id uuid;
update object_metadata om set tenant_id = o.tenant_id from object o where o.id = om.object_id;

alter table content_derived add column tenant_id uuid;
update content_derived d set tenant_id = c.tenant_id from content c where c.id = d.content_id;

-- ── 2) nil 永远不是合法租户(NULL = 个人内容,CHECK 对 NULL 放行)──
alter table content          add constraint content_tenant_not_nil          check (tenant_id <> '00000000-0000-0000-0000-000000000000');
alter table content_metadata add constraint content_metadata_tenant_not_nil check (tenant_id <> '00000000-0000-0000-0000-000000000000');
alter table object           add constraint object_tenant_not_nil           check (tenant_id <> '00000000-0000-0000-0000-000000000000');
alter table object_metadata  add constraint object_metadata_tenant_not_nil  check (tenant_id <> '00000000-0000-0000-0000-000000000000');
alter table content_derived  add constraint content_derived_tenant_not_nil  check (tenant_id <> '00000000-0000-0000-0000-000000000000');

-- ── 3) (tenant_id, …) 索引:仓储 SQL 显式带租户条件,走这些索引 ──
drop index if exists content_owner_tenant_alive_idx;
create index content_tenant_owner_alive_idx   on content (tenant_id, owner_id) where deleted_at is null;
create index content_metadata_tenant_idx      on content_metadata (tenant_id, content_id);
create index object_tenant_content_alive_idx  on object (tenant_id, content_id) where deleted_at is null;
create index object_metadata_tenant_idx       on object_metadata (tenant_id, object_id);
create index content_derived_tenant_parent_alive_idx on content_derived (tenant_id, parent_id) where deleted_at is null;

-- ── 4) RLS:租户分支 OR 个人分支(USING 与 WITH CHECK 同一表达式)──
alter table content enable row level security;
alter table content force row level security;
create policy tenant_isolation on content
    using      (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid
                or (tenant_id is null and current_setting('app.personal_scope', true) = 'on'))
    with check (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid
                or (tenant_id is null and current_setting('app.personal_scope', true) = 'on'));

alter table content_metadata enable row level security;
alter table content_metadata force row level security;
create policy tenant_isolation on content_metadata
    using      (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid
                or (tenant_id is null and current_setting('app.personal_scope', true) = 'on'))
    with check (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid
                or (tenant_id is null and current_setting('app.personal_scope', true) = 'on'));

alter table object enable row level security;
alter table object force row level security;
create policy tenant_isolation on object
    using      (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid
                or (tenant_id is null and current_setting('app.personal_scope', true) = 'on'))
    with check (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid
                or (tenant_id is null and current_setting('app.personal_scope', true) = 'on'));

alter table object_metadata enable row level security;
alter table object_metadata force row level security;
create policy tenant_isolation on object_metadata
    using      (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid
                or (tenant_id is null and current_setting('app.personal_scope', true) = 'on'))
    with check (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid
                or (tenant_id is null and current_setting('app.personal_scope', true) = 'on'));

alter table content_derived enable row level security;
alter table content_derived force row level security;
create policy tenant_isolation on content_derived
    using      (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid
                or (tenant_id is null and current_setting('app.personal_scope', true) = 'on'))
    with check (tenant_id = nullif(current_setting('app.tenant_id', true), '')::uuid
                or (tenant_id is null and current_setting('app.personal_scope', true) = 'on'));
