use crate::{
    error::{ContractError, ContractResult},
    helper::{convert_pool_id_to_v3_pool_key, denom_to_asset_info, parse_to_swap_msg},
    state::{ENTRY_POINT_CONTRACT_ADDRESS, ORAIDEX_ROUTER_ADDRESS},
};
use cosmwasm_std::{
    entry_point, from_json, to_json_binary, Binary, Decimal, Deps, DepsMut, Env, MessageInfo,
    Response, Uint128, WasmMsg,
};
use cw2::set_contract_version;
use cw20::{Cw20Coin, Cw20ReceiveMsg};
use cw_utils::one_coin;

use oraiswap::mixed_router::{
    QueryMsg as OraidexQueryMsg, SimulateSwapOperationsResponse,
    SwapOperation as OraidexSwapOperation,
};
use skip::{
    asset::{get_current_asset_available, Asset},
    swap::{
        execute_transfer_funds_back, get_ask_denom_for_routes, Cw20HookMsg, ExecuteMsg, MigrateMsg,
        OraidexInstantiateMsg, QueryMsg, Route, SimulateSmartSwapExactAssetInResponse,
        SimulateSwapExactAssetInResponse, SimulateSwapExactAssetOutResponse, SwapOperation,
    },
};

///////////////
/// MIGRATE ///
///////////////

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(_deps: DepsMut, _env: Env, _msg: MigrateMsg) -> ContractResult<Response> {
    Ok(Response::default())
}

///////////////////
/// INSTANTIATE ///
///////////////////

// Contract name and version used for migration.
const CONTRACT_NAME: &str = env!("CARGO_PKG_NAME");
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: OraidexInstantiateMsg,
) -> ContractResult<Response> {
    // Set contract version
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    // Validate entry point contract address
    let checked_entry_point_contract_address =
        deps.api.addr_validate(&msg.entry_point_contract_address)?;

    let oraidex_router_contract_address = deps
        .api
        .addr_validate(&msg.oraidex_router_contract_address)?;

    // Store the entry point contract address
    ENTRY_POINT_CONTRACT_ADDRESS.save(deps.storage, &checked_entry_point_contract_address)?;
    ORAIDEX_ROUTER_ADDRESS.save(deps.storage, &oraidex_router_contract_address)?;

    Ok(Response::new()
        .add_attribute("action", "instantiate")
        .add_attribute(
            "entry_point_contract_address",
            checked_entry_point_contract_address.to_string(),
        ))
}

///////////////
/// RECEIVE ///
///////////////

// Receive is the main entry point for the contract to
// receive cw20 tokens and execute the swap
pub fn receive_cw20(
    deps: DepsMut,
    env: Env,
    mut info: MessageInfo,
    cw20_msg: Cw20ReceiveMsg,
) -> ContractResult<Response> {
    let sent_asset = Asset::Cw20(Cw20Coin {
        address: info.sender.to_string(),
        amount: cw20_msg.amount,
    });
    sent_asset.validate(&deps, &env, &info)?;

    // Set the sender to the originating address that triggered the cw20 send call
    // This is later validated / enforced to be the entry point contract address
    info.sender = deps.api.addr_validate(&cw20_msg.sender)?;

    match from_json(&cw20_msg.msg)? {
        Cw20HookMsg::Swap { operations } => execute_swap(deps, env, info, operations),
    }
}

///////////////
/// EXECUTE ///
///////////////

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> ContractResult<Response> {
    match msg {
        ExecuteMsg::Receive(cw20_msg) => receive_cw20(deps, env, info, cw20_msg),
        ExecuteMsg::Swap { operations } => {
            let coin = one_coin(&info)?;

            // validate that there's at least one swap operation
            if operations.is_empty() {
                return Err(ContractError::SwapOperationsEmpty);
            }

            // validate that the one coin is the same as the first swap operation's denom in
            if coin.denom != operations.first().unwrap().denom_in {
                return Err(ContractError::CoinInDenomMismatch);
            }

            execute_swap(deps, env, info, operations)
        }
        ExecuteMsg::TransferFundsBack {
            swapper,
            return_denom,
        } => Ok(execute_transfer_funds_back(
            deps,
            env,
            info,
            swapper,
            return_denom,
        )?),
        ExecuteMsg::OraidexPoolSwap { operation } => {
            execute_oraidex_pool_swap(deps, env, info, operation)
        }
        _ => {
            unimplemented!()
        }
    }
}

fn execute_swap(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    operations: Vec<SwapOperation>,
) -> ContractResult<Response> {
    // Get entry point contract address from storage
    let entry_point_contract_address = ENTRY_POINT_CONTRACT_ADDRESS.load(deps.storage)?;

    // Enforce the caller is the entry point contract
    if info.sender != entry_point_contract_address {
        return Err(ContractError::Unauthorized);
    }

    // Create a response object to return
    let mut response: Response = Response::new().add_attribute("action", "execute_swap");

    // Add an oraidex pool swap message to the response for each swap operation
    for operation in &operations {
        let swap_msg = WasmMsg::Execute {
            contract_addr: env.contract.address.to_string(),
            msg: to_json_binary(&ExecuteMsg::OraidexPoolSwap {
                operation: operation.clone(),
            })?,
            funds: vec![],
        };
        response = response.add_message(swap_msg);
    }

    let return_denom = match operations.last() {
        Some(last_op) => last_op.denom_out.clone(),
        None => return Err(ContractError::SwapOperationsEmpty),
    };

    // Create the transfer funds back message
    let transfer_funds_back_msg = WasmMsg::Execute {
        contract_addr: env.contract.address.to_string(),
        msg: to_json_binary(&ExecuteMsg::TransferFundsBack {
            swapper: entry_point_contract_address,
            return_denom,
        })?,
        funds: vec![],
    };

    Ok(response
        .add_message(transfer_funds_back_msg)
        .add_attribute("action", "dispatch_swaps_and_transfer_back"))
}

fn execute_oraidex_pool_swap(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    operation: SwapOperation,
) -> ContractResult<Response> {
    // Ensure the caller is the contract itself
    if info.sender != env.contract.address {
        return Err(ContractError::Unauthorized);
    }

    // Get the current asset available on contract to swap in
    let offer_asset = get_current_asset_available(&deps, &env, &operation.denom_in)?;
    // Error if the offer asset amount is zero
    if offer_asset.amount().is_zero() {
        return Err(ContractError::NoOfferAssetAmount);
    }

    // Create the oraidex pool swap msg depending on the offer asset type
    let (contract_addr, msg) = parse_to_swap_msg(&deps.as_ref(), operation)?;

    // Create the wasm oraidex pool swap message
    let swap_msg = offer_asset.into_wasm_msg(contract_addr.to_string(), msg)?;

    Ok(Response::new()
        .add_message(swap_msg)
        .add_attribute("action", "dispatch_oraidex_pool_swap"))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> ContractResult<Binary> {
    match msg {
        QueryMsg::SimulateSwapExactAssetIn {
            asset_in,
            swap_operations,
        } => to_json_binary(&query_simulate_swap_exact_asset_in(
            deps,
            asset_in,
            swap_operations,
        )?),
        QueryMsg::SimulateSwapExactAssetOut {
            asset_out,
            swap_operations,
        } => to_json_binary(&query_simulate_swap_exact_asset_out(
            deps,
            asset_out,
            swap_operations,
        )?),
        QueryMsg::SimulateSwapExactAssetInWithMetadata {
            asset_in,
            swap_operations,
            include_spot_price,
        } => to_json_binary(&query_simulate_swap_exact_asset_in_with_metadata(
            deps,
            asset_in,
            swap_operations,
            include_spot_price,
        )?),
        QueryMsg::SimulateSwapExactAssetOutWithMetadata {
            asset_out,
            swap_operations,
            include_spot_price,
        } => to_json_binary(&query_simulate_swap_exact_asset_out_with_metadata(
            deps,
            asset_out,
            swap_operations,
            include_spot_price,
        )?),
        QueryMsg::SimulateSmartSwapExactAssetIn { routes, .. } => {
            let ask_denom = get_ask_denom_for_routes(&routes)?;

            to_json_binary(&query_simulate_smart_swap_exact_asset_in(
                deps, ask_denom, routes,
            )?)
        }
        QueryMsg::SimulateSmartSwapExactAssetInWithMetadata {
            routes,
            asset_in,
            include_spot_price,
        } => {
            let ask_denom = get_ask_denom_for_routes(&routes)?;

            to_json_binary(&query_simulate_smart_swap_exact_asset_in_with_metadata(
                deps,
                asset_in,
                ask_denom,
                routes,
                include_spot_price,
            )?)
        }
    }
    .map_err(From::from)
}

// Queries the dexter pool contracts to simulate a swap exact amount in
fn query_simulate_swap_exact_asset_in(
    deps: Deps,
    asset_in: Asset,
    swap_operations: Vec<SwapOperation>,
) -> ContractResult<Asset> {
    // Error if swap operations is empty
    let Some(first_op) = swap_operations.first() else {
        return Err(ContractError::SwapOperationsEmpty);
    };

    // Ensure asset_in's denom is the same as the first swap operation's denom in
    if asset_in.denom() != first_op.denom_in {
        return Err(ContractError::CoinInDenomMismatch);
    }

    let asset_out = simulate_swap_exact_asset_in(deps, asset_in, swap_operations)?;

    // Return the asset out
    Ok(asset_out)
}

// Queries the dexter pool contracts to simulate a multi-hop swap exact amount out
fn query_simulate_swap_exact_asset_out(
    deps: Deps,
    asset_out: Asset,
    swap_operations: Vec<SwapOperation>,
) -> ContractResult<Asset> {
    // Error if swap operations is empty
    let Some(last_op) = swap_operations.last() else {
        return Err(ContractError::SwapOperationsEmpty);
    };

    // Ensure asset_out's denom is the same as the last swap operation's denom out
    if asset_out.denom() != last_op.denom_out {
        return Err(ContractError::CoinOutDenomMismatch);
    }

    let asset_in = simulate_swap_exact_asset_out(deps, asset_out, swap_operations)?;

    // Return the asset in needed
    Ok(asset_in)
}

fn query_simulate_smart_swap_exact_asset_in(
    deps: Deps,
    ask_denom: String,
    routes: Vec<Route>,
) -> ContractResult<Asset> {
    simulate_smart_swap_exact_asset_in(deps, ask_denom, routes)
}

// Queries the dexter pool contracts to simulate a swap exact amount in with metadata
fn query_simulate_swap_exact_asset_in_with_metadata(
    deps: Deps,
    asset_in: Asset,
    swap_operations: Vec<SwapOperation>,
    include_spot_price: bool,
) -> ContractResult<SimulateSwapExactAssetInResponse> {
    // Error if swap operations is empty
    let Some(first_op) = swap_operations.first() else {
        return Err(ContractError::SwapOperationsEmpty);
    };

    // Ensure asset_in's denom is the same as the first swap operation's denom in
    if asset_in.denom() != first_op.denom_in {
        return Err(ContractError::CoinInDenomMismatch);
    }

    // Simulate the swap exact amount in
    let asset_out = simulate_swap_exact_asset_in(deps, asset_in.clone(), swap_operations.clone())?;

    // Create the response
    let mut response = SimulateSwapExactAssetInResponse {
        asset_out,
        spot_price: None,
    };

    // Include the spot price in the response if requested
    if include_spot_price {
        let spot_price = calculate_spot_price(deps, swap_operations)?;
        response.spot_price = Some(spot_price);
    }

    Ok(response)
}

// Queries the dexter pool contracts to simulate a multi-hop swap exact amount out with metadata
fn query_simulate_swap_exact_asset_out_with_metadata(
    deps: Deps,
    asset_out: Asset,
    swap_operations: Vec<SwapOperation>,
    include_spot_price: bool,
) -> ContractResult<SimulateSwapExactAssetOutResponse> {
    // Error if swap operations is empty
    let Some(last_op) = swap_operations.last() else {
        return Err(ContractError::SwapOperationsEmpty);
    };

    // Ensure asset_out's denom is the same as the last swap operation's denom out
    if asset_out.denom() != last_op.denom_out {
        return Err(ContractError::CoinOutDenomMismatch);
    }

    // Simulate the swap exact amount out
    let asset_in = simulate_swap_exact_asset_out(deps, asset_out.clone(), swap_operations.clone())?;

    // Create the response
    let mut response = SimulateSwapExactAssetOutResponse {
        asset_in,
        spot_price: None,
    };

    // Include the spot price in the response if requested
    if include_spot_price {
        let spot_price = calculate_spot_price(deps, swap_operations)?;
        response.spot_price = Some(spot_price);
    }

    Ok(response)
}

fn query_simulate_smart_swap_exact_asset_in_with_metadata(
    deps: Deps,
    asset_in: Asset,
    ask_denom: String,
    routes: Vec<Route>,
    include_spot_price: bool,
) -> ContractResult<SimulateSmartSwapExactAssetInResponse> {
    let asset_out = simulate_smart_swap_exact_asset_in(deps, ask_denom, routes.clone())?;

    let mut response = SimulateSmartSwapExactAssetInResponse {
        asset_out,
        spot_price: None,
    };

    if include_spot_price {
        response.spot_price = Some(calculate_weighted_spot_price(deps, asset_in, routes)?)
    }

    Ok(response)
}

// Simulates a swap exact amount in request, returning the asset out and optionally the reverse simulation responses
fn simulate_swap_exact_asset_in(
    deps: Deps,
    asset_in: Asset,
    swap_operations: Vec<SwapOperation>,
) -> ContractResult<Asset> {
    let oraidex_router_address = ORAIDEX_ROUTER_ADDRESS.load(deps.storage)?;

    let mut hop_swap_requests: Vec<OraidexSwapOperation> = vec![];
    for operation in &swap_operations {
        if operation.pool.contains("-") {
            // v3
            let pool_key = convert_pool_id_to_v3_pool_key(&operation.pool)?;
            let x_to_y = pool_key.token_x == operation.denom_in;

            hop_swap_requests.push(OraidexSwapOperation::SwapV3 { pool_key, x_to_y })
        } else {
            // v2
            hop_swap_requests.push(OraidexSwapOperation::OraiSwap {
                offer_asset_info: denom_to_asset_info(deps.api, &operation.denom_in),
                ask_asset_info: denom_to_asset_info(deps.api, &operation.denom_out),
            })
        }
    }

    let oraidex_router_query = OraidexQueryMsg::SimulateSwapOperations {
        offer_amount: asset_in.amount(),
        operations: hop_swap_requests,
    };

    let oraidex_router_response: SimulateSwapOperationsResponse = deps
        .querier
        .query_wasm_smart(oraidex_router_address, &oraidex_router_query)?;

    let asset_out = Asset::new(
        deps.api,
        &swap_operations.last().unwrap().denom_out,
        oraidex_router_response.amount,
    );
    Ok(asset_out)
}

// Simulates a swap exact amount out request, returning the asset in needed and optionally the reverse simulation responses
fn simulate_swap_exact_asset_out(
    _deps: Deps,
    _asset_out: Asset,
    _swap_operations: Vec<SwapOperation>,
) -> ContractResult<Asset> {
    panic!("not implemented")
    // let dexter_router_address = DEXTER_ROUTER_ADDRESS.load(deps.storage)?;

    // let mut hop_swap_requests: Vec<HopSwapRequest> = vec![];
    // for operation in &swap_operations {
    //     let pool_id: u64 = operation.pool.parse().unwrap();
    //     let pool_id_u128 = Uint128::from(pool_id);

    //     hop_swap_requests.push(HopSwapRequest {
    //         pool_id: pool_id_u128,
    //         asset_in: dexter::asset::AssetInfo::native_token(operation.denom_in.clone()),
    //         asset_out: dexter::asset::AssetInfo::native_token(operation.denom_out.clone()),
    //     });
    // }

    // let dexter_router_query = RouterQueryMsg::SimulateMultihopSwap {
    //     multiswap_request: hop_swap_requests,
    //     swap_type: dexter::vault::SwapType::GiveOut {},
    //     amount: asset_out.amount(),
    // };

    // let dexter_router_response: dexter::router::SimulateMultiHopResponse = deps
    //     .querier
    //     .query_wasm_smart(dexter_router_address, &dexter_router_query)?;

    // if let ResponseType::Success {} = dexter_router_response.response {
    //     // Get the asset out
    //     let first_response = dexter_router_response.swap_operations.first().unwrap();

    //     let asset_in = Asset::Native(Coin {
    //         denom: first_response.asset_in.to_string(),
    //         amount: first_response.offered_amount,
    //     });

    //     // Return the asset out and optionally the simulation responses
    //     Ok(asset_in)
    // } else {
    //     Err(ContractError::SimulationError)
    // }
}

fn simulate_smart_swap_exact_asset_in(
    deps: Deps,
    ask_denom: String,
    routes: Vec<Route>,
) -> ContractResult<Asset> {
    let mut asset_out = Asset::new(deps.api, &ask_denom, Uint128::zero());

    for route in &routes {
        let route_asset_out = simulate_swap_exact_asset_in(
            deps,
            route.offer_asset.clone(),
            route.operations.clone(),
        )?;

        asset_out.add(route_asset_out.amount())?;
    }

    Ok(asset_out)
}

// find spot prices for all the pools in the swap operations
fn calculate_spot_price(
    _deps: Deps,
    _swap_operations: Vec<SwapOperation>,
) -> ContractResult<Decimal> {
    panic!("not implemented")
    // let dexter_vault_address = DEXTER_VAULT_ADDRESS.load(deps.storage)?;
    // let mut final_price = Decimal::one();
    // for operation in &swap_operations {
    //     let pool_id: u64 = operation.pool.parse().unwrap();
    //     let pool_id_u128 = Uint128::from(pool_id);

    //     let pool_info: PoolInfoResponse = deps.querier.query_wasm_smart(
    //         dexter_vault_address.clone(),
    //         &VaultQueryMsg::GetPoolById {
    //             pool_id: pool_id_u128,
    //         },
    //     )?;

    //     let spot_price: SpotPrice = deps.querier.query_wasm_smart(
    //         pool_info.pool_addr,
    //         &pool::QueryMsg::SpotPrice {
    //             offer_asset: dexter::asset::AssetInfo::native_token(operation.denom_in.clone()),
    //             ask_asset: dexter::asset::AssetInfo::native_token(operation.denom_out.clone()),
    //         },
    //     )?;

    //     final_price = final_price
    //         .checked_mul(Decimal::from_str(&spot_price.price_including_fee.to_string()).unwrap())
    //         .unwrap();
    // }

    // Ok(final_price)
}

fn calculate_weighted_spot_price(
    deps: Deps,
    asset_in: Asset,
    routes: Vec<Route>,
) -> ContractResult<Decimal> {
    let spot_price = routes.into_iter().try_fold(
        Decimal::zero(),
        |curr_spot_price, route| -> ContractResult<Decimal> {
            let route_spot_price = calculate_spot_price(deps, route.operations)?;

            let weight = Decimal::from_ratio(route.offer_asset.amount(), asset_in.amount());

            Ok(curr_spot_price + (route_spot_price * weight))
        },
    )?;

    Ok(spot_price)
}
