# changelog

## 3.0.22
* add owner-only `RedelegateInactive { limit }` to both hubs: moves stake off validators outside the active set (paused, or removed without redelegation) onto the active validators. Each redelegation is an isolated submessage, so ones the staking module refuses (transitive redelegation, max entries) are skipped and reported, and the call can be re-run once they mature. Rewards withdrawn by redelegation are harvested and reinvested in the same transaction without losing the pre-redelegation balance baseline

## 3.0.21
* account for every on-chain delegation exactly once when reporting state and submitting batches
* repair and preserve the active-validator registry invariant during migration and validator updates; migration drops active entries for removed validators instead of re-whitelisting them
* a failed automatic batch submission no longer reverts the user's unbond request
* reject empty or unbacked staking operations and use checked exchange-rate arithmetic

## 3.0.17
* started changelog
