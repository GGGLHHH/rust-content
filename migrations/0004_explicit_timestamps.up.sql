-- 时间戳由调用方决定:仓储(rust-content 0.5.0 起)显式写 created_at / updated_at / deleted_at,
-- 值是 service 经注入的时钟取的 now,不再靠列默认值。
--
-- 0001 的 set_updated_at_utc() 无条件 new.updated_at = now(),会把显式写入的 updated_at 覆盖掉。改成:
-- * 这次 UPDATE 改了 updated_at(新值 ≠ 旧值)→ 保留显式写入的值;
-- * 没改(新值 = 旧值)→ 照旧置 now(),仓储之外的 SQL(运维脚本、回填)仍自动维护;
-- * 例外:事务级开关 app.explicit_updated_at = 'on' 时一律保留。仓储每个事务都设它(pg_support::scoped_tx)——
--   显式写入的值恰好等于旧值(同一个 now 先建行再翻状态、测试用固定时钟)也是调用方要的值,
--   光比新旧值分不清「没改」和「改成同一个值」。
-- 五张表(content / content_metadata / object / object_metadata / content_derived)的触发器都调这个函数,
-- 只换函数体,不动触发器。
create or replace function set_updated_at_utc()
returns trigger as $$
begin
    if new.updated_at is not distinct from old.updated_at
       and coalesce(current_setting('app.explicit_updated_at', true), '') <> 'on' then
        new.updated_at = (now() at time zone 'utc');
    end if;
    return new;
end;
$$ language plpgsql;
