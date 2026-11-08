-- A run can now end as force_canceled.
alter table runs drop constraint runs_status_check;
alter table runs add constraint runs_status_check check (status in (
  'pending','planning','needs_confirmation','apply_queued','applying',
  'applied','planned_and_finished','errored','discarded','canceled','force_canceled'));
