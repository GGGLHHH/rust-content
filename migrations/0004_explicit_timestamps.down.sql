-- 回退 0004_explicit_timestamps:恢复 0001 的函数体(每次 UPDATE 都无条件把 updated_at 置 now())。
create or replace function set_updated_at_utc()
returns trigger as $$
begin
    new.updated_at = (now() at time zone 'utc');
    return new;
end;
$$ language plpgsql;
