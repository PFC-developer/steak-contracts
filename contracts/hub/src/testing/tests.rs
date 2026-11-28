use std::str::FromStr;

use cosmwasm_std::{
    Addr, BankMsg, Coin, CosmosMsg, Decimal, DistributionMsg, Event, Order, OwnedDeps, Reply,
    ReplyOn, StdError, SubMsg, SubMsgResponse, Uint128, WasmMsg,
    testing::{MOCK_CONTRACT_ADDR, MockApi, MockStorage, mock_env, mock_info},
    to_json_binary,
};
use cw20::{Cw20ExecuteMsg, MinterResponse};
use cw20_base::msg::InstantiateMsg as Cw20InstantiateMsg;
use pfc_steak::hub::{
    Batch, CallbackMsg, ConfigResponse, ExecuteMsg, IdleFundsResponse, InstantiateMsg,
    PendingBatch, QueryMsg, ReceiveMsg, StateResponse, UnbondRequest,
    UnbondRequestsByBatchResponseItem, UnbondRequestsByUserResponseItem,
};

use super::{
    custom_querier::CustomQuerier,
    helpers::{mock_dependencies, mock_env_at_timestamp, query_helper},
};
use crate::{
    contract::{
        REDELEGATE_INACTIVE_MAX_LIMIT, REPLY_INSTANTIATE_TOKEN, REPLY_REDELEGATE_INACTIVE_BASE,
        REPLY_REGISTER_RECEIVED_COINS, REPLY_SUBMIT_BATCH, REPLY_UNDELEGATE, execute, instantiate,
        migrate, reply,
    },
    helpers::{parse_coin, parse_received_fund},
    math::{
        compute_idle_sweep_shares, compute_redelegations_for_inactive,
        compute_redelegations_for_rebalancing, compute_redelegations_for_removal,
        compute_undelegations, reconcile_batches,
    },
    state::{DEAD_SHARES, NATIVE_RESERVED, State},
    types::{Coins, Delegation, Redelegation, Undelegation},
};

//--------------------------------------------------------------------------------------------------
// Test setup
//--------------------------------------------------------------------------------------------------

fn setup_test() -> OwnedDeps<MockStorage, MockApi, CustomQuerier> {
    let mut deps = mock_dependencies();

    let res = instantiate(
        deps.as_mut(),
        mock_env_at_timestamp(10000),
        mock_info("deployer", &[]),
        InstantiateMsg {
            cw20_code_id: 69420,
            owner: "larry".to_string(),
            name: "Steak Token".to_string(),
            symbol: "STEAK".to_string(),
            denom: "uxyz".to_string(),
            fee_account_type: "Wallet".to_string(),
            fee_account: "the_fee_man".to_string(),
            fee_amount: Decimal::from_ratio(10_u128, 100_u128), //10%
            max_fee_amount: Decimal::from_ratio(20_u128, 100_u128), //20%
            decimals: 6,
            epoch_period: 259200,   // 3 * 24 * 60 * 60 = 3 days
            unbond_period: 1814400, // 21 * 24 * 60 * 60 = 21 days
            validators: vec!["alice".to_string(), "bob".to_string(), "charlie".to_string()],
            label: None,
            marketing: None,
            dust_collector: None,
        },
    )
    .unwrap();

    assert_eq!(res.messages.len(), 1);
    assert_eq!(
        res.messages[0],
        SubMsg::reply_on_success(
            CosmosMsg::Wasm(WasmMsg::Instantiate {
                admin: Some("larry".to_string()),
                code_id: 69420,
                msg: to_json_binary(&Cw20InstantiateMsg {
                    name: "Steak Token".to_string(),
                    symbol: "STEAK".to_string(),
                    decimals: 6,
                    initial_balances: vec![],
                    mint: Some(MinterResponse {
                        minter: MOCK_CONTRACT_ADDR.to_string(),
                        cap: None,
                    }),
                    marketing: None,
                })
                .unwrap(),
                funds: vec![],
                label: "steak_token".to_string(),
            }),
            REPLY_INSTANTIATE_TOKEN,
        )
    );

    let event = Event::new("instantiate")
        .add_attribute("code_id", "69420")
        .add_attribute("_contract_address", "steak_token");

    let res = reply(deps.as_mut(), mock_env_at_timestamp(10000), Reply {
        id: REPLY_INSTANTIATE_TOKEN,
        result: cosmwasm_std::SubMsgResult::Ok(SubMsgResponse {
            events: vec![event],
            data: None,
        }),
    })
    .unwrap();

    assert_eq!(res.messages.len(), 0);

    deps.querier.set_cw20_total_supply("steak_token", 0);
    deps.querier.set_bonded_validators(&["alice", "bob", "charlie"]);
    deps
}

fn callback_submsg(callback: CallbackMsg) -> SubMsg {
    SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: MOCK_CONTRACT_ADDR.to_string(),
        msg: to_json_binary(&ExecuteMsg::Callback(callback)).unwrap(),
        funds: vec![],
    }))
}

fn observe_and_reconcile(
    deps: &mut OwnedDeps<MockStorage, MockApi, CustomQuerier>,
    timestamp: u64,
) {
    let mut env = mock_env_at_timestamp(timestamp);
    execute(deps.as_mut(), env.clone(), mock_info("worker", &[]), ExecuteMsg::Reconcile {})
        .unwrap();
    env.block.height += 1;
    execute(deps.as_mut(), env, mock_info("worker", &[]), ExecuteMsg::Reconcile {}).unwrap();
}

#[test]
fn migration_repairs_active_validator_registry() {
    let mut deps = setup_test();
    let state = State::default();
    state
        .validators
        .update(deps.as_mut().storage, |mut validators| -> Result<_, StdError> {
            validators.retain(|validator| validator != "charlie");
            Ok(validators)
        })
        .unwrap();

    let response = migrate(deps.as_mut(), mock_env(), Default::default()).unwrap();

    // A validator the owner removed must stay removed: the stale active entry is dropped.
    let validators = state.validators.load(deps.as_ref().storage).unwrap();
    assert!(!validators.contains(&"charlie".to_string()));
    let validators_active = state.validators_active.load(deps.as_ref().storage).unwrap();
    assert_eq!(validators_active, vec!["alice".to_string(), "bob".to_string()]);
    assert!(
        response.attributes.iter().any(
            |attribute| attribute.key == "validator_registry_repairs" && attribute.value == "1"
        )
    );
}

#[test]
fn failed_auto_submit_batch_does_not_revert_unbond() {
    let mut deps = setup_test();

    let res = reply(deps.as_mut(), mock_env(), cosmwasm_std::Reply {
        id: REPLY_SUBMIT_BATCH,
        result: cosmwasm_std::SubMsgResult::Err(
            "unbond batch exceeds the derivative token supply".to_string(),
        ),
    })
    .unwrap();

    assert!(res.messages.is_empty());
    assert_eq!(res.events, vec![
        Event::new("steakhub/submit_batch_deferred")
            .add_attribute("reason", "unbond batch exceeds the derivative token supply")
    ]);
}

fn setup_test_fee_split() -> OwnedDeps<MockStorage, MockApi, CustomQuerier> {
    let mut deps = mock_dependencies();

    let res = instantiate(
        deps.as_mut(),
        mock_env_at_timestamp(10000),
        mock_info("deployer", &[]),
        InstantiateMsg {
            cw20_code_id: 69420,
            owner: "larry".to_string(),
            name: "Steak Token".to_string(),
            symbol: "STEAK".to_string(),
            denom: "uxyz".to_string(),
            fee_account_type: "FeeSplit".to_string(),
            fee_account: "fee_split_contract".to_string(),
            fee_amount: Decimal::from_ratio(10_u128, 100_u128), //10%
            max_fee_amount: Decimal::from_ratio(20_u128, 100_u128), //20%
            decimals: 6,
            epoch_period: 259200,   // 3 * 24 * 60 * 60 = 3 days
            unbond_period: 1814400, // 21 * 24 * 60 * 60 = 21 days
            validators: vec!["alice".to_string(), "bob".to_string(), "charlie".to_string()],
            label: None,
            marketing: None,
            dust_collector: None,
        },
    )
    .unwrap();

    assert_eq!(res.messages.len(), 1);
    assert_eq!(
        res.messages[0],
        SubMsg::reply_on_success(
            CosmosMsg::Wasm(WasmMsg::Instantiate {
                admin: Some("larry".to_string()),
                code_id: 69420,
                msg: to_json_binary(&Cw20InstantiateMsg {
                    name: "Steak Token".to_string(),
                    symbol: "STEAK".to_string(),
                    decimals: 6,
                    initial_balances: vec![],
                    mint: Some(MinterResponse {
                        minter: MOCK_CONTRACT_ADDR.to_string(),
                        cap: None,
                    }),
                    marketing: None,
                })
                .unwrap(),
                funds: vec![],
                label: "steak_token".to_string(),
            }),
            REPLY_INSTANTIATE_TOKEN,
        )
    );

    let event = Event::new("instantiate")
        .add_attribute("code_id", "69420")
        .add_attribute("_contract_address", "steak_token");

    let res = reply(deps.as_mut(), mock_env_at_timestamp(10000), Reply {
        id: REPLY_INSTANTIATE_TOKEN,
        result: cosmwasm_std::SubMsgResult::Ok(SubMsgResponse {
            events: vec![event],
            data: None,
        }),
    })
    .unwrap();

    assert_eq!(res.messages.len(), 0);

    deps.querier.set_cw20_total_supply("steak_token", 0);
    deps.querier.set_bonded_validators(&["alice", "bob", "charlie"]);
    deps
}

//--------------------------------------------------------------------------------------------------
// Execution
//--------------------------------------------------------------------------------------------------

#[test]
fn proper_instantiation() {
    let deps = setup_test();

    let res: ConfigResponse = query_helper(deps.as_ref(), QueryMsg::Config {});
    assert_eq!(res, ConfigResponse {
        owner: "larry".to_string(),
        new_owner: None,
        steak_token: "steak_token".to_string(),
        epoch_period: 259200,
        unbond_period: 1814400,
        denom: "uxyz".to_string(),
        fee_type: "Wallet".to_string(),
        fee_account: "the_fee_man".to_string(),
        fee_rate: Decimal::from_ratio(10_u128, 100_u128),
        max_fee_rate: Decimal::from_ratio(20_u128, 100_u128),
        validators: vec!["alice".to_string(), "bob".to_string(), "charlie".to_string(),],
        paused_validators: vec![],
        dust_collector: None,
        token_factory: None
    });

    let res: StateResponse = query_helper(deps.as_ref(), QueryMsg::State {});
    assert_eq!(res, StateResponse {
        total_usteak: Uint128::zero(),
        total_native: Uint128::zero(),
        exchange_rate: Decimal::one(),
        unlocked_coins: vec![],
    },);

    let res: PendingBatch = query_helper(deps.as_ref(), QueryMsg::PendingBatch {});
    assert_eq!(res, PendingBatch {
        id: 1,
        usteak_to_burn: Uint128::zero(),
        est_unbond_start_time: 269200, // 10,000 + 259,200
    },);
    let deps_fee_split = setup_test_fee_split();

    let res_fee_split: ConfigResponse = query_helper(deps_fee_split.as_ref(), QueryMsg::Config {});
    assert_eq!(res_fee_split, ConfigResponse {
        owner: "larry".to_string(),
        new_owner: None,
        steak_token: "steak_token".to_string(),
        epoch_period: 259200,
        unbond_period: 1814400,
        denom: "uxyz".to_string(),
        fee_type: "FeeSplit".to_string(),
        fee_account: "fee_split_contract".to_string(),
        fee_rate: Decimal::from_ratio(10_u128, 100_u128),
        max_fee_rate: Decimal::from_ratio(20_u128, 100_u128),
        validators: vec!["alice".to_string(), "bob".to_string(), "charlie".to_string(),],
        paused_validators: vec![],
        dust_collector: None,
        token_factory: None
    });
}

#[test]
fn bonding() {
    let mut deps = setup_test();

    // Bond when no delegation has been made
    // In this case, the full deposit simply goes to the first validator
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("user_1", &[Coin::new(1_000_000, "uxyz")]),
        ExecuteMsg::Bond {
            receiver: None,
            exec_msg: None,
        },
    )
    .unwrap();

    // 3 messages. (switched to 3 so we can 'send' instead of 'transfer' minted tokens, so contract
    // will know about it 1 - delegate
    // 2 - mint token (to ourselves)
    // 3 - send/transfer it
    assert_eq!(res.messages.len(), 4);
    assert_eq!(
        res.messages[0],
        SubMsg::reply_on_success(
            Delegation::new("alice", 1_000_000, "uxyz").to_cosmos_msg(),
            REPLY_REGISTER_RECEIVED_COINS
        )
    );
    assert_eq!(res.messages[1], SubMsg {
        id: 0,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: "steak_token".to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Mint {
                recipient: "cosmos2contract".to_string(),
                amount: Uint128::new(999_000),
            })
            .unwrap(),
            funds: vec![],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });

    assert_eq!(res.messages[2], SubMsg {
        id: 0,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: "steak_token".to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: "user_1".to_string(),
                amount: Uint128::new(999_000),
            })
            .unwrap(),
            funds: vec![],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });

    // First bond at zero supply: 1000 dead shares (MIN_DEAD_SHARES) are minted to the hub itself.
    assert_eq!(
        res.messages.last().unwrap(),
        &SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: "steak_token".to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Mint {
                recipient: MOCK_CONTRACT_ADDR.to_string(),
                amount: Uint128::new(1_000),
            })
            .unwrap(),
            funds: vec![],
        }))
    );

    // Bond when there are existing delegations, and Luna:Steak exchange rate is >1
    // Previously user 1 delegated 1,000,000 uluna. We assume we have accumulated 2.5% yield at
    // 1025000 staked
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 341667, "uxyz"),
        Delegation::new("bob", 341667, "uxyz"),
        Delegation::new("charlie", 341666, "uxyz"),
    ]);
    deps.querier.set_cw20_total_supply("steak_token", 1000000);

    // Charlie has the smallest amount of delegation, so the full deposit goes to him
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("user_2", &[Coin::new(12345, "uxyz")]),
        ExecuteMsg::Bond {
            receiver: Some("user_3".to_string()),
            exec_msg: None,
        },
    )
    .unwrap();

    assert_eq!(res.messages.len(), 3);
    assert_eq!(
        res.messages[0],
        SubMsg::reply_on_success(
            Delegation::new("charlie", 12345, "uxyz").to_cosmos_msg(),
            REPLY_REGISTER_RECEIVED_COINS
        )
    );
    assert_eq!(res.messages[1], SubMsg {
        id: 0,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: "steak_token".to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Mint {
                recipient: "cosmos2contract".to_string(),
                amount: Uint128::new(12043),
            })
            .unwrap(),
            funds: vec![],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });

    assert_eq!(res.messages[2], SubMsg {
        id: 0,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: "steak_token".to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Transfer {
                recipient: "user_3".to_string(),
                amount: Uint128::new(12043),
            })
            .unwrap(),
            funds: vec![],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });

    // Check the state after bonding
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 341667, "uxyz"),
        Delegation::new("bob", 341667, "uxyz"),
        Delegation::new("charlie", 354011, "uxyz"),
    ]);
    deps.querier.set_cw20_total_supply("steak_token", 1012043);

    let res: StateResponse = query_helper(deps.as_ref(), QueryMsg::State {});
    assert_eq!(res, StateResponse {
        total_usteak: Uint128::new(1012043),
        total_native: Uint128::new(1037345),
        exchange_rate: Decimal::from_ratio(1037345u128, 1012043u128),
        unlocked_coins: vec![],
    });
}

#[test]
fn state_uses_every_onchain_delegation() {
    let mut deps = setup_test();
    let state = State::default();

    state.validators.save(deps.as_mut().storage, &vec!["alice".to_string()]).unwrap();
    state.validators_active.save(deps.as_mut().storage, &vec!["alice".to_string()]).unwrap();
    deps.querier.set_cw20_total_supply("steak_token", 100);
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 40, "uxyz"),
        Delegation::new("charlie", 60, "uxyz"),
    ]);

    let response: StateResponse = query_helper(deps.as_ref(), QueryMsg::State {});
    assert_eq!(response.total_native, Uint128::new(100));
    assert_eq!(response.exchange_rate, Decimal::one());
}
#[test]
fn bond_ex() {
    let mut deps = setup_test();

    // Bond when no delegation has been made
    // In this case, the full deposit simply goes to the first validator
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("user_1", &[Coin::new(1_000_000, "uxyz")]),
        ExecuteMsg::BondEx {
            receiver: None,
        },
    )
    .unwrap();

    // 3 messages. (switched to 3 so we can 'send' instead of 'transfer' minted tokens, so contract
    // will know about it 1 - delegate
    // 2 - mint token (to ourselves)
    // 3 - send/transfer it
    assert_eq!(res.messages.len(), 3);
    // First bond at zero supply: 1000 dead shares (MIN_DEAD_SHARES) are minted to the hub itself.
    assert_eq!(
        res.messages.last().unwrap(),
        &SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: "steak_token".to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Mint {
                recipient: MOCK_CONTRACT_ADDR.to_string(),
                amount: Uint128::new(1_000),
            })
            .unwrap(),
            funds: vec![],
        }))
    );
    assert_eq!(
        res.messages[0],
        SubMsg::reply_on_success(
            Delegation::new("alice", 1_000_000, "uxyz").to_cosmos_msg(),
            REPLY_REGISTER_RECEIVED_COINS
        )
    );
    assert_eq!(res.messages[1], SubMsg {
        id: 0,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: "steak_token".to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Mint {
                recipient: "user_1".to_string(),
                amount: Uint128::new(999_000),
            })
            .unwrap(),
            funds: vec![],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });
}

#[test]
fn harvesting() {
    let mut deps = setup_test();

    // Assume users have bonded a total of 1,000,000 uluna and minted the same amount of usteak
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 341667, "uxyz"),
        Delegation::new("bob", 341667, "uxyz"),
        Delegation::new("charlie", 341666, "uxyz"),
    ]);
    deps.querier.set_cw20_total_supply("steak_token", 1000000);

    let res = execute(deps.as_mut(), mock_env(), mock_info("worker", &[]), ExecuteMsg::Harvest {})
        .unwrap();

    assert_eq!(res.messages.len(), 4);
    assert_eq!(
        res.messages[0],
        SubMsg::reply_on_success(
            CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                validator: "alice".to_string(),
            }),
            REPLY_REGISTER_RECEIVED_COINS,
        )
    );
    assert_eq!(
        res.messages[1],
        SubMsg::reply_on_success(
            CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                validator: "bob".to_string(),
            }),
            REPLY_REGISTER_RECEIVED_COINS,
        )
    );
    assert_eq!(
        res.messages[2],
        SubMsg::reply_on_success(
            CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                validator: "charlie".to_string(),
            }),
            REPLY_REGISTER_RECEIVED_COINS,
        )
    );
    assert_eq!(res.messages[3], SubMsg {
        id: 0,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: MOCK_CONTRACT_ADDR.to_string(),
            msg: to_json_binary(&ExecuteMsg::Callback(CallbackMsg::ReinvestIfNeeded {
                exclude: vec![],
            }))
            .unwrap(),
            funds: vec![],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });
}

#[test]
fn harvesting_and_reinvesting_after_inactive_redelegation() {
    let mut deps = setup_test();
    let state = State::default();

    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 100, "uxyz"),
        Delegation::new("bob", 200, "uxyz"),
        Delegation::new("charlie", 300, "uxyz"),
    ]);

    let err = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("worker", &[]),
        ExecuteMsg::Callback(CallbackMsg::HarvestAfterRedelegation {
            exclude: vec![],
        }),
    )
    .unwrap_err();
    assert_eq!(err, StdError::generic_err("callbacks can only be invoked by the contract itself"));

    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::HarvestAfterRedelegation {
            exclude: vec![],
        }),
    )
    .unwrap();

    assert_eq!(res.messages, vec![
        SubMsg::reply_on_success(
            CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                validator: "alice".to_string(),
            }),
            REPLY_REGISTER_RECEIVED_COINS,
        ),
        SubMsg::reply_on_success(
            CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                validator: "bob".to_string(),
            }),
            REPLY_REGISTER_RECEIVED_COINS,
        ),
        SubMsg::reply_on_success(
            CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                validator: "charlie".to_string(),
            }),
            REPLY_REGISTER_RECEIVED_COINS,
        ),
        callback_submsg(CallbackMsg::ReinvestIfNeeded {
            exclude: vec![],
        }),
    ]);

    deps.querier.set_bank_balances(&[Coin::new(150, "uxyz")]);
    // The redelegations' and the harvest's withdrawals were registered as 50 uxyz of rewards.
    state.unlocked_coins.save(deps.as_mut().storage, &vec![Coin::new(50, "uxyz")]).unwrap();
    // No active validator bonded: skip, keeping the 50 recorded for a later harvest, rather than
    // revert the harvest (or the redelegations this follows).
    deps.querier.set_bonded_validators(&[]);
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::ReinvestIfNeeded {
            exclude: vec![],
        }),
    )
    .unwrap();
    assert!(res.messages.is_empty());
    assert!(
        res.attributes.iter().any(|a| a.key == "reason" && a.value == "no bonded active validator")
    );

    // alice has the smallest delegation but is no longer bonded, so reinvest with bob instead.
    deps.querier.set_bonded_validators(&["bob", "charlie"]);
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::ReinvestIfNeeded {
            exclude: vec![],
        }),
    )
    .unwrap();
    assert_eq!(res.messages[0], SubMsg::new(Delegation::new("bob", 45, "uxyz").to_cosmos_msg()));

    state.unlocked_coins.save(deps.as_mut().storage, &vec![]).unwrap();
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::ReinvestIfNeeded {
            exclude: vec![],
        }),
    )
    .unwrap();
    assert!(res.messages.is_empty());
    assert_eq!(res.attributes, vec![cosmwasm_std::Attribute::new(
        "action",
        "steakhub/reinvest_not_needed"
    )]);
}

#[test]
fn registering_unlocked_coins() {
    let mut deps = setup_test();
    let state = State::default();

    // After withdrawing staking rewards, we parse the `coin_received` event to find the received
    // amounts
    let event = Event::new("coin_received")
        .add_attribute("receiver", MOCK_CONTRACT_ADDR.to_string())
        .add_attribute(
            "amount",
            "123ukrw,234uxyz,345uusd,69420ibc/\
             0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B",
        );

    reply(deps.as_mut(), mock_env(), Reply {
        id: 2,
        result: cosmwasm_std::SubMsgResult::Ok(SubMsgResponse {
            events: vec![event],
            data: None,
        }),
    })
    .unwrap();

    // Unlocked coins in contract state should have been updated
    let unlocked_coins = state.unlocked_coins.load(deps.as_ref().storage).unwrap();
    assert_eq!(unlocked_coins, vec![
        Coin::new(123, "ukrw"),
        Coin::new(234, "uxyz"),
        Coin::new(345, "uusd"),
        Coin::new(69420, "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B",),
    ]);
}

#[test]
fn reinvesting() {
    let mut deps = setup_test();
    let state = State::default();

    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 333334, "uxyz"),
        Delegation::new("bob", 333333, "uxyz"),
        Delegation::new("charlie", 333333, "uxyz"),
    ]);
    state.prev_denom.save(deps.as_mut().storage, &Uint128::zero()).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(234u128, "uxyz")]);

    // After the swaps, `unlocked_coins` should contain only uxyz and unknown denoms
    state
        .unlocked_coins
        .save(deps.as_mut().storage, &vec![
            Coin::new(234, "uxyz"),
            Coin::new(
                69420,
                "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B",
            ),
        ])
        .unwrap();

    // Bob has the smallest amount of delegations, so all proceeds go to him
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::Reinvest {}),
    )
    .unwrap();

    assert_eq!(res.messages.len(), 2);
    assert_eq!(res.messages[0], SubMsg {
        id: 0,
        msg: Delegation::new("bob", 234 - 23, "uxyz").to_cosmos_msg(),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });
    let send_msg = BankMsg::Send {
        to_address: "the_fee_man".into(),
        amount: vec![Coin::new(23u128, "uxyz")],
    };
    assert_eq!(res.messages[1], SubMsg {
        id: 0,
        msg: CosmosMsg::Bank(send_msg),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });

    // Storage should have been updated
    let unlocked_coins = state.unlocked_coins.load(deps.as_ref().storage).unwrap();
    assert_eq!(unlocked_coins, vec![Coin::new(
        69420,
        "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B",
    )],);
}

#[test]
fn reinvesting_fee_split() {
    let mut deps = setup_test_fee_split();
    let state = State::default();

    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 333334, "uxyz"),
        Delegation::new("bob", 333333, "uxyz"),
        Delegation::new("charlie", 333333, "uxyz"),
    ]);
    state.prev_denom.save(deps.as_mut().storage, &Uint128::zero()).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(234u128, "uxyz")]);

    // After the swaps, `unlocked_coins` should contain only uxyz and unknown denoms
    state
        .unlocked_coins
        .save(deps.as_mut().storage, &vec![
            Coin::new(234, "uxyz"),
            Coin::new(
                69420,
                "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B",
            ),
        ])
        .unwrap();

    // Bob has the smallest amount of delegations, so all proceeds go to him
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::Reinvest {}),
    )
    .unwrap();

    assert_eq!(res.messages.len(), 2);
    assert_eq!(res.messages[0], SubMsg {
        id: 0,
        msg: Delegation::new("bob", 234 - 23, "uxyz").to_cosmos_msg(),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });
    let send_msg = pfc_fee_split::fee_split_msg::ExecuteMsg::Deposit {
        flush: false,
    };

    assert_eq!(res.messages[1], SubMsg {
        id: 0,
        msg: send_msg
            .into_cosmos_msg("fee_split_contract", vec![Coin::new(23u128, "uxyz")])
            .unwrap(),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });

    // Storage should have been updated
    let unlocked_coins = state.unlocked_coins.load(deps.as_ref().storage).unwrap();
    assert_eq!(unlocked_coins, vec![Coin::new(
        69420,
        "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B",
    )],);
}

#[test]
fn queuing_unbond() {
    let mut deps = setup_test();
    let state = State::default();

    // Only Steak token is accepted for unbonding requests
    let err = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("random_token", &[]),
        ExecuteMsg::Receive(cw20::Cw20ReceiveMsg {
            sender: "hacker".to_string(),
            amount: Uint128::new(69420),
            msg: to_json_binary(&ReceiveMsg::QueueUnbond {
                receiver: None,
            })
            .unwrap(),
        }),
    )
    .unwrap_err();

    assert_eq!(err, StdError::generic_err("expecting Steak token, received random_token"));

    // User 1 creates an unbonding request before `est_unbond_start_time` is reached. The unbond
    // request is saved, but not the pending batch is not submitted for unbonding
    let res = execute(
        deps.as_mut(),
        mock_env_at_timestamp(12345), // est_unbond_start_time = 269200
        mock_info("steak_token", &[]),
        ExecuteMsg::Receive(cw20::Cw20ReceiveMsg {
            sender: "user_1".to_string(),
            amount: Uint128::new(23456),
            msg: to_json_binary(&ReceiveMsg::QueueUnbond {
                receiver: None,
            })
            .unwrap(),
        }),
    )
    .unwrap();

    assert_eq!(res.messages.len(), 0);

    // User 2 creates an unbonding request after `est_unbond_start_time` is reached. The unbond
    // request is saved, and the pending is automatically submitted for unbonding
    let res = execute(
        deps.as_mut(),
        mock_env_at_timestamp(269201), // est_unbond_start_time = 269200
        mock_info("steak_token", &[]),
        ExecuteMsg::Receive(cw20::Cw20ReceiveMsg {
            sender: "user_2".to_string(),
            amount: Uint128::new(69420),
            msg: to_json_binary(&ReceiveMsg::QueueUnbond {
                receiver: Some("user_3".to_string()),
            })
            .unwrap(),
        }),
    )
    .unwrap();

    assert_eq!(res.messages.len(), 1);
    assert_eq!(res.messages[0], SubMsg {
        id: REPLY_SUBMIT_BATCH,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: MOCK_CONTRACT_ADDR.to_string(),
            msg: to_json_binary(&ExecuteMsg::SubmitBatch {}).unwrap(),
            funds: vec![],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Error,
    });

    // The users' unbonding requests should have been saved
    let ubr1 = state
        .unbond_requests
        .load(deps.as_ref().storage, (1u64, &Addr::unchecked("user_1")))
        .unwrap();
    let ubr2 = state
        .unbond_requests
        .load(deps.as_ref().storage, (1u64, &Addr::unchecked("user_3")))
        .unwrap();

    assert_eq!(ubr1, UnbondRequest {
        id: 1,
        user: Addr::unchecked("user_1"),
        shares: Uint128::new(23456),
    });
    assert_eq!(ubr2, UnbondRequest {
        id: 1,
        user: Addr::unchecked("user_3"),
        shares: Uint128::new(69420),
    });

    // Pending batch should have been updated
    let pending_batch = state.pending_batch.load(deps.as_ref().storage).unwrap();
    assert_eq!(pending_batch, PendingBatch {
        id: 1,
        usteak_to_burn: Uint128::new(92876), // 23,456 + 69,420
        est_unbond_start_time: 269200,
    });
}

#[test]
fn submitting_batch() {
    let mut deps = setup_test();
    let state = State::default();

    // Reproduce the split registry state from the drain incident: Charlie is active but absent
    // from the primary validator registry. His delegation must still be counted exactly once.
    state
        .validators
        .update(deps.as_mut().storage, |mut validators| -> Result<_, StdError> {
            validators.retain(|validator| validator != "charlie");
            Ok(validators)
        })
        .unwrap();

    // uluna bonded: 1,037,345
    // usteak supply: 1,012,043
    // uluna per ustake: 1.025
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 345782, "uxyz"),
        Delegation::new("bob", 345782, "uxyz"),
        Delegation::new("charlie", 345781, "uxyz"),
    ]);
    deps.querier.set_cw20_total_supply("steak_token", 1012043);

    // We continue from the contract state at the end of the last test
    let unbond_requests = vec![
        UnbondRequest {
            id: 1,
            user: Addr::unchecked("user_1"),
            shares: Uint128::new(23456),
        },
        UnbondRequest {
            id: 1,
            user: Addr::unchecked("user_3"),
            shares: Uint128::new(69420),
        },
    ];

    for unbond_request in &unbond_requests {
        state
            .unbond_requests
            .save(
                deps.as_mut().storage,
                (unbond_request.id, &Addr::unchecked(unbond_request.user.clone())),
                unbond_request,
            )
            .unwrap();
    }

    state
        .pending_batch
        .save(deps.as_mut().storage, &PendingBatch {
            id: 1,
            usteak_to_burn: Uint128::new(92876), // 23,456 + 69,420
            est_unbond_start_time: 269200,
        })
        .unwrap();

    // Anyone can invoke `submit_batch`. Here we continue from the previous test and assume it is
    // invoked automatically as user 2 submits the unbonding request
    //
    // usteak to burn: 23,456 + 69,420 = 92,876
    // uluna to unbond: 1,037,345 * 92,876 / 1,012,043 = 95,197
    //
    // Target: (1,037,345 - 95,197) / 3 = 314,049
    // Remainer: 1
    // Alice:   345,782 - (314,049 + 1) = 31,732
    // Bob:     345,782 - (314,049 + 0) = 31,733
    // Charlie: 345,781 - (314,049 + 0) = 31,732
    let res = execute(
        deps.as_mut(),
        mock_env_at_timestamp(269201),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::SubmitBatch {},
    )
    .unwrap();

    assert_eq!(res.messages.len(), 4);
    assert_eq!(
        res.messages[0],
        SubMsg::reply_on_success(
            Undelegation::new("alice", 31732, "uxyz").to_cosmos_msg(),
            REPLY_UNDELEGATE
        )
    );
    assert_eq!(
        res.messages[1],
        SubMsg::reply_on_success(
            Undelegation::new("bob", 31733, "uxyz").to_cosmos_msg(),
            REPLY_UNDELEGATE
        )
    );
    assert_eq!(
        res.messages[2],
        SubMsg::reply_on_success(
            Undelegation::new("charlie", 31732, "uxyz").to_cosmos_msg(),
            REPLY_UNDELEGATE,
        )
    );
    assert_eq!(res.messages[3], SubMsg {
        id: 0,
        msg: CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: "steak_token".to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Burn {
                amount: Uint128::new(92876)
            })
            .unwrap(),
            funds: vec![],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });

    // A new pending batch should have been created
    let pending_batch = state.pending_batch.load(deps.as_ref().storage).unwrap();
    assert_eq!(pending_batch, PendingBatch {
        id: 2,
        usteak_to_burn: Uint128::zero(),
        est_unbond_start_time: 528401, // 269,201 + 259,200
    });

    // Previous batch should have been updated
    let previous_batch = state.previous_batches.load(deps.as_ref().storage, 1u64).unwrap();
    assert_eq!(previous_batch, Batch {
        id: 1,
        reconciled: false,
        total_shares: Uint128::new(92876),
        amount_unclaimed: Uint128::new(95197),
        est_unbond_end_time: 2083601, // 269,201 + 1,814,400
    });
}

#[test]
fn submitting_empty_batch_is_rejected() {
    let mut deps = setup_test();

    let err = execute(
        deps.as_mut(),
        mock_env_at_timestamp(269_201),
        mock_info("worker", &[]),
        ExecuteMsg::SubmitBatch {},
    )
    .unwrap_err();

    assert_eq!(err, StdError::generic_err("cannot submit an empty unbond batch"));
}

#[test]
fn reconciling() {
    let mut deps = setup_test();
    let state = State::default();

    let previous_batches = vec![
        Batch {
            id: 1,
            reconciled: true,
            total_shares: Uint128::new(92876),
            amount_unclaimed: Uint128::new(95197), // 1.025 Luna per Steak
            est_unbond_end_time: 10000,
        },
        Batch {
            id: 2,
            reconciled: false,
            total_shares: Uint128::new(1345),
            amount_unclaimed: Uint128::new(1385), // 1.030 Luna per Steak
            est_unbond_end_time: 20000,
        },
        Batch {
            id: 3,
            reconciled: false,
            total_shares: Uint128::new(1456),
            amount_unclaimed: Uint128::new(1506), // 1.035 Luna per Steak
            est_unbond_end_time: 30000,
        },
        Batch {
            id: 4,
            reconciled: false,
            total_shares: Uint128::new(1567),
            amount_unclaimed: Uint128::new(1629), // 1.040 Luna per Steak
            est_unbond_end_time: 40000,           // not yet finished unbonding, ignored
        },
    ];

    for previous_batch in &previous_batches {
        state
            .previous_batches
            .save(deps.as_mut().storage, previous_batch.id, previous_batch)
            .unwrap();
    }
    state
        .unlocked_coins
        .save(deps.as_mut().storage, &vec![
            Coin::new(10000, "uxyz"),
            Coin::new(234, "ukrw"),
            Coin::new(345, "uusd"),
            Coin::new(
                69420,
                "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B",
            ),
        ])
        .unwrap();

    deps.querier.set_bank_balances(&[
        Coin::new(12345 + 95197, "uxyz"), // includes batch 1, reconciled but unclaimed
        Coin::new(234, "ukrw"),
        Coin::new(345, "uusd"),
        Coin::new(69420, "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B"),
    ]);

    NATIVE_RESERVED.save(deps.as_mut().storage, &Uint128::new(95197)).unwrap();
    observe_and_reconcile(&mut deps, 35000);

    // Expected received: batch 2 + batch 3 = 1385 + 1506 = 2891
    // Expected unlocked: 10000
    // Expected: 12891
    // Actual: 12345
    // Shortfall: 12891 - 12345 = 456
    //
    // Shared pro rata to what each batch holds (2891 in total), exactly 546 overall:
    // batch 2: floor(546 * 1385 / 2891) = 261 -> 1385 - 261 = 1124
    // batch 3: 546 - 261 = 285                -> 1506 - 285 = 1221
    let batch = state.previous_batches.load(deps.as_ref().storage, 2u64).unwrap();
    assert_eq!(batch, Batch {
        id: 2,
        reconciled: true,
        total_shares: Uint128::new(1345),
        amount_unclaimed: Uint128::new(1124), // 1385 - 261
        est_unbond_end_time: 20000,
    });

    let batch = state.previous_batches.load(deps.as_ref().storage, 3u64).unwrap();
    assert_eq!(batch, Batch {
        id: 3,
        reconciled: true,
        total_shares: Uint128::new(1456),
        amount_unclaimed: Uint128::new(1221), // 1506 - 285
        est_unbond_end_time: 30000,
    });

    // Batches 1 and 4 should not have changed
    let batch = state.previous_batches.load(deps.as_ref().storage, 1u64).unwrap();
    assert_eq!(batch, previous_batches[0]);

    let batch = state.previous_batches.load(deps.as_ref().storage, 4u64).unwrap();
    assert_eq!(batch, previous_batches[3]);
}

#[test]
fn withdrawing_unbonded() {
    let mut deps = setup_test();
    let state = State::default();

    // We simulate a most general case:
    // - batches 1 and 2 have finished unbonding
    // - batch 3 have been submitted for unbonding but have not finished
    // - batch 4 is still pending
    let unbond_requests = vec![
        UnbondRequest {
            id: 1,
            user: Addr::unchecked("user_1"),
            shares: Uint128::new(23456),
        },
        UnbondRequest {
            id: 1,
            user: Addr::unchecked("user_3"),
            shares: Uint128::new(69420),
        },
        UnbondRequest {
            id: 2,
            user: Addr::unchecked("user_1"),
            shares: Uint128::new(34567),
        },
        UnbondRequest {
            id: 3,
            user: Addr::unchecked("user_1"),
            shares: Uint128::new(45678),
        },
        UnbondRequest {
            id: 4,
            user: Addr::unchecked("user_1"),
            shares: Uint128::new(56789),
        },
    ];

    for unbond_request in &unbond_requests {
        state
            .unbond_requests
            .save(
                deps.as_mut().storage,
                (unbond_request.id, &Addr::unchecked(unbond_request.user.clone())),
                unbond_request,
            )
            .unwrap();
    }

    let previous_batches = vec![
        Batch {
            id: 1,
            reconciled: true,
            total_shares: Uint128::new(92876),
            amount_unclaimed: Uint128::new(95197), // 1.025 Luna per Steak
            est_unbond_end_time: 10000,
        },
        Batch {
            id: 2,
            reconciled: true,
            total_shares: Uint128::new(34567),
            amount_unclaimed: Uint128::new(35604), // 1.030 Luna per Steak
            est_unbond_end_time: 20000,
        },
        Batch {
            id: 3,
            reconciled: false, // finished unbonding, but not reconciled; ignored
            total_shares: Uint128::new(45678),
            amount_unclaimed: Uint128::new(47276), // 1.035 Luna per Steak
            est_unbond_end_time: 20000,
        },
        Batch {
            id: 4,
            reconciled: true,
            total_shares: Uint128::new(56789),
            amount_unclaimed: Uint128::new(59060), // 1.040 Luna per Steak
            est_unbond_end_time: 30000,            /* reconciled, but not yet finished unbonding;
                                                    * ignored */
        },
    ];

    for previous_batch in &previous_batches {
        state
            .previous_batches
            .save(deps.as_mut().storage, previous_batch.id, previous_batch)
            .unwrap();
    }

    NATIVE_RESERVED
        .save(
            deps.as_mut().storage,
            &previous_batches
                .iter()
                .filter(|batch| batch.reconciled)
                .map(|batch| batch.amount_unclaimed)
                .sum(),
        )
        .unwrap();

    state
        .pending_batch
        .save(deps.as_mut().storage, &PendingBatch {
            id: 4,
            usteak_to_burn: Uint128::new(56789),
            est_unbond_start_time: 100000,
        })
        .unwrap();

    // Attempt to withdraw before any batch has completed unbonding. Should error
    let err = execute(
        deps.as_mut(),
        mock_env_at_timestamp(5000),
        mock_info("user_1", &[]),
        ExecuteMsg::WithdrawUnbonded {
            receiver: None,
        },
    )
    .unwrap_err();

    assert_eq!(err, StdError::generic_err("withdrawable amount is zero"));

    // Attempt to withdraw once batches 1 and 2 have finished unbonding, but 3 has not yet
    //
    // Withdrawable from batch 1: 95,197 * 23,456 / 92,876 = 24,042
    // Withdrawable from batch 2: 35,604
    // Total withdrawable: 24,042 + 35,604 = 59,646
    //
    // Batch 1 should be updated:
    // Total shares: 92,876 - 23,456 = 69,420
    // Unclaimed uluna: 95,197 - 24,042 = 71,155
    //
    // Batch 2 is completely withdrawn, should be purged from storage
    let res = execute(
        deps.as_mut(),
        mock_env_at_timestamp(25000),
        mock_info("user_1", &[]),
        ExecuteMsg::WithdrawUnbonded {
            receiver: None,
        },
    )
    .unwrap();

    assert_eq!(res.messages.len(), 1);
    assert_eq!(res.messages[0], SubMsg {
        id: 0,
        msg: CosmosMsg::Bank(BankMsg::Send {
            to_address: "user_1".to_string(),
            amount: vec![Coin::new(59646, "uxyz")],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });

    // Previous batches should have been updated
    let batch = state.previous_batches.load(deps.as_ref().storage, 1u64).unwrap();
    assert_eq!(batch, Batch {
        id: 1,
        reconciled: true,
        total_shares: Uint128::new(69420),
        amount_unclaimed: Uint128::new(71155),
        est_unbond_end_time: 10000,
    });

    let err = state.previous_batches.load(deps.as_ref().storage, 2u64).unwrap_err();
    assert_eq!(err, StdError::NotFound {
        kind: "pfc_steak::hub::Batch".to_string(),
    });

    // User 1's unbond requests in batches 1 and 2 should have been deleted
    let err1 = state
        .unbond_requests
        .load(deps.as_ref().storage, (1u64, &Addr::unchecked("user_1")))
        .unwrap_err();
    let err2 = state
        .unbond_requests
        .load(deps.as_ref().storage, (1u64, &Addr::unchecked("user_1")))
        .unwrap_err();

    assert_eq!(err1, StdError::NotFound {
        kind: "pfc_steak::hub::UnbondRequest".to_string()
    });
    assert_eq!(err2, StdError::NotFound {
        kind: "pfc_steak::hub::UnbondRequest".to_string()
    });

    // User 3 attempt to withdraw; also specifying a receiver
    let res = execute(
        deps.as_mut(),
        mock_env_at_timestamp(25000),
        mock_info("user_3", &[]),
        ExecuteMsg::WithdrawUnbonded {
            receiver: Some("user_2".to_string()),
        },
    )
    .unwrap();

    assert_eq!(res.messages.len(), 1);
    assert_eq!(res.messages[0], SubMsg {
        id: 0,
        msg: CosmosMsg::Bank(BankMsg::Send {
            to_address: "user_2".to_string(),
            amount: vec![Coin::new(71155, "uxyz")],
        }),
        gas_limit: None,
        reply_on: ReplyOn::Never,
    });

    // Batch 1 and user 2's unbonding request should have been purged from storage
    let err = state.previous_batches.load(deps.as_ref().storage, 1u64).unwrap_err();
    assert_eq!(err, StdError::NotFound {
        kind: "pfc_steak::hub::Batch".to_string()
    });

    let err = state
        .unbond_requests
        .load(deps.as_ref().storage, (1u64, &Addr::unchecked("user_3")))
        .unwrap_err();

    assert_eq!(err, StdError::NotFound {
        kind: "pfc_steak::hub::UnbondRequest".to_string()
    });
}

#[test]
fn adding_validator() {
    let mut deps = setup_test();
    let state = State::default();

    let err =
        execute(deps.as_mut(), mock_env(), mock_info("jake", &[]), ExecuteMsg::AddValidator {
            validator: "dave".to_string(),
        })
        .unwrap_err();

    assert_eq!(err, StdError::generic_err("unauthorized: sender is not owner"));

    let err =
        execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::AddValidator {
            validator: "alice".to_string(),
        })
        .unwrap_err();

    assert_eq!(err, StdError::generic_err("validator is already whitelisted"));

    let res =
        execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::AddValidator {
            validator: "dave".to_string(),
        })
        .unwrap();

    assert_eq!(res.messages.len(), 0);

    let validators = state.validators.load(deps.as_ref().storage).unwrap();
    assert_eq!(validators, vec![
        String::from("alice"),
        String::from("bob"),
        String::from("charlie"),
        String::from("dave"),
    ],);
}

#[test]
fn removing_validator() {
    let mut deps = setup_test();
    let state = State::default();

    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 341667, "uxyz"),
        Delegation::new("bob", 341667, "uxyz"),
        Delegation::new("charlie", 341666, "uxyz"),
    ]);

    let err =
        execute(deps.as_mut(), mock_env(), mock_info("jake", &[]), ExecuteMsg::RemoveValidator {
            validator: "charlie".to_string(),
        })
        .unwrap_err();

    assert_eq!(err, StdError::generic_err("unauthorized: sender is not owner"));

    let err =
        execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::RemoveValidator {
            validator: "dave".to_string(),
        })
        .unwrap_err();

    assert_eq!(err, StdError::generic_err("validator is not already whitelisted"));

    // Target: (341667 + 341667 + 341666) / 2 = 512500
    // Remainder: 0
    // Alice:   512500 + 0 - 341667 = 170833
    // Bob:     512500 + 0 - 341667 = 170833
    let res =
        execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::RemoveValidator {
            validator: "charlie".to_string(),
        })
        .unwrap();

    assert_eq!(res.messages.len(), 2);
    assert_eq!(
        res.messages[0],
        SubMsg::reply_on_success(
            Redelegation::new("charlie", "alice", 170833, "uxyz").to_cosmos_msg(),
            REPLY_REGISTER_RECEIVED_COINS,
        ),
    );
    assert_eq!(
        res.messages[1],
        SubMsg::reply_on_success(
            Redelegation::new("charlie", "bob", 170833, "uxyz").to_cosmos_msg(),
            REPLY_REGISTER_RECEIVED_COINS,
        ),
    );

    let validators = state.validators.load(deps.as_ref().storage).unwrap();
    assert_eq!(validators, vec![String::from("alice"), String::from("bob")],);
    let validators_active = state.validators_active.load(deps.as_ref().storage).unwrap();
    assert!(!validators_active.contains(&"charlie".to_string()));

    let err =
        execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::UnPauseValidator {
            validator: "charlie".to_string(),
        })
        .unwrap_err();
    assert_eq!(err, StdError::generic_err("validator is not in the delegation set"));
}

#[test]
fn transferring_ownership() {
    let mut deps = setup_test();
    let state = State::default();

    let err =
        execute(deps.as_mut(), mock_env(), mock_info("jake", &[]), ExecuteMsg::TransferOwnership {
            new_owner: "jake".to_string(),
        })
        .unwrap_err();

    assert_eq!(err, StdError::generic_err("unauthorized: sender is not owner"));

    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("larry", &[]),
        ExecuteMsg::TransferOwnership {
            new_owner: "jake".to_string(),
        },
    )
    .unwrap();

    assert_eq!(res.messages.len(), 0);

    let owner = state.owner.load(deps.as_ref().storage).unwrap();
    assert_eq!(owner, Addr::unchecked("larry"));

    let err = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("pumpkin", &[]),
        ExecuteMsg::AcceptOwnership {},
    )
    .unwrap_err();

    assert_eq!(err, StdError::generic_err("unauthorized: sender is not new owner"));

    let res =
        execute(deps.as_mut(), mock_env(), mock_info("jake", &[]), ExecuteMsg::AcceptOwnership {})
            .unwrap();

    assert_eq!(res.messages.len(), 0);

    let owner = state.owner.load(deps.as_ref().storage).unwrap();
    assert_eq!(owner, Addr::unchecked("jake"));
}

#[test]
fn splitting_fees() {
    let mut deps = setup_test();

    let err = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("jake", &[]),
        ExecuteMsg::TransferFeeAccount {
            fee_account_type: "Wallet".to_string(),
            new_fee_account: "charlie".to_string(),
        },
    )
    .unwrap_err();

    assert_eq!(err, StdError::generic_err("unauthorized: sender is not owner"));

    let err = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("larry", &[]),
        ExecuteMsg::TransferFeeAccount {
            fee_account_type: "xxxx".to_string(),
            new_fee_account: "charlie".to_string(),
        },
    )
    .unwrap_err();

    assert_eq!(err, StdError::generic_err("Invalid Fee type: Wallet or FeeSplit only"));

    execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::TransferFeeAccount {
        fee_account_type: "Wallet".to_string(),
        new_fee_account: "charlie".to_string(),
    })
    .unwrap();
    let res: ConfigResponse = query_helper(deps.as_ref(), QueryMsg::Config {});
    assert_eq!(res, ConfigResponse {
        owner: "larry".to_string(),
        new_owner: None,
        steak_token: "steak_token".to_string(),
        epoch_period: 259200,
        unbond_period: 1814400,
        denom: "uxyz".to_string(),
        fee_type: "Wallet".to_string(),
        fee_account: "charlie".to_string(),
        fee_rate: Decimal::from_ratio(10_u128, 100_u128),
        max_fee_rate: Decimal::from_ratio(20_u128, 100_u128),
        validators: vec!["alice".to_string(), "bob".to_string(), "charlie".to_string(),],
        paused_validators: vec![],
        dust_collector: None,
        token_factory: None
    });

    execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::TransferFeeAccount {
        fee_account_type: "FeeSplit".to_string(),
        new_fee_account: "contract".to_string(),
    })
    .unwrap();
    let res: ConfigResponse = query_helper(deps.as_ref(), QueryMsg::Config {});
    assert_eq!(res, ConfigResponse {
        owner: "larry".to_string(),
        new_owner: None,
        steak_token: "steak_token".to_string(),
        epoch_period: 259200,
        unbond_period: 1814400,
        denom: "uxyz".to_string(),
        fee_type: "FeeSplit".to_string(),
        fee_account: "contract".to_string(),
        fee_rate: Decimal::from_ratio(10_u128, 100_u128),
        max_fee_rate: Decimal::from_ratio(20_u128, 100_u128),
        validators: vec!["alice".to_string(), "bob".to_string(), "charlie".to_string(),],
        paused_validators: vec![],
        dust_collector: None,
        token_factory: None
    });
}
//--------------------------------------------------------------------------------------------------
// Queries
//--------------------------------------------------------------------------------------------------

#[test]
fn querying_previous_batches() {
    let mut deps = mock_dependencies();

    let batches = vec![
        Batch {
            id: 1,
            reconciled: false,
            total_shares: Uint128::new(123),
            amount_unclaimed: Uint128::new(678),
            est_unbond_end_time: 10000,
        },
        Batch {
            id: 2,
            reconciled: true,
            total_shares: Uint128::new(234),
            amount_unclaimed: Uint128::new(789),
            est_unbond_end_time: 15000,
        },
        Batch {
            id: 3,
            reconciled: false,
            total_shares: Uint128::new(345),
            amount_unclaimed: Uint128::new(890),
            est_unbond_end_time: 20000,
        },
        Batch {
            id: 4,
            reconciled: true,
            total_shares: Uint128::new(456),
            amount_unclaimed: Uint128::new(999),
            est_unbond_end_time: 25000,
        },
    ];

    let state = State::default();
    for batch in &batches {
        state.previous_batches.save(deps.as_mut().storage, batch.id, batch).unwrap();
    }

    // Querying a single batch
    let res: Batch = query_helper(deps.as_ref(), QueryMsg::PreviousBatch(1));
    assert_eq!(res, batches[0].clone());

    let res: Batch = query_helper(deps.as_ref(), QueryMsg::PreviousBatch(2));
    assert_eq!(res, batches[1].clone());

    // Query multiple batches
    let res: Vec<Batch> = query_helper(deps.as_ref(), QueryMsg::PreviousBatches {
        start_after: None,
        limit: None,
    });
    assert_eq!(res, batches.clone());

    let res: Vec<Batch> = query_helper(deps.as_ref(), QueryMsg::PreviousBatches {
        start_after: Some(1),
        limit: None,
    });
    assert_eq!(res, vec![batches[1].clone(), batches[2].clone(), batches[3].clone()]);

    let res: Vec<Batch> = query_helper(deps.as_ref(), QueryMsg::PreviousBatches {
        start_after: Some(4),
        limit: None,
    });
    assert_eq!(res, vec![]);

    // Query multiple batches, indexed by whether it has been reconciled
    let res = state
        .previous_batches
        .idx
        .reconciled
        .prefix(true.into())
        .range(deps.as_ref().storage, None, None, Order::Ascending)
        .map(|item| {
            let (_, v) = item.unwrap();
            v
        })
        .collect::<Vec<_>>();

    assert_eq!(res, vec![batches[1].clone(), batches[3].clone()]);

    let res = state
        .previous_batches
        .idx
        .reconciled
        .prefix(false.into())
        .range(deps.as_ref().storage, None, None, Order::Ascending)
        .map(|item| {
            let (_, v) = item.unwrap();
            v
        })
        .collect::<Vec<_>>();

    assert_eq!(res, vec![batches[0].clone(), batches[2].clone()]);
}

#[test]
fn querying_unbond_requests() {
    let mut deps = mock_dependencies();
    let state = State::default();

    let unbond_requests = vec![
        UnbondRequest {
            id: 1,
            user: Addr::unchecked("alice"),
            shares: Uint128::new(123),
        },
        UnbondRequest {
            id: 1,
            user: Addr::unchecked("bob"),
            shares: Uint128::new(234),
        },
        UnbondRequest {
            id: 1,
            user: Addr::unchecked("charlie"),
            shares: Uint128::new(345),
        },
        UnbondRequest {
            id: 2,
            user: Addr::unchecked("alice"),
            shares: Uint128::new(456),
        },
    ];

    for unbond_request in &unbond_requests {
        state
            .unbond_requests
            .save(
                deps.as_mut().storage,
                (unbond_request.id, &Addr::unchecked(unbond_request.user.clone())),
                unbond_request,
            )
            .unwrap();
    }

    let res: Vec<UnbondRequestsByBatchResponseItem> =
        query_helper(deps.as_ref(), QueryMsg::UnbondRequestsByBatch {
            id: 1,
            start_after: None,
            limit: None,
        });
    assert_eq!(res, vec![
        unbond_requests[0].clone().into(),
        unbond_requests[1].clone().into(),
        unbond_requests[2].clone().into(),
    ]);

    let res: Vec<UnbondRequestsByBatchResponseItem> =
        query_helper(deps.as_ref(), QueryMsg::UnbondRequestsByBatch {
            id: 2,
            start_after: None,
            limit: None,
        });
    assert_eq!(res, vec![unbond_requests[3].clone().into()]);

    let res: Vec<UnbondRequestsByUserResponseItem> =
        query_helper(deps.as_ref(), QueryMsg::UnbondRequestsByUser {
            user: "alice".to_string(),
            start_after: None,
            limit: None,
        });
    assert_eq!(res, vec![unbond_requests[0].clone().into(), unbond_requests[3].clone().into(),]);

    let res: Vec<UnbondRequestsByUserResponseItem> =
        query_helper(deps.as_ref(), QueryMsg::UnbondRequestsByUser {
            user: "alice".to_string(),
            start_after: Some(2),
            limit: None,
        });
    assert_eq!(res, vec![unbond_requests[3].clone().into()]);
}

//--------------------------------------------------------------------------------------------------
// Delegations
//--------------------------------------------------------------------------------------------------

#[test]
fn computing_undelegations() {
    let current_delegations = [
        Delegation::new("alice", 400, "uxyz"),
        Delegation::new("bob", 300, "uxyz"),
        Delegation::new("charlie", 200, "uxyz"),
    ];

    // Target: (400 + 300 + 200 - 451) / 3 = 149
    // Remainder: 2
    // Alice:   400 - (149 + 1) = 250
    // Bob:     300 - (149 + 1) = 150
    // Charlie: 200 - (149 + 0) = 51
    let new_undelegations = compute_undelegations(Uint128::new(451), &current_delegations, "uxyz");
    let expected = vec![
        Undelegation::new("alice", 250, "uxyz"),
        Undelegation::new("bob", 150, "uxyz"),
        Undelegation::new("charlie", 51, "uxyz"),
    ];
    assert_eq!(new_undelegations, expected);
}

#[test]
fn computing_redelegations_for_removal() {
    let current_delegations = [
        Delegation::new("alice", 13000, "uxyz"),
        Delegation::new("bob", 12000, "uxyz"),
        Delegation::new("charlie", 11000, "uxyz"),
        Delegation::new("dave", 10000, "uxyz"),
    ];

    // Suppose Dave will be removed
    // uluna_per_validator = (13000 + 12000 + 11000 + 10000) / 3 = 15333
    // remainder = 1
    // to Alice:   15333 + 1 - 13000 = 2334
    // to Bob:     15333 + 0 - 12000 = 3333
    // to Charlie: 15333 + 0 - 11000 = 4333
    let expected = vec![
        Redelegation::new("dave", "alice", 2334, "uxyz"),
        Redelegation::new("dave", "bob", 3333, "uxyz"),
        Redelegation::new("dave", "charlie", 4333, "uxyz"),
    ];

    assert_eq!(
        compute_redelegations_for_removal(
            &current_delegations[3],
            &current_delegations[..3],
            "uxyz",
        ),
        expected,
    );
}

#[test]
fn computing_redelegations_for_rebalancing() {
    let current_delegations = vec![
        Delegation::new("alice", 69420, "uxyz"),
        Delegation::new("bob", 1234, "uxyz"),
        Delegation::new("charlie", 88888, "uxyz"),
        Delegation::new("dave", 40471, "uxyz"),
        Delegation::new("evan", 2345, "uxyz"),
    ];
    let active_validators: Vec<String> = vec![
        "alice".to_string(),
        "bob".to_string(),
        "charlie".to_string(),
        "dave".to_string(),
        "evan".to_string(),
    ];
    // uluna_per_validator = (69420 + 88888 + 1234 + 40471 + 2345) / 4 = 40471
    // remainer = 3
    // src_delegations:
    //  - alice:   69420 - (40471 + 1) = 28948
    //  - charlie: 88888 - (40471 + 1) = 48416
    // dst_delegations:
    //  - bob:     (40471 + 1) - 1234  = 39238
    //  - evan:    (40471 + 0) - 2345  = 38126
    //
    // Round 1: alice --(28948)--> bob
    // src_delegations:
    //  - charlie: 48416
    // dst_delegations:
    //  - bob:     39238 - 28948 = 10290
    //  - evan:    38126
    //
    // Round 2: charlie --(10290)--> bob
    // src_delegations:
    //  - charlie: 48416 - 10290 = 38126
    // dst_delegations:
    //  - evan:    38126
    //
    // Round 3: charlie --(38126)--> evan
    // Queues are emptied
    let expected = vec![
        Redelegation::new("alice", "bob", 28948, "uxyz"),
        Redelegation::new("charlie", "bob", 10290, "uxyz"),
        Redelegation::new("charlie", "evan", 38126, "uxyz"),
    ];

    assert_eq!(
        compute_redelegations_for_rebalancing(
            active_validators,
            &current_delegations,
            Uint128::from(10u64)
        ),
        expected,
    );

    let partially_active =
        vec!["alice".to_string(), "charlie".to_string(), "dave".to_string(), "evan".to_string()];

    let partially_expected = vec![
        Redelegation::new("alice", "dave", 10118, "uxyz"),
        Redelegation::new("alice", "evan", 8712, "uxyz"),
        Redelegation::new("charlie", "evan", 38299, "uxyz"),
    ];
    assert_eq!(
        compute_redelegations_for_rebalancing(
            partially_active.clone(),
            &current_delegations,
            Uint128::from(10u64)
        ),
        partially_expected,
    );

    let partially_expected_minimums = vec![
        Redelegation::new("alice", "evan", 18830, "uxyz"),
        Redelegation::new("charlie", "evan", 29414, "uxyz"),
    ];
    assert_eq!(
        compute_redelegations_for_rebalancing(
            partially_active,
            &current_delegations,
            Uint128::from(15_000u64)
        ),
        partially_expected_minimums,
    );
}

//--------------------------------------------------------------------------------------------------
// Coins
//--------------------------------------------------------------------------------------------------

#[test]
fn parsing_coin() {
    let coin = parse_coin("12345uatom").unwrap();
    assert_eq!(coin, Coin::new(12345, "uatom"));

    let coin =
        parse_coin("23456ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B")
            .unwrap();
    assert_eq!(
        coin,
        Coin::new(23456, "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B",)
    );

    let err = parse_coin("69420").unwrap_err();
    assert_eq!(err, StdError::generic_err("failed to parse coin: 69420"));

    let err = parse_coin("ngmi").unwrap_err();
    assert_eq!(err, StdError::generic_err("Parsing u128: cannot parse integer from empty string"));

    assert!(parse_coin("12💥uatom").is_err());
}

#[test]
fn parsing_coins() {
    let coins = Coins::from_str("").unwrap();
    assert_eq!(coins.0, vec![]);

    let coins = Coins::from_str("12345uatom").unwrap();
    assert_eq!(coins.0, vec![Coin::new(12345, "uatom")]);

    let coins = Coins::from_str("12345uatom,23456uxyz").unwrap();
    assert_eq!(coins.0, vec![Coin::new(12345, "uatom"), Coin::new(23456, "uxyz")]);
}

#[test]
fn adding_coins() {
    let mut coins = Coins(vec![]);

    coins.add(&Coin::new(12345, "uatom")).unwrap();
    assert_eq!(coins.0, vec![Coin::new(12345, "uatom")]);

    coins.add(&Coin::new(23456, "uxyz")).unwrap();
    assert_eq!(coins.0, vec![Coin::new(12345, "uatom"), Coin::new(23456, "uxyz")]);

    coins.add_many(&Coins::from_str("76543uatom,69420uusd").unwrap()).unwrap();
    assert_eq!(coins.0, vec![
        Coin::new(88888, "uatom"),
        Coin::new(23456, "uxyz"),
        Coin::new(69420, "uusd"),
    ]);
}

#[test]
fn receiving_funds() {
    let err = parse_received_fund(&[], "uxyz").unwrap_err();
    assert_eq!(err, StdError::generic_err("must deposit exactly one coin; received 0"));

    let err = parse_received_fund(&[Coin::new(12345, "uatom"), Coin::new(23456, "uxyz")], "uxyz")
        .unwrap_err();
    assert_eq!(err, StdError::generic_err("must deposit exactly one coin; received 2"));

    let err = parse_received_fund(&[Coin::new(12345, "uatom")], "uxyz").unwrap_err();
    assert_eq!(err, StdError::generic_err("expected uxyz deposit, received uatom"));

    let err = parse_received_fund(&[Coin::new(0, "uxyz")], "uxyz").unwrap_err();
    assert_eq!(err, StdError::generic_err("deposit amount must be non-zero"));

    let amount = parse_received_fund(&[Coin::new(69420, "uxyz")], "uxyz").unwrap();
    assert_eq!(amount, Uint128::new(69420));
}

#[test]
fn reconciling_underflow() {
    let mut deps = setup_test();
    let state = State::default();
    let previous_batches = vec![
        Batch {
            id: 1,
            reconciled: true,
            total_shares: Uint128::new(92876),
            amount_unclaimed: Uint128::new(95197), // 1.025 Token per Stake
            est_unbond_end_time: 10000,
        },
        Batch {
            id: 2,
            reconciled: false,
            total_shares: Uint128::new(1345),
            amount_unclaimed: Uint128::new(1385), // 1.030 Token per Stake
            est_unbond_end_time: 20000,
        },
        Batch {
            id: 3,
            reconciled: false,
            total_shares: Uint128::new(1456),
            amount_unclaimed: Uint128::new(1506), // 1.035 Token per Stake
            est_unbond_end_time: 30000,
        },
        Batch {
            id: 4,
            reconciled: false,
            total_shares: Uint128::new(1),
            amount_unclaimed: Uint128::new(1),
            est_unbond_end_time: 30001,
        },
    ];
    for previous_batch in &previous_batches {
        state
            .previous_batches
            .save(deps.as_mut().storage, previous_batch.id, previous_batch)
            .unwrap();
    }
    state
        .unlocked_coins
        .save(deps.as_mut().storage, &vec![
            Coin::new(10000, "uatom"),
            Coin::new(234, "ukrw"),
            Coin::new(345, "uusd"),
            Coin::new(
                69420,
                "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B",
            ),
        ])
        .unwrap();
    deps.querier.set_bank_balances(&[
        Coin::new(12345, "uatom"),
        Coin::new(234, "ukrw"),
        Coin::new(345, "uusd"),
        Coin::new(69420, "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B"),
    ]);
    observe_and_reconcile(&mut deps, 35000);
}

#[test]
fn reconciling_underflow_second() {
    let mut deps = setup_test();
    let state = State::default();
    let previous_batches = vec![
        Batch {
            id: 1,
            reconciled: true,
            total_shares: Uint128::new(92876),
            amount_unclaimed: Uint128::new(95197), // 1.025 Token per Stake
            est_unbond_end_time: 10000,
        },
        Batch {
            id: 2,
            reconciled: false,
            total_shares: Uint128::new(1345),
            amount_unclaimed: Uint128::new(1385), // 1.030 Token per Stake
            est_unbond_end_time: 20000,
        },
        Batch {
            id: 3,
            reconciled: false,
            total_shares: Uint128::new(176),
            amount_unclaimed: Uint128::new(183), // 1.035 Token per Stake
            est_unbond_end_time: 30000,
        },
        Batch {
            id: 4,
            reconciled: false,
            total_shares: Uint128::new(1),
            amount_unclaimed: Uint128::new(1),
            est_unbond_end_time: 30001,
        },
    ];
    for previous_batch in &previous_batches {
        state
            .previous_batches
            .save(deps.as_mut().storage, previous_batch.id, previous_batch)
            .unwrap();
    }
    state
        .unlocked_coins
        .save(deps.as_mut().storage, &vec![
            Coin::new(10000, "uatom"),
            Coin::new(234, "ukrw"),
            Coin::new(345, "uusd"),
            Coin::new(
                69420,
                "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B",
            ),
        ])
        .unwrap();
    deps.querier.set_bank_balances(&[
        Coin::new(12345 - 1323, "uatom"),
        Coin::new(234, "ukrw"),
        Coin::new(345, "uusd"),
        Coin::new(69420, "ibc/0471F1C4E7AFD3F07702BEF6DC365268D64570F7C1FDC98EA6098DD6DE59817B"),
    ]);
    observe_and_reconcile(&mut deps, 35000);
}

#[test]
fn redelegating_inactive_validators() {
    let mut deps = setup_test();
    deps.querier.set_bonded_validators(&["alice", "bob", "charlie"]);

    // charlie is paused (whitelisted, inactive); dave was removed without redelegation and still
    // holds stake. alice and bob are the active set.
    execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::PauseValidator {
        validator: "charlie".to_string(),
    })
    .unwrap();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 100, "uxyz"),
        Delegation::new("bob", 300, "uxyz"),
        Delegation::new("charlie", 200, "uxyz"),
        Delegation::new("dave", 50, "uxyz"),
    ]);

    let err = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("jake", &[]),
        ExecuteMsg::RedelegateInactive {
            limit: None,
            exclude: None,
        },
    )
    .unwrap_err();
    assert_eq!(err, StdError::generic_err("unauthorized: sender is not owner"));

    for limit in [0, REDELEGATE_INACTIVE_MAX_LIMIT + 1] {
        let err = execute(
            deps.as_mut(),
            mock_env(),
            mock_info("larry", &[]),
            ExecuteMsg::RedelegateInactive {
                limit: Some(limit),
                exclude: None,
            },
        )
        .unwrap_err();
        assert_eq!(err, StdError::generic_err("limit must be between 1 and 50"));
    }

    // Full plan: charlie->alice 200 (alice 300, bob 300), then dave's 50 split 25/25.
    // limit 2 cuts the run short; the remainder is picked up by the next call.
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("larry", &[]),
        ExecuteMsg::RedelegateInactive {
            limit: Some(2),
            exclude: None,
        },
    )
    .unwrap();
    assert_eq!(res.messages, vec![
        SubMsg {
            id: REPLY_REDELEGATE_INACTIVE_BASE,
            msg: Redelegation::new("charlie", "alice", 200, "uxyz").to_cosmos_msg(),
            gas_limit: None,
            reply_on: ReplyOn::Always,
        },
        SubMsg {
            id: REPLY_REDELEGATE_INACTIVE_BASE + 1,
            msg: Redelegation::new("dave", "alice", 25, "uxyz").to_cosmos_msg(),
            gas_limit: None,
            reply_on: ReplyOn::Always,
        },
        callback_submsg(CallbackMsg::HarvestAfterRedelegation {
            exclude: vec![],
        }),
    ]);
    let summary = res.events.iter().find(|e| e.ty == "steakhub/redelegated_inactive").unwrap();
    let attr = |key: &str| {
        summary.attributes.iter().find(|a| a.key == key).map(|a| a.value.clone()).unwrap()
    };
    assert_eq!(attr("amount_planned"), "225");
    assert_eq!(attr("inactive_before"), "250");
    assert_eq!(attr("inactive_unplanned"), "25");

    // Restart: charlie's move went through but dave's was refused (e.g. a transitive
    // redelegation). The next call plans only what is still on inactive validators.
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 300, "uxyz"),
        Delegation::new("bob", 300, "uxyz"),
        Delegation::new("dave", 50, "uxyz"),
    ]);
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("larry", &[]),
        ExecuteMsg::RedelegateInactive {
            limit: None,
            exclude: None,
        },
    )
    .unwrap();
    assert_eq!(res.messages, vec![
        SubMsg {
            id: REPLY_REDELEGATE_INACTIVE_BASE,
            msg: Redelegation::new("dave", "alice", 25, "uxyz").to_cosmos_msg(),
            gas_limit: None,
            reply_on: ReplyOn::Always,
        },
        SubMsg {
            id: REPLY_REDELEGATE_INACTIVE_BASE + 1,
            msg: Redelegation::new("dave", "bob", 25, "uxyz").to_cosmos_msg(),
            gas_limit: None,
            reply_on: ReplyOn::Always,
        },
        callback_submsg(CallbackMsg::HarvestAfterRedelegation {
            exclude: vec![],
        }),
    ]);

    // Nothing left on inactive validators: re-running is a harmless no-op.
    let state = State::default();
    state.prev_denom.save(deps.as_mut().storage, &Uint128::new(123)).unwrap();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 325, "uxyz"),
        Delegation::new("bob", 325, "uxyz"),
    ]);
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("larry", &[]),
        ExecuteMsg::RedelegateInactive {
            limit: None,
            exclude: None,
        },
    )
    .unwrap();
    assert!(res.messages.is_empty());
    assert_eq!(state.prev_denom.load(deps.as_ref().storage).unwrap(), Uint128::new(123));
}

#[test]
fn refused_inactive_redelegation_is_skipped() {
    let mut deps = setup_test();

    let res = reply(deps.as_mut(), mock_env(), Reply {
        id: REPLY_REDELEGATE_INACTIVE_BASE + 1,
        result: cosmwasm_std::SubMsgResult::Err(
            "redelegation to this validator already in progress; first redelegation to this \
             validator must complete before next redelegation"
                .to_string(),
        ),
    })
    .unwrap();
    assert!(res.messages.is_empty());
    assert_eq!(res.events, vec![
        Event::new("steakhub/redelegation_skipped").add_attribute("index", "1").add_attribute(
            "reason",
            "redelegation to this validator already in progress; first redelegation to this \
             validator must complete before next redelegation",
        )
    ]);

    // Success registers withdrawn rewards like other redelegations; with no coin_received events
    // there is nothing to register.
    let res = reply(deps.as_mut(), mock_env(), Reply {
        id: REPLY_REDELEGATE_INACTIVE_BASE,
        result: cosmwasm_std::SubMsgResult::Ok(SubMsgResponse {
            events: vec![],
            data: None,
        }),
    })
    .unwrap();
    assert!(res.events.is_empty());
}

#[test]
fn computing_redelegations_for_inactive() {
    let active = vec!["alice".to_string(), "bob".to_string()];
    let delegations = [
        Delegation::new("alice", 100, "uxyz"),
        Delegation::new("bob", 300, "uxyz"),
        Delegation::new("charlie", 200, "uxyz"),
        Delegation::new("dave", 50, "uxyz"),
    ];

    assert_eq!(
        compute_redelegations_for_inactive(&active, &active, &delegations, 10, "uxyz"),
        vec![
            Redelegation::new("charlie", "alice", 200, "uxyz"),
            Redelegation::new("dave", "alice", 25, "uxyz"),
            Redelegation::new("dave", "bob", 25, "uxyz"),
        ]
    );
    assert_eq!(
        compute_redelegations_for_inactive(&active, &active, &delegations, 1, "uxyz"),
        vec![Redelegation::new("charlie", "alice", 200, "uxyz"),]
    );
    // An active validator with no delegation yet is still a target.
    let active = vec!["alice".to_string(), "erin".to_string()];
    let delegations = [Delegation::new("alice", 100, "uxyz"), Delegation::new("dave", 100, "uxyz")];
    assert_eq!(
        compute_redelegations_for_inactive(&active, &active, &delegations, 10, "uxyz"),
        vec![Redelegation::new("dave", "erin", 100, "uxyz"),]
    );
    assert!(compute_redelegations_for_inactive(&[], &[], &delegations, 10, "uxyz").is_empty());
}

#[test]
fn redelegating_inactive_respects_exclude_and_bonded_set() {
    let mut deps = setup_test();
    execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::PauseValidator {
        validator: "charlie".to_string(),
    })
    .unwrap();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 100, "uxyz"),
        Delegation::new("bob", 300, "uxyz"),
        Delegation::new("charlie", 200, "uxyz"),
        Delegation::new("dave", 50, "uxyz"),
    ]);

    // Nothing in the active set is bonded on chain: refuse rather than move stake nowhere useful.
    deps.querier.set_bonded_validators(&[]);
    let err = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("larry", &[]),
        ExecuteMsg::RedelegateInactive {
            limit: None,
            exclude: None,
        },
    )
    .unwrap_err();
    assert_eq!(
        err,
        StdError::generic_err("no active validator is currently bonded on chain to redelegate to")
    );

    // bob is active but not bonded (e.g. jailed): he receives nothing. charlie is excluded as a
    // source (e.g. blocked by an incoming redelegation), so only dave's stake moves, all to alice.
    deps.querier.set_bonded_validators(&["alice", "charlie"]);
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("larry", &[]),
        ExecuteMsg::RedelegateInactive {
            limit: None,
            exclude: Some(vec!["charlie".to_string()]),
        },
    )
    .unwrap();
    assert_eq!(res.messages, vec![
        SubMsg {
            id: REPLY_REDELEGATE_INACTIVE_BASE,
            msg: Redelegation::new("dave", "alice", 50, "uxyz").to_cosmos_msg(),
            gas_limit: None,
            reply_on: ReplyOn::Always,
        },
        callback_submsg(CallbackMsg::HarvestAfterRedelegation {
            exclude: vec!["charlie".to_string()],
        }),
    ]);
    let summary = res.events.iter().find(|e| e.ty == "steakhub/redelegated_inactive").unwrap();
    let attr = |key: &str| {
        summary.attributes.iter().find(|a| a.key == key).map(|a| a.value.clone()).unwrap()
    };
    assert_eq!(attr("inactive_before"), "250");
    assert_eq!(attr("inactive_excluded"), "200");
    assert_eq!(attr("inactive_unplanned"), "200");
}

#[test]
fn successful_inactive_redelegation_registers_withdrawn_rewards() {
    let mut deps = setup_test();
    let state = State::default();
    state.unlocked_coins.save(deps.as_mut().storage, &vec![]).unwrap();

    reply(deps.as_mut(), mock_env(), Reply {
        id: REPLY_REDELEGATE_INACTIVE_BASE,
        result: cosmwasm_std::SubMsgResult::Ok(SubMsgResponse {
            events: vec![
                Event::new("coin_received")
                    .add_attribute("receiver", MOCK_CONTRACT_ADDR)
                    .add_attribute("amount", "123uxyz"),
                Event::new("coin_received")
                    .add_attribute("receiver", "someone_else")
                    .add_attribute("amount", "999uxyz"),
            ],
            data: None,
        }),
    })
    .unwrap();

    let unlocked = state.unlocked_coins.load(deps.as_ref().storage).unwrap();
    assert_eq!(unlocked, vec![Coin::new(123, "uxyz")]);
}

#[test]
fn reconcile_waits_for_a_completed_block_after_maturity() {
    let mut deps = setup_test();
    let state = State::default();
    let batch = Batch {
        id: 1,
        reconciled: false,
        total_shares: Uint128::new(1000),
        amount_unclaimed: Uint128::new(1000),
        est_unbond_end_time: 20000,
    };
    state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    deps.querier.set_bank_balances(&[]);

    // Even after a long chain halt, the first block past maturity only records an observation.
    let mut env = mock_env_at_timestamp(30000);
    execute(deps.as_mut(), env.clone(), mock_info("anyone", &[]), ExecuteMsg::Reconcile {})
        .unwrap();
    assert_eq!(
        state.previous_batches.load(deps.as_ref().storage, 1u64).unwrap(),
        batch,
        "the first post-halt block must not write the batch down"
    );

    // A second transaction in that same block is still unable to reconcile it.
    execute(deps.as_mut(), env.clone(), mock_info("anyone", &[]), ExecuteMsg::Reconcile {})
        .unwrap();
    assert_eq!(state.previous_batches.load(deps.as_ref().storage, 1u64).unwrap(), batch);

    // The staking module pays at EndBlock; reconciliation in the next block sees those funds.
    deps.querier.set_bank_balances(&[Coin::new(1000, "uxyz")]);
    env.block.height += 1;
    execute(deps.as_mut(), env, mock_info("anyone", &[]), ExecuteMsg::Reconcile {}).unwrap();
    let reconciled = state.previous_batches.load(deps.as_ref().storage, 1u64).unwrap();
    assert!(reconciled.reconciled);
    assert_eq!(reconciled.amount_unclaimed, Uint128::new(1000));
}

#[test]
fn reconcile_does_not_count_funds_owed_to_reconciled_batches() {
    let mut deps = setup_test();
    let state = State::default();
    // Batch 1 is reconciled and still owed 500. Batch 2 expected 1000 but only 800 arrived.
    let batch1 = Batch {
        id: 1,
        reconciled: true,
        total_shares: Uint128::new(500),
        amount_unclaimed: Uint128::new(500),
        est_unbond_end_time: 10000,
    };
    let batch2 = Batch {
        id: 2,
        reconciled: false,
        total_shares: Uint128::new(1000),
        amount_unclaimed: Uint128::new(1000),
        est_unbond_end_time: 20000,
    };
    state.previous_batches.save(deps.as_mut().storage, batch1.id, &batch1).unwrap();
    state.previous_batches.save(deps.as_mut().storage, batch2.id, &batch2).unwrap();
    NATIVE_RESERVED.save(deps.as_mut().storage, &Uint128::new(500)).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(500 + 800, "uxyz")]);

    observe_and_reconcile(&mut deps, 30000);

    // Without the reserve, batch 1's 500 would cover the 200 shortfall and batch 2 would keep 1000.
    assert_eq!(
        state.previous_batches.load(deps.as_ref().storage, 2u64).unwrap().amount_unclaimed,
        Uint128::new(800)
    );
    assert_eq!(
        state.previous_batches.load(deps.as_ref().storage, 1u64).unwrap().amount_unclaimed,
        Uint128::new(500)
    );
}

#[test]
fn reinvest_restakes_recorded_side_effect_rewards() {
    let mut deps = setup_test();
    let state = State::default();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 333334, "uxyz"),
        Delegation::new("bob", 333333, "uxyz"),
        Delegation::new("charlie", 333333, "uxyz"),
    ]);
    // 300 uxyz of rewards were withdrawn automatically by earlier bonds / unbonds / redelegations
    // and recorded; the old balance-delta baseline would have seen no new rewards at all.
    deps.querier.set_bank_balances(&[Coin::new(300, "uxyz")]);
    state.prev_denom.save(deps.as_mut().storage, &Uint128::new(300)).unwrap();
    state.unlocked_coins.save(deps.as_mut().storage, &vec![Coin::new(300, "uxyz")]).unwrap();

    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::Reinvest {}),
    )
    .unwrap();
    assert_eq!(res.messages[0], SubMsg::new(Delegation::new("bob", 270, "uxyz").to_cosmos_msg()));
    assert_eq!(
        res.messages[1],
        SubMsg::new(CosmosMsg::Bank(BankMsg::Send {
            to_address: "the_fee_man".into(),
            amount: vec![Coin::new(30, "uxyz")],
        }))
    );
    assert!(state.unlocked_coins.load(deps.as_ref().storage).unwrap().is_empty());
}

#[test]
fn reinvest_never_bonds_funds_owed_to_batches() {
    let mut deps = setup_test();
    let state = State::default();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 333334, "uxyz"),
        Delegation::new("bob", 333333, "uxyz"),
        Delegation::new("charlie", 333333, "uxyz"),
    ]);
    // A matured batch is owed 400 of the 500 balance, so only 100 of the 300 recorded can be
    // bonded.
    let batch = Batch {
        id: 1,
        reconciled: false,
        total_shares: Uint128::new(400),
        amount_unclaimed: Uint128::new(400),
        est_unbond_end_time: 100,
    };
    state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(500, "uxyz")]);
    state.unlocked_coins.save(deps.as_mut().storage, &vec![Coin::new(300, "uxyz")]).unwrap();

    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::Reinvest {}),
    )
    .unwrap();
    assert_eq!(res.messages[0], SubMsg::new(Delegation::new("bob", 90, "uxyz").to_cosmos_msg()));
    // The rest stays recorded for a later reinvest.
    assert_eq!(state.unlocked_coins.load(deps.as_ref().storage).unwrap(), vec![Coin::new(
        200, "uxyz"
    )]);
}

#[test]
fn querying_and_reinvesting_idle_funds() {
    let mut deps = setup_test();
    let state = State::default();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 333334, "uxyz"),
        Delegation::new("bob", 333333, "uxyz"),
        Delegation::new("charlie", 333333, "uxyz"),
    ]);
    // Balance 1000: 100 recorded rewards, 200 owed to a matured batch whose funds are here. A
    // batch still unbonding (300) is not paid yet, so its claim is not on this balance: 700 is
    // idle.
    let batch = Batch {
        id: 1,
        reconciled: false,
        total_shares: Uint128::new(300),
        amount_unclaimed: Uint128::new(300),
        est_unbond_end_time: u64::MAX,
    };
    state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    let batch = Batch {
        id: 2,
        reconciled: false,
        total_shares: Uint128::new(200),
        amount_unclaimed: Uint128::new(200),
        est_unbond_end_time: 100,
    };
    state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    // Batch 1's end was reported by the chain, so before it the funds certainly have not arrived.
    crate::state::CHAIN_REPORTED_END.save(deps.as_mut().storage, 1, &true).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(1000, "uxyz")]);
    state.unlocked_coins.save(deps.as_mut().storage, &vec![Coin::new(100, "uxyz")]).unwrap();

    let idle: IdleFundsResponse = query_helper(deps.as_ref(), QueryMsg::IdleFunds {});
    assert_eq!(idle, IdleFundsResponse {
        denom: "uxyz".to_string(),
        balance: Uint128::new(1000),
        unlocked: Uint128::new(100),
        owed_to_batches: Uint128::new(200),
        idle: Uint128::new(700),
    });

    let err =
        execute(deps.as_mut(), mock_env(), mock_info("jake", &[]), ExecuteMsg::ReinvestIdle {
            amount: None,
        })
        .unwrap_err();
    assert_eq!(err, StdError::generic_err("unauthorized: sender is not owner"));

    let err =
        execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::ReinvestIdle {
            amount: Some(Uint128::new(701)),
        })
        .unwrap_err();
    assert_eq!(err, StdError::generic_err("amount exceeds idle funds: 701 > 700"));

    let res =
        execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::ReinvestIdle {
            amount: None,
        })
        .unwrap();
    assert_eq!(res.messages, vec![SubMsg::reply_on_success(
        Delegation::new("bob", 700, "uxyz").to_cosmos_msg(),
        REPLY_REGISTER_RECEIVED_COINS,
    )]);
}

#[test]
fn bonding_prices_in_rewards_not_yet_restaked() {
    let mut deps = setup_test();
    let state = State::default();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 400, "uxyz"),
        Delegation::new("bob", 300, "uxyz"),
        Delegation::new("charlie", 300, "uxyz"),
    ]);
    deps.querier.set_cw20_total_supply("steak_token", 1000);
    // 100 of recorded rewards belong to existing holders: 90 after the 10% fee. Backing is
    // 1000 + 90 = 1090 per 1000 shares, so a 1090 deposit buys 1000 shares, not 1090.
    state.unlocked_coins.save(deps.as_mut().storage, &vec![Coin::new(100, "uxyz")]).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(100 + 1090, "uxyz")]);

    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("user_1", &[Coin::new(1090, "uxyz")]),
        ExecuteMsg::Bond {
            receiver: None,
            exec_msg: None,
        },
    )
    .unwrap();
    let bonded = res.events.iter().find(|e| e.ty == "steakhub/bonded").unwrap();
    let minted = bonded.attributes.iter().find(|a| a.key == "usteak_minted").unwrap();
    assert_eq!(minted.value, "1000");
}

#[test]
fn reconcile_waits_for_every_matured_batch_before_accounting() {
    let mut deps = setup_test();
    let state = State::default();
    let batch_a = Batch {
        id: 1,
        reconciled: false,
        total_shares: Uint128::new(100),
        amount_unclaimed: Uint128::new(100),
        est_unbond_end_time: 10000,
    };
    let batch_b = Batch {
        id: 2,
        reconciled: false,
        total_shares: Uint128::new(100),
        amount_unclaimed: Uint128::new(100),
        est_unbond_end_time: 20000,
    };
    state.previous_batches.save(deps.as_mut().storage, 1, &batch_a).unwrap();
    state.previous_batches.save(deps.as_mut().storage, 2, &batch_b).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(80, "uxyz")]);

    let mut env = mock_env_at_timestamp(15000);
    execute(deps.as_mut(), env.clone(), mock_info("anyone", &[]), ExecuteMsg::Reconcile {})
        .unwrap();

    // B has now matured and paid, but observing B must also hold A back for this entire block.
    deps.querier.set_bank_balances(&[Coin::new(180, "uxyz")]);
    env.block.height += 1;
    env.block.time = cosmwasm_std::Timestamp::from_seconds(20300);
    execute(deps.as_mut(), env.clone(), mock_info("anyone", &[]), ExecuteMsg::Reconcile {})
        .unwrap();
    execute(deps.as_mut(), env.clone(), mock_info("anyone", &[]), ExecuteMsg::Reconcile {})
        .unwrap();
    assert!(!state.previous_batches.load(deps.as_ref().storage, 1).unwrap().reconciled);

    env.block.height += 1;
    execute(deps.as_mut(), env, mock_info("anyone", &[]), ExecuteMsg::Reconcile {}).unwrap();
    assert_eq!(
        state.previous_batches.load(deps.as_ref().storage, 1).unwrap().amount_unclaimed,
        Uint128::new(90)
    );
    assert_eq!(
        state.previous_batches.load(deps.as_ref().storage, 2).unwrap().amount_unclaimed,
        Uint128::new(90)
    );
}

#[test]
fn reconcile_batches_applies_the_exact_capped_deduction() {
    let mut batches = [1u128, 1, 100, 100, 9798].map(|amount| Batch {
        id: amount as u64,
        reconciled: false,
        total_shares: Uint128::new(amount),
        amount_unclaimed: Uint128::new(amount),
        est_unbond_end_time: 1,
    });

    // Pro rata to what each holds (10000 in total), exactly 500 overall. Cumulative rounding,
    // floor(500 * running total / 10000): 0, 0, 5, 10, 500 -> shares 0, 0, 5, 5, 490.
    reconcile_batches(&mut batches, Uint128::new(500));
    assert_eq!(batches.map(|batch| batch.amount_unclaimed), [
        Uint128::new(1),
        Uint128::new(1),
        Uint128::new(95),
        Uint128::new(95),
        Uint128::new(9308),
    ]);

    let mut fully_slashed = [1u128, 1, 100, 100, 298].map(|amount| Batch {
        id: amount as u64,
        reconciled: false,
        total_shares: Uint128::new(amount),
        amount_unclaimed: Uint128::new(amount),
        est_unbond_end_time: 1,
    });
    reconcile_batches(&mut fully_slashed, Uint128::new(500));
    assert!(fully_slashed.iter().all(|batch| batch.amount_unclaimed.is_zero()));
}

#[test]
fn withdrawing_a_fully_written_down_request_clears_it() {
    let mut deps = setup_test();
    let state = State::default();
    state
        .previous_batches
        .save(deps.as_mut().storage, 1, &Batch {
            id: 1,
            reconciled: true,
            total_shares: Uint128::new(100),
            amount_unclaimed: Uint128::zero(),
            est_unbond_end_time: 10000,
        })
        .unwrap();
    let user = Addr::unchecked("user");
    state
        .unbond_requests
        .save(deps.as_mut().storage, (1, &user), &UnbondRequest {
            id: 1,
            user: user.clone(),
            shares: Uint128::new(100),
        })
        .unwrap();

    let response = execute(
        deps.as_mut(),
        mock_env_at_timestamp(20000),
        mock_info(user.as_str(), &[]),
        ExecuteMsg::WithdrawUnbonded {
            receiver: None,
        },
    )
    .unwrap();
    assert!(response.messages.is_empty());
    assert!(state.previous_batches.may_load(deps.as_ref().storage, 1).unwrap().is_none());
    assert!(state.unbond_requests.may_load(deps.as_ref().storage, (1, &user)).unwrap().is_none());
}

#[test]
fn reinvest_after_redelegation_skips_excluded_validators() {
    let mut deps = setup_test();
    let state = State::default();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 100, "uxyz"),
        Delegation::new("bob", 200, "uxyz"),
        Delegation::new("charlie", 300, "uxyz"),
    ]);
    deps.querier.set_bonded_validators(&["alice", "bob", "charlie"]);
    // The redelegations and the harvest recorded 50 of rewards.
    state.unlocked_coins.save(deps.as_mut().storage, &vec![Coin::new(50, "uxyz")]).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(150, "uxyz")]);

    // The harvest passes the exclude list on to the reinvest.
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::HarvestAfterRedelegation {
            exclude: vec!["alice".to_string()],
        }),
    )
    .unwrap();
    assert_eq!(
        res.messages.last().unwrap(),
        &callback_submsg(CallbackMsg::ReinvestIfNeeded {
            exclude: vec!["alice".to_string()],
        })
    );

    // alice has the smallest delegation but was excluded (e.g. she refuses new stake): the 45
    // after fee goes to bob instead.
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::ReinvestIfNeeded {
            exclude: vec!["alice".to_string()],
        }),
    )
    .unwrap();
    assert_eq!(res.messages[0], SubMsg::new(Delegation::new("bob", 45, "uxyz").to_cosmos_msg()));

    // Older callback messages without the field still parse.
    let legacy: CallbackMsg = cosmwasm_std::from_json(br#"{"reinvest_if_needed":{}}"#).unwrap();
    assert_eq!(legacy, CallbackMsg::ReinvestIfNeeded {
        exclude: vec![],
    });
}

#[test]
fn reserve_tracks_reconciles_and_withdrawals() {
    let mut deps = setup_test();
    let state = State::default();
    let batch = Batch {
        id: 1,
        reconciled: false,
        total_shares: Uint128::new(1000),
        amount_unclaimed: Uint128::new(1000),
        est_unbond_end_time: 20000,
    };
    state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    state
        .unbond_requests
        .save(deps.as_mut().storage, (1, &Addr::unchecked("user_1")), &UnbondRequest {
            id: 1,
            user: Addr::unchecked("user_1"),
            shares: Uint128::new(400),
        })
        .unwrap();
    deps.querier.set_bank_balances(&[Coin::new(1000, "uxyz")]);

    // Observed in one block, reconciled in the next.
    let mut env = mock_env_at_timestamp(20001);
    execute(deps.as_mut(), env.clone(), mock_info("anyone", &[]), ExecuteMsg::Reconcile {})
        .unwrap();
    assert_eq!(NATIVE_RESERVED.load(deps.as_ref().storage).unwrap(), Uint128::zero());
    env.block.height += 1;
    execute(deps.as_mut(), env.clone(), mock_info("anyone", &[]), ExecuteMsg::Reconcile {})
        .unwrap();
    assert!(state.previous_batches.load(deps.as_ref().storage, 1u64).unwrap().reconciled);
    assert_eq!(NATIVE_RESERVED.load(deps.as_ref().storage).unwrap(), Uint128::new(1000));

    // A withdrawal releases exactly what it pays out.
    execute(deps.as_mut(), env, mock_info("user_1", &[]), ExecuteMsg::WithdrawUnbonded {
        receiver: None,
    })
    .unwrap();
    assert_eq!(NATIVE_RESERVED.load(deps.as_ref().storage).unwrap(), Uint128::new(600));
}

#[test]
fn migration_shares_an_unattributable_shortfall_pro_rata() {
    let mut deps = setup_test();
    let state = State::default();
    let env = mock_env();
    // An older version reconciled two batches but left part of a shortfall unapplied, and a newer
    // batch matured just before migration. Together they are owed 2700 while the contract holds
    // 2200: which of them the missing 500 belongs to cannot be told, so it is shared pro rata.
    for id in 1..=2u64 {
        let batch = Batch {
            id,
            reconciled: true,
            total_shares: Uint128::new(1000),
            amount_unclaimed: Uint128::new(1000),
            est_unbond_end_time: 10000,
        };
        state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    }
    let batch = Batch {
        id: 3,
        reconciled: false,
        total_shares: Uint128::new(700),
        amount_unclaimed: Uint128::new(700),
        est_unbond_end_time: env.block.time.seconds() - 1,
    };
    state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    let user = Addr::unchecked("legacy_user");
    state
        .unbond_requests
        .save(deps.as_mut().storage, (1, &user), &UnbondRequest {
            id: 1,
            user: user.clone(),
            shares: Uint128::new(100),
        })
        .unwrap();
    deps.querier.set_bank_balances(&[Coin::new(2200, "uxyz")]);
    NATIVE_RESERVED.remove(deps.as_mut().storage);

    // The migration block only records the newly matured batch: its payout may arrive at EndBlock.
    let res = migrate(deps.as_mut(), env.clone(), Default::default()).unwrap();
    assert!(res.attributes.iter().any(|a| a.key == "pool_written_down" && a.value == "pending"));
    assert!(NATIVE_RESERVED.may_load(deps.as_ref().storage).unwrap().is_none());

    // Reconciled funds cannot leave while the reserve is unknown.
    let err = execute(
        deps.as_mut(),
        env.clone(),
        mock_info(user.as_str(), &[]),
        ExecuteMsg::WithdrawUnbonded {
            receiver: None,
        },
    )
    .unwrap_err();
    assert_eq!(
        err,
        StdError::generic_err(
            "native reserve initialization is pending; call Reconcile in a later block"
        )
    );

    // In a later block, one Reconcile finishes the initialisation (a short history fits in one
    // call).
    let mut later = env;
    later.block.height += 1;
    let res =
        execute(deps.as_mut(), later.clone(), mock_info("anyone", &[]), ExecuteMsg::Reconcile {})
            .unwrap();
    assert_eq!(res.events, vec![
        Event::new("steakhub/reserve_initialization")
            .add_attribute("status", "done")
            .add_attribute("native_reserved", "2200")
            .add_attribute("pool_written_down", "500")
            .add_attribute("unlocked", "0")
    ]);

    // 500 shared over 1000 / 1000 / 700: floor(500 * 1000 / 2700) = 185, then 370 - 185 = 185,
    // then 500 - 370 = 130.
    assert_eq!(
        state.previous_batches.load(deps.as_ref().storage, 1u64).unwrap().amount_unclaimed,
        Uint128::new(815)
    );
    assert_eq!(
        state.previous_batches.load(deps.as_ref().storage, 2u64).unwrap().amount_unclaimed,
        Uint128::new(815)
    );
    let batch3 = state.previous_batches.load(deps.as_ref().storage, 3u64).unwrap();
    assert!(batch3.reconciled);
    assert_eq!(batch3.amount_unclaimed, Uint128::new(570));
    assert_eq!(NATIVE_RESERVED.load(deps.as_ref().storage).unwrap(), Uint128::new(2200));

    // Running the migration again changes nothing.
    let res = migrate(deps.as_mut(), later.clone(), Default::default()).unwrap();
    assert!(res.attributes.iter().any(|a| a.key == "pool_written_down" && a.value == "0"));
    assert_eq!(
        state.previous_batches.load(deps.as_ref().storage, 1u64).unwrap().amount_unclaimed,
        Uint128::new(815)
    );

    // Withdrawals work again: 100 of batch 1's 1000 shares get 81 of its 815.
    execute(deps.as_mut(), later, mock_info(user.as_str(), &[]), ExecuteMsg::WithdrawUnbonded {
        receiver: None,
    })
    .unwrap();
    assert_eq!(NATIVE_RESERVED.load(deps.as_ref().storage).unwrap(), Uint128::new(2200 - 81));
}

#[test]
fn reserve_initialization_is_paginated() {
    let mut deps = setup_test();
    let state = State::default();
    // 150 reconciled batches, each owed 10 (1500), against 1200 held: 300 short, 2 each.
    for id in 1..=150u64 {
        let batch = Batch {
            id,
            reconciled: true,
            total_shares: Uint128::new(10),
            amount_unclaimed: Uint128::new(10),
            est_unbond_end_time: 10000,
        };
        state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    }
    deps.querier.set_bank_balances(&[Coin::new(1200, "uxyz")]);
    NATIVE_RESERVED.remove(deps.as_mut().storage);

    // Two pages to sum, two to apply.
    let env = mock_env();
    for step in 0..4 {
        assert!(
            NATIVE_RESERVED.may_load(deps.as_ref().storage).unwrap().is_none(),
            "step {}",
            step
        );
        let res =
            execute(deps.as_mut(), env.clone(), mock_info("anyone", &[]), ExecuteMsg::Reconcile {})
                .unwrap();
        let reserved = res.attributes.iter().find(|a| a.key == "native_reserved").unwrap();
        assert_eq!(
            reserved.value,
            if step < 3 {
                "pending"
            } else {
                "1200"
            },
            "step {}",
            step
        );
    }
    assert_eq!(NATIVE_RESERVED.load(deps.as_ref().storage).unwrap(), Uint128::new(1200));
    for id in [1u64, 100, 101, 150] {
        assert_eq!(
            state.previous_batches.load(deps.as_ref().storage, id).unwrap().amount_unclaimed,
            Uint128::new(8)
        );
    }
}

#[test]
fn withdrawing_from_a_batch_awaiting_reconcile_records_it() {
    let mut deps = setup_test();
    let state = State::default();
    let batch = Batch {
        id: 1,
        reconciled: false,
        total_shares: Uint128::new(100),
        amount_unclaimed: Uint128::new(100),
        est_unbond_end_time: 20000,
    };
    state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    let user = Addr::unchecked("user_1");
    state
        .unbond_requests
        .save(deps.as_mut().storage, (1, &user), &UnbondRequest {
            id: 1,
            user: user.clone(),
            shares: Uint128::new(100),
        })
        .unwrap();
    deps.querier.set_bank_balances(&[Coin::new(100, "uxyz")]);

    // A client bundling Reconcile + WithdrawUnbonded in the first block after maturity: the
    // withdrawal pays nothing yet but succeeds and records the batch, so it is not lost.
    let mut env = mock_env_at_timestamp(20001);
    let res = execute(
        deps.as_mut(),
        env.clone(),
        mock_info(user.as_str(), &[]),
        ExecuteMsg::WithdrawUnbonded {
            receiver: None,
        },
    )
    .unwrap();
    assert!(res.messages.is_empty());
    assert!(res.events.iter().any(|e| e.ty == "steakhub/withdraw_awaiting_reconcile"));

    // Next block: Reconcile settles it (observed a block ago) and the withdrawal pays.
    env.block.height += 1;
    execute(deps.as_mut(), env.clone(), mock_info("anyone", &[]), ExecuteMsg::Reconcile {})
        .unwrap();
    let res =
        execute(deps.as_mut(), env, mock_info(user.as_str(), &[]), ExecuteMsg::WithdrawUnbonded {
            receiver: None,
        })
        .unwrap();
    assert_eq!(res.messages.len(), 1);
}

#[test]
fn withdrawals_fail_closed_on_a_short_reserve_and_keep_dust_claims() {
    let mut deps = setup_test();
    let state = State::default();
    let batch = Batch {
        id: 1,
        reconciled: true,
        total_shares: Uint128::new(1000),
        amount_unclaimed: Uint128::new(100),
        est_unbond_end_time: 10000,
    };
    state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    let whale = Addr::unchecked("whale");
    let dust = Addr::unchecked("dust");
    state
        .unbond_requests
        .save(deps.as_mut().storage, (1, &whale), &UnbondRequest {
            id: 1,
            user: whale.clone(),
            shares: Uint128::new(995),
        })
        .unwrap();
    state
        .unbond_requests
        .save(deps.as_mut().storage, (1, &dust), &UnbondRequest {
            id: 1,
            user: dust.clone(),
            shares: Uint128::new(5),
        })
        .unwrap();
    // The reserve has drifted below what the batch is owed.
    NATIVE_RESERVED.save(deps.as_mut().storage, &Uint128::new(50)).unwrap();

    // 5 of 1000 shares of 100 rounds to nothing while the batch still holds value: keep the claim.
    let err = execute(
        deps.as_mut(),
        mock_env_at_timestamp(20000),
        mock_info(dust.as_str(), &[]),
        ExecuteMsg::WithdrawUnbonded {
            receiver: None,
        },
    )
    .unwrap_err();
    assert_eq!(
        err,
        StdError::generic_err("claim in batch(es) 1 currently rounds to zero; it stays claimable")
    );
    assert!(state.unbond_requests.may_load(deps.as_ref().storage, (1, &dust)).unwrap().is_some());

    // The whale's 99 exceeds the 50 reserve. Paying it would take funds from another accounting
    // bucket, so the withdrawal fails before changing either the claim or its batch.
    let err = execute(
        deps.as_mut(),
        mock_env_at_timestamp(20000),
        mock_info(whale.as_str(), &[]),
        ExecuteMsg::WithdrawUnbonded {
            receiver: None,
        },
    )
    .unwrap_err();
    assert_eq!(
        err,
        StdError::generic_err("native reserve is smaller than the reconciled withdrawal")
    );
    assert_eq!(NATIVE_RESERVED.load(deps.as_ref().storage).unwrap(), Uint128::new(50));
    assert!(state.unbond_requests.may_load(deps.as_ref().storage, (1, &whale)).unwrap().is_some());
    assert_eq!(
        state.previous_batches.load(deps.as_ref().storage, 1).unwrap().amount_unclaimed,
        Uint128::new(100)
    );
}

#[test]
fn a_batch_maturing_during_reserve_initialization_joins_the_pool() {
    let mut deps = setup_test();
    let state = State::default();
    // 120 reconciled batches owed 10 each (1200), fully funded, so summing takes two pages.
    for id in 1..=120u64 {
        let batch = Batch {
            id,
            reconciled: true,
            total_shares: Uint128::new(10),
            amount_unclaimed: Uint128::new(10),
            est_unbond_end_time: 10000,
        };
        state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    }
    // Batch 999 is still unbonding when the initialisation starts.
    let batch = Batch {
        id: 999,
        reconciled: false,
        total_shares: Uint128::new(500),
        amount_unclaimed: Uint128::new(500),
        est_unbond_end_time: 30000,
    };
    state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(1200, "uxyz")]);
    NATIVE_RESERVED.remove(deps.as_mut().storage);

    // First page summed with only the 120 batches in the pool.
    let mut env = mock_env_at_timestamp(20000);
    execute(deps.as_mut(), env.clone(), mock_info("anyone", &[]), ExecuteMsg::Reconcile {})
        .unwrap();
    // Batch 999 matures, slashed: 400 of its 500 arrives. Observed first, holding things a block.
    env.block.time = cosmwasm_std::Timestamp::from_seconds(30001);
    deps.querier.set_bank_balances(&[Coin::new(1600, "uxyz")]);
    execute(deps.as_mut(), env.clone(), mock_info("anyone", &[]), ExecuteMsg::Reconcile {})
        .unwrap();
    // Later blocks: summing finds batch 999 matured since the pool was fixed and restarts with it.
    for _ in 0..10 {
        env.block.height += 1;
        execute(deps.as_mut(), env.clone(), mock_info("anyone", &[]), ExecuteMsg::Reconcile {})
            .unwrap();
        if NATIVE_RESERVED.may_load(deps.as_ref().storage).unwrap().is_some() {
            break;
        }
    }

    // The missing 100 cannot be attributed, so it is shared over 1700: the 120 small batches bear
    // floor(100 * 1200 / 1700) = 70 between them, batch 999 the other 30. It is not charged to the
    // older batches alone as if batch 999 had been paid in full.
    assert_eq!(NATIVE_RESERVED.load(deps.as_ref().storage).unwrap(), Uint128::new(1600));
    let batch999 = state.previous_batches.load(deps.as_ref().storage, 999u64).unwrap();
    assert!(batch999.reconciled);
    assert_eq!(batch999.amount_unclaimed, Uint128::new(470));
    let small: Uint128 = (1..=120u64)
        .map(|id| state.previous_batches.load(deps.as_ref().storage, id).unwrap().amount_unclaimed)
        .sum();
    assert_eq!(small, Uint128::new(1130));
}

#[test]
fn withdrawing_a_batch_observed_earlier_asks_for_reconcile() {
    let mut deps = setup_test();
    let state = State::default();
    let batch = Batch {
        id: 1,
        reconciled: false,
        total_shares: Uint128::new(100),
        amount_unclaimed: Uint128::new(100),
        est_unbond_end_time: 20000,
    };
    state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    let user = Addr::unchecked("user_1");
    state
        .unbond_requests
        .save(deps.as_mut().storage, (1, &user), &UnbondRequest {
            id: 1,
            user: user.clone(),
            shares: Uint128::new(100),
        })
        .unwrap();
    deps.querier.set_bank_balances(&[Coin::new(100, "uxyz")]);

    // Observed by a Reconcile in this block: a withdrawal bundled after it succeeds, pays nothing.
    let mut env = mock_env_at_timestamp(20001);
    execute(deps.as_mut(), env.clone(), mock_info("anyone", &[]), ExecuteMsg::Reconcile {})
        .unwrap();
    let res = execute(
        deps.as_mut(),
        env.clone(),
        mock_info(user.as_str(), &[]),
        ExecuteMsg::WithdrawUnbonded {
            receiver: None,
        },
    )
    .unwrap();
    assert!(res.messages.is_empty());

    // Later blocks with nobody calling Reconcile: no more silent no-op successes.
    env.block.height += 1;
    let err =
        execute(deps.as_mut(), env, mock_info(user.as_str(), &[]), ExecuteMsg::WithdrawUnbonded {
            receiver: None,
        })
        .unwrap_err();
    assert_eq!(
        err,
        StdError::generic_err(
            "batch(es) 1 matured and await Reconcile; call Reconcile, then withdraw"
        )
    );
}

#[test]
fn migration_initializes_the_reserve_in_one_call_for_a_short_history() {
    let mut deps = setup_test();
    // A deployed hub with no batches (or a handful) must not be left with withdrawals blocked
    // until someone sends a separate Reconcile.
    NATIVE_RESERVED.remove(deps.as_mut().storage);
    let res = migrate(deps.as_mut(), mock_env(), Default::default()).unwrap();
    assert!(res.attributes.iter().any(|a| a.key == "pool_written_down" && a.value == "0"));
    assert_eq!(NATIVE_RESERVED.load(deps.as_ref().storage).unwrap(), Uint128::zero());
}

#[test]
fn reconcile_and_withdraw_events_say_what_is_pending() {
    let mut deps = setup_test();
    let state = State::default();
    // Batch 3 was observed in an earlier block; batch 4 matures now.
    for id in [3u64, 4] {
        let batch = Batch {
            id,
            reconciled: false,
            total_shares: Uint128::new(10),
            amount_unclaimed: Uint128::new(10),
            est_unbond_end_time: 20000,
        };
        state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    }
    crate::state::RECONCILE_OBSERVED_HEIGHTS.save(deps.as_mut().storage, 3, &1).unwrap();
    let res = execute(
        deps.as_mut(),
        mock_env_at_timestamp(30000),
        mock_info("anyone", &[]),
        ExecuteMsg::Reconcile {},
    )
    .unwrap();
    let observed = res.events.iter().find(|e| e.ty == "steakhub/reconcile_observed").unwrap();
    let attr = |key: &str| {
        observed.attributes.iter().find(|a| a.key == key).map(|a| a.value.clone()).unwrap()
    };
    assert_eq!(attr("ids"), "4");
    assert_eq!(attr("deferred_ids"), "3");

    // A withdrawal that pays one claim but leaves a dust claim open says so.
    for (id, amount, shares) in [(1u64, 100u128, 1000u128), (2, 100, 100)] {
        let batch = Batch {
            id,
            reconciled: true,
            total_shares: Uint128::new(shares),
            amount_unclaimed: Uint128::new(amount),
            est_unbond_end_time: 10000,
        };
        state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    }
    NATIVE_RESERVED.save(deps.as_mut().storage, &Uint128::new(200)).unwrap();
    let user = Addr::unchecked("user_1");
    let whale = Addr::unchecked("whale");
    state
        .unbond_requests
        .save(deps.as_mut().storage, (1, &user), &UnbondRequest {
            id: 1,
            user: user.clone(),
            shares: Uint128::new(5),
        })
        .unwrap();
    state
        .unbond_requests
        .save(deps.as_mut().storage, (1, &whale), &UnbondRequest {
            id: 1,
            user: whale.clone(),
            shares: Uint128::new(995),
        })
        .unwrap();
    state
        .unbond_requests
        .save(deps.as_mut().storage, (2, &user), &UnbondRequest {
            id: 2,
            user: user.clone(),
            shares: Uint128::new(100),
        })
        .unwrap();
    let res = execute(
        deps.as_mut(),
        mock_env_at_timestamp(30000),
        mock_info(user.as_str(), &[]),
        ExecuteMsg::WithdrawUnbonded {
            receiver: None,
        },
    )
    .unwrap();
    let withdrawn = res.events.iter().find(|e| e.ty == "steakhub/unbonded_withdrawn").unwrap();
    assert!(withdrawn.attributes.iter().any(|a| a.key == "ids" && a.value == "2"));
    assert!(withdrawn.attributes.iter().any(|a| a.key == "dust_ids" && a.value == "1"));
}

#[test]
fn a_pending_migration_says_what_to_do_next() {
    let mut deps = setup_test();
    let state = State::default();
    let batch = Batch {
        id: 1,
        reconciled: false,
        total_shares: Uint128::new(10),
        amount_unclaimed: Uint128::new(10),
        est_unbond_end_time: mock_env().block.time.seconds() - 1,
    };
    state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    NATIVE_RESERVED.remove(deps.as_mut().storage);
    let res = migrate(deps.as_mut(), mock_env(), Default::default()).unwrap();
    let next = res.attributes.iter().find(|a| a.key == "next_step").unwrap();
    assert!(next.value.starts_with("send Reconcile in later blocks"), "{}", next.value);
}

#[test]
fn reinvest_if_needed_skips_rewards_that_cannot_be_bonded_yet() {
    let mut deps = setup_test();
    let state = State::default();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 100, "uxyz"),
        Delegation::new("bob", 200, "uxyz"),
    ]);
    // 200 recorded, but a matured batch awaiting Reconcile is owed the whole 400 balance.
    state.unlocked_coins.save(deps.as_mut().storage, &vec![Coin::new(200, "uxyz")]).unwrap();
    let batch = Batch {
        id: 1,
        reconciled: false,
        total_shares: Uint128::new(400),
        amount_unclaimed: Uint128::new(400),
        est_unbond_end_time: 100,
    };
    state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(400, "uxyz")]);

    // Reinvest itself refuses; the callback after RedelegateInactive skips instead of reverting
    // the redelegations.
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::ReinvestIfNeeded {
            exclude: vec![],
        }),
    )
    .unwrap();
    assert!(res.messages.is_empty());
    assert_eq!(res.attributes, vec![cosmwasm_std::Attribute::new(
        "action",
        "steakhub/reinvest_not_needed"
    )]);

    // While the reserve initialisation is pending, what batches are owed is unknown: nothing is
    // bondable and nothing counts as idle.
    NATIVE_RESERVED.remove(deps.as_mut().storage);
    let idle: IdleFundsResponse = query_helper(deps.as_ref(), QueryMsg::IdleFunds {});
    assert_eq!(idle.idle, Uint128::zero());
}

#[test]
fn idle_funds_do_not_count_in_bond_pricing() {
    let mut deps = setup_test();
    let state = State::default();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 400, "uxyz"),
        Delegation::new("bob", 300, "uxyz"),
        Delegation::new("charlie", 300, "uxyz"),
    ]);
    deps.querier.set_cw20_total_supply("steak_token", 1000);
    // 100 idle (not recorded, not owed). Anyone could raise idle with a plain transfer, so it is
    // not part of the backing bonds are priced against: 1000 per 1000 shares.
    deps.querier.set_bank_balances(&[Coin::new(100 + 1100, "uxyz")]);
    state.unlocked_coins.save(deps.as_mut().storage, &vec![]).unwrap();

    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("user_1", &[Coin::new(1100, "uxyz")]),
        ExecuteMsg::Bond {
            receiver: None,
            exec_msg: None,
        },
    )
    .unwrap();
    let bonded = res.events.iter().find(|e| e.ty == "steakhub/bonded").unwrap();
    let minted = bonded.attributes.iter().find(|a| a.key == "usteak_minted").unwrap();
    assert_eq!(minted.value, "1100");
}

#[test]
fn reinvest_idle_locks_shares_instead_of_raising_the_exchange_rate() {
    let mut deps = setup_test();
    let state = State::default();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 800, "uxyz"),
        Delegation::new("bob", 600, "uxyz"),
        Delegation::new("charlie", 600, "uxyz"),
    ]);
    deps.querier.set_cw20_total_supply("steak_token", 2000);
    deps.querier.set_bank_balances(&[Coin::new(2000, "uxyz")]);
    state.unlocked_coins.save(deps.as_mut().storage, &vec![]).unwrap();

    let res =
        execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::ReinvestIdle {
            amount: None,
        })
        .unwrap();

    assert_eq!(res.messages[0].msg, Delegation::new("bob", 2000, "uxyz").to_cosmos_msg());
    assert_eq!(
        res.messages[1],
        SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: "steak_token".to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Mint {
                recipient: MOCK_CONTRACT_ADDR.to_string(),
                amount: Uint128::new(2000),
            })
            .unwrap(),
            funds: vec![],
        }))
    );
    assert_eq!(DEAD_SHARES.load(deps.as_ref().storage).unwrap(), Uint128::new(2000));
    let event = res.events.iter().find(|e| e.ty == "steakhub/reinvested_idle").unwrap();
    assert!(event.attributes.iter().any(|a| a.key == "dead_shares" && a.value == "2000"));

    // Division rounds up: a sweep may never leave a one-unit exchange-rate increase to capture.
    assert_eq!(
        compute_idle_sweep_shares(Uint128::new(3), Uint128::new(1), Uint128::new(2)).unwrap(),
        Uint128::new(2)
    );
}

#[test]
fn reinvest_idle_only_stakes_to_bonded_active_validators() {
    let mut deps = setup_test();
    let state = State::default();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 300, "uxyz"),
        Delegation::new("bob", 100, "uxyz"),
        Delegation::new("charlie", 200, "uxyz"),
    ]);
    // bob has the smallest delegation but is jailed.
    deps.querier.set_bonded_validators(&["alice", "charlie"]);
    deps.querier.set_bank_balances(&[Coin::new(50, "uxyz")]);
    state.unlocked_coins.save(deps.as_mut().storage, &vec![]).unwrap();

    let res =
        execute(deps.as_mut(), mock_env(), mock_info("larry", &[]), ExecuteMsg::ReinvestIdle {
            amount: None,
        })
        .unwrap();
    assert_eq!(res.messages[0].msg, Delegation::new("charlie", 50, "uxyz").to_cosmos_msg());
}

#[test]
fn first_bond_does_not_lock_stray_native_before_it_is_swept() {
    let mut deps = setup_test();
    let state = State::default();
    deps.querier.set_staking_delegations(&[]);
    deps.querier.set_cw20_total_supply("steak_token", 0);
    // 5000 was sent directly to the hub before this 2000 deposit arrived.
    deps.querier.set_bank_balances(&[Coin::new(7000, "uxyz")]);
    state.unlocked_coins.save(deps.as_mut().storage, &vec![]).unwrap();

    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("user_1", &[Coin::new(2000, "uxyz")]),
        ExecuteMsg::Bond {
            receiver: None,
            exec_msg: None,
        },
    )
    .unwrap();

    let bonded = res.events.iter().find(|event| event.ty == "steakhub/bonded").unwrap();
    let attr = |key: &str| {
        bonded.attributes.iter().find(|attr| attr.key == key).map(|attr| attr.value.as_str())
    };
    // The idle 5000 is not part of the first mint. It will receive proportional dead shares only
    // if ReinvestIdle delegates it, preventing the same backing from being locked twice.
    assert_eq!(attr("usteak_minted"), Some("1000"));
    assert_eq!(attr("dead_shares"), Some("1000"));
    assert_eq!(DEAD_SHARES.load(deps.as_ref().storage).unwrap(), Uint128::new(1000));
}

#[test]
fn bonds_while_the_reserve_is_pending_still_price_in_recorded_rewards() {
    let mut deps = setup_test();
    let state = State::default();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 400, "uxyz"),
        Delegation::new("bob", 300, "uxyz"),
        Delegation::new("charlie", 300, "uxyz"),
    ]);
    deps.querier.set_cw20_total_supply("steak_token", 1000);
    state.unlocked_coins.save(deps.as_mut().storage, &vec![Coin::new(100, "uxyz")]).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(100 + 1090, "uxyz")]);
    NATIVE_RESERVED.remove(deps.as_mut().storage);

    // Nothing is bondable while the reserve is pending, but the 100 recorded (90 after fee) still
    // belongs to existing holders: 1090 buys 1000 shares, as it would afterwards.
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("user_1", &[Coin::new(1090, "uxyz")]),
        ExecuteMsg::Bond {
            receiver: None,
            exec_msg: None,
        },
    )
    .unwrap();
    let bonded = res.events.iter().find(|e| e.ty == "steakhub/bonded").unwrap();
    let minted = bonded.attributes.iter().find(|a| a.key == "usteak_minted").unwrap();
    assert_eq!(minted.value, "1000");
}

#[test]
fn an_unparsable_coin_received_event_is_skipped_not_fatal() {
    let mut deps = setup_test();
    let state = State::default();
    state.unlocked_coins.save(deps.as_mut().storage, &vec![]).unwrap();

    let res = reply(deps.as_mut(), mock_env(), Reply {
        id: REPLY_REGISTER_RECEIVED_COINS,
        result: cosmwasm_std::SubMsgResult::Ok(SubMsgResponse {
            events: vec![
                Event::new("coin_received").add_attribute("receiver", MOCK_CONTRACT_ADDR),
                Event::new("coin_received")
                    .add_attribute("receiver", MOCK_CONTRACT_ADDR)
                    .add_attribute("amount", "12uxyz"),
            ],
            data: None,
        }),
    })
    .unwrap();
    assert!(res.attributes.iter().any(|a| a.key == "unparsed_coin_received" && a.value == "1"));
    assert_eq!(state.unlocked_coins.load(deps.as_ref().storage).unwrap(), vec![Coin::new(
        12, "uxyz"
    )]);
}

#[test]
fn a_full_fee_reinvest_sends_the_fee_without_a_zero_delegation() {
    let mut deps = setup_test();
    let state = State::default();
    state.fee_rate.save(deps.as_mut().storage, &Decimal::one()).unwrap();
    state.unlocked_coins.save(deps.as_mut().storage, &vec![Coin::new(300, "uxyz")]).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(300, "uxyz")]);

    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::Reinvest {}),
    )
    .unwrap();

    assert_eq!(res.messages, vec![SubMsg::new(CosmosMsg::Bank(BankMsg::Send {
        to_address: "the_fee_man".into(),
        amount: vec![Coin::new(300, "uxyz")],
    }))]);
    assert!(state.unlocked_coins.load(deps.as_ref().storage).unwrap().is_empty());
    let harvested = res.events.iter().find(|event| event.ty == "steakhub/harvested").unwrap();
    assert!(
        harvested.attributes.iter().any(|attr| { attr.key == "denom_bonded" && attr.value == "0" })
    );
}

#[test]
fn bonding_rejects_a_deposit_that_would_mint_zero_shares() {
    let mut deps = setup_test();
    let state = State::default();
    deps.querier.set_staking_delegations(&[Delegation::new("alice", 1, "uxyz")]);
    deps.querier.set_cw20_total_supply("steak_token", 1);
    // The current holder has one delegated unit and two recorded reward units (the 10% fee on 2
    // rounds to 0), so the backing is 3. A one-unit deposit would mint floor(1 * 1 / 3) = 0 and
    // must not be accepted. (Idle native no longer counts in pricing, so recorded rewards are
    // used to reach a zero mint.)
    deps.querier.set_bank_balances(&[Coin::new(2 + 1, "uxyz")]);
    state.unlocked_coins.save(deps.as_mut().storage, &vec![Coin::new(2, "uxyz")]).unwrap();

    let err = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("user_2", &[Coin::new(1, "uxyz")]),
        ExecuteMsg::Bond {
            receiver: None,
            exec_msg: None,
        },
    )
    .unwrap_err();

    assert_eq!(err, StdError::generic_err("bond amount is too small to mint derivative tokens"));
}

#[test]
fn first_bond_after_zero_supply_locks_the_leftover_backing_in_dead_shares() {
    let mut deps = setup_test();
    let state = State::default();
    // Every holder unbonded, but 5000 stays delegated (donations, rounding).
    deps.querier.set_staking_delegations(&[Delegation::new("alice", 5000, "uxyz")]);
    deps.querier.set_cw20_total_supply("steak_token", 0);
    deps.querier.set_bank_balances(&[Coin::new(1000, "uxyz")]);
    state.unlocked_coins.save(deps.as_mut().storage, &vec![]).unwrap();

    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("user_1", &[Coin::new(1000, "uxyz")]),
        ExecuteMsg::Bond {
            receiver: None,
            exec_msg: None,
        },
    )
    .unwrap();
    // The bonder gets 1000 shares for 1000; the 5000 left over is locked behind 5000 dead shares,
    // so redeeming 1000 of 6000 shares returns 1000 of 6000, not the leftover too.
    let bonded = res.events.iter().find(|e| e.ty == "steakhub/bonded").unwrap();
    let attr = |key: &str| {
        bonded.attributes.iter().find(|a| a.key == key).map(|a| a.value.clone()).unwrap()
    };
    assert_eq!(attr("usteak_minted"), "1000");
    assert_eq!(attr("dead_shares"), "5000");
    assert_eq!(DEAD_SHARES.load(deps.as_ref().storage).unwrap(), Uint128::new(5000));
    // The bonder's 1000, plus 5000 dead shares minted to the hub itself.
    assert_eq!(
        res.messages.last().unwrap().msg,
        CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: "steak_token".to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Mint {
                recipient: MOCK_CONTRACT_ADDR.to_string(),
                amount: Uint128::new(5000),
            })
            .unwrap(),
            funds: vec![],
        })
    );
}

#[test]
fn computing_the_first_mint() {
    use crate::math::{MIN_DEAD_SHARES, compute_first_mint};
    // Nothing left: the minimum is locked, and its unbacked part comes out of the bonder's mint, so
    // 2000 shares (1000 + 1000 dead) stand for exactly the 2000 deposited.
    assert_eq!(
        compute_first_mint(Uint128::new(2000), Uint128::zero()).unwrap(),
        (Uint128::new(2000 - MIN_DEAD_SHARES), Uint128::new(MIN_DEAD_SHARES))
    );
    // A deposit that cannot cover the unbacked minimum is refused.
    assert_eq!(
        compute_first_mint(Uint128::new(1000), Uint128::zero()).unwrap_err(),
        StdError::generic_err("the first bond into an empty hub must exceed 1000 base units")
    );
    // Leftover backing above the minimum: locked 1:1, the bonder pays nothing for it.
    assert_eq!(
        compute_first_mint(Uint128::new(1000), Uint128::new(5000)).unwrap(),
        (Uint128::new(1000), Uint128::new(5000))
    );
    // Partly covered: 400 left, so 600 of the minimum is unbacked.
    assert_eq!(
        compute_first_mint(Uint128::new(1000), Uint128::new(400)).unwrap(),
        (Uint128::new(400), Uint128::new(1000))
    );
}

#[test]
fn a_batch_without_a_chain_reported_end_is_never_idle() {
    let mut deps = setup_test();
    let state = State::default();
    // A batch submitted before this version: its end is only the configured estimate, so its
    // 1000 may already be here (the chain unbonds faster than configured).
    let batch = Batch {
        id: 1,
        reconciled: false,
        total_shares: Uint128::new(1000),
        amount_unclaimed: Uint128::new(1000),
        est_unbond_end_time: u64::MAX,
    };
    state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(1000, "uxyz")]);
    state.unlocked_coins.save(deps.as_mut().storage, &vec![]).unwrap();

    let idle: IdleFundsResponse = query_helper(deps.as_ref(), QueryMsg::IdleFunds {});
    assert_eq!(idle.owed_to_batches, Uint128::new(1000));
    assert_eq!(idle.idle, Uint128::zero());
}

#[test]
fn recorded_rewards_count_in_bond_pricing_while_batches_are_owed_the_balance() {
    let mut deps = setup_test();
    let state = State::default();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 400, "uxyz"),
        Delegation::new("bob", 300, "uxyz"),
        Delegation::new("charlie", 300, "uxyz"),
    ]);
    deps.querier.set_cw20_total_supply("steak_token", 1000);
    // 100 recorded rewards in a balance a matured, unpaid batch is owed more than.
    let batch = Batch {
        id: 1,
        reconciled: false,
        total_shares: Uint128::new(5000),
        amount_unclaimed: Uint128::new(5000),
        est_unbond_end_time: 100,
    };
    state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    state.unlocked_coins.save(deps.as_mut().storage, &vec![Coin::new(100, "uxyz")]).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(100 + 1090, "uxyz")]);

    // They still belong to existing holders: 1090 buys 1000 shares (backing 1000 + 90 after fee).
    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("user_1", &[Coin::new(1090, "uxyz")]),
        ExecuteMsg::Bond {
            receiver: None,
            exec_msg: None,
        },
    )
    .unwrap();
    let bonded = res.events.iter().find(|e| e.ty == "steakhub/bonded").unwrap();
    let minted = bonded.attributes.iter().find(|a| a.key == "usteak_minted").unwrap();
    assert_eq!(minted.value, "1000");
}

#[test]
fn an_in_flight_legacy_batch_does_not_stop_harvest_compounding() {
    let mut deps = setup_test();
    let state = State::default();
    deps.querier.set_staking_delegations(&[
        Delegation::new("alice", 100, "uxyz"),
        Delegation::new("bob", 200, "uxyz"),
    ]);
    // charlie (bonded, no delegation yet) is the smallest. A batch submitted before the upgrade,
    // still unbonding (no chain-reported end): its 5000 is not here yet. Recorded rewards (50)
    // are, and can be bonded.
    let batch = Batch {
        id: 1,
        reconciled: false,
        total_shares: Uint128::new(5000),
        amount_unclaimed: Uint128::new(5000),
        est_unbond_end_time: u64::MAX,
    };
    state.previous_batches.save(deps.as_mut().storage, batch.id, &batch).unwrap();
    state.unlocked_coins.save(deps.as_mut().storage, &vec![Coin::new(50, "uxyz")]).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(50, "uxyz")]);

    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info(MOCK_CONTRACT_ADDR, &[]),
        ExecuteMsg::Callback(CallbackMsg::ReinvestIfNeeded {
            exclude: vec![],
        }),
    )
    .unwrap();
    assert_eq!(res.messages[0].msg, Delegation::new("charlie", 45, "uxyz").to_cosmos_msg());
    // ...while for sweeping untracked native the same batch still counts as possibly paid.
    let idle: IdleFundsResponse = query_helper(deps.as_ref(), QueryMsg::IdleFunds {});
    assert_eq!(idle.owed_to_batches, Uint128::new(5000));
}

#[test]
fn worthless_dead_shares_are_burned_so_bonding_can_restart() {
    let mut deps = setup_test();
    let state = State::default();
    // Everyone left; the dead shares' stake was then slashed away: supply is dead shares only and
    // nothing tracked backs them.
    deps.querier.set_staking_delegations(&[]);
    deps.querier.set_cw20_total_supply("steak_token", 1000);
    DEAD_SHARES.save(deps.as_mut().storage, &Uint128::new(1000)).unwrap();
    deps.querier.set_bank_balances(&[Coin::new(2000, "uxyz")]);
    state.unlocked_coins.save(deps.as_mut().storage, &vec![]).unwrap();

    let res = execute(
        deps.as_mut(),
        mock_env(),
        mock_info("user_1", &[Coin::new(2000, "uxyz")]),
        ExecuteMsg::Bond {
            receiver: None,
            exec_msg: None,
        },
    )
    .unwrap();
    let bonded = res.events.iter().find(|e| e.ty == "steakhub/bonded").unwrap();
    let attr = |key: &str| {
        bonded.attributes.iter().find(|a| a.key == key).map(|a| a.value.clone()).unwrap()
    };
    assert_eq!(attr("usteak_minted"), "1000");
    assert_eq!(attr("dead_shares"), "1000");
    assert_eq!(DEAD_SHARES.load(deps.as_ref().storage).unwrap(), Uint128::new(1000));
    // The 1000 worthless dead shares are burned before minting.
    assert_eq!(
        res.messages[1].msg,
        CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: "steak_token".to_string(),
            msg: to_json_binary(&Cw20ExecuteMsg::Burn {
                amount: Uint128::new(1000),
            })
            .unwrap(),
            funds: vec![],
        })
    );
}

#[test]
fn a_partly_unparsable_coin_received_amount_still_records_the_native() {
    let mut deps = setup_test();
    let state = State::default();
    state.unlocked_coins.save(deps.as_mut().storage, &vec![]).unwrap();

    reply(deps.as_mut(), mock_env(), Reply {
        id: REPLY_REGISTER_RECEIVED_COINS,
        result: cosmwasm_std::SubMsgResult::Ok(SubMsgResponse {
            events: vec![
                Event::new("coin_received")
                    .add_attribute("receiver", MOCK_CONTRACT_ADDR)
                    .add_attribute("amount", "12uxyz,3"),
            ],
            data: None,
        }),
    })
    .unwrap();
    assert_eq!(state.unlocked_coins.load(deps.as_ref().storage).unwrap(), vec![Coin::new(
        12, "uxyz"
    )]);
}
