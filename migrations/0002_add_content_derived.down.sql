drop trigger if exists content_derived_set_updated_at on content_derived;
drop table if exists content_derived;
-- set_updated_at_utc() 是 0001 建的、core 四表仍在用,本迁移**不删**(留给 0001 down)。
