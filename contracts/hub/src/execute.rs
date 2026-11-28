use std::str::FromStr;

use cosmwasm_std::{
    Addr, BankMsg, Binary, Coin, CosmosMsg, Decimal, DepsMut, DistributionMsg, Env, Event, Order,
    Reply, ReplyOn, Response, StdError, StdResult, SubMsg, SubMsgResponse, SubMsgResult, Uint128,
    WasmMsg, from_json, to_json_binary,
};
use cw_storage_plus::Bound;
use cw20::{Cw20ExecuteMsg, MinterResponse};
use cw20_base::msg::InstantiateMsg as Cw20InstantiateMsg;
use pfc_steak::{
    DecimalCheckedOps,
    hub::{
        Batch, CallbackMsg, Cw20HookMsg, ExecuteMsg, FeeType, IdleFundsResponse, InstantiateMsg,
        PendingBatch, UnbondRequest,
    },
};

use crate::{
    contract::{
        REDELEGATE_INACTIVE_DEFAULT_LIMIT, REDELEGATE_INACTIVE_MAX_LIMIT, REPLY_INSTANTIATE_TOKEN,
        REPLY_REDELEGATE_INACTIVE_BASE, REPLY_REGISTER_RECEIVED_COINS, REPLY_SUBMIT_BATCH,
    },
    helpers::{
        get_denom_balance, parse_received_fund, query_all_delegations, query_cw20_total_supply,
        query_delegation, query_delegations,
    },
    math::{
        compute_mint_amount, compute_redelegations_for_inactive,
        compute_redelegations_for_rebalancing, compute_redelegations_for_removal,
        compute_unbond_amount, compute_undelegations, cumulative_share, reconcile_batches,
    },
    state::{NATIVE_RESERVED, RECONCILE_OBSERVED_HEIGHTS, RESERVE_INIT, ReserveInit, State},
    types::{Coins, Delegation, Redelegation},
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
    state.validators.save(deps.storage, &msg.validators)?;
    state.unlocked_coins.save(deps.storage, &vec![])?;
    state.prev_denom.save(deps.storage, &Uint128::zero())?;
    NATIVE_RESERVED.save(deps.storage, &Uint128::zero())?;
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
    state.validators_active.save(deps.storage, &msg.validators)?;

    Ok(Response::new().add_submessage(SubMsg::reply_on_success(
        CosmosMsg::Wasm(WasmMsg::Instantiate {
            admin: Some(msg.owner), /* use the owner as admin for now; can be changed later by a
                                     * `MsgUpdateAdmin` */
            code_id: msg.cw20_code_id,
            msg: to_json_binary(&Cw20InstantiateMsg {
                name: msg.name,
                symbol: msg.symbol,
                decimals: msg.decimals,
                initial_balances: vec![],
                mint: Some(MinterResponse {
                    minter: env.contract.address.into(),
                    cap: None,
                }),
                marketing: msg.marketing,
            })?,
            funds: vec![],
            label: msg.label.unwrap_or_else(|| "steak_token".to_string()),
        }),
        REPLY_INSTANTIATE_TOKEN,
    )))
}

pub fn register_steak_token(deps: DepsMut, response: SubMsgResponse) -> StdResult<Response> {
    let state = State::default();

    let event = response
        .events
        .iter()
        .find(|event| event.ty == "instantiate")
        .ok_or_else(|| StdError::generic_err("cannot find `instantiate` event"))?;

    let contract_addr_str = &event
        .attributes
        .iter()
        .find(|attr| attr.key == "_contract_address")
        .ok_or_else(|| StdError::generic_err("cannot find `_contract_address` attribute"))?
        .value;

    let contract_addr = deps.api.addr_validate(contract_addr_str)?;
    state.steak_token.save(deps.storage, &contract_addr)?;

    Ok(Response::new())
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
    bond_msg: Option<Binary>,
    mint_it: bool,
) -> StdResult<Response> {
    let state = State::default();
    let denom = state.denom.load(deps.storage)?;
    let amount_to_bond = parse_received_fund(&funds, &denom)?;
    let steak_token = state.steak_token.load(deps.storage)?;
    let validators = state.validators_active.load(deps.storage)?;
    if validators.is_empty() {
        return Err(StdError::generic_err("cannot bond without an active validator"));
    }

    // Query the current delegations made to validators, and find the validator with the smallest
    // delegated amount through a linear search
    // The code for linear search is a bit uglier than using `sort_by` but cheaper: O(n) vs O(n *
    // log(n))
    let all_delegations = query_all_delegations(&deps.querier, &env.contract.address)?;
    let delegations = query_delegations(&deps.querier, &validators, &env.contract.address, &denom)?;

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

    // Query the current supply of Steak and compute the amount to mint
    let usteak_supply = query_cw20_total_supply(&deps.querier, &steak_token)?;
    if !usteak_supply.is_zero() && all_delegations.iter().all(|d| d.amount == 0) {
        return Err(StdError::generic_err(
            "cannot bond while derivative supply exists without delegated backing",
        ));
    }
    let pending =
        pending_rewards_net(deps.as_ref(), &env, &all_delegations, &denom, amount_to_bond)?;
    let usteak_to_mint =
        compute_mint_amount(usteak_supply, amount_to_bond, &all_delegations, pending)?;

    let delegate_submsg =
        SubMsg::reply_on_success(new_delegation.to_cosmos_msg(), REPLY_REGISTER_RECEIVED_COINS);

    let mint_msgs: Vec<CosmosMsg> = if mint_it {
        vec![CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: steak_token.to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Mint {
                recipient: receiver.to_string(),
                amount: usteak_to_mint,
            })?,
            funds: vec![],
        })]
    } else {
        let contract_info = deps.querier.query_wasm_contract_info(receiver.to_string());

        let send_transfer_msg: CosmosMsg = match contract_info {
            Ok(_) => {
                if let Some(exec_msg) = bond_msg {
                    match from_json(exec_msg)? {
                        Cw20HookMsg::Transfer {} => CosmosMsg::Wasm(WasmMsg::Execute {
                            contract_addr: steak_token.to_string(),
                            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                                recipient: receiver.to_string(),
                                amount: usteak_to_mint,
                            })?,
                            funds: vec![],
                        }),
                        Cw20HookMsg::Distribute {} => CosmosMsg::Wasm(WasmMsg::Execute {
                            contract_addr: steak_token.to_string(),
                            msg: to_json_binary(&Cw20ExecuteMsg::Send {
                                contract: receiver.to_string(),
                                amount: usteak_to_mint,
                                msg: to_json_binary(
                                    &funds_distributor_api::msg::Cw20HookMsg::Distribute {},
                                )?,
                            })?,
                            funds: vec![],
                        }),
                    }
                } else {
                    CosmosMsg::Wasm(WasmMsg::Execute {
                        contract_addr: steak_token.to_string(),
                        msg: to_json_binary(&Cw20ExecuteMsg::Send {
                            contract: receiver.to_string(),
                            amount: usteak_to_mint,
                            msg: Default::default(),
                        })?,
                        funds: vec![],
                    })
                }
            },
            Err(_) => {
                CosmosMsg::Wasm(WasmMsg::Execute {
                    contract_addr: steak_token.to_string(),
                    msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                        recipient: receiver.to_string(),
                        amount: usteak_to_mint,
                        //  msg: Default::default(),
                    })?,
                    funds: vec![],
                })
            },
        };
        vec![
            CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: steak_token.to_string(),
                msg: to_json_binary(&Cw20ExecuteMsg::Mint {
                    recipient: env.contract.address.to_string(), //receiver.to_string(),
                    amount: usteak_to_mint,
                })?,
                funds: vec![],
            }),
            send_transfer_msg,
        ]
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
        .add_messages(mint_msgs)
        //   .add_message(send_msg)
        .add_event(event)
        .add_attribute("action", "steakhub/bond"))
}

pub fn harvest(deps: DepsMut, env: Env) -> StdResult<Response> {
    let withdraw_submsgs = deps
        .querier
        .query_all_delegations(&env.contract.address)?
        .into_iter()
        .map(|d| {
            SubMsg::reply_on_success(
                CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                    validator: d.validator,
                }),
                REPLY_REGISTER_RECEIVED_COINS,
            )
        })
        .collect::<Vec<_>>();

    // Skip, rather than revert the harvest, when nothing can be bonded (no rewards, or the
    // reserve initialisation is pending, or matured batches are owed the balance).
    let callback_msg = CallbackMsg::ReinvestIfNeeded {
        exclude: vec![],
    }
    .into_cosmos_msg(&env.contract.address)?;

    Ok(Response::new()
        .add_submessages(withdraw_submsgs)
        .add_message(callback_msg)
        .add_attribute("action", "steakhub/harvest"))
}

/// Complete reward processing after inactive-validator redelegations: withdraw rewards from every
/// remaining delegation (registered in `unlocked_coins` like the redelegations' own automatic
/// withdrawals), then reinvest everything registered. Withdrawing first also keeps the final
/// delegation from surfacing an active validator's older rewards after reinvestment.
pub fn harvest_after_redelegation(
    deps: DepsMut,
    env: Env,
    exclude: Vec<String>,
) -> StdResult<Response> {
    let withdraw_submsgs = deps
        .querier
        .query_all_delegations(&env.contract.address)?
        .into_iter()
        .map(|d| {
            SubMsg::reply_on_success(
                CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                    validator: d.validator,
                }),
                REPLY_REGISTER_RECEIVED_COINS,
            )
        })
        .collect::<Vec<_>>();

    let callback_msg = CallbackMsg::ReinvestIfNeeded {
        exclude,
    }
    .into_cosmos_msg(&env.contract.address)?;

    Ok(Response::new()
        .add_submessages(withdraw_submsgs)
        .add_message(callback_msg)
        .add_attribute("action", "steakhub/harvest_after_redelegation"))
}

pub fn reinvest_if_needed(deps: DepsMut, env: Env, exclude: Vec<String>) -> StdResult<Response> {
    let denom = State::default().denom.load(deps.storage)?;
    // Same cap as `reinvest`: recorded rewards may exist that cannot be bonded yet (matured
    // batches are owed the balance). Skip instead of failing, which would revert the
    // redelegations this follows.
    let (_, amount) = reinvestable(deps.as_ref(), &env, &denom, Uint128::zero())?;
    if amount.is_zero() {
        return Ok(Response::new().add_attribute("action", "steakhub/reinvest_not_needed"));
    }

    reinvest_excluding(deps, env, &exclude)
}

/// NOTE:
/// 1. When delegation Native denom here, we don't need to use a `SubMsg` to handle the received
///    coins, because we have already withdrawn all claimable staking rewards previously in the same
///    atomic execution.
/// 2. Same as with `bond`, in the latest implementation we only delegate staking rewards with the
///    validator that has the smallest delegation amount.
pub fn reinvest(deps: DepsMut, env: Env) -> StdResult<Response> {
    reinvest_excluding(deps, env, &[])
}

/// `reinvest`, never choosing a validator in `exclude` for the delegation. After
/// `RedelegateInactive`, the operator's exclude list may name a destination that refuses new stake
/// (e.g. one at a liquid-staking cap); delegating the rewards to it would revert the whole call on
/// every re-run, since reward handling is atomic with the redelegations.
fn reinvest_excluding(deps: DepsMut, env: Env, exclude: &[String]) -> StdResult<Response> {
    let state = State::default();
    let denom = state.denom.load(deps.storage)?;
    let fee = state.fee_rate.load(deps.storage)?;

    let validators = bonded_active_targets(deps.as_ref(), exclude)?;
    // Every staking message registers the rewards it withdraws (harvest's withdrawals, and the
    // automatic withdrawals of delegate / undelegate / redelegate) in `unlocked_coins`; bond all of
    // them. Never bond funds that matured unbonding batches are owed.
    let mut unlocked_coins = state.unlocked_coins.load(deps.storage)?;
    let (unlocked_native, amount_to_bond) =
        reinvestable(deps.as_ref(), &env, &denom, Uint128::zero())?;
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

/// What the backing that bonds are priced against holds beyond delegated stake: the holders'
/// after-fee share of rewards not yet restaked (recorded ones, as far as the balance backs them,
/// plus those still accruing on each delegation) and idle native (see `idle_native`), which
/// `ReinvestIdle` restakes fee-free. Counting idle here means restaking it does not move the
/// exchange rate, so nobody can bond just before a sweep to take part of it. `deposit` is the
/// native that arrived with the current message.
pub(crate) fn pending_rewards_net(
    deps: cosmwasm_std::Deps,
    env: &Env,
    delegations: &[Delegation],
    denom: &str,
    deposit: Uint128,
) -> StdResult<Uint128> {
    let fee = State::default().fee_rate.load(deps.storage)?;
    let (unlocked, reinvestable_now) = reinvestable(deps, env, denom, deposit)?;
    // While the reserve initialisation is pending nothing is bondable yet, but the recorded rewards
    // still belong to existing holders: count them up to what the balance holds, so a bond in that
    // window does not buy into them cheaply.
    let mut gross = if NATIVE_RESERVED.may_load(deps.storage)?.is_some() {
        reinvestable_now
    } else {
        unlocked.min(
            get_denom_balance(&deps.querier, env.contract.address.clone(), denom.into())?
                .saturating_sub(deposit),
        )
    };
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
    let idle = idle_native_with_deposit(deps, env, deposit)?.idle;
    Ok(gross.saturating_sub(fee_amount).checked_add(idle)?)
}

/// Native that unbonding batches are owed out of the balance. Reconciled batches come from the
/// running `NATIVE_RESERVED` total; unreconciled ones (few: only those still unbonding or awaiting
/// `Reconcile`) are summed. With `Some(now)`, only unreconciled batches whose funds should already
/// be here (matured by `now`) count; with `None`, all of them do, the conservative figure used
/// before treating any balance as idle. Returns `None` while the one-time reserve initialisation is
/// pending, when what is owed is not known yet.
pub(crate) fn native_owed_to_batches(
    storage: &dyn cosmwasm_std::Storage,
    now: Option<u64>,
) -> StdResult<Option<Uint128>> {
    let reserved = match NATIVE_RESERVED.may_load(storage)? {
        Some(reserved) => reserved,
        None => return Ok(None),
    };
    let unreconciled = State::default()
        .previous_batches
        .idx
        .reconciled
        .prefix(false.into())
        .range(storage, None, None, Order::Ascending)
        .map(|item| item.map(|(_, b)| b))
        .collect::<StdResult<Vec<Batch>>>()?
        .into_iter()
        .filter(|b| match now {
            Some(now) => b.est_unbond_end_time <= now,
            None => true,
        })
        .map(|b| b.amount_unclaimed)
        .sum::<Uint128>();
    Ok(Some(reserved.checked_add(unreconciled)?))
}

/// The recorded native rewards, and how much of them may be bonded now: never funds that matured
/// or reconciled batches are owed (nothing while the reserve initialisation is pending), and never
/// `deposit` (funds that arrived with the current message).
pub(crate) fn reinvestable(
    deps: cosmwasm_std::Deps,
    env: &Env,
    denom: &str,
    deposit: Uint128,
) -> StdResult<(Uint128, Uint128)> {
    let unlocked = Coins(State::default().unlocked_coins.load(deps.storage)?).find(denom).amount;
    let available = match native_owed_to_batches(deps.storage, Some(env.block.time.seconds()))? {
        Some(owed) => get_denom_balance(&deps.querier, env.contract.address.clone(), denom.into())?
            .saturating_sub(deposit)
            .saturating_sub(owed),
        None => Uint128::zero(),
    };
    Ok((unlocked, unlocked.min(available)))
}

/// Where reinvested or swept funds may be delegated: active validators the chain has bonded right
/// now (stake on a jailed or unbonded validator earns nothing), minus `exclude`.
pub(crate) fn bonded_active_targets(
    deps: cosmwasm_std::Deps,
    exclude: &[String],
) -> StdResult<Vec<String>> {
    let active = State::default().validators_active.load(deps.storage)?;
    let bonded: Vec<String> =
        deps.querier.query_all_validators()?.into_iter().map(|v| v.address).collect();
    let targets: Vec<String> =
        active.into_iter().filter(|v| bonded.contains(v) && !exclude.contains(v)).collect();
    if targets.is_empty() {
        return Err(StdError::generic_err("cannot reinvest without a bonded active validator"));
    }
    Ok(targets)
}

/// Native sitting in the contract that nothing accounts for: not delegated, not recorded as
/// rewards waiting to be reinvested, and not owed to a batch whose funds can already be here
/// (reconciled, or matured). It comes from rewards
/// withdrawn as a side effect of staking messages before they were recorded, and from unbonding
/// funds that arrived after their batch was written down. Zero while the reserve initialisation is
/// pending (what is owed is not known yet).
pub(crate) fn idle_native(deps: cosmwasm_std::Deps, env: &Env) -> StdResult<IdleFundsResponse> {
    idle_native_with_deposit(deps, env, Uint128::zero())
}

fn idle_native_with_deposit(
    deps: cosmwasm_std::Deps,
    env: &Env,
    deposit: Uint128,
) -> StdResult<IdleFundsResponse> {
    let state = State::default();
    let denom = state.denom.load(deps.storage)?;
    let balance = get_denom_balance(&deps.querier, env.contract.address.clone(), denom.clone())?
        .saturating_sub(deposit);
    let unlocked = Coins(state.unlocked_coins.load(deps.storage)?).find(&denom).amount;
    // Only batches whose funds can already be in the balance (reconciled, or matured by now):
    // batches still unbonding have not been paid yet. This relies on accurate batch end times.
    let owed_to_batches =
        native_owed_to_batches(deps.storage, Some(env.block.time.seconds()))?.unwrap_or(balance);
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

    let validators = bonded_active_targets(deps.as_ref(), &[])?;
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

    // An event this cannot parse is skipped and counted rather than failing the reply, which
    // would revert the staking message (and, for `RedelegateInactive`, the whole run). Native it
    // carried then shows up as idle and can be restaked with `ReinvestIdle`.
    let mut received_coins = Coins(vec![]);
    let mut unparsed = 0u32;
    for event in &events {
        match parse_coin_receiving_event(&env, event) {
            Ok(coins) => received_coins.add_many(&coins)?,
            Err(_) => unparsed += 1,
        }
    }

    let state = State::default();
    state.unlocked_coins.update(deps.storage, |coins| -> StdResult<_> {
        let mut coins = Coins(coins);
        coins.add_many(&received_coins)?;
        Ok(coins.0)
    })?;

    let mut response = Response::new().add_attribute("action", "steakhub/register_received_coins");
    if unparsed > 0 {
        response = response.add_attribute("unparsed_coin_received", unparsed.to_string());
    }
    Ok(response)
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
    usteak_to_burn: Uint128,
) -> StdResult<Response> {
    let state = State::default();

    let mut pending_batch = state.pending_batch.load(deps.storage)?;
    pending_batch.usteak_to_burn += usteak_to_burn;
    state.pending_batch.save(deps.storage, &pending_batch)?;

    state.unbond_requests.update(
        deps.storage,
        (pending_batch.id, &receiver),
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
    let steak_token = state.steak_token.load(deps.storage)?;
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

    let delegations = query_all_delegations(&deps.querier, &env.contract.address)?;
    let usteak_supply = query_cw20_total_supply(&deps.querier, &steak_token)?;
    if usteak_supply.is_zero() || pending_batch.usteak_to_burn > usteak_supply {
        return Err(StdError::generic_err("unbond batch exceeds the derivative token supply"));
    }
    if delegations.is_empty() || delegations.iter().all(|d| d.amount == 0) {
        return Err(StdError::generic_err("cannot unbond without delegated backing"));
    }

    let amount_to_bond =
        compute_unbond_amount(usteak_supply, pending_batch.usteak_to_burn, &delegations)?;
    let new_undelegations = compute_undelegations(amount_to_bond, &delegations, &denom);

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
    state.previous_batches.save(deps.storage, pending_batch.id, &Batch {
        id: pending_batch.id,
        reconciled: false,
        total_shares: pending_batch.usteak_to_burn,
        amount_unclaimed: amount_to_bond,
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
        .map(|d| SubMsg::reply_on_success(d.to_cosmos_msg(), REPLY_REGISTER_RECEIVED_COINS))
        .collect::<Vec<_>>();

    let event = Event::new("steakhub/unbond_submitted")
        .add_attribute("time", env.block.time.seconds().to_string())
        .add_attribute("height", env.block.height.to_string())
        .add_attribute("id", pending_batch.id.to_string())
        .add_attribute("native_unbonded", amount_to_bond)
        .add_attribute("usteak_burned", pending_batch.usteak_to_burn);

    if pending_batch.usteak_to_burn.is_zero() {
        return Ok(Response::new()
            .add_submessages(undelegate_submsgs)
            //        .add_message(burn_msg)
            .add_event(event));
    }

    let burn_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: steak_token.into(),
        msg: to_json_binary(&Cw20ExecuteMsg::Burn {
            amount: pending_batch.usteak_to_burn,
        })?,
        funds: vec![],
    });

    Ok(Response::new()
        .add_submessages(undelegate_submsgs)
        .add_message(burn_msg)
        .add_event(event)
        .add_attribute("action", "steakhub/unbond"))
}

pub fn reconcile(mut deps: DepsMut, env: Env) -> StdResult<Response> {
    let state = State::default();
    let current_time = env.block.time.seconds();

    // Load batches that have not been reconciled
    let all_batches = state
        .previous_batches
        .idx
        .reconciled
        .prefix(false.into())
        .range(deps.storage, None, None, Order::Ascending)
        .map(|item| {
            let (_, v) = item?;
            Ok(v)
        })
        .collect::<StdResult<Vec<_>>>()?;

    let mut batches = Vec::new();
    let mut waiting = Vec::new();
    for batch in all_batches.into_iter().filter(|b| current_time > b.est_unbond_end_time) {
        match RECONCILE_OBSERVED_HEIGHTS.may_load(deps.storage, batch.id)? {
            Some(observed_height) if env.block.height > observed_height => batches.push(batch),
            Some(_) => waiting.push(batch.id),
            None => {
                RECONCILE_OBSERVED_HEIGHTS.save(deps.storage, batch.id, &env.block.height)?;
                waiting.push(batch.id);
            },
        }
    }

    // Do not reconcile older batches while a newly matured batch may already have paid into the
    // shared balance. Waiting one more block makes every matured batch part of the same accounting
    // set and guarantees that each has had an EndBlock opportunity to receive its unbonding.
    if !waiting.is_empty() {
        let ids = waiting.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
        let deferred = batches.iter().map(|b| b.id.to_string()).collect::<Vec<_>>().join(",");
        let native_reserved = NATIVE_RESERVED
            .may_load(deps.storage)?
            .map(|reserved| reserved.to_string())
            .unwrap_or_else(|| "pending".to_string());
        return Ok(Response::new()
            .add_event(
                Event::new("steakhub/reconcile_observed")
                    .add_attribute("ids", ids)
                    .add_attribute("deferred_ids", deferred)
                    .add_attribute("height", env.block.height.to_string())
                    .add_attribute("retry_after_height", env.block.height.to_string()),
            )
            .add_attribute("native_reserved", native_reserved)
            .add_attribute("action", "steakhub/reconcile"));
    }

    // Until the one-time reserve initialisation finishes, `Reconcile` only advances it: the
    // matured batches are part of its pool and are reconciled by it.
    let native_reserved = match NATIVE_RESERVED.may_load(deps.storage)? {
        Some(reserved) => reserved,
        None => {
            let step = advance_reserve_initialization(deps.branch(), &env)?;
            let native_reserved = match &step {
                ReserveInitStep::Done {
                    reserved,
                    ..
                } => reserved.to_string(),
                _ => "pending".to_string(),
            };
            return Ok(Response::new()
                .add_event(reserve_initialization_event(&step))
                .add_attribute("native_reserved", native_reserved)
                .add_attribute("action", "steakhub/reconcile"));
        },
    };
    if batches.is_empty() {
        return Ok(Response::new()
            .add_event(Event::new("steakhub/reconciled").add_attribute("ids", ""))
            .add_attribute("native_reserved", native_reserved)
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
        state.previous_batches.save(deps.storage, batch.id, batch)?;
        RECONCILE_OBSERVED_HEIGHTS.remove(deps.storage, batch.id);
    }
    let newly_reserved: Uint128 = batches.iter().map(|b| b.amount_unclaimed).sum();
    let native_reserved = native_reserved.checked_add(newly_reserved)?;
    NATIVE_RESERVED.save(deps.storage, &native_reserved)?;

    let ids = batches.iter().map(|b| b.id.to_string()).collect::<Vec<_>>().join(",");

    let event = Event::new("steakhub/reconciled")
        .add_attribute("ids", ids)
        .add_attribute("native_deducted", native_to_deduct.to_string());

    Ok(Response::new()
        .add_event(event)
        .add_attribute("native_reserved", native_reserved)
        .add_attribute("action", "steakhub/reconcile"))
}

/// Called by `migrate`. A matured unreconciled batch is ambiguous in the migration block: its
/// payout may arrive only at that block's EndBlock. Record the observation and leave the reserve
/// pending until every matured batch was observed in an earlier block; then (and on every later
/// `Reconcile`) advance `advance_reserve_initialization`.
pub(crate) fn prepare_native_reserved_initialization(
    deps: DepsMut,
    env: &Env,
) -> StdResult<ReserveInitStep> {
    if let Some(reserved) = NATIVE_RESERVED.may_load(deps.storage)? {
        return Ok(ReserveInitStep::Done {
            reserved,
            written_down: Uint128::zero(),
            unlocked: Uint128::zero(),
        });
    }

    let state = State::default();
    let current_time = env.block.time.seconds();
    let unreconciled = state
        .previous_batches
        .idx
        .reconciled
        .prefix(false.into())
        .range(deps.storage, None, None, Order::Ascending)
        .map(|item| item.map(|(_, batch)| batch))
        .collect::<StdResult<Vec<_>>>()?;

    let mut waiting = false;
    for batch in unreconciled.into_iter().filter(|b| current_time > b.est_unbond_end_time) {
        match RECONCILE_OBSERVED_HEIGHTS.may_load(deps.storage, batch.id)? {
            Some(observed_height) if env.block.height > observed_height => {},
            Some(_) => waiting = true,
            None => {
                RECONCILE_OBSERVED_HEIGHTS.save(deps.storage, batch.id, &env.block.height)?;
                waiting = true;
            },
        }
    }

    if waiting {
        Ok(ReserveInitStep::Waiting)
    } else {
        advance_reserve_initialization(deps, env)
    }
}

/// Where the one-time reserve initialisation stands after a step.
pub(crate) enum ReserveInitStep {
    /// Matured batches still need an EndBlock before the balance can be classified.
    Waiting,
    /// A page was processed; call `Reconcile` again to continue.
    InProgress(ReserveInit),
    /// `NATIVE_RESERVED` now exists.
    Done {
        reserved: Uint128,
        written_down: Uint128,
        /// Recorded rewards held out of the pool (zero if the reserve already existed).
        unlocked: Uint128,
    },
}

pub(crate) fn reserve_initialization_event(step: &ReserveInitStep) -> Event {
    let event = Event::new("steakhub/reserve_initialization");
    match step {
        ReserveInitStep::Waiting => event.add_attribute("status", "waiting"),
        ReserveInitStep::InProgress(init) => event
            .add_attribute("status", "in_progress")
            .add_attribute(
                "pass",
                if init.applying {
                    "applying"
                } else {
                    "summing"
                },
            )
            .add_attribute("owed", init.owed)
            .add_attribute("shortfall", init.shortfall)
            .add_attribute("unlocked", init.unlocked)
            .add_attribute("written_down", init.applied),
        ReserveInitStep::Done {
            reserved,
            written_down,
            unlocked,
        } => event
            .add_attribute("status", "done")
            .add_attribute("native_reserved", *reserved)
            .add_attribute("pool_written_down", *written_down)
            .add_attribute("unlocked", *unlocked),
    }
}

/// Batches processed per call of `advance_reserve_initialization`.
const RESERVE_INIT_PAGE: usize = 100;

/// Advance the one-time initialisation of `NATIVE_RESERVED` by up to `RESERVE_INIT_PAGE` batches.
///
/// Reconciled batches written by earlier versions may claim more than the contract holds (the old
/// `reconcile_batches` could leave part of a shortfall unapplied), and matured batches not yet
/// reconciled draw on the same balance. Which of them a missing amount belongs to cannot be told,
/// so together they form a pool that shares it pro rata: one pass sums what the pool is owed, a
/// second writes the shortfall down (marking the matured batches reconciled), and the reserve
/// starts from what remains. Both passes are paginated so a long batch history cannot exceed the
/// gas limit; withdrawals wait until the reserve exists. Nothing else changes the pool's batches
/// meanwhile: `Reconcile` only advances this, and withdrawals are blocked.
///
/// Native in the balance that nothing tracks (e.g. rewards withdrawn as a side effect and never
/// recorded) counts as available to the pool.
pub(crate) fn advance_reserve_initialization(
    deps: DepsMut,
    env: &Env,
) -> StdResult<ReserveInitStep> {
    if let Some(reserved) = NATIVE_RESERVED.may_load(deps.storage)? {
        return Ok(ReserveInitStep::Done {
            reserved,
            written_down: Uint128::zero(),
            unlocked: Uint128::zero(),
        });
    }
    let state = State::default();
    let now = env.block.time.seconds();
    let mut init = RESERVE_INIT.may_load(deps.storage)?.unwrap_or(ReserveInit {
        pool_time: now,
        applying: false,
        cursor: None,
        owed: Uint128::zero(),
        shortfall: Uint128::zero(),
        applied: Uint128::zero(),
        cumulative: Uint128::zero(),
        joined: vec![],
        unlocked: Uint128::zero(),
    });

    // Up to `RESERVE_INIT_PAGE` batches per call across both passes, so a short history (or none)
    // finishes in the call that starts it, typically `migrate` itself.
    let mut budget = RESERVE_INIT_PAGE;
    loop {
        if budget == 0 {
            RESERVE_INIT.save(deps.storage, &init)?;
            return Ok(ReserveInitStep::InProgress(init));
        }
        let pool_time = init.pool_time;
        let joined = init.joined.clone();
        let in_pool =
            |b: &Batch| b.reconciled || b.est_unbond_end_time < pool_time || joined.contains(&b.id);

        let page = state
            .previous_batches
            .range(deps.storage, init.cursor.map(Bound::exclusive), None, Order::Ascending)
            .take(budget)
            .map(|item| item.map(|(_, b)| b))
            .collect::<StdResult<Vec<Batch>>>()?;
        let pass_done = page.len() < budget;
        budget -= page.len();
        init.cursor = page.last().map(|b| b.id).or(init.cursor);

        if !init.applying {
            for batch in page.iter().filter(|b| in_pool(b)) {
                init.owed = init.owed.checked_add(batch.amount_unclaimed)?;
            }
            if !pass_done {
                continue;
            }
            // A batch that matured after the pool was fixed may already have been paid, and if it
            // was slashed that is just as unattributable: it joins the pool rather than being
            // assumed paid in full. Unreconciled batches are few, so this stays bounded.
            let matured_since = state
                .previous_batches
                .idx
                .reconciled
                .prefix(false.into())
                .range(deps.storage, None, None, Order::Ascending)
                .map(|item| item.map(|(_, b)| b))
                .collect::<StdResult<Vec<Batch>>>()?
                .into_iter()
                .filter(|b| !in_pool(b) && b.est_unbond_end_time < now)
                .collect::<Vec<_>>();
            for batch in &matured_since {
                init.owed = init.owed.checked_add(batch.amount_unclaimed)?;
                init.joined.push(batch.id);
            }
            let denom = state.denom.load(deps.storage)?;
            let unlocked = Coins(state.unlocked_coins.load(deps.storage)?).find(&denom).amount;
            let available = deps
                .querier
                .query_balance(&env.contract.address, &denom)?
                .amount
                .saturating_sub(unlocked);
            init.shortfall = init.owed.saturating_sub(available);
            init.unlocked = unlocked;
            init.applying = true;
            init.cursor = None;
            continue;
        }

        for mut batch in page.into_iter().filter(|b| in_pool(b)) {
            // Nothing changes the pool between the passes (withdrawals wait, `Reconcile` only
            // drives this); clamp anyway so a surprise cannot wedge the initialisation.
            init.cumulative = init.cumulative.saturating_add(batch.amount_unclaimed).min(init.owed);
            let target = cumulative_share(init.shortfall, init.cumulative, init.owed)
                .min(init.shortfall)
                .max(init.applied);
            let share = (target - init.applied).min(batch.amount_unclaimed);
            batch.amount_unclaimed -= share;
            init.applied += share;
            if !batch.reconciled {
                batch.reconciled = true;
                RECONCILE_OBSERVED_HEIGHTS.remove(deps.storage, batch.id);
            }
            state.previous_batches.save(deps.storage, batch.id, &batch)?;
        }
        if !pass_done {
            continue;
        }

        let reserved = init.owed.saturating_sub(init.applied);
        NATIVE_RESERVED.save(deps.storage, &reserved)?;
        RESERVE_INIT.remove(deps.storage);
        return Ok(ReserveInitStep::Done {
            reserved,
            written_down: init.applied,
            unlocked: init.unlocked,
        });
    }
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
    let reserved = NATIVE_RESERVED.may_load(deps.storage)?.ok_or_else(|| {
        StdError::generic_err(
            "native reserve initialization is pending; call Reconcile in a later block",
        )
    })?;

    // NOTE: If the user has too many unclaimed requests, this may not fit in the WASM memory...
    // However, this is practically never going to happen. Who would create hundreds of unbonding
    // requests and never claim them?
    let requests = state
        .unbond_requests
        .idx
        .user
        .prefix(user.to_string())
        .range(deps.storage, None, None, Order::Ascending)
        .map(|item| {
            let (_, v) = item?;
            Ok(v)
        })
        .collect::<StdResult<Vec<_>>>()?;

    // Native in a batch is withdrawable once the batch is a _previous_ batch, has finished
    // unbonding, and is reconciled. `Reconcile` settles a matured batch only in a block after the
    // one in which it first saw it, so a withdrawal bundled after `Reconcile` in the same
    // transaction finds a just-matured batch still waiting (see below).
    let mut total_native_to_refund = Uint128::zero();
    let mut released = Uint128::zero();
    let mut ids: Vec<String> = vec![];
    let mut zero_refund_ids: Vec<String> = vec![];
    let mut awaiting: Vec<u64> = vec![];
    let mut dust_ids: Vec<String> = vec![];
    let mut withdrawals: Vec<(u64, Option<Batch>)> = vec![];
    for request in &requests {
        if let Ok(mut batch) = state.previous_batches.load(deps.storage, request.id) {
            if batch.est_unbond_end_time >= current_time {
                continue;
            }
            if !batch.reconciled {
                awaiting.push(batch.id);
                continue;
            }
            let native_to_refund =
                batch.amount_unclaimed.multiply_ratio(request.shares, batch.total_shares);
            if native_to_refund.is_zero() {
                if !batch.amount_unclaimed.is_zero() {
                    // The share rounds to nothing while the batch still holds value: keep the
                    // claim rather than deleting it for nothing.
                    dust_ids.push(request.id.to_string());
                    continue;
                }
                // Fully written down: clear the request, but say so.
                zero_refund_ids.push(request.id.to_string());
            }

            ids.push(request.id.to_string());

            total_native_to_refund += native_to_refund;
            batch.total_shares -= request.shares;
            batch.amount_unclaimed -= native_to_refund;

            if batch.total_shares.is_zero() {
                released += batch.amount_unclaimed;
                withdrawals.push((request.id, None));
            } else {
                withdrawals.push((request.id, Some(batch)));
            }
        }
    }

    if ids.is_empty() {
        if awaiting.is_empty() {
            if !dust_ids.is_empty() {
                return Err(StdError::generic_err(format!(
                    "claim in batch(es) {} currently rounds to zero; it stays claimable",
                    dust_ids.join(",")
                )));
            }
            return Err(StdError::generic_err("withdrawable amount is zero"));
        }
        // A matured batch still awaiting `Reconcile`. When it is first observed in this block (by
        // this call or by a `Reconcile` bundled before it), record that and succeed without paying,
        // so a bundled client still moves it towards settlement. Once it was observed in an earlier
        // block it only needs `Reconcile`: say so instead of succeeding with nothing.
        let mut observed_now = false;
        for id in &awaiting {
            match RECONCILE_OBSERVED_HEIGHTS.may_load(deps.storage, *id)? {
                None => {
                    RECONCILE_OBSERVED_HEIGHTS.save(deps.storage, *id, &env.block.height)?;
                    observed_now = true;
                },
                Some(height) if height == env.block.height => observed_now = true,
                Some(_) => {},
            }
        }
        let awaiting = awaiting.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
        if !observed_now {
            return Err(StdError::generic_err(format!(
                "batch(es) {} matured and await Reconcile; call Reconcile, then withdraw",
                awaiting
            )));
        }
        return Ok(Response::new()
            .add_event(
                Event::new("steakhub/withdraw_awaiting_reconcile")
                    .add_attribute("ids", awaiting)
                    .add_attribute("amount_refunded", "0")
                    .add_attribute("height", env.block.height.to_string())
                    .add_attribute("retry_after_height", env.block.height.to_string()),
            )
            .add_attribute("action", "steakhub/withdraw_unbonded"));
    }

    // Only reconciled batches pay out, so everything refunded (plus dust left on a batch that was
    // removed) must be part of the reserve. Paying more would draw from funds belonging to
    // unreconciled batches or recorded rewards, so fail closed if the accounting invariant does
    // not hold. No withdrawal state has been changed yet.
    let released_total = total_native_to_refund.checked_add(released)?;
    let remaining_reserved = reserved.checked_sub(released_total).map_err(|_| {
        StdError::generic_err("native reserve is smaller than the reconciled withdrawal")
    })?;

    for (id, batch) in withdrawals {
        if let Some(batch) = batch {
            state.previous_batches.save(deps.storage, id, &batch)?;
        } else {
            state.previous_batches.remove(deps.storage, id)?;
        }
        state.unbond_requests.remove(deps.storage, (id, &user))?;
    }
    for id in &awaiting {
        if RECONCILE_OBSERVED_HEIGHTS.may_load(deps.storage, *id)?.is_none() {
            RECONCILE_OBSERVED_HEIGHTS.save(deps.storage, *id, &env.block.height)?;
        }
    }
    NATIVE_RESERVED.save(deps.storage, &remaining_reserved)?;

    let mut event = Event::new("steakhub/unbonded_withdrawn")
        .add_attribute("time", env.block.time.seconds().to_string())
        .add_attribute("height", env.block.height.to_string())
        .add_attribute("ids", ids.join(","))
        .add_attribute("user", user)
        .add_attribute("receiver", &receiver)
        .add_attribute("amount_refunded", total_native_to_refund);
    if !zero_refund_ids.is_empty() {
        event = event.add_attribute("zero_refund_ids", zero_refund_ids.join(","));
    }
    if !dust_ids.is_empty() {
        event = event.add_attribute("dust_ids", dust_ids.join(","));
    }
    let mut response =
        Response::new().add_event(event).add_attribute("action", "steakhub/withdraw_unbonded");
    if !total_native_to_refund.is_zero() {
        response = response.add_message(CosmosMsg::Bank(BankMsg::Send {
            to_address: receiver.into(),
            amount: vec![Coin::new(total_native_to_refund.u128(), denom)],
        }));
    }

    Ok(response)
}

//--------------------------------------------------------------------------------------------------
// Ownership and management logics
//--------------------------------------------------------------------------------------------------

pub fn rebalance(deps: DepsMut, env: Env, minimum: Uint128) -> StdResult<Response> {
    let state = State::default();
    //let validators = state.validators.load(deps.storage)?;
    let validators_active = state.validators_active.load(deps.storage)?;
    if validators_active.is_empty() {
        return Err(StdError::generic_err("cannot rebalance without an active validator"));
    }

    let delegations = query_all_delegations(&deps.querier, &env.contract.address)?;

    let new_redelegations =
        compute_redelegations_for_rebalancing(validators_active, &delegations, minimum);

    let redelegate_submsgs = new_redelegations
        .iter()
        .map(|rd| SubMsg::reply_on_success(rd.to_cosmos_msg(), REPLY_REGISTER_RECEIVED_COINS))
        .collect::<Vec<_>>();

    let amount: u128 = new_redelegations.iter().map(|rd| rd.amount).sum();

    let event = Event::new("steakhub/rebalanced").add_attribute("amount_moved", amount.to_string());

    Ok(Response::new()
        .add_submessages(redelegate_submsgs)
        .add_event(event)
        .add_attribute("action", "steakhub/rebalance"))
}

pub fn add_validator(deps: DepsMut, sender: Addr, validator: String) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;

    state.validators.update(deps.storage, |mut validators| {
        if validators.contains(&validator) {
            return Err(StdError::generic_err("validator is already whitelisted"));
        }
        validators.push(validator.clone());
        Ok(validators)
    })?;

    let mut validators_active = state.validators_active.load(deps.storage)?;
    if !validators_active.contains(&validator) {
        validators_active.push(validator.clone());
    }
    state.validators_active.save(deps.storage, &validators_active)?;
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
    let validators_active = state.validators_active.load(deps.storage)?;
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
            CallbackMsg::HarvestAfterRedelegation {
                exclude,
            }
            .into_cosmos_msg(&env.contract.address)?,
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
/// redelegation withdrew, as for other redelegations; failure (the staking module refused it, and
/// its state changes are already reverted) is recorded so the operator knows to re-run later.
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

    let validators = state.validators.update(deps.storage, |mut validators| {
        if !validators.contains(&validator) {
            return Err(StdError::generic_err("validator is not already whitelisted"));
        }
        if validators.len() <= 1 {
            return Err(StdError::generic_err(
                "cannot remove the last validator with redelegation",
            ));
        }
        validators.retain(|v| *v != validator);
        Ok(validators)
    })?;
    let mut validators_active = state.validators_active.load(deps.storage)?;
    validators_active.retain(|v| *v != validator);
    state.validators_active.save(deps.storage, &validators_active)?;

    let delegations = query_delegations(&deps.querier, &validators, &env.contract.address, &denom)?;
    let delegation_to_remove =
        query_delegation(&deps.querier, &validator, &env.contract.address, &denom)?;
    let new_redelegations =
        compute_redelegations_for_removal(&delegation_to_remove, &delegations, &denom);

    let redelegate_submsgs = new_redelegations
        .iter()
        .map(|d| SubMsg::reply_on_success(d.to_cosmos_msg(), REPLY_REGISTER_RECEIVED_COINS))
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

    state.validators.update(deps.storage, |mut validators| {
        if !validators.contains(&validator) {
            return Err(StdError::generic_err("validator is not already whitelisted"));
        }
        validators.retain(|v| *v != validator);
        Ok(validators)
    })?;
    state.validators_active.update(deps.storage, |mut validators| -> StdResult<_> {
        validators.retain(|v| *v != validator);
        Ok(validators)
    })?;

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

    state.validators_active.update(deps.storage, |mut validators| {
        if !validators.contains(&validator) {
            return Err(StdError::generic_err("validator is not already whitelisted"));
        }
        validators.retain(|v| *v != validator);
        Ok(validators)
    })?;

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
    if !state.validators.load(deps.storage)?.contains(&validator) {
        return Err(StdError::generic_err("validator is not in the delegation set"));
    }
    let mut validators_active = state.validators_active.load(deps.storage)?;
    if !validators_active.contains(&validator) {
        validators_active.push(validator.clone());
    }
    state.validators_active.save(deps.storage, &validators_active)?;

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

pub fn collect_dust(deps: DepsMut, _env: Env, _max_tokens: usize) -> StdResult<Response> {
    let state = State::default();

    if let Some(_dust_addr) = state.dust_collector.load(deps.storage)? {
        Ok(Response::new().add_attribute("dust", "tbd"))
    } else {
        Err(StdError::generic_err("No dust collector set"))
    }
}
pub fn set_base_denom(deps: DepsMut, sender: Addr, new_denom: String) -> StdResult<Response> {
    let state = State::default();

    state.assert_owner(deps.storage, &sender)?;
    state.denom.save(deps.storage, &new_denom)?;

    let event = Event::new("steak/set_base_denom").add_attribute("base_denom", new_denom);

    Ok(Response::new().add_event(event).add_attribute("action", "steakhub/set_base_denom"))
}

pub fn return_denom(deps: DepsMut, _env: Env, _funds: Vec<Coin>) -> StdResult<Response> {
    let state = State::default();

    if let Some(_dust_addr) = state.dust_collector.load(deps.storage)? {
        Ok(Response::new().add_attribute("dust", "returned"))
    } else {
        Err(StdError::generic_err("No dust collector set"))
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

    let redelegation_msg = SubMsg::reply_on_success(
        Redelegation::new(&validator_from, &validator_to, delegation.amount, &denom)
            .to_cosmos_msg(),
        REPLY_REGISTER_RECEIVED_COINS,
    );

    let event = Event::new("steak/redelegate")
        .add_attribute("validator_from", validator_from)
        .add_attribute("validator_to", validator_to)
        .add_attribute("amount", delegation.amount.to_string());

    Ok(Response::new()
        .add_submessage(redelegation_msg)
        .add_event(event)
        .add_attribute("action", "steakhub/redelegate"))
}
