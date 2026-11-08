-- A workspace's connection to a repository, and where a run came from.
create table vcs_links (
  workspace_id text primary key references workspaces(id) on delete cascade,
  repo text not null,
  branch text not null,
  working_dir text not null default '',
  webhook_secret text not null check (webhook_secret like 'enc:v1:%'),
  token text check (token is null or token like 'enc:v1:%'),
  created_at timestamptz not null default now()
);
alter table runs add column vcs jsonb;
