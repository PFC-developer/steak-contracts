# changelog

## 3.0.22
* `SubmitBatch` takes each batch's unbonding end from the staking module's `unbond` event (`completion_time`, the latest across its undelegations) instead of `submit time + unbond_period`, so a configured `unbond_period` that differs from the chain's can no longer make `Reconcile` write a batch down before its funds are paid; a difference is reported as `steakhub/unbond_completion_time`, and the estimate stays if the chain reports no completion time
* the reward reinvest that follows `RedelegateInactive` honours its `exclude` list, so an excluded validator that refuses new stake can no longer revert every re-run (`CallbackMsg::HarvestAfterRedelegation` / `ReinvestIfNeeded` gain a defaulted `exclude` field)
* add owner-only `RedelegateInactive { limit, exclude }` to both hubs: moves stake off validators outside the active set (paused, or removed without redelegation) onto the active validators the chain currently has bonded. Each redelegation is an isolated submessage, so ones the staking module refuses (transitive redelegation, max entries) are skipped and reported, and the call can be re-run once they mature; `exclude` keeps known-blocked validators from using up `limit` (default 25, max 50). Rewards withdrawn by the redelegations are harvested and reinvested atomically through the new `CallbackMsg::HarvestAfterRedelegation` / `ReinvestIfNeeded`; a reward-finalization failure reverts the transaction so no reward balance can be stranded, and reinvestment targets only active validators currently bonded on chain
* `pfc-steak` 3.0.21: new `ExecuteMsg::RedelegateInactive` and `CallbackMsg` variants

## 3.0.21
* account for every on-chain delegation exactly once when reporting state and submitting batches
* repair and preserve the active-validator registry invariant during migration and validator updates; migration drops active entries for removed validators instead of re-whitelisting them
* a failed automatic batch submission no longer reverts the user's unbond request
* reject empty or unbacked staking operations and use checked exchange-rate arithmetic

## 3.0.17
* started changelog
