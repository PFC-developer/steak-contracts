use std::{cmp, cmp::Ordering, convert::TryFrom};

use cosmwasm_std::{StdError, StdResult, Uint128, Uint256};
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
    // With no supply there is nobody to price against: mint 1:1 regardless of any native the
    // contract already holds (pricing against it would mint 0 for the first bonder).
    if usteak_supply.is_zero() {
        return Ok(native_to_bond);
    }
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

/// Derivative tokens locked in the hub, at least, when it mints at zero supply.
pub(crate) const MIN_DEAD_SHARES: u128 = 1_000;

/// The first mint at zero supply. There is no price to go by, so dead shares, minted to the hub
/// itself and never redeemable, are locked alongside the bonder's: worth `existing_backing`
/// (tracked stake or rewards left from earlier holders) so the bonder cannot claim what was
/// already there, and at least `MIN_DEAD_SHARES`. Untracked idle is excluded: it receives
/// proportional dead shares only if it is later swept into delegated backing. Whatever part of
/// the minimum no existing backing covers comes out of the bonder's own mint, so the exchange
/// rate stays exactly 1:1 and the bonder's cost is at most `MIN_DEAD_SHARES`; a deposit too small
/// to cover it is refused.
///
/// Returns (minted to the bonder, dead shares).
pub(crate) fn compute_first_mint(
    native_to_bond: Uint128,
    existing_backing: Uint128,
) -> StdResult<(Uint128, Uint128)> {
    let dead = existing_backing.max(Uint128::new(MIN_DEAD_SHARES));
    let unbacked = dead - existing_backing;
    if native_to_bond <= unbacked {
        return Err(StdError::generic_err(format!(
            "the first bond into an empty hub must exceed {} base units",
            unbacked
        )));
    }
    Ok((native_to_bond - unbacked, dead))
}

/// Shares to lock when idle native becomes delegated backing.
///
/// Idle is deliberately excluded from bond pricing because anyone can increase it with a plain
/// transfer. Delegating it without increasing supply would nevertheless raise the exchange rate,
/// letting a bonder immediately before the sweep capture some of it. Minting these shares to the
/// hub keeps the priced backing per share from increasing. Round up so integer truncation cannot
/// leave even a one-unit upward jump.
pub(crate) fn compute_idle_sweep_shares(
    share_supply: Uint128,
    idle_to_delegate: Uint128,
    tracked_backing: Uint128,
) -> StdResult<Uint128> {
    // With no supply there is nobody to front-run. With no tracked backing, public bonds are
    // already refused, so the owner may use the sweep to restore backing without a cheap mint.
    if share_supply.is_zero() || idle_to_delegate.is_zero() || tracked_backing.is_zero() {
        return Ok(Uint128::zero());
    }

    let numerator = Uint256::from(share_supply).checked_mul(Uint256::from(idle_to_delegate))?;
    let denominator = Uint256::from(tracked_backing);
    let rounded_up = numerator
        .checked_add(denominator.checked_sub(Uint256::from(1u128))?)?
        .checked_div(denominator)?;
    Uint128::try_from(rounded_up)
        .map_err(|err| StdError::generic_err(format!("idle sweep share amount overflow: {err}")))
}

pub(crate) fn total_delegated(delegations: &[Delegation]) -> StdResult<u128> {
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

/// Compute redelegation moves that will make each validator's delegation the targeted amount
/// (hopefully this sentence makes sense)
///
/// This algorithm does not guarantee the minimal number of moves, but is the best I can some up
/// with...
pub(crate) fn compute_redelegations_for_rebalancing(
    validators_active: Vec<String>,
    current_delegations: &[Delegation],
    min_difference: Uint128,
) -> Vec<Redelegation> {
    let native_staked: u128 = current_delegations.iter().map(|d| d.amount).sum();
    let validator_count = validators_active.len() as u128;

    let native_per_validator = native_staked / validator_count;
    let remainder = native_staked % validator_count;

    // If a validator's current delegated amount is greater than the target amount, native will be
    // redelegated _from_ them. They will be put in `src_validators` vector
    // If a validator's current delegated amount is smaller than the target amount, native will be
    // redelegated _to_ them. They will be put in `dst_validators` vector
    let mut src_delegations: Vec<Delegation> = vec![];
    let mut dst_delegations: Vec<Delegation> = vec![];
    for (i, d) in current_delegations.iter().enumerate() {
        let remainder_for_validator: u128 = u128::from((i + 1) as u128 <= remainder);
        let native_for_validator = native_per_validator + remainder_for_validator;
        // eprintln!("{} amount ={} native={} min={}", d.validator, d.amount, native_for_validator,
        // min_difference);
        match d.amount.cmp(&native_for_validator) {
            Ordering::Greater => {
                if d.amount - native_for_validator > min_difference.u128() {
                    src_delegations.push(Delegation::new(
                        &d.validator,
                        d.amount - native_for_validator,
                        &d.denom,
                    ));
                }
            },
            Ordering::Less => {
                if validators_active.contains(&d.validator)
                    && native_for_validator - d.amount > min_difference.u128()
                {
                    dst_delegations.push(Delegation::new(
                        &d.validator,
                        native_for_validator - d.amount,
                        &d.denom,
                    ));
                }
            },
            Ordering::Equal => (),
        }
    }

    let mut new_redelegations: Vec<Redelegation> = vec![];
    while !src_delegations.is_empty() && !dst_delegations.is_empty() {
        let src_delegation = src_delegations[0].clone();
        let dst_delegation = dst_delegations[0].clone();
        let native_to_redelegate = cmp::min(src_delegation.amount, dst_delegation.amount);

        if src_delegation.amount == native_to_redelegate {
            src_delegations.remove(0);
        } else {
            src_delegations[0].amount -= native_to_redelegate;
        }

        if dst_delegation.amount == native_to_redelegate {
            dst_delegations.remove(0);
        } else {
            dst_delegations[0].amount -= native_to_redelegate;
        }
        new_redelegations.push(Redelegation::new(
            &src_delegation.validator,
            &dst_delegation.validator,
            native_to_redelegate,
            &src_delegation.denom,
        ));
    }
    // eprintln!("new redelegations ={:?}", new_redelegations);

    new_redelegations
}

//--------------------------------------------------------------------------------------------------
// Batch logics
//--------------------------------------------------------------------------------------------------

/// `floor(deduct * cumulative / total)`: how much of `deduct` the batches holding the first
/// `cumulative` of `total` bear together. Charging each batch the step between consecutive
/// cumulative values shares `deduct` pro rata, keeps every batch within one unit of its exact
/// share, and adds up to exactly `deduct` once `cumulative` reaches `total`.
pub(crate) fn cumulative_share(deduct: Uint128, cumulative: Uint128, total: Uint128) -> Uint128 {
    if total.is_zero() {
        return Uint128::zero();
    }
    let share = deduct.full_mul(cumulative) / Uint256::from(total);
    Uint128::try_from(share).unwrap_or(Uint128::MAX)
}

/// Write `native_to_deduct` (capped at what the batches hold) down across `batches` in proportion
/// to what each holds, and mark them reconciled. The batches share one balance, so a missing
/// amount cannot be attributed to any one of them. Exactly the capped amount is applied.
pub(crate) fn reconcile_batches(batches: &mut [Batch], native_to_deduct: Uint128) {
    let total: Uint128 = batches.iter().map(|b| b.amount_unclaimed).sum();
    let deduct = native_to_deduct.min(total);
    let mut cumulative = Uint128::zero();
    let mut taken = Uint128::zero();
    for batch in batches.iter_mut() {
        cumulative += batch.amount_unclaimed;
        let target = cumulative_share(deduct, cumulative, total);
        batch.amount_unclaimed -= target - taken;
        taken = target;
        batch.reconciled = true;
    }
}
