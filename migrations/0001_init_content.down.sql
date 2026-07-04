drop trigger if exists object_metadata_set_updated_at on object_metadata;
drop trigger if exists object_set_updated_at on object;
drop trigger if exists content_metadata_set_updated_at on content_metadata;
drop trigger if exists content_set_updated_at on content;
drop table if exists object_metadata;
drop table if exists object;
drop table if exists content_metadata;
drop table if exists content;
-- content 自有函数,本 migration 自包含 → down 最后删掉(不留垃圾)。
drop function if exists set_updated_at_utc();
