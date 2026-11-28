use cosmwasm_std::{Addr, Coin, Decimal, StdError, StdResult, Storage, Uint128};
use cw_storage_plus::{Index, IndexList, IndexedMap, Item, Map, MultiIndex};
use pfc_steak::hub::{Batch, FeeType, PendingBatch, UnbondRequest};

use crate::types::BooleanKey;
pub(crate) const BATCH_KEY_V101: &str = "previous_batches_101";
pub(crate) const BATCH_KEY_RECONCILED_V101: &str = "previous_batches__reconciled_101";
/// First block height at which an unreconciled batch was observed past its estimated maturity.
/// Reconciliation is allowed only in a later block, after the staking module had an EndBlock in
/// which to pay the matured unbonding.
pub(crate) const RECONCILE_OBSERVED_HEIGHTS: Map<u64, u64> =
    Map::new("reconcile_observed_heights_001");

/// Native the contract owes to reconciled batches that are not yet fully withdrawn: the sum of
/// their `amount_unclaimed`. Kept as a running total (added on reconcile, subtracted on withdraw)
/// so `Reconcile` does not have to scan every reconciled batch, which grows without bound when
/// unbond requests are never claimed. Initialised by `migrate`.
pub(crate) const NATIVE_RESERVED: Item<Uint128> = Item::new("native_reserved_001");

/// Derivative tokens the hub minted to itself at zero supply (see `compute_first_mint`). They are
/// held by the hub and never redeemed; burning stray tokens must leave them alone.
pub(crate) const DEAD_SHARES: Item<Uint128> = Item::new("dead_shares_001");

/// Progress of the one-time, paginated initialisation of `NATIVE_RESERVED` (see
/// `advance_reserve_initialization`). Removed once the reserve exists.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReserveInit {
    /// Batches reconciled, or matured before this time, form the pool that shares any shortfall.
    pub pool_time: u64,
    /// `false` while summing what the pool is owed, `true` while applying the write-down.
    pub applying: bool,
    /// Last batch id processed in the current pass.
    pub cursor: Option<u64>,
    /// What the pool is owed, before any write-down.
    pub owed: Uint128,
    /// What the pool is short of, shared pro rata.
    pub shortfall: Uint128,
    /// How much of `shortfall` has been written down so far.
    pub applied: Uint128,
    /// What the pool batches processed so far in the applying pass are owed (for pro-rata
    /// rounding).
    #[serde(default)]
    pub cumulative: Uint128,
    /// Batches that matured after `pool_time` but before the sum finished; they join the pool.
    #[serde(default)]
    pub joined: Vec<u64>,
    /// Recorded rewards (`unlocked_coins`) kept out of what the pool could draw on; reported so
    /// operators can audit it, since an overstated figure deepens the pool's shortfall.
    #[serde(default)]
    pub unlocked: Uint128,
}

pub(crate) const RESERVE_INIT: Item<ReserveInit> = Item::new("reserve_init_001");

/// The batch `SubmitBatch` is undelegating, and the latest unbonding completion time the staking
/// module reported for it so far (see `register_undelegation`).
pub(crate) const SUBMITTED_BATCH: Item<(u64, Option<u64>)> = Item::new("submitted_batch_001");

pub(crate) struct State<'a> {
    /// Account who can call certain privileged functions
    pub owner: Item<'a, Addr>,
    /// Pending ownership transfer, awaiting acceptance by the new owner
    pub new_owner: Item<'a, Addr>,
    pub fee_account_type: Item<'a, FeeType>,
    /// Account to send fees to
    pub fee_account: Item<'a, Addr>,
    /// Current fee rate
    pub fee_rate: Item<'a, Decimal>,
    /// Maximum fee rate
    pub max_fee_rate: Item<'a, Decimal>,
    /// denom to accept
    pub denom: Item<'a, String>,
    /// Address of the Steak token
    pub steak_token: Item<'a, Addr>,
    /// How often the unbonding queue is to be executed
    pub epoch_period: Item<'a, u64>,
    /// The staking module's unbonding time, in seconds
    pub unbond_period: Item<'a, u64>,
    /// Validators who will receive the delegations
    pub validators: Item<'a, Vec<String>>,

    /// Coins that can be reinvested
    pub unlocked_coins: Item<'a, Vec<Coin>>,
    /// The current batch of unbonding requests queded to be executed
    pub pending_batch: Item<'a, PendingBatch>,

    /// Previous batches that have started unbonding but not yet finished
    pub previous_batches: IndexedMap<'a, u64, Batch, PreviousBatchesIndexes<'a>>,
    /// Users' shares in unbonding batches
    pub unbond_requests: IndexedMap<'a, (u64, &'a Addr), UnbondRequest, UnbondRequestsIndexes<'a>>,
    pub validators_active: Item<'a, Vec<String>>,
    /// coins in 'denom' held before reinvest was called.
    pub prev_denom: Item<'a, Uint128>,
    /// Dust Collector contract
    pub dust_collector: Item<'a, Option<Addr>>,
}

impl Default for State<'static> {
    fn default() -> Self {
        let pb_indexes = PreviousBatchesIndexes {
            reconciled: MultiIndex::new(
                |d: &Batch| d.reconciled.into(),
                BATCH_KEY_V101,
                BATCH_KEY_RECONCILED_V101,
            ),
        };
        let ubr_indexes = UnbondRequestsIndexes {
            user: MultiIndex::new(
                |d: &UnbondRequest| d.user.clone().into(),
                "unbond_requests",
                "unbond_requests__user",
            ),
        };
        Self {
            owner: Item::new("owner"),
            new_owner: Item::new("new_owner"),
            fee_account: Item::new("fee_account"),
            fee_rate: Item::new("fee_rate"),
            max_fee_rate: Item::new("max_fee_rate"),
            denom: Item::new("denom"),
            steak_token: Item::new("steak_token"),
            epoch_period: Item::new("epoch_period"),
            unbond_period: Item::new("unbond_period"),
            validators: Item::new("validators"),
            unlocked_coins: Item::new("unlocked_coins"),
            pending_batch: Item::new("pending_batch"),
            previous_batches: IndexedMap::new(BATCH_KEY_V101, pb_indexes),
            unbond_requests: IndexedMap::new("unbond_requests", ubr_indexes),
            validators_active: Item::new("validators_active"),
            prev_denom: Item::new("prev_denom"),
            fee_account_type: Item::new("fee_account_type"),
            dust_collector: Item::new("dust_collector"),
        }
    }
}

impl State<'_> {
    pub fn assert_owner(&self, storage: &dyn Storage, sender: &Addr) -> StdResult<()> {
        let owner = self.owner.load(storage)?;
        if *sender == owner {
            Ok(())
        } else {
            Err(StdError::generic_err("unauthorized: sender is not owner"))
        }
    }
}

pub(crate) struct PreviousBatchesIndexes<'a> {
    // pk goes to second tuple element
    pub reconciled: MultiIndex<'a, BooleanKey, Batch, Vec<u8>>,
}

impl IndexList<Batch> for PreviousBatchesIndexes<'_> {
    fn get_indexes(&'_ self) -> Box<dyn Iterator<Item = &'_ dyn Index<Batch>> + '_> {
        let v: Vec<&dyn Index<Batch>> = vec![&self.reconciled];
        Box::new(v.into_iter())
    }
}

pub(crate) struct UnbondRequestsIndexes<'a> {
    // pk goes to second tuple element
    pub user: MultiIndex<'a, String, UnbondRequest, Vec<u8>>,
}

impl IndexList<UnbondRequest> for UnbondRequestsIndexes<'_> {
    fn get_indexes(&'_ self) -> Box<dyn Iterator<Item = &'_ dyn Index<UnbondRequest>> + '_> {
        let v: Vec<&dyn Index<UnbondRequest>> = vec![&self.user];
        Box::new(v.into_iter())
    }
}
