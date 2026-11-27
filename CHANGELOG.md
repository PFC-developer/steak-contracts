# changelog

## 3.0.22
* bond, reinvest, `ReinvestIdle` and `RemoveValidator` only send stake to active validators the chain currently has bonded (falling back to the whole active set if none is bonded), instead of the smallest delegation, which was often a slashed, jailed validator; `RemoveValidator` no longer redelegates onto paused validators
* `Rebalance` is owner only (anyone could lock validators against redelegation for an unbonding period), drains every validator outside the bonded active set in full, tops up active validators that hold nothing, and sends each move as an isolated submessage (max 50 per call)
* rewards withdrawn automatically by bond / unbond / redelegate / rebalance / remove-validator are now recorded (hub-tf replies to every staking message, like the CW20 hub) and reinvested at the next harvest; `Reinvest` bonds the recorded rewards instead of a balance delta that skipped them, and never bonds funds owed to matured batches
* new `QueryMsg::IdleFunds` and owner-only `ExecuteMsg::ReinvestIdle { amount }` to find and restake native that sits in the contract unaccounted for (legacy unrecorded rewards, late unbonding funds), fee-free
* new bonds are priced including the holders' share of rewards not yet restaked (recorded and still accruing), so a bonder no longer buys into rewards earned before they arrived
* `Reconcile` waits `RECONCILE_GRACE_SECONDS` (10 min) past a batch's estimated unbond end before writing it down, so a reconcile in the block where the unbonding matures can no longer wipe out the batch; funds owed to already-reconciled batches no longer hide a shortfall; the write-down is capped at the batches' total
* add owner-only `RedelegateInactive { limit, exclude }` to both hubs: moves stake off validators outside the active set (paused, or removed without redelegation) onto the active validators the chain currently has bonded. Each redelegation is an isolated submessage, so ones the staking module refuses (transitive redelegation, max entries) are skipped and reported, and the call can be re-run once they mature; `exclude` keeps known-blocked validators from using up `limit` (default 25, max 50). Rewards withdrawn by the redelegations are harvested and reinvested atomically through the new `CallbackMsg::HarvestAfterRedelegation` / `ReinvestIfNeeded`; a reward-finalization failure reverts the transaction so no reward balance can be stranded, and reinvestment targets only active validators currently bonded on chain
* `pfc-steak` 3.0.21: new `ExecuteMsg::RedelegateInactive` and `CallbackMsg` variants

## 3.0.21
* account for every on-chain delegation exactly once when reporting state and submitting batches
* repair and preserve the active-validator registry invariant during migration and validator updates; migration drops active entries for removed validators instead of re-whitelisting them
* a failed automatic batch submission no longer reverts the user's unbond request
* reject empty or unbacked staking operations and use checked exchange-rate arithmetic

## 3.0.17
* started changelog
