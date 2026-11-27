use std::{cmp, cmp::Ordering, collections::HashMap};

use cosmwasm_std::{StdError, StdResult, Uint128};
use pfc_steak::hub::Batch;

use crate::types::{Delegation, Redelegation, Undelegation};

//--------------------------------------------------------------------------------------------------
// Minting/burning logics
//--------------------------------------------------------------------------------------------------

/// Compute the amount of Steak token to mint for a specific Luna stake amount. If current total
/// staked amount is zero, we use 1 usteak = 1 native; otherwise, we calculate base on the current
/// native per ustake ratio.
///
/// `pending_rewards` is the holders' share (after fee) of rewards earned but not yet restaked.
/// It belongs to existing holders, so it is part of the backing a new bonder buys into; leaving
/// it out would hand the bonder a cut of rewards earned before they arrived.
pub(crate) fn compute_mint_amount(
    usteak_supply: Uint128,
    native_to_bond: Uint128,
    all_delegations: &[Delegation],
    pending_rewards: Uint128,
) -> StdResult<Uint128> {
    let native_bonded = total_delegated(all_delegations)?
        .checked_add(pending_rewards.u128())
        .ok_or_else(|| StdError::generic_err("delegation total overflow"))?;
    if native_bonded == 0 {
        if usteak_supply.is_zero() {
            Ok(native_to_bond)
        } else {
            Err(StdError::generic_err(
                "cannot mint while derivative supply exists without delegated backing",
            ))
        }
    } else {
        usteak_supply
            .checked_multiply_ratio(native_to_bond, native_bonded)
            .map_err(|err| StdError::generic_err(format!("failed to compute mint amount: {err}")))
    }
}

/// Compute the amount of `native` to unbond for a specific `usteak` burn amount
///
/// There is no way `usteak` total supply is zero when the user is senting a non-zero amount of
/// `usteak` to burn, so we don't need to handle division-by-zero here
pub(crate) fn compute_unbond_amount(
    usteak_supply: Uint128,
    usteak_to_burn: Uint128,
    all_delegations: &[Delegation],
) -> StdResult<Uint128> {
    if usteak_supply.is_zero() || usteak_to_burn > usteak_supply {
        return Err(StdError::generic_err("unbond amount exceeds the derivative token supply"));
    }

    Uint128::new(total_delegated(all_delegations)?)
        .checked_multiply_ratio(usteak_to_burn, usteak_supply)
        .map_err(|err| StdError::generic_err(format!("failed to compute unbond amount: {err}")))
}

fn total_delegated(delegations: &[Delegation]) -> StdResult<u128> {
    delegations.iter().try_fold(0u128, |total, delegation| {
        total
            .checked_add(delegation.amount)
            .ok_or_else(|| StdError::generic_err("delegation total overflow"))
    })
}

//--------------------------------------------------------------------------------------------------
// Delegation logics
//--------------------------------------------------------------------------------------------------

/// Given the current delegations made to validators, and a specific amount of `native` to unstake,
/// compute the undelegations to make such that the delegated amount to each validator is as even
/// as possible.
///
/// This function is based on Lido's implementation:
/// https://github.com/lidofinance/lido-terra-contracts/blob/v1.0.2/contracts/lido_terra_validators_registry/src/common.rs#L55-102
pub(crate) fn compute_undelegations(
    native_to_unbond: Uint128,
    current_delegations: &[Delegation],
    denom: &str,
) -> Vec<Undelegation> {
    let native_staked: u128 = current_delegations.iter().map(|d| d.amount).sum();
    let validator_count = current_delegations.len() as u128;

    let native_to_distribute = native_staked - native_to_unbond.u128();
    let native_per_validator = native_to_distribute / validator_count;
    let remainder = native_to_distribute % validator_count;

    let mut new_undelegations: Vec<Undelegation> = vec![];
    let mut native_available = native_to_unbond.u128();
    for (i, d) in current_delegations.iter().enumerate() {
        let remainder_for_validator: u128 = u128::from((i + 1) as u128 <= remainder);
        let native_for_validator = native_per_validator + remainder_for_validator;

        let mut native_to_undelegate = d.amount.saturating_sub(native_for_validator);

        native_to_undelegate = cmp::min(native_to_undelegate, native_available);
        native_available -= native_to_undelegate;

        if native_to_undelegate > 0 {
            new_undelegations.push(Undelegation::new(&d.validator, native_to_undelegate, denom));
        }

        if native_available == 0 {
            break;
        }
    }

    new_undelegations
}

/// Given a validator who is to be removed from the whitelist, and current delegations made to other
/// validators, compute the new delegations to make such that the delegated amount to each validator
// is as even as possible.
///
/// This function is based on Lido's implementation:
/// https://github.com/lidofinance/lido-terra-contracts/blob/v1.0.2/contracts/lido_terra_validators_registry/src/common.rs#L19-L53
pub(crate) fn compute_redelegations_for_removal(
    delegation_to_remove: &Delegation,
    current_delegations: &[Delegation],
    denom: &str,
) -> Vec<Redelegation> {
    let native_staked: u128 = current_delegations.iter().map(|d| d.amount).sum();
    let validator_count = current_delegations.len() as u128;

    let native_to_distribute = native_staked + delegation_to_remove.amount;
    let native_per_validator = native_to_distribute / validator_count;
    let remainder = native_to_distribute % validator_count;

    let mut new_redelegations: Vec<Redelegation> = vec![];
    let mut native_available = delegation_to_remove.amount;
    for (i, d) in current_delegations.iter().enumerate() {
        let remainder_for_validator: u128 = u128::from((i + 1) as u128 <= remainder);
        let native_for_validator = native_per_validator + remainder_for_validator;

        let mut native_to_redelegate = native_for_validator.saturating_sub(d.amount);

        native_to_redelegate = cmp::min(native_to_redelegate, native_available);
        native_available -= native_to_redelegate;

        if native_to_redelegate > 0 {
            new_redelegations.push(Redelegation::new(
                &delegation_to_remove.validator,
                &d.validator,
                native_to_redelegate,
                denom,
            ));
        }

        if native_available == 0 {
            break;
        }
    }

    new_redelegations
}

/// Plan redelegations that move stake off every validator that is not in `non_sources` and spread
/// it over `destinations`, topping up the destination delegations toward an even split.
///
/// `non_sources` is the contract's active set plus anything the caller excluded; `destinations`
/// is the subset of the active set that should receive stake (bonded on chain, not excluded).
///
/// The plan is derived purely from the current on-chain delegations, so running it again after
/// some redelegations were skipped (or after `limit` cut a run short) picks up exactly what is
/// left. At most `limit` redelegations are returned.
pub(crate) fn compute_redelegations_for_inactive(
    non_sources: &[String],
    destinations: &[String],
    current_delegations: &[Delegation],
    limit: usize,
    denom: &str,
) -> Vec<Redelegation> {
    if destinations.is_empty() || limit == 0 {
        return vec![];
    }

    let mut targets: Vec<Delegation> = destinations
        .iter()
        .map(|v| {
            let amount =
                current_delegations.iter().filter(|d| d.validator == *v).map(|d| d.amount).sum();
            Delegation::new(v, amount, denom)
        })
        .collect();

    let mut new_redelegations: Vec<Redelegation> = vec![];
    for source in
        current_delegations.iter().filter(|d| d.amount > 0 && !non_sources.contains(&d.validator))
    {
        for rd in compute_redelegations_for_removal(source, &targets, denom) {
            if new_redelegations.len() >= limit {
                return new_redelegations;
            }
            if let Some(target) = targets.iter_mut().find(|t| t.validator == rd.dst) {
                target.amount += rd.amount;
            }
            new_redelegations.push(rd);
        }
    }

    new_redelegations
}

/// Compute redelegation moves that even out stake across `targets`.
///
/// Every delegation on a validator that is not a target (paused, removed, or not currently bonded
/// on chain) is moved off in full. Each target is brought to `total / targets` (remainder to the
/// first ones), counting targets that hold nothing yet. Moves into or out of a target smaller than
/// `min_difference` are skipped; draining a non-target ignores it.
///
/// This does not guarantee the minimal number of moves.
pub(crate) fn compute_redelegations_for_rebalancing(
    targets: Vec<String>,
    current_delegations: &[Delegation],
    min_difference: Uint128,
    denom: &str,
) -> Vec<Redelegation> {
    if targets.is_empty() {
        return vec![];
    }
    let native_staked: u128 = current_delegations.iter().map(|d| d.amount).sum();
    let validator_count = targets.len() as u128;
    let native_per_validator = native_staked / validator_count;
    let remainder = native_staked % validator_count;

    let mut src_delegations: Vec<Delegation> = current_delegations
        .iter()
        .filter(|d| d.amount > 0 && !targets.contains(&d.validator))
        .cloned()
        .collect();
    let mut dst_delegations: Vec<Delegation> = vec![];
    for (i, validator) in targets.iter().enumerate() {
        let native_for_validator = native_per_validator + u128::from((i + 1) as u128 <= remainder);
        let current: u128 = current_delegations
            .iter()
            .filter(|d| d.validator == *validator)
            .map(|d| d.amount)
            .sum();
        match current.cmp(&native_for_validator) {
            Ordering::Greater if current - native_for_validator > min_difference.u128() => {
                src_delegations.push(Delegation::new(
                    validator,
                    current - native_for_validator,
                    denom,
                ));
            },
            Ordering::Less if native_for_validator - current > min_difference.u128() => {
                dst_delegations.push(Delegation::new(
                    validator,
                    native_for_validator - current,
                    denom,
                ));
            },
            _ => (),
        }
    }

    let mut new_redelegations: Vec<Redelegation> = vec![];
    while !src_delegations.is_empty() && !dst_delegations.is_empty() {
        let native_to_redelegate = cmp::min(src_delegations[0].amount, dst_delegations[0].amount);
        new_redelegations.push(Redelegation::new(
            &src_delegations[0].validator,
            &dst_delegations[0].validator,
            native_to_redelegate,
            denom,
        ));
        if src_delegations[0].amount == native_to_redelegate {
            src_delegations.remove(0);
        } else {
            src_delegations[0].amount -= native_to_redelegate;
        }
        if dst_delegations[0].amount == native_to_redelegate {
            dst_delegations.remove(0);
        } else {
            dst_delegations[0].amount -= native_to_redelegate;
        }
    }

    new_redelegations
}

//--------------------------------------------------------------------------------------------------
// Batch logics
//--------------------------------------------------------------------------------------------------

/// If the received native amount after the unbonding period is less than expected, e.g. due to
/// rounding error or the validator(s) being slashed, then deduct the difference in amount evenly
/// from each unreconciled batch.
///
/// The idea of "reconciling" is based on Stader's implementation:
/// https://github.com/stader-labs/stader-liquid-token/blob/v0.2.1/contracts/staking/src/contract.rs#L968-L1048
pub(crate) fn reconcile_batches(batches: &mut [Batch], native_to_deduct: Uint128) {
    let batch_count = batches.len() as u128;
    if batch_count == 0 {
        return;
    }
    let native_per_batch = native_to_deduct.u128() / batch_count;
    let remainder = native_to_deduct.u128() % batch_count;
    //let mut remaining_underflow = Uint128::zero();
    let mut underflows: HashMap<usize, Uint128> = HashMap::default();

    // distribute the underflows uniformly accross non-underflowing batches
    for (i, batch) in batches.iter_mut().enumerate() {
        let remainder_for_batch: u128 = u128::from((i + 1) as u128 <= remainder);
        let native_for_batch = Uint128::new(native_per_batch + remainder_for_batch);

        if batch.amount_unclaimed < native_for_batch && batch_count > 1 {
            //    remaining_underflow += native_for_batch - batch.amount_unclaimed;
            underflows.insert(i, native_for_batch - batch.amount_unclaimed);
        }
        batch.amount_unclaimed = batch.amount_unclaimed.saturating_sub(native_for_batch);

        batch.reconciled = true;
    }
    if !underflows.is_empty() {
        let batch_count: u128 = batch_count - (underflows.len() as u128);
        if batch_count == 0 {
            return;
        }
        let to_deduct: Uint128 = underflows.iter().map(|v| v.1).sum();
        let native_per_batch = to_deduct.u128() / batch_count;
        let remainder = to_deduct.u128() % batch_count;
        let mut remaining_underflow = Uint128::zero();
        // the remaining underflow will be applied by oldest batch first.
        for (i, batch) in batches.iter_mut().enumerate() {
            if !batch.amount_unclaimed.is_zero() {
                let remainder_for_batch: u128 = u128::from((i + 1) as u128 <= remainder);
                let native_for_batch = Uint128::new(native_per_batch + remainder_for_batch);
                if batch.amount_unclaimed < native_for_batch && batch_count > 1 {
                    remaining_underflow += native_for_batch - batch.amount_unclaimed;
                }
                batch.amount_unclaimed = batch.amount_unclaimed.saturating_sub(native_for_batch);
            }
        }

        if !remaining_underflow.is_zero() {
            // the remaining underflow will be applied by oldest batch first.
            for batch in batches.iter_mut() {
                //} .enumerate() {
                if !batch.amount_unclaimed.is_zero() && !remaining_underflow.is_zero() {
                    if batch.amount_unclaimed >= remaining_underflow {
                        batch.amount_unclaimed -= remaining_underflow;
                        remaining_underflow = Uint128::zero()
                    } else {
                        remaining_underflow -= batch.amount_unclaimed;
                        batch.amount_unclaimed = Uint128::zero();
                    }
                }
            }

            if !remaining_underflow.is_zero() {
                // no way to reconcile right now, need to top up some funds.
            }
        }
    }
}
