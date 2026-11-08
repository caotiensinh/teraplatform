-- What the lint step found in a run's configuration.
alter table runs add column lint jsonb not null default '[]';
