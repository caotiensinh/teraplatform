-- A configuration version can exist before its archive is uploaded.
alter table configuration_versions add column status text not null default 'uploaded' check (status in ('pending','uploaded'));
