# changelog

## 3.0.22
* `Reconcile` waits `RECONCILE_GRACE_SECONDS` (10 min) past a batch's estimated unbond end before writing it down, so a reconcile in the block where the unbonding matures can no longer wipe out the batch; funds owed to already-reconciled batches no longer hide a shortfall; the write-down is capped at the batches' total
* add owner-only `RedelegateInactive { limit, exclude }` to both hubs: moves stake off validators outside the active set (paused, or removed without redelegation) onto the active validators the chain currently has bonded. Each redelegation is an isolated submessage, so ones the staking module refuses (transitive redelegation, max entries) are skipped and reported, and the call can be re-run once they mature; `exclude` keeps known-blocked validators from using up `limit` (default 25, max 50). Rewards withdrawn by the redelegations are harvested and reinvested in the same transaction through the new `CallbackMsg::HarvestAfterRedelegation` / `ReinvestIfNeeded`; a failure there is reported as `steakhub/reinvest_deferred` instead of reverting the redelegations
* `pfc-steak` 3.0.21: new `ExecuteMsg::RedelegateInactive` and `CallbackMsg` variants

## 3.0.21
* account for every on-chain delegation exactly once when reporting state and submitting batches
* repair and preserve the active-validator registry invariant during migration and validator updates; migration drops active entries for removed validators instead of re-whitelisting them
* a failed automatic batch submission no longer reverts the user's unbond request
* reject empty or unbacked staking operations and use checked exchange-rate arithmetic

## 3.0.17
* started changelog
