use std::str::FromStr;

use cosmwasm_std::{
    Addr, BankMsg, Coin, CosmosMsg, Decimal, DepsMut, DistributionMsg, Env, Event, Order, Reply,
    ReplyOn, Response, StdError, StdResult, SubMsg, SubMsgResult, Uint128, WasmMsg, to_json_binary,
};
use pfc_steak::{
    DecimalCheckedOps,
    hub::{Batch, CallbackMsg, FeeType, IdleFundsResponse, PendingBatch, UnbondRequest},
    hub_tf::{ExecuteMsg, InstantiateMsg, TokenFactoryType},
};

//use crate::token_factory::denom::{MsgBurn, MsgCreateDenom, MsgMint};
use crate::types::{Coins, Delegation, Redelegation};
use crate::{
    contract::{
        RECONCILE_GRACE_SECONDS, REDELEGATE_INACTIVE_DEFAULT_LIMIT, REDELEGATE_INACTIVE_MAX_LIMIT,
        REPLY_REDELEGATE_INACTIVE_BASE, REPLY_REGISTER_RECEIVED_COINS, REPLY_SUBMIT_BATCH,
        SPECIAL_SEND_MESSAGE_TO_TRANSFER,
    },
    helpers::{
        get_denom_balance, parse_received_fund, query_all_delegations, query_delegation,
        query_delegations,
    },
    injective, kujira,
    math::{
        compute_mint_amount, compute_redelegations_for_inactive,
        compute_redelegations_for_rebalancing, compute_redelegations_for_removal,
        compute_unbond_amount, compute_undelegations, reconcile_batches,
    },
    osmosis,
    state::{State, VALIDATORS, VALIDATORS_ACTIVE, previous_batches, unbond_requests},
    token_factory,
};

//--------------------------------------------------------------------------------------------------
// Instantiation
//--------------------------------------------------------------------------------------------------

pub fn instantiate(deps: DepsMut, env: Env, msg: InstantiateMsg) -> StdResult<Response> {
    let state = State::default();

    if msg.max_fee_amount > Decimal::from_str("1.00")? {
        return Err(StdError::generic_err("Max fee can not exceed 1/100%"));
    }

    if msg.fee_amount > msg.max_fee_amount {
        return Err(StdError::generic_err("fee can not exceed max fee"));
    }
    let fee_type = FeeType::from_str(&msg.fee_account_type)
        .map_err(|_| StdError::generic_err("Invalid Fee type: Wallet or FeeSplit only"))?;

    state.owner.save(deps.storage, &deps.api.addr_validate(&msg.owner)?)?;
    state.epoch_period.save(deps.storage, &msg.epoch_period)?;
    state.unbond_period.save(deps.storage, &msg.unbond_period)?;
    state.unlocked_coins.save(deps.storage, &vec![])?;
    state.prev_denom.save(deps.storage, &Uint128::zero())?;
    state.denom.save(deps.storage, &msg.denom)?;
    state.max_fee_rate.save(deps.storage, &msg.max_fee_amount)?;
    state.fee_rate.save(deps.storage, &msg.fee_amount)?;
    state.fee_account_type.save(deps.storage, &fee_type)?;

    state.fee_account.save(deps.storage, &deps.api.addr_validate(&msg.fee_account)?)?;

    state.pending_batch.save(deps.storage, &PendingBatch {
        id: 1,
        usteak_to_burn: Uint128::zero(),
        est_unbond_start_time: env.block.time.seconds() + msg.epoch_period,
    })?;

    for v in msg.validators {
        VALIDATORS.insert(deps.storage, &v)?;
        VALIDATORS_ACTIVE.insert(deps.storage, &v)?;
    }
    let token_factory_type = TokenFactoryType::from_str(&msg.token_factory).map_err(|_| {
        StdError::generic_err("Invalid Token Factory type: CosmWasm, Kujira, or Injective only")
    })?;

    state.token_factory_type.save(deps.storage, &token_factory_type)?;

    let steak_denom = format!("factory/{0}/{1}", env.contract.address, msg.steak_denom);
    let steak_denom_msg = msg.steak_denom;
    state.steak_denom.save(deps.storage, &steak_denom)?;
    state.steak_minted.save(deps.storage, &Uint128::zero())?;

    if let Some(dust) = msg.dust_collector {
        state.dust_collector.save(deps.storage, &Some(deps.api.addr_validate(&dust)?))?
    } else {
        state.dust_collector.save(deps.storage, &None)?
    }
    let c = match state.token_factory_type.load(deps.storage)? {
        TokenFactoryType::CosmWasm => {
            <token_factory::denom::MsgCreateDenom as Into<CosmosMsg>>::into(
                token_factory::denom::MsgCreateDenom {
                    sender: env.contract.address.to_string(),
                    subdenom: steak_denom_msg,
                },
            )
        },
        TokenFactoryType::Kujira => <kujira::denom::MsgCreateDenom as Into<CosmosMsg>>::into(
            kujira::denom::MsgCreateDenom {
                sender: env.contract.address.to_string(),
                subdenom: steak_denom_msg,
            },
        ),
        TokenFactoryType::Injective => <injective::denom::MsgCreateDenom as Into<CosmosMsg>>::into(
            injective::denom::MsgCreateDenom {
                sender: env.contract.address.to_string(),
                subdenom: steak_denom_msg,
            },
        ),
        TokenFactoryType::Osmosis => <osmosis::denom::MsgCreateDenom as Into<CosmosMsg>>::into(
            osmosis::denom::MsgCreateDenom {
                sender: env.contract.address.to_string(),
                subdenom: steak_denom_msg,
            },
        ),
    };

    Ok(Response::new().add_message(c))
}

fn active_validator_list(storage: &dyn cosmwasm_std::Storage) -> StdResult<Vec<String>> {
    VALIDATORS_ACTIVE.items(storage, None, None, Order::Ascending).collect()
}

//--------------------------------------------------------------------------------------------------
// Bonding and harvesting logics
//--------------------------------------------------------------------------------------------------

/// bond tokens (XXX) to validators, returning bXXXX
///
/// To save gas for users, now we simply delegate all deposited Luna to the validator with the
/// smallest amount of delegation. If delegations become severely unbalance as a result of this
/// (e.g. when a single user makes a very big deposit), anyone can invoke `ExecuteMsg::Rebalance`
/// to balance the delegations.
pub fn bond(
    deps: DepsMut,
    env: Env,
    receiver: Addr,
    funds: Vec<Coin>,
    bond_msg: Option<String>,
    mint_steak: bool,
) -> StdResult<Response> {
    let state = State::default();
    let denom = state.denom.load(deps.storage)?;
    let amount_to_bond = parse_received_fund(&funds, &denom)?;
    let steak_minted = state.steak_minted.load(deps.storage)?;
    let steak_denom = state.steak_denom.load(deps.storage)?;
    let token_factory_type = state.token_factory_type.load(deps.storage)?;
    let mut validators: Vec<String> = Default::default();
    for v in VALIDATORS_ACTIVE.items(deps.storage, None, None, Order::Ascending) {
        validators.push(v?);
    }
    if validators.is_empty() {
        return Err(StdError::generic_err("cannot bond without an active validator"));
    }

    // Query the current delegations made to validators, and find the validator with the smallest
    // delegated amount through a linear search
    // The code for linear search is a bit uglier than using `sort_by` but cheaper: O(n) vs O(n *
    // log(n))
    let delegations_all = query_all_delegations(&deps.querier, &env.contract.address)?;
    let validators = bonded_targets(&deps.querier, validators)?;
    let delegations = query_delegations(&deps.querier, &validators, &env.contract.address, &denom)?;

    if !steak_minted.is_zero() && delegations_all.iter().all(|d| d.amount == 0) {
        return Err(StdError::generic_err(
            "cannot bond while derivative supply exists without delegated backing",
        ));
    }

    let mut validator = &delegations[0].validator;
    let mut amount = delegations[0].amount;
    for d in &delegations[1..] {
        if d.amount < amount {
            validator = &d.validator;
            amount = d.amount;
        }
    }
    let new_delegation = Delegation {
        validator: validator.clone(),
        amount: amount_to_bond.u128(),
        denom: denom.clone(),
    };
    let delegate_submsg = SubMsg {
        msg: new_delegation.to_cosmos_msg(),
        id: REPLY_REGISTER_RECEIVED_COINS,
        gas_limit: None,
        reply_on: ReplyOn::Success,
    };

    if mint_steak {
        // Query the current supply of Steak and compute the amount to mint
        //   let usteak_supply = steak_minted;
        let pending =
            pending_rewards_net(deps.as_ref(), &env, &delegations_all, &denom, amount_to_bond)?;
        let usteak_to_mint =
            compute_mint_amount(steak_minted, amount_to_bond, &delegations_all, pending)?;
        state.steak_minted.save(deps.storage, &(steak_minted + usteak_to_mint))?;
        // TODO deal with multiple token returns

        let mint_msg = match token_factory_type {
            TokenFactoryType::CosmWasm => <token_factory::denom::MsgMint as Into<CosmosMsg>>::into(
                token_factory::denom::MsgMint {
                    sender: env.contract.address.to_string(),
                    amount: Some(token_factory::denom::Coin {
                        denom: steak_denom.clone(),
                        amount: usteak_to_mint.to_string(),
                    }),
                },
            ),
            TokenFactoryType::Kujira => {
                <kujira::denom::MsgMint as Into<CosmosMsg>>::into(kujira::denom::MsgMint {
                    sender: env.contract.address.to_string(),
                    recipient: env.contract.address.to_string(),
                    amount: Some(kujira::denom::Coin {
                        denom: steak_denom.clone(),
                        amount: usteak_to_mint.to_string(),
                    }),
                })
            },
            TokenFactoryType::Injective => {
                <injective::denom::MsgMint as Into<CosmosMsg>>::into(injective::denom::MsgMint {
                    sender: env.contract.address.to_string(),
                    amount: Some(injective::denom::Coin {
                        denom: steak_denom.clone(),
                        amount: usteak_to_mint.to_string(),
                    }),
                })
            },
            TokenFactoryType::Osmosis => {
                <osmosis::denom::MsgMint as Into<CosmosMsg>>::into(osmosis::denom::MsgMint {
                    sender: env.contract.address.to_string(),
                    amount: Some(osmosis::denom::Coin {
                        denom: steak_denom.clone(),
                        amount: usteak_to_mint.to_string(),
                    }),
                    mint_to_address: env.contract.address.to_string(),
                })
            },
        };

        let contract_info = deps.querier.query_wasm_contract_info(receiver.to_string());

        // send the uSteak, optionally calling a smart contract
        let send_transfer_msg: CosmosMsg = match contract_info {
            Ok(_) => {
                if let Some(exec_msg) = bond_msg {
                    if exec_msg == SPECIAL_SEND_MESSAGE_TO_TRANSFER {
                        // this is for backwards compatibility only
                        CosmosMsg::Bank(BankMsg::Send {
                            to_address: receiver.to_string(),
                            amount: vec![Coin {
                                denom: steak_denom,
                                amount: usteak_to_mint,
                            }],
                        })
                    } else {
                        CosmosMsg::Wasm(WasmMsg::Execute {
                            contract_addr: receiver.to_string(),
                            msg: to_json_binary(&exec_msg)?,
                            funds: vec![Coin {
                                denom: steak_denom,
                                amount: usteak_to_mint,
                            }],
                        })
                    }
                } else {
                    CosmosMsg::Bank(BankMsg::Send {
                        to_address: receiver.to_string(),
                        amount: vec![Coin {
                            denom: steak_denom,
                            amount: usteak_to_mint,
                        }],
                    })
                }
            },
            Err(_) => CosmosMsg::Bank(BankMsg::Send {
                to_address: receiver.to_string(),
                amount: vec![Coin {
                    denom: steak_denom,
                    amount: usteak_to_mint,
                }],
            }),
        };

        let event = Event::new("steakhub/bonded")
            .add_attribute("time", env.block.time.seconds().to_string())
            .add_attribute("height", env.block.height.to_string())
            .add_attribute("steak_receiver", receiver)
            .add_attribute("denom_bonded", denom)
            .add_attribute("denom_amount", amount_to_bond)
            .add_attribute("usteak_minted", usteak_to_mint);

        Ok(Response::new()
            .add_submessage(delegate_submsg)
            .add_messages(vec![mint_msg, send_transfer_msg])
            //   .add_message(send_msg)
            .add_event(event)
            .add_attribute("action", "steakhub/bond"))
    } else {
        let event = Event::new("steakhub/bonded-nomint")
            .add_attribute("time", env.block.time.seconds().to_string())
            .add_attribute("height", env.block.height.to_string())
            .add_attribute("from", receiver)
            .add_attribute("denom_bonded", denom)
            .add_attribute("denom_amount", amount_to_bond);

        Ok(Response::new()
            .add_submessage(delegate_submsg)
            .add_event(event)
            .add_attribute("action", "steakhub/bond-nomint"))
    }
}

pub fn harvest(deps: DepsMut, env: Env) -> StdResult<Response> {
    let withdraw_submsgs = deps
        .querier
        .query_all_delegations(&env.contract.address)?
        .into_iter()
        .map(|d| SubMsg {
            id: REPLY_REGISTER_RECEIVED_COINS,
            msg: CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                validator: d.validator,
            }),
            gas_limit: None,
            reply_on: ReplyOn::Success,
        })
        .collect::<Vec<_>>();

    let callback_msg = CallbackMsg::Reinvest {}.into_cosmos_msg(&env.contract.address)?;

    Ok(Response::new()
        .add_submessages(withdraw_submsgs)
        .add_message(callback_msg)
        .add_attribute("action", "steakhub/harvest"))
}

/// Complete reward processing after inactive-validator redelegations: withdraw rewards from every
/// remaining delegation (registered in `unlocked_coins` like the redelegations' own automatic
/// withdrawals), then reinvest everything registered. Withdrawing first also keeps the final
/// delegation from surfacing an active validator's older rewards after reinvestment.
pub fn harvest_after_redelegation(deps: DepsMut, env: Env) -> StdResult<Response> {
    let withdraw_submsgs = deps
        .querier
        .query_all_delegations(&env.contract.address)?
        .into_iter()
        .map(|d| SubMsg {
            id: REPLY_REGISTER_RECEIVED_COINS,
            msg: CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                validator: d.validator,
            }),
            gas_limit: None,
            reply_on: ReplyOn::Success,
        })
        .collect::<Vec<_>>();

    let callback_msg = CallbackMsg::ReinvestIfNeeded {}.into_cosmos_msg(&env.contract.address)?;

    Ok(Response::new()
        .add_submessages(withdraw_submsgs)
        .add_message(callback_msg)
        .add_attribute("action", "steakhub/harvest_after_redelegation"))
}

pub fn reinvest_if_needed(deps: DepsMut, env: Env) -> StdResult<Response> {
    let state = State::default();
    let denom = state.denom.load(deps.storage)?;
    let unlocked_native = Coins(state.unlocked_coins.load(deps.storage)?).find(&denom).amount;
    if unlocked_native.is_zero() {
        return Ok(Response::new().add_attribute("action", "steakhub/reinvest_not_needed"));
    }

    reinvest(deps, env)
}

/// NOTE:
/// 1. When delegation Native denom here, we don't need to use a `SubMsg` to handle the received
///    coins, because we have already withdrawn all claimable staking rewards previously in the same
///    atomic execution.
/// 2. Same as with `bond`, in the latest implementation we only delegate staking rewards with the
///    validator that has the smallest delegation amount.
pub fn reinvest(deps: DepsMut, env: Env) -> StdResult<Response> {
    let state = State::default();
    let denom = state.denom.load(deps.storage)?;
    let fee = state.fee_rate.load(deps.storage)?;

    let mut validators_active: Vec<String> = Default::default();
    for v in VALIDATORS_ACTIVE.items(deps.storage, None, None, Order::Ascending) {
        validators_active.push(v?);
    }
    let bonded: Vec<String> =
        deps.querier.query_all_validators()?.into_iter().map(|v| v.address).collect();
    let validators: Vec<String> =
        validators_active.into_iter().filter(|v| bonded.contains(v)).collect();
    if validators.is_empty() {
        return Err(StdError::generic_err("cannot reinvest without a bonded active validator"));
    }
    // Every staking message registers the rewards it withdraws (harvest's withdrawals, and the
    // automatic withdrawals of delegate / undelegate / redelegate) in `unlocked_coins`; bond all of
    // them. Never bond funds that matured unbonding batches are owed.
    let mut unlocked_coins = state.unlocked_coins.load(deps.storage)?;
    let unlocked_native = Coins(unlocked_coins.clone()).find(&denom).amount;
    let balance = get_denom_balance(&deps.querier, env.contract.address.clone(), denom.clone())?;
    let available = balance
        .saturating_sub(native_owed_to_batches(deps.storage, Some(env.block.time.seconds()))?);
    let amount_to_bond = unlocked_native.min(available);
    if amount_to_bond.is_zero() {
        return Err(StdError::generic_err("no rewards"));
    }
    let delegations = query_delegations(&deps.querier, &validators, &env.contract.address, &denom)?;
    let mut validator = &delegations[0].validator;
    let mut amount = delegations[0].amount;
    for d in &delegations[1..] {
        if d.amount < amount {
            validator = &d.validator;
            amount = d.amount;
        }
    }
    let fee_amount = if fee.is_zero() {
        Uint128::zero()
    } else {
        fee.checked_mul_uint(amount_to_bond)?
    };
    let amount_to_bond_minus_fees = amount_to_bond.saturating_sub(fee_amount);

    let new_delegation = Delegation::new(validator, amount_to_bond_minus_fees.u128(), &denom);

    unlocked_coins.retain(|coin| coin.denom != denom);
    let native_left = unlocked_native - amount_to_bond;
    if !native_left.is_zero() {
        unlocked_coins.push(Coin::new(native_left.u128(), &denom));
    }
    state.unlocked_coins.save(deps.storage, &unlocked_coins)?;

    let event = Event::new("steakhub/harvested")
        .add_attribute("time", env.block.time.seconds().to_string())
        .add_attribute("height", env.block.height.to_string())
        .add_attribute("denom", &denom)
        .add_attribute("fees_deducted", fee_amount)
        .add_attribute("denom_bonded", amount_to_bond_minus_fees);

    if fee_amount > Uint128::zero() {
        let fee_account = state.fee_account.load(deps.storage)?;
        let fee_type = state.fee_account_type.load(deps.storage)?;

        let send_msgs = match fee_type {
            FeeType::Wallet => vec![CosmosMsg::Bank(BankMsg::Send {
                to_address: fee_account.to_string(),
                amount: vec![Coin::new(fee_amount.into(), &denom)],
            })],
            FeeType::FeeSplit => {
                let msg = pfc_fee_split::fee_split_msg::ExecuteMsg::Deposit {
                    flush: false,
                };

                vec![msg.into_cosmos_msg(fee_account, vec![Coin::new(fee_amount.into(), &denom)])?]
            },
        };
        Ok(Response::new()
            .add_message(new_delegation.to_cosmos_msg())
            .add_messages(send_msgs)
            .add_event(event)
            .add_attribute("action", "steakhub/reinvest"))
    } else {
        Ok(Response::new()
            .add_message(new_delegation.to_cosmos_msg())
            .add_event(event)
            .add_attribute("action", "steakhub/reinvest"))
    }
}

/// The validators new stake should go to: the active set, narrowed to those the chain currently
/// has bonded (`StakingQuery::AllValidators` returns only the bonded set). Stake on a jailed or
/// unbonded validator earns nothing, and the smallest delegation, which bonds and reinvests
/// target, is often exactly such a slashed validator. Falls back to the whole active set if none
/// of it is bonded, so bonding keeps working.
pub(crate) fn bonded_targets(
    querier: &cosmwasm_std::QuerierWrapper,
    active: Vec<String>,
) -> StdResult<Vec<String>> {
    let bonded: Vec<String> =
        querier.query_all_validators()?.into_iter().map(|v| v.address).collect();
    let targets: Vec<String> = active.iter().filter(|v| bonded.contains(v)).cloned().collect();
    Ok(if targets.is_empty() {
        active
    } else {
        targets
    })
}

/// Dispatch redelegations as isolated submessages (reply ids from `REPLY_REDELEGATE_INACTIVE_BASE`)
/// so one the staking module refuses is skipped and reported instead of reverting the rest.
fn isolated_redelegations(redelegations: &[Redelegation]) -> (Vec<SubMsg>, Vec<Event>) {
    redelegations
        .iter()
        .enumerate()
        .map(|(index, rd)| {
            (
                SubMsg {
                    id: REPLY_REDELEGATE_INACTIVE_BASE + index as u64,
                    msg: rd.to_cosmos_msg(),
                    gas_limit: None,
                    reply_on: REDELEGATE_INACTIVE_REPLY_ON,
                },
                Event::new("steakhub/redelegation_planned")
                    .add_attribute("index", index.to_string())
                    .add_attribute("validator_from", &rd.src)
                    .add_attribute("validator_to", &rd.dst)
                    .add_attribute("amount", rd.amount.to_string()),
            )
        })
        .unzip()
}

/// Holders' share (after the fee) of rewards earned but not yet restaked: recorded rewards waiting
/// for the next reinvest (capped by what the balance can actually back, excluding the deposit being
/// bonded and funds owed to matured batches) plus rewards still accruing on each delegation.
pub(crate) fn pending_rewards_net(
    deps: cosmwasm_std::Deps,
    env: &Env,
    delegations: &[Delegation],
    denom: &str,
    deposit: Uint128,
) -> StdResult<Uint128> {
    let state = State::default();
    let fee = state.fee_rate.load(deps.storage)?;
    let unlocked = Coins(state.unlocked_coins.load(deps.storage)?).find(denom).amount;
    let available = get_denom_balance(&deps.querier, env.contract.address.clone(), denom.into())?
        .saturating_sub(deposit)
        .saturating_sub(native_owed_to_batches(deps.storage, Some(env.block.time.seconds()))?);
    let mut gross = unlocked.min(available);
    for d in delegations {
        if let Some(full) = deps.querier.query_delegation(&env.contract.address, &d.validator)? {
            gross += full
                .accumulated_rewards
                .iter()
                .filter(|c| c.denom == denom)
                .map(|c| c.amount)
                .sum::<Uint128>();
        }
    }
    let fee_amount = if fee.is_zero() {
        Uint128::zero()
    } else {
        fee.checked_mul_uint(gross)?
    };
    Ok(gross.saturating_sub(fee_amount))
}

/// Native the unbonding batches are owed out of the contract's balance. With `Some(now)`, only
/// batches whose funds should already have arrived (reconciled, or past their estimated end);
/// with `None`, every batch still holding claims, which is the conservative figure used before
/// treating any balance as idle.
pub(crate) fn native_owed_to_batches(
    storage: &dyn cosmwasm_std::Storage,
    now: Option<u64>,
) -> StdResult<Uint128> {
    previous_batches()
        .range(storage, None, None, Order::Ascending)
        .map(|item| item.map(|(_, b)| b))
        .filter(|b| match (b, now) {
            (Ok(b), Some(now)) => b.reconciled || b.est_unbond_end_time <= now,
            _ => true,
        })
        .map(|b| b.map(|b| b.amount_unclaimed))
        .sum()
}

/// Native sitting in the contract that nothing accounts for: not delegated, not recorded as
/// rewards waiting to be reinvested, and not owed to any unbonding batch. It comes from rewards
/// withdrawn as a side effect of staking messages before they were recorded, and from unbonding
/// funds that arrived after their batch was written down.
pub(crate) fn idle_native(deps: cosmwasm_std::Deps, env: &Env) -> StdResult<IdleFundsResponse> {
    let state = State::default();
    let denom = state.denom.load(deps.storage)?;
    let balance = get_denom_balance(&deps.querier, env.contract.address.clone(), denom.clone())?;
    let unlocked = Coins(state.unlocked_coins.load(deps.storage)?).find(&denom).amount;
    let owed_to_batches = native_owed_to_batches(deps.storage, None)?;
    Ok(IdleFundsResponse {
        denom,
        balance,
        unlocked,
        owed_to_batches,
        idle: balance.saturating_sub(unlocked).saturating_sub(owed_to_batches),
    })
}

/// Owner only: delegate idle native (see `idle_native`) to the active validator with the smallest
/// delegation, so it earns for current holders. No fee is taken: idle funds are not all rewards.
pub fn reinvest_idle(
    deps: DepsMut,
    env: Env,
    sender: Addr,
    amount: Option<Uint128>,
) -> StdResult<Response> {
    let state = State::default();
    state.assert_owner(deps.storage, &sender)?;

    let idle = idle_native(deps.as_ref(), &env)?;
    let amount = amount.unwrap_or(idle.idle);
    if amount.is_zero() {
        return Err(StdError::generic_err("no idle funds to reinvest"));
    }
    if amount > idle.idle {
        return Err(StdError::generic_err(format!(
            "amount exceeds idle funds: {} > {}",
            amount, idle.idle
        )));
    }

    let validators = active_validator_list(deps.storage)?;
    if validators.is_empty() {
        return Err(StdError::generic_err("cannot reinvest without an active validator"));
    }
    let validators = bonded_targets(&deps.querier, validators)?;
    let delegations =
        query_delegations(&deps.querier, &validators, &env.contract.address, &idle.denom)?;
    let target = delegations.iter().min_by_key(|d| d.amount).map(|d| d.validator.clone()).unwrap();

    let event = Event::new("steakhub/reinvested_idle")
        .add_attribute("validator", &target)
        .add_attribute("amount", amount)
        .add_attribute("idle_before", idle.idle);

    Ok(Response::new()
        .add_submessage(SubMsg::reply_on_success(
            Delegation::new(&target, amount.u128(), &idle.denom).to_cosmos_msg(),
            REPLY_REGISTER_RECEIVED_COINS,
        ))
        .add_event(event)
        .add_attribute("action", "steakhub/reinvest_idle"))
}

/// NOTE: a `SubMsgResponse` may contain multiple coin-receiving events, must handle them
/// individually
pub fn register_received_coins(
    deps: DepsMut,
    env: Env,
    mut events: Vec<Event>,
) -> StdResult<Response> {
    events.retain(|event| event.ty == "coin_received");
    if events.is_empty() {
        return Ok(Response::new());
    }

    let mut received_coins = Coins(vec![]);
    for event in &events {
        received_coins.add_many(&parse_coin_receiving_event(&env, event)?)?;
    }

    let state = State::default();
    state.unlocked_coins.update(deps.storage, |coins| -> StdResult<_> {
        let mut coins = Coins(coins);
        coins.add_many(&received_coins)?;
        Ok(coins.0)
    })?;

    Ok(Response::new().add_attribute("action", "steakhub/register_received_coins"))
}

fn parse_coin_receiving_event(env: &Env, event: &Event) -> StdResult<Coins> {
    let receiver = &event
        .attributes
        .iter()
        .find(|attr| attr.key == "receiver")
        .ok_or_else(|| StdError::generic_err("cannot find `receiver` attribute"))?
        .value;

    let amount_str = &event
        .attributes
        .iter()
        .find(|attr| attr.key == "amount")
        .ok_or_else(|| StdError::generic_err("cannot find `amount` attribute"))?
        .value;

    let amount = if *receiver == env.contract.address {
        Coins::from_str(amount_str)?
    } else {
        Coins(vec![])
    };

    Ok(amount)
}

//--------------------------------------------------------------------------------------------------
// Unbonding logics
//--------------------------------------------------------------------------------------------------

pub fn queue_unbond(
    deps: DepsMut,
    env: Env,
    receiver: Addr,
    funds: Vec<Coin>,
) -> StdResult<Response> {
    let state = State::default();
    let steak_denom = state.steak_denom.load(deps.storage)?;

    let usteak_to_burn = funds
        .iter()
        .filter(|p| p.denom == steak_denom)
        .map(|steak_funds| steak_funds.amount)
        .sum::<Uint128>();
    if funds.len() != 1 || usteak_to_burn.is_zero() {
        return Err(StdError::generic_err(format!(
            "you can only send {} tokens to unbond",
            steak_denom
        )));
    }
    let mut pending_batch = state.pending_batch.load(deps.storage)?;
    pending_batch.usteak_to_burn += usteak_to_burn;
    state.pending_batch.save(deps.storage, &pending_batch)?;

    unbond_requests().update(
        deps.storage,
        (pending_batch.id, receiver.as_ref()),
        |x| -> StdResult<_> {
            let mut request = x.unwrap_or_else(|| UnbondRequest {
                id: pending_batch.id,
                user: receiver.clone(),
                shares: Uint128::zero(),
            });
            request.shares += usteak_to_burn;
            Ok(request)
        },
    )?;

    // Submitting the batch is opportunistic: if it fails (e.g. a supply/backing invariant trips),
    // the reply handler swallows the error so the user's request still lands in the queue and the
    // batch can be submitted later, instead of reverting every unbond.
    let mut msgs: Vec<SubMsg> = vec![];
    if env.block.time.seconds() >= pending_batch.est_unbond_start_time {
        msgs.push(SubMsg::reply_on_error(
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: env.contract.address.into(),
                msg: to_json_binary(&ExecuteMsg::SubmitBatch {})?,
                funds: vec![],
            }),
            REPLY_SUBMIT_BATCH,
        ));
    }

    let event = Event::new("steakhub/unbond_queued")
        .add_attribute("time", env.block.time.seconds().to_string())
        .add_attribute("height", env.block.height.to_string())
        .add_attribute("id", pending_batch.id.to_string())
        .add_attribute("receiver", receiver)
        .add_attribute("usteak_to_burn", usteak_to_burn);

    Ok(Response::new()
        .add_submessages(msgs)
        .add_event(event)
        .add_attribute("action", "steakhub/queue_unbond"))
}

/// Reply handler for the opportunistic `SubmitBatch` dispatched by `queue_unbond`. Only invoked on
/// error; the failed submission's state changes are already reverted, so record why and carry on.
pub fn submit_batch_deferred(reply: cosmwasm_std::Reply) -> Response {
    let reason = match reply.result {
        cosmwasm_std::SubMsgResult::Err(err) => err,
        cosmwasm_std::SubMsgResult::Ok(_) => String::new(),
    };
    Response::new()
        .add_event(Event::new("steakhub/submit_batch_deferred").add_attribute("reason", reason))
}

pub fn submit_batch(deps: DepsMut, env: Env) -> StdResult<Response> {
    let state = State::default();
    let denom = state.denom.load(deps.storage)?;
    let steak_denom = state.steak_denom.load(deps.storage)?;
    let token_factory_type = state.token_factory_type.load(deps.storage)?;
    let usteak_supply = state.steak_minted.load(deps.storage)?;
    let unbond_period = state.unbond_period.load(deps.storage)?;
    let pending_batch = state.pending_batch.load(deps.storage)?;

    let current_time = env.block.time.seconds();
    if current_time < pending_batch.est_unbond_start_time {
        return Err(StdError::generic_err(format!(
            "batch can only be submitted for unbonding after {}",
            pending_batch.est_unbond_start_time
        )));
    }
    if pending_batch.usteak_to_burn.is_zero() {
        return Err(StdError::generic_err("cannot submit an empty unbond batch"));
    }
    if usteak_supply.is_zero() || pending_batch.usteak_to_burn > usteak_supply {
        return Err(StdError::generic_err("unbond batch exceeds the derivative token supply"));
    }

    let delegations = query_all_delegations(&deps.querier, &env.contract.address)?;
    if delegations.is_empty() || delegations.iter().all(|d| d.amount == 0) {
        return Err(StdError::generic_err("cannot unbond without delegated backing"));
    }

    let amount_to_unbond =
        compute_unbond_amount(usteak_supply, pending_batch.usteak_to_burn, &delegations)?;

    let new_undelegations = compute_undelegations(amount_to_unbond, &delegations, &denom);

    // NOTE: Regarding the `amount_unclaimed` value
    //
    // If validators misbehave and get slashed during the unbonding period, the contract can receive
    // LESS Luna than `amount_to_unbond` when unbonding finishes!
    //
    // In this case, users who invokes `withdraw_unbonded` will have their txs failed as the
    // contract does not have enough Luna balance.
    //
    // I don't have a solution for this... other than to manually fund contract with the slashed
    // amount.
    previous_batches().save(deps.storage, pending_batch.id, &Batch {
        id: pending_batch.id,
        reconciled: false,
        total_shares: pending_batch.usteak_to_burn,
        amount_unclaimed: amount_to_unbond,
        est_unbond_end_time: current_time + unbond_period,
    })?;

    let epoch_period = state.epoch_period.load(deps.storage)?;
    state.pending_batch.save(deps.storage, &PendingBatch {
        id: pending_batch.id + 1,
        usteak_to_burn: Uint128::zero(),
        est_unbond_start_time: current_time + epoch_period,
    })?;

    let undelegate_submsgs = new_undelegations
        .iter()
        .map(|d| SubMsg {
            id: REPLY_REGISTER_RECEIVED_COINS,
            msg: d.to_cosmos_msg(),
            gas_limit: None,
            reply_on: ReplyOn::Success,
        })
        .collect::<Vec<_>>();

    let mut msgs = vec![];
    if !pending_batch.usteak_to_burn.is_zero() {
        let burn_msg = match token_factory_type {
            TokenFactoryType::Kujira => {
                <kujira::denom::MsgBurn as Into<CosmosMsg>>::into(kujira::denom::MsgBurn {
                    sender: env.contract.address.to_string(),
                    amount: Some(kujira::denom::Coin {
                        denom: steak_denom,
                        amount: pending_batch.usteak_to_burn.to_string(),
                    }),
                })
            },
            TokenFactoryType::CosmWasm => <token_factory::denom::MsgBurn as Into<CosmosMsg>>::into(
                token_factory::denom::MsgBurn {
                    sender: env.contract.address.to_string(),
                    amount: Some(token_factory::denom::Coin {
                        denom: steak_denom,
                        amount: pending_batch.usteak_to_burn.to_string(),
                    }),
                },
            ),
            TokenFactoryType::Injective => {
                <injective::denom::MsgBurn as Into<CosmosMsg>>::into(injective::denom::MsgBurn {
                    sender: env.contract.address.to_string(),
                    amount: Some(injective::denom::Coin {
                        denom: steak_denom,
                        amount: pending_batch.usteak_to_burn.to_string(),
                    }),
                })
            },
            TokenFactoryType::Osmosis => {
                <osmosis::denom::MsgBurn as Into<CosmosMsg>>::into(osmosis::denom::MsgBurn {
                    sender: env.contract.address.to_string(),
                    amount: Some(osmosis::denom::Coin {
                        denom: steak_denom,
                        amount: pending_batch.usteak_to_burn.to_string(),
                    }),
                    burn_from_address: env.contract.address.to_string(),
                })
            },
        };
        // yes.. this will fail if supply is less than the amount to burn. this is intentional.
        state.steak_minted.save(deps.storage, &(usteak_supply - pending_batch.usteak_to_burn))?;
        msgs.push(burn_msg);
    }
    let event = Event::new("steakhub/unbond_submitted")
        .add_attribute("time", env.block.time.seconds().to_string())
        .add_attribute("height", env.block.height.to_string())
        .add_attribute("id", pending_batch.id.to_string())
        .add_attribute("native_unbonded", amount_to_unbond)
        .add_attribute("usteak_burned", pending_batch.usteak_to_burn);

    Ok(Response::new()
        .add_submessages(undelegate_submsgs)
        .add_messages(msgs)
        .add_event(event)
        .add_attribute("action", "steakhub/unbond"))
}

pub fn reconcile(deps: DepsMut, env: Env) -> StdResult<Response> {
    let state = State::default();
    let current_time = env.block.time.seconds();

    // Load batches that have not been reconciled
    let all_batches = previous_batches()
        .idx
        .reconciled
        .prefix("false".to_string())
        .range(deps.storage, None, None, Order::Ascending)
        .map(|item| {
            let (_, v) = item?;
            Ok(v)
        })
        .collect::<StdResult<Vec<_>>>()?;

    let mut batches = all_batches
        .into_iter()
        .filter(|b| current_time > b.est_unbond_end_time.saturating_add(RECONCILE_GRACE_SECONDS))
        .collect::<Vec<_>>();
    if batches.is_empty() {
        return Ok(Response::new()
            .add_event(Event::new("steakhub/reconciled").add_attribute("ids", ""))
            .add_attribute("action", "steakhub/reconcile"));
    }

    let native_expected_received: Uint128 = batches.iter().map(|b| b.amount_unclaimed).sum();
    let denom = state.denom.load(deps.storage)?;
    let unlocked_coins = state.unlocked_coins.load(deps.storage)?;

    let native_expected_unlocked = Coins(unlocked_coins).find(&denom).amount;

    let native_expected = native_expected_received + native_expected_unlocked;
    // Funds owed to batches that were already reconciled but not yet fully withdrawn sit in the
    // same balance. They are not available to cover the batches being reconciled now; counting
    // them would hide a shortfall until the last claimers of the older batches fail to withdraw.
    let native_reserved: Uint128 = previous_batches()
        .idx
        .reconciled
        .prefix("true".to_string())
        .range(deps.storage, None, None, Order::Ascending)
        .map(|item| item.map(|(_, b)| b.amount_unclaimed))
        .sum::<StdResult<Uint128>>()?;
    let native_actual = deps
        .querier
        .query_balance(&env.contract.address, &denom)?
        .amount
        .saturating_sub(native_reserved);

    // A batch cannot lose more than it holds; capping also keeps `reconcile_batches` from having
    // to spread a deficit larger than every batch combined.
    let native_to_deduct = native_expected
        .checked_sub(native_actual)
        .unwrap_or_else(|_| Uint128::zero())
        .min(native_expected_received);
    if !native_to_deduct.is_zero() {
        reconcile_batches(&mut batches, native_to_deduct);
    }

    for batch in batches.iter_mut() {
        batch.reconciled = true;
        previous_batches().save(deps.storage, batch.id, batch)?;
    }

    let ids = batches.iter().map(|b| b.id.to_string()).collect::<Vec<_>>().join(",");

    let event = Event::new("steakhub/reconciled")
        .add_attribute("ids", ids)
        .add_attribute("native_deducted", native_to_deduct.to_string());

    Ok(Response::new().add_event(event).add_attribute("action", "steakhub/reconcile"))
}

pub fn withdraw_unbonded_admin(
    deps: DepsMut,
    env: Env,
    user: Addr,
    receiver: Addr,
) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &user)?;

    withdraw_unbonded(deps, env, receiver.clone(), receiver)
}

pub fn withdraw_unbonded(
    deps: DepsMut,
    env: Env,
    user: Addr,
    receiver: Addr,
) -> StdResult<Response> {
    let state = State::default();
    let denom = state.denom.load(deps.storage)?;
    let current_time = env.block.time.seconds();

    // NOTE: If the user has too many unclaimed requests, this may not fit in the WASM memory...
    // However, this is practically never going to happen. Who would create hundreds of unbonding
    // requests and never claim them?
    let requests = unbond_requests()
        .idx
        .user
        .prefix(user.to_string())
        .range(deps.storage, None, None, Order::Ascending)
        .map(|item| {
            let (_, v) = item?;
            Ok(v)
        })
        .collect::<StdResult<Vec<_>>>()?;

    // NOTE: Native in the following batches are withdrawn it the batch:
    // - is a _previous_ batch, not a _pending_ batch
    // - is reconciled
    // - has finished unbonding
    // If not sure whether the batches have been reconciled, the user should first invoke
    // `ExecuteMsg::Reconcile` before withdrawing.
    let mut total_native_to_refund = Uint128::zero();
    let mut ids: Vec<String> = vec![];
    for request in &requests {
        if let Ok(mut batch) = previous_batches().load(deps.storage, request.id) {
            if batch.reconciled && batch.est_unbond_end_time < current_time {
                let native_to_refund =
                    batch.amount_unclaimed.multiply_ratio(request.shares, batch.total_shares);

                ids.push(request.id.to_string());

                total_native_to_refund += native_to_refund;
                batch.total_shares -= request.shares;
                batch.amount_unclaimed -= native_to_refund;

                if batch.total_shares.is_zero() {
                    previous_batches().remove(deps.storage, request.id)?;
                } else {
                    previous_batches().save(deps.storage, batch.id, &batch)?;
                }
                unbond_requests().remove(deps.storage, (request.id, user.as_ref()))?;
            }
        }
    }

    if total_native_to_refund.is_zero() {
        return Err(StdError::generic_err("withdrawable amount is zero"));
    }

    let refund_msg = CosmosMsg::Bank(BankMsg::Send {
        to_address: receiver.clone().into(),
        amount: vec![Coin::new(total_native_to_refund.u128(), denom)],
    });

    let event = Event::new("steakhub/unbonded_withdrawn")
        .add_attribute("time", env.block.time.seconds().to_string())
        .add_attribute("height", env.block.height.to_string())
        .add_attribute("ids", ids.join(","))
        .add_attribute("user", user)
        .add_attribute("receiver", receiver)
        .add_attribute("amount_refunded", total_native_to_refund);

    Ok(Response::new()
        .add_message(refund_msg)
        .add_event(event)
        .add_attribute("action", "steakhub/withdraw_unbonded"))
}

//--------------------------------------------------------------------------------------------------
// Ownership and management logics
//--------------------------------------------------------------------------------------------------

/// Owner only. Even out stake across the active validators the chain has bonded, draining
/// validators that are paused, removed, or not bonded. Each move is an isolated submessage (see
/// `RedelegateInactive`), and at most `REDELEGATE_INACTIVE_MAX_LIMIT` moves are sent per call.
///
/// Owner only because every redelegation locks its destination as a redelegation source for an
/// unbonding period; a permissionless caller could otherwise keep the owner from moving stake off
/// a validator.
pub fn rebalance(deps: DepsMut, env: Env, sender: Addr, minimum: Uint128) -> StdResult<Response> {
    let state = State::default();
    state.assert_owner(deps.storage, &sender)?;

    let mut validators_active: Vec<String> = Default::default();
    for v in VALIDATORS_ACTIVE.items(deps.storage, None, None, Order::Ascending) {
        validators_active.push(v?);
    }
    if validators_active.is_empty() {
        return Err(StdError::generic_err("cannot rebalance without an active validator"));
    }
    let targets = bonded_targets(&deps.querier, validators_active)?;

    let denom = state.denom.load(deps.storage)?;
    let delegations = query_all_delegations(&deps.querier, &env.contract.address)?;
    let mut new_redelegations =
        compute_redelegations_for_rebalancing(targets, &delegations, minimum, &denom);
    new_redelegations.truncate(REDELEGATE_INACTIVE_MAX_LIMIT as usize);

    let (submsgs, mut events) = isolated_redelegations(&new_redelegations);
    let amount: u128 = new_redelegations.iter().map(|rd| rd.amount).sum();
    events.push(
        Event::new("steakhub/rebalanced")
            .add_attribute("amount_moved", amount.to_string())
            .add_attribute("redelegations", new_redelegations.len().to_string()),
    );

    Ok(Response::new()
        .add_submessages(submsgs)
        .add_events(events)
        .add_attribute("action", "steakhub/rebalance"))
}

pub fn add_validator(deps: DepsMut, sender: Addr, validator: String) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    if VALIDATORS.contains(deps.storage, &validator) {
        return Err(StdError::generic_err("validator is already whitelisted"));
    }
    VALIDATORS.insert(deps.storage, &validator)?;
    VALIDATORS_ACTIVE.insert(deps.storage, &validator)?;

    let event = Event::new("steakhub/validator_added").add_attribute("validator", validator);

    Ok(Response::new().add_event(event).add_attribute("action", "steakhub/add_validator"))
}

/// See `ExecuteMsg::RedelegateInactive`. Stateless: every call plans from the current on-chain
/// delegations, so it is safe to re-run after skipped redelegations mature.
///
/// Staking-module rules that can refuse an individual redelegation (each is isolated in its own
/// submessage and reported by the reply handler rather than failing the whole call):
/// - transitive redelegation: stake cannot leave a validator while this contract has an unmatured
///   redelegation *into* that validator; this blocks the whole source until it matures. A blocked
///   source is planned again on every run, so pass it in `exclude` until it matures
/// - max entries: at most `MaxEntries` (default 7) unmatured redelegations per (delegator, source,
///   destination) triple
/// - destinations: only validators in the contract's active set that the chain currently has bonded
///   are used, so stake is not moved onto a jailed, unbonded or missing validator
///
/// A redelegation out of a bonded or unbonding (e.g. jailed) source creates an entry that matures
/// when the source's own unbonding completes; until then its destination cannot act as a
/// redelegation source, which affects later `Rebalance` / `RemoveValidator` / `Redelegate` calls.
/// A redelegation out of an unbonded source completes immediately and creates no entry.
///
/// Running out of gas is not isolated (submessages carry no gas limit), so `limit` is capped at
/// `REDELEGATE_INACTIVE_MAX_LIMIT`.
pub fn redelegate_inactive(
    deps: DepsMut,
    env: Env,
    sender: Addr,
    limit: Option<u32>,
    exclude: Vec<String>,
) -> StdResult<Response> {
    let state = State::default();
    state.assert_owner(deps.storage, &sender)?;

    let limit = limit.unwrap_or(REDELEGATE_INACTIVE_DEFAULT_LIMIT);
    if limit == 0 || limit > REDELEGATE_INACTIVE_MAX_LIMIT {
        return Err(StdError::generic_err(format!(
            "limit must be between 1 and {}",
            REDELEGATE_INACTIVE_MAX_LIMIT
        )));
    }

    let denom = state.denom.load(deps.storage)?;
    let mut validators_active: Vec<String> = Default::default();
    for v in VALIDATORS_ACTIVE.items(deps.storage, None, None, Order::Ascending) {
        validators_active.push(v?);
    }
    if validators_active.is_empty() {
        return Err(StdError::generic_err("cannot redelegate without an active validator"));
    }

    // Only move stake onto validators the chain has bonded right now: the staking module accepts
    // redelegations to jailed or unbonded validators, but that stake would earn nothing, and one
    // that no longer exists would refuse every move and stall re-runs.
    let bonded: Vec<String> =
        deps.querier.query_all_validators()?.into_iter().map(|v| v.address).collect();
    let destinations: Vec<String> = validators_active
        .iter()
        .filter(|v| bonded.contains(v) && !exclude.contains(v))
        .cloned()
        .collect();
    if destinations.is_empty() {
        return Err(StdError::generic_err(
            "no active validator is currently bonded on chain to redelegate to",
        ));
    }
    let mut non_sources = validators_active.clone();
    non_sources.extend(exclude.iter().cloned());

    let delegations = query_all_delegations(&deps.querier, &env.contract.address)?;
    let new_redelegations = compute_redelegations_for_inactive(
        &non_sources,
        &destinations,
        &delegations,
        limit as usize,
        &denom,
    );

    let inactive_total: u128 = delegations
        .iter()
        .filter(|d| !validators_active.contains(&d.validator))
        .map(|d| d.amount)
        .sum();
    let excluded_total: u128 = delegations
        .iter()
        .filter(|d| !validators_active.contains(&d.validator) && exclude.contains(&d.validator))
        .map(|d| d.amount)
        .sum();
    let planned_total: u128 = new_redelegations.iter().map(|rd| rd.amount).sum();

    let mut submsgs = Vec::with_capacity(new_redelegations.len());
    let mut events = Vec::with_capacity(new_redelegations.len() + 1);
    for (index, rd) in new_redelegations.iter().enumerate() {
        submsgs.push(SubMsg {
            id: REPLY_REDELEGATE_INACTIVE_BASE + index as u64,
            msg: rd.to_cosmos_msg(),
            gas_limit: None,
            reply_on: REDELEGATE_INACTIVE_REPLY_ON,
        });
        events.push(
            Event::new("steakhub/redelegation_planned")
                .add_attribute("index", index.to_string())
                .add_attribute("validator_from", &rd.src)
                .add_attribute("validator_to", &rd.dst)
                .add_attribute("amount", rd.amount.to_string()),
        );
    }
    events.push(
        Event::new("steakhub/redelegated_inactive")
            .add_attribute("redelegations", new_redelegations.len().to_string())
            .add_attribute("amount_planned", planned_total.to_string())
            .add_attribute("inactive_before", inactive_total.to_string())
            .add_attribute("inactive_excluded", excluded_total.to_string())
            .add_attribute(
                "inactive_unplanned",
                inactive_total.saturating_sub(planned_total).to_string(),
            ),
    );

    let mut response = Response::new()
        .add_submessages(submsgs)
        .add_events(events)
        .add_attribute("action", "steakhub/redelegate_inactive");
    if !new_redelegations.is_empty() {
        // Reward finalization is atomic with the successful redelegations. If withdrawing,
        // reinvesting or paying fees fails, rolling the whole transaction back preserves the
        // pre-redelegation balance baseline instead of committing rewards that no later harvest
        // can distinguish from the contract's existing balance.
        response = response.add_message(
            CallbackMsg::HarvestAfterRedelegation {}.into_cosmos_msg(&env.contract.address)?,
        );
    }

    Ok(response)
}

fn redelegation_skipped(id: u64, reason: String) -> Response {
    Response::new().add_event(
        Event::new("steakhub/redelegation_skipped")
            .add_attribute("index", (id - REPLY_REDELEGATE_INACTIVE_BASE).to_string())
            .add_attribute("reason", reason),
    )
}

/// Reply for a `RedelegateInactive` redelegation. Success registers the staking rewards the
/// redelegation withdrew, as for every other staking message; failure (the staking module refused
/// it, and its state changes are already reverted) is recorded so the operator knows to re-run.
pub fn redelegate_inactive_reply(deps: DepsMut, env: Env, reply: Reply) -> StdResult<Response> {
    match reply.result {
        SubMsgResult::Ok(response) => register_received_coins(deps, env, response.events),
        SubMsgResult::Err(err) => Ok(redelegation_skipped(reply.id, err)),
    }
}

// Success is needed too: the redelegation withdraws pending rewards, which must be registered.
const REDELEGATE_INACTIVE_REPLY_ON: ReplyOn = ReplyOn::Always;

pub fn remove_validator(
    deps: DepsMut,
    env: Env,
    sender: Addr,
    validator: String,
) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    let denom = state.denom.load(deps.storage)?;

    if !VALIDATORS.contains(deps.storage, &validator) {
        return Err(StdError::generic_err("validator is not already whitelisted"));
    }
    if VALIDATORS.count(deps.storage)? <= 1 {
        return Err(StdError::generic_err("cannot remove the last validator with redelegation"));
    }
    VALIDATORS.remove(deps.storage, &validator)?;
    VALIDATORS_ACTIVE.remove(deps.storage, &validator)?;
    // Spread the removed stake over the active validators the chain has bonded, never onto a
    // paused or jailed one.
    let validators = bonded_targets(&deps.querier, active_validator_list(deps.storage)?)?;
    if validators.is_empty() {
        return Err(StdError::generic_err("no active validator to redelegate to"));
    }

    let delegations = query_delegations(&deps.querier, &validators, &env.contract.address, &denom)?;
    let delegation_to_remove =
        query_delegation(&deps.querier, &validator, &env.contract.address, &denom)?;
    let new_redelegations =
        compute_redelegations_for_removal(&delegation_to_remove, &delegations, &denom);

    let redelegate_submsgs = new_redelegations
        .iter()
        .map(|d| SubMsg {
            id: REPLY_REGISTER_RECEIVED_COINS,
            msg: d.to_cosmos_msg(),
            gas_limit: None,
            reply_on: ReplyOn::Success,
        })
        .collect::<Vec<_>>();

    let event = Event::new("steak/validator_removed").add_attribute("validator", validator);

    Ok(Response::new()
        .add_submessages(redelegate_submsgs)
        .add_event(event)
        .add_attribute("action", "steakhub/remove_validator"))
}

pub fn remove_validator_ex(
    deps: DepsMut,
    _env: Env,
    sender: Addr,
    validator: String,
) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;

    if !VALIDATORS.contains(deps.storage, &validator) {
        return Err(StdError::generic_err("validator is not already whitelisted"));
    }
    VALIDATORS.remove(deps.storage, &validator)?;
    VALIDATORS_ACTIVE.remove(deps.storage, &validator)?;

    let event = Event::new("steak/validator_removed_ex").add_attribute("validator", validator);

    Ok(Response::new().add_event(event).add_attribute("action", "steakhub/remove_validator_ex"))
}

pub fn pause_validator(
    deps: DepsMut,
    _env: Env,
    sender: Addr,
    validator: String,
) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;

    if !VALIDATORS_ACTIVE.contains(deps.storage, &validator) {
        return Err(StdError::generic_err("validator is not already whitelisted"));
    }
    VALIDATORS_ACTIVE.remove(deps.storage, &validator)?;

    let event = Event::new("steak/pause_validator").add_attribute("validator", validator);

    Ok(Response::new().add_event(event).add_attribute("action", "steakhub/pause_validator"))
}

pub fn unpause_validator(
    deps: DepsMut,
    _env: Env,
    sender: Addr,
    validator: String,
) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    if !VALIDATORS.contains(deps.storage, &validator) {
        return Err(StdError::generic_err("validator is not in the delegation set"));
    }
    VALIDATORS_ACTIVE.insert(deps.storage, &validator)?;

    let event = Event::new("steak/unpause_validator").add_attribute("validator", validator);

    Ok(Response::new().add_event(event).add_attribute("action", "steakhub/unpause_validator"))
}

pub fn set_unbond_period(
    deps: DepsMut,
    _env: Env,
    sender: Addr,
    unbond_period: u64,
) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    state.unbond_period.save(deps.storage, &unbond_period)?;
    let event = Event::new("steak/set_unbond_period")
        .add_attribute("unbond_period", format!("{}", unbond_period));

    Ok(Response::new().add_event(event).add_attribute("action", "steakhub/set_unbond_period"))
}

pub fn transfer_ownership(deps: DepsMut, sender: Addr, new_owner: String) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    state.new_owner.save(deps.storage, &deps.api.addr_validate(&new_owner)?)?;

    Ok(Response::new().add_attribute("action", "steakhub/transfer_ownership"))
}

pub fn accept_ownership(deps: DepsMut, sender: Addr) -> StdResult<Response> {
    let state = State::default();

    let previous_owner = state.owner.load(deps.storage)?;
    let new_owner = state.new_owner.load(deps.storage)?;

    if sender != new_owner {
        return Err(StdError::generic_err("unauthorized: sender is not new owner"));
    }

    state.owner.save(deps.storage, &sender)?;
    state.new_owner.remove(deps.storage);

    let event = Event::new("steakhub/ownership_transferred")
        .add_attribute("new_owner", new_owner)
        .add_attribute("previous_owner", previous_owner);

    Ok(Response::new().add_event(event).add_attribute("action", "steakhub/transfer_ownership"))
}

pub fn transfer_fee_account(
    deps: DepsMut,
    sender: Addr,
    fee_account_type: String,
    new_fee_account: String,
) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    let fee_type = FeeType::from_str(&fee_account_type)
        .map_err(|_| StdError::generic_err("Invalid Fee type: Wallet or FeeSplit only"))?;

    state.fee_account_type.save(deps.storage, &fee_type)?;

    state.fee_account.save(deps.storage, &deps.api.addr_validate(&new_fee_account)?)?;

    Ok(Response::new().add_attribute("action", "steakhub/transfer_fee_account"))
}

pub fn change_denom(deps: DepsMut, sender: Addr, new_denom: String) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    state.denom.save(deps.storage, &new_denom)?;

    Ok(Response::new().add_attribute("action", "steakhub/change_denom"))
}

pub fn update_fee(deps: DepsMut, sender: Addr, new_fee: Decimal) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    if new_fee > state.max_fee_rate.load(deps.storage)? {
        return Err(StdError::generic_err("refusing to set fee above maximum set"));
    }
    state.fee_rate.save(deps.storage, &new_fee)?;

    Ok(Response::new().add_attribute("action", "steakhub/update_fee"))
}

pub fn set_dust_collector(
    deps: DepsMut,
    _env: Env,
    sender: Addr,
    dust_collector: Option<String>,
) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    if let Some(ref dust_addr) = dust_collector {
        state.dust_collector.save(deps.storage, &Some(deps.api.addr_validate(dust_addr)?))?;
    } else {
        state.dust_collector.save(deps.storage, &None)?;
    };

    let event = Event::new("steak/set_dust_collector")
        .add_attribute("dust_collector", dust_collector.unwrap_or("-cleared-".into()));

    Ok(Response::new().add_event(event).add_attribute("action", "steakhub/set_dust_collector"))
}

pub fn collect_dust(deps: DepsMut, env: Env, max_tokens: usize) -> StdResult<Response> {
    let state = State::default();
    let denom = state.denom.load(deps.storage)?;
    let steak_denom = state.steak_denom.load(deps.storage)?;
    let balances = deps.querier.query_all_balances(env.contract.address)?;
    let balances_filtered = balances
        .into_iter()
        .filter(|p| !(p.denom == denom || p.denom == steak_denom))
        .take(max_tokens)
        .collect::<Vec<Coin>>();
    if balances_filtered.is_empty() {
        return Ok(Response::new().add_attribute("dust", "no-dust"));
    }
    if let Some(dust_addr) = state.dust_collector.load(deps.storage)? {
        let balances_count = balances_filtered.len();
        let msg = CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: dust_addr.to_string(),
            funds: balances_filtered,
            msg: to_json_binary(&pfc_dust_collector::dust_collector::ExecuteMsg::DustReceived {})?,
        });
        Ok(Response::new()
            .add_attribute("dust", format!("sent {} tokens", balances_count))
            .add_message(msg))
    } else {
        Ok(Response::new().add_attribute("dust", "dust-collector-called"))
    }
}

pub fn redelegate(
    deps: DepsMut,
    env: Env,
    sender: Addr,
    validator_from: String,
    validator_to: String,
) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    let denom = state.denom.load(deps.storage)?;

    let delegation =
        query_delegation(&deps.querier, &validator_from, &env.contract.address, &denom)?;

    let redelegation_msg = SubMsg {
        id: REPLY_REGISTER_RECEIVED_COINS,
        msg: Redelegation::new(&validator_from, &validator_to, delegation.amount, &denom)
            .to_cosmos_msg(),
        gas_limit: None,
        reply_on: ReplyOn::Success,
    };

    let event = Event::new("steak/redelegate")
        .add_attribute("validator_from", validator_from)
        .add_attribute("validator_to", validator_to)
        .add_attribute("amount", delegation.amount.to_string());

    Ok(Response::new()
        .add_submessage(redelegation_msg)
        .add_event(event)
        .add_attribute("action", "steakhub/redelegate"))
}
pub fn set_base_denom(deps: DepsMut, sender: Addr, new_denom: String) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    state.denom.save(deps.storage, &new_denom)?;

    let event = Event::new("steak/set_base_denom").add_attribute("base_denom", new_denom);

    Ok(Response::new().add_event(event).add_attribute("action", "steakhub/set_base_denom"))
}

pub fn change_token_factory(
    deps: DepsMut,
    sender: Addr,
    token_factory_type: &str,
) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;

    let tf = TokenFactoryType::from_str(token_factory_type)
        .map_err(|_| StdError::generic_err("Invalid Token Factory type"))?;

    state.token_factory_type.save(deps.storage, &tf)?;
    let event = Event::new("steak/change_token_factory")
        .add_attribute("token_factory_type", token_factory_type);

    Ok(Response::new().add_event(event).add_attribute("action", "steakhub/change_token_factory"))
}
