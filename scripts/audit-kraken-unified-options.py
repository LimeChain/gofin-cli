#!/usr/bin/env python3
"""Explicit unified-to-native option inventory from pinned CCXT Rust sources.

This audit writes only unified-options.{json,md}; it does not modify the existing
CCXT metadata, endpoint inventory, schemas, or executable adapter behavior.
"""
import argparse
import hashlib
import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DOCS = ROOT / "docs/kraken"
PARAM = re.compile(r"@param\s+\{([^}]+)\}\s+(\[[^\]]+\]|\S+)\s*(.*)")
FUNCTION = re.compile(r"^\s*(?:pub\s+)?(?:async\s+)?fn\s+(\w+)\(", re.M)


def functions(text):
    matches=list(FUNCTION.finditer(text))
    return {match[1]:{"text":text[match.start():matches[i+1].start() if i+1<len(matches) else len(text)].lstrip(),
                     "line":text[:match.start()+len(match[0])-len(match[0].lstrip())].count("\n")+1}
            for i,match in enumerate(matches)}


def snake(name):
    return re.sub(r"(?<!^)([A-Z])",r"_\1",name).lower().replace("o_h_l_c_v","ohlcv")


def parameter(declaration):
    match=PARAM.search(declaration)
    if not match:
        return None
    kind,name,description=match.groups()
    optional=name.startswith("[")
    return {"name":name.strip("[]"),"type":kind,"optional":optional,"declaration":declaration,"description":description}


def map_option(client, method, name, operations, contracts):
    spot=client=="kraken"
    option=name.removeprefix("params.")
    result={"kind":"native_input","nativePointers":[],"transform":"","units":"native","finBehavior":"explicit_native_input_required"}
    def native(pointers,transform="Copy exact value without numeric rounding.",units="native"):
        result.update(nativePointers=pointers,transform=transform,units=units)
    def caller(behavior,units="response-native",kind="caller_response_filter"):
        result.update(kind=kind,transform=behavior,units=units,finBehavior="caller_side_required")
    def unavailable(reason):
        result.update(kind="unimplemented",transform=reason,finBehavior="incomplete")
    if method=="create_orders" and name!="orders":
        single=[op for op,contract in contracts.items() if contract["client"]==client and contract["method"]==("private_post_add_order" if spot else "private_post_sendorder")]
        child=map_option(client,"create_order",name,single,contracts)
        def child_pointer(pointer):
            if spot and pointer in ("/pair","/asset_class","/deadline","/validate","/broker"):return pointer
            if spot and pointer.startswith("/close["):return "/orders/*/close/"+pointer[7:-1]
            return ("/orders/*" if spot else "/batchOrder/*")+pointer
        child["nativePointers"]=[child_pointer(pointer) for pointer in child["nativePointers"]]
        child["nativeMappings"]=[{"pointer":pointer,"operations":operations} for pointer in child["nativePointers"]]
        child.pop("unavailableNativePointers",None)
        if child["nativeMappings"]:child["finBehavior"]="explicit_native_child_input_required"
        child["transform"]="Per send child: "+child["transform"]
        return child
    if name=="params":
        result.update(kind="closed_native_expansion",finBehavior="closed_native_fields_only",transform="Expand only the explicitly enumerated operation fields below; reject a params object and unknown keys.",nativePointers=sorted({"/"+field for op in operations for field in contracts.get(op,{}).get("schema",{}).get("properties",{})}))
    elif option in ("paginate","paginationCalls","maxEntriesPerRequest","paginationDirection","paginationTimestamp","paginationCursor"):
        caller("CCXT automatic pagination is a caller loop over the listed typed reads; keep immutable page/cursor provenance, stop on repeated cursors or bounded limits, and never turn an empty page into write settlement.","bounded pages", "caller_pagination")
    elif option in ("type","account","symbol") and method=="fetch_balance" and not spot:caller("Select accounts response wallet only: cash aliases main/funding, flex default, or resolved isolated market. Preserve free/collateral/PnL/debt separately; no wire balance selector exists.","selected native wallet quantities")
    elif option in ("symbol","symbols"):
        if method in ("cancel_all_orders","fetch_order_book","fetch_ohlcv","fetch_ticker","create_order","create_orders","edit_order","fetch_trades","fetch_trading_fee","set_leverage"):
            if not spot and method=="edit_order":caller("CCXT ignores symbol when editing; Fin retains original verified contract/account identity.")
            elif method=="cancel_all_orders" and spot:caller("Kraken ignores symbol: cancel-all affects every open order for the exact approved account; never imply a market-local cancellation.")
            else:native((["/pair"] if spot else (["/tradeable","/symbol"] if method=="fetch_trades" else ["/symbol"])),"Resolve unified symbol using exact venue market metadata; use native market ID, not a display symbol guess.","native market identity")
        elif not spot and method in ("fetch_trading_fees","fetch_trading_fee"):
            native(["/pair"],"Current financial authority selects exact native Futures contract in pair=[{asset:resolved_id,aclass:futures_contract}] through related verified Spot account; normalize/filter current fee rows by that contract.","exact Futures contract / verified Spot fee authority")
        elif method in ("fetch_tickers","fetch_orders_by_ids") and spot:
            if method=="fetch_tickers":native(["/pair"],"Resolve each market ID and comma-join selected native pairs; preserve all-ticker response selection/filtering.","native pair list")
            else:caller("Filter returned orders by resolved native pair; query IDs are the server selector.")
        elif method=="fetch_positions" and spot:caller("The pinned implementation documents symbols as unused. Native OpenPositions txid/consolidation selectors remain explicit.")
        elif method=="fetch_positions_history":native(["/tradeable"],"Server accepts one resolved contract; additional selected symbols require separate typed reads or caller filtering.","native Futures contract")
        else:caller("Filter native response rows by exact resolved market identity after the bounded typed read; Fin does not automatically normalize or filter unified symbols.")
    elif option=="code":
        if method in ("fetch_funding_history","fetch_leverage_tiers","fetch_ledger_entry","fetch_ledger_entries_by_ids") or not spot and method=="fetch_ledger":
            caller("Filter response by exact native asset/settlement identity; do not infer asset from symbol alone.")
        elif method=="transfer" and not spot:
            native(["/unit","/currency"],"Resolve authenticated exact native asset: unit for wallet transfer, currency for cash-to-Spot withdrawal.","native asset identity")
        else:native(["/asset"],"Resolve official currency/altname and native class/denomination; preserve tokenized base versus rebased units explicitly.","native asset identity")
    elif option=="timeframe":
        native(["/interval" if spot else "/resolution"],"Use pinned exact timeframe map. Spot: 1m=1,5m=5,15m=15,30m=30,1h=60,4h=240,1d=1440,1w=10080,2w=21600 minutes. Futures: 1m,5m,15m,30m,1h,4h,12h,1d,1w literal resolution.","Spot minutes; Futures resolution token")
    elif option=="since":
        if spot:
            if method=="fetch_ohlcv":native(["/since"],"CCXT truncates milliseconds/1000 then subtracts one timeframe in seconds; filter returned candles to original since/limit.","Unix seconds")
            elif method=="fetch_trades":native(["/since"],"Pinned source divides milliseconds by 1000 and truncates, then sends decimal text seconds; keep returned opaque last cursor distinct from since.","Unix seconds / separate opaque cursor")
            elif method=="fetch_open_orders":caller("Pinned CCXT sends start seconds, but current official OpenOrders no longer exposes start. Use the closed native read and caller filter normalized order timestamps by original milliseconds; do not pass an unlisted start field.","milliseconds")
            elif method in ("fetch_ledger","fetch_my_trades","fetch_closed_orders"):
                native(["/start"],"Truncate milliseconds/1000 to native seconds; CCXT additionally filters normalized returned rows by original millisecond since.","Unix seconds")
            elif method in ("fetch_deposits","fetch_withdrawals"):
                native(["/start"],"Exact decimal milliseconds/1000, without truncation in pinned CCXT; caller filters returned normalized timestamps to original since.","Unix seconds decimal")
            else:caller("Apply millisecond since filter to returned native order/trade rows after correct native timestamp conversion.","milliseconds")
        elif method=="fetch_ohlcv":native(["/from","/to"],"from=floor(since_ms/1000); to=min(from+window_limit*resolution_seconds-1,current_seconds), window_limit=min(limit,2000) or 2000; filter candles by original millisecond since.","Unix seconds exact decimal text")
        elif method in ("fetch_closed_orders","fetch_canceled_orders","fetch_ledger","fetch_funding_history","fetch_positions_history","fetch_trades"):
            native(["/since"],"History endpoint receives original millisecond timestamp and sort=asc when since is set; public recent-trades branch needs separate caller filtering.","Unix milliseconds")
        else:caller("The native endpoint does not accept unified since; filter returned timestamps in milliseconds, preserving endpoint-specific timestamp conversion.","milliseconds")
    elif option in ("until","till","end"):
        if spot:
            native(["/end"],"until/till milliseconds become exact seconds plus 1 to preserve inclusive upper filtering; params.end is already native seconds and may explicitly override the computed end. Filter original millisecond bound after response.","Unix seconds; until/till input milliseconds")
        elif method=="fetch_my_trades":native(["/lastFillTime"],"Source handle_until_option selects lastFillTime; encode the exact native endpoint time format from the millisecond upper bound and apply caller filtering.","native fill time / unified milliseconds")
        else:native(["/before"],"Copy unified millisecond upper timestamp to native history before; caller applies original bound when normalization filters rows.","Unix milliseconds")
    elif option=="limit":
        if method=="fetch_order_book":
            if spot:native(["/count"],"Copy requested bounded depth count.","book entries")
            else:caller("Pinned KrakenFutures ignores limit for order book; native orderbook has no depth selector.","book entries")
        elif spot and method=="fetch_trades":native(["/count"],"Copy native trade count within endpoint bounds.","trades")
        elif not spot and method in ("fetch_closed_orders","fetch_canceled_orders","fetch_trades","fetch_funding_history","fetch_positions_history"):
            native(["/count"],"Copy native bounded history count; preserve continuation token and final caller since/limit filtering.","history elements")
        elif not spot and method=="fetch_ledger":native(["/count"],"Pinned source requests 2*limit because each journal event can parse into multiple ledger entries, then caller trims parsed ledger rows.","native events versus normalized ledger entries")
        elif method=="fetch_ohlcv" and not spot:caller("Limit also constructs native from/to window capped at 2000; caller trims candle rows to requested normalized count.","candles")
        else:caller("CCXT trims parsed rows after the request; native page-size/offset/cursor fields are separately explicit and are not equivalent to the unified total result limit.","normalized rows")
    elif option in ("amount","cost"):
        if method in ("create_order","create_orders"):
            native(["/volume" if spot else "/size"],"Exact native amount; Spot base units except explicit cost/viqc market orders use quote units. Futures amount is contracts, never collateral. Fin rejects excess venue precision rather than CCXT rounding.","Spot base/explicitquote; Futures contracts")
        elif method=="edit_order":native(["/order_qty" if spot else "/size"],"Exact revised total order quantity; original fills retain ownership and revised cumulative scope includes consumed quantity.","Spot base; Futures contracts")
        elif "with_cost" in method:native(["/volume","/oflags","/ordertype"],"ordertype=market, volume=exact quote cost, oflags includes viqc; pinned convenience supports USD markets only. Keep finite maximumSpend/fees and native precision.","Spot quote currency")
        else:native(["/amount"],"Exact native asset quantity/denomination; quote fees and bind account/product/recipient before financial dispatch.","native asset units")
    elif option=="type":
        if method=="edit_order":caller("Pinned atomic edit ignores order type; Fin retains and validates the original proven type and prevents retyping its economic scope.")
        elif spot:native(["/ordertype"],"market/limit map directly; explicit stop-loss/take-profit/trailing families select native ordertype and price/price2 meaning.","native order enum")
        else:native(["/orderType"],"limit→lmt; market→mkt; postOnly→post; timeInForce=ioc→ioc; trigger/stop→stp; take profit→take_profit.","native Futures order enum")
    elif option=="orderType" and not spot:native(["/orderType"],"Native order-type override is validated against lmt/post/ioc/mkt/stp/take_profit/trailing_stop/fok; unified limit/market aliases are explicitly translated, never forwarded as unknown native enums.","native Futures order enum")
    elif option=="limitPrice" and not spot:native(["/limitPrice"],"Exact native limit price; a native parameter may supply price when positional unified price is absent. Bind the approved execution bound and native tick precision.","native contract price")
    elif option=="side":
        if method=="edit_order":caller("An amendment retains the original proven side; CCXT's positional side is ignored or must not retype exposure.")
        else:native(["/type" if spot else "/side"],"Copy buy/sell without reinterpreting derivative exposure as cash.","buy/sell")
    elif option=="price":
        if name=="params.price" and method=="fetch_ohlcv":native(["/tick_type"],"CCXT trade→trade,mark→mark,index→spot,spot→spot. Native charts does not accept index.","candle price source")
        elif method=="edit_order":native(["/limit_price" if spot else "/limitPrice"],"Exact native amendment price; original type determines limit/trigger meaning.","quote per base or native contract price")
        elif spot:native(["/price","/price2"],"Regular limit uses price. Stop-loss/take-profit limit uses trigger in price and positional limit in price2; trailing uses explicit signed offsets.","Spot quote/base or signed native offsets")
        else:native(["/limitPrice"],"Exact limit price for lmt/post/ioc; market ignores price. Trigger-family price is its bounded limit execution price.","native contract price")
    elif option=="postOnly":native(["/post_only" if spot and method=="edit_order" else ("/oflags" if spot else "/orderType")],"Spot create appends post to oflags; Spot amend uses post_only=true. Futures changes orderType to post. Never combine a market immediate execution with post-only.","boolean / native flag")
    elif option in ("reduceOnly","reduce_only"):
        native(["/reduce_only" if spot else "/reduceOnly"],"Copy boolean reduction requirement; bind original margin/contract exposure. Futures stop-loss/take-profit convenience forces true.","position reduction")
    elif option in ("clientOrderId","cl_ord_id","cliOrdId"):
        if method=="cancel_order":native(["/cl_ord_id" if spot else "/cliOrdId"],"Use exact venue client identity instead of native order ID; this remains separate from Fin requestId and is not guaranteed idempotency.","venue client ID")
        elif method=="edit_order":native(["/cl_ord_id" if spot else "/cliOrdId"],"Select the original venue client identity, never the Fin amendment requestId.","venue client ID")
        else:native(["/cl_ord_id" if spot else "/cliOrdId"],"Copy exact bounded venue client identity; native IDs remain separate from Fin immutable request/submission identity.","venue client ID")
    elif option in ("triggerPrice","stopPrice","stopLossPrice","takeProfitPrice"):
        if spot:
            native(["/trigger_price" if method=="edit_order" else "/price","/ordertype"] if method!="edit_order" else ["/trigger_price"],"Set trigger price and stop-loss(-limit) or take-profit(-limit) native ordertype; limit execution price goes to price2. Generic inherited stopPrice forwarding is not implemented by pinned Spot order_request; Fin requires explicit native fields.","Spot trigger quote/base")
        else:native(["/stopPrice","/orderType","/reduceOnly"],"triggerPrice/stopPrice→stp; stopLossPrice→stp+reduceOnly=true; takeProfitPrice→take_profit+reduceOnly=true. Positional price supplies limitPrice for bounded execution.","native contract trigger price")
    elif option in ("trailingAmount","trailingPercent","trailingLimitAmount","trailingLimitPercent","offset","trigger") and spot:
        native(["/price","/price2","/ordertype","/trigger"],"Trailing distance uses positive +amount or +percent%; ordertype=trailing-stop or trailing-stop-limit. price2 uses offset '-' by default or explicit '+', with % suffix for percentage limit. trigger is last/index. Atomic amend uses trigger_price instead of create price; native trigger type cannot change silently.","quote distance or percentage points; signed offsets")
        if method=="edit_order":result["nativePointers"]=["/trigger_price","/limit_price"]
    elif option=="trailingTriggerPrice":unavailable("Inherited CCXT helper adds trailingTriggerPrice, but pinned Spot order_request does not translate it and official AddOrder has no separate trailing activation field. Keep this alias option explicitly incomplete; do not pass an unknown key or fabricate activation.")
    elif option=="triggerSignal":native(["/triggerSignal"],"Copy last/mark/index trigger source; pin it in the exact approved Futures trigger scope.","native trigger price source")
    elif option=="timeInForce":native(["/timeinforce" if spot else "/orderType"],"Spot copies supported native GTC/IOC/GTD policy; Futures ioc changes orderType to ioc and PO/postOnly changes it to post. Other unsupported policies fail closed.","execution policy enum")
    elif option=="close" and spot:native(["/close[ordertype]","/close[price]","/close[price2]"],"Flatten conditional close object into exact standalone AddOrder close[...] fields; batch children retain nested close object. Fin persists child order ownership and separately bounded close-sequence effects; a trigger is not guaranteed reduction.","opposite-side conditional child quantity / exact native prices")
    elif option=="orders":native(["/orders" if spot else "/batchOrder"],"Translate each child through its exact single-order field mapping. Spot batch shares pair; Futures uses batchOrder array children in JSON text. Child identities, maximumFees and partial outcomes remain independent.","ordered finite native child list")
    elif option in ("id","ids"):
        if method in ("fetch_ledger_entry","fetch_ledger_entries_by_ids"):native(["/id"],"Exact native ledger ID(s), comma-join for query batch with venue maximum 20.","ledger IDs")
        elif method=="cancel_orders":native(["/orders" if spot else "/batchOrder/*/order_id"],"Spot copies native order/userref identities into orders; Futures encodes order=cancel with order_id per independently linked child.","venue order IDs")
        elif method=="fetch_order_trades":caller("The id alone is insufficient: pinned source requires params.trades from a prior authenticated order response, queries those trade IDs in batches of 20, then filters by order/symbol/since/limit.","original order/trade IDs", "caller_composition")
        elif method=="edit_order":native(["/txid","/cl_ord_id"] if spot else ["/orderId","/cliOrdId"],"Select original native order identity from protected journal; Spot atomic amend translates txid→txid or client identity selector. Separate Fin operationId is mandatory for revision ownership.","immutable original order identity")
        elif spot:native(["/txid"],"Copy order ID or comma-join query IDs. Cancel client/userref alternatives have explicit selectors.","venue order IDs")
        else:native(["/orderIds"] if method.startswith("fetch_") else ["/order_id"],"Status query uses array orderIds; cancel uses exact order_id. Preserve original UUID and selected account.","native Futures UUIDs")
    elif option=="clientOrderIds":native(["/batchOrder/*/cliOrdId"],"Build cancel child with cliOrdId for each selected client ID (CCXT max 10); retain independent original journal linkage and duplicate checks.","venue client IDs")
    elif option=="userref":native(["/txid"] if method=="cancel_order" else ["/userref"],"Cancel userref selects native txid alternative; read APIs expose explicit userref filtering. Group selectors never claim a uniquely identified order without disambiguation.","bounded integer user reference")
    elif option=="timeout":native(["/timeout"],"If positive, CCXT truncates milliseconds/1000; zero disables. Fin native timeout is explicitly approved seconds and never silently renewed.","native seconds / unified milliseconds")
    elif option=="address":native(["/address"],"Bind exact destination identity against saved Kraken key/address method; empty upstream address is not sufficient independent recipient verification in Fin.","exact destination")
    elif option=="tag":caller("Pinned wrapper handles tag/params but legacy Kraken Withdraw exposes no separate tag field. Resolve tag/memo in the exact saved recipient key and route; do not copy a tag into address or allow unknown tag passthrough. Unsupported independent observer requires explicit venue_only consent.","native memo/tag recipient identity","caller_recipient_resolution")
    elif option=="network" and method in ("fetch_deposit_address","fetch_deposit_methods","create_deposit_address"):
        result.update(kind="caller_composition",nativePointers=["/method","/network"],transform="Discover exact network and method with current Funding methods/networks or legacy DepositMethods; select exactly one asset/network/memo route and supply its explicit native method. The network alias cannot be blindly copied into DepositAddresses.",finBehavior="caller_discovery_required",units="exact network/method identity")
    elif option in ("fromAccount","toAccount"):
        if spot:native(["/from" if option=="fromAccount" else "/to"],"Only spot/Spot Wallet→swap/Futures Wallet. Bind separately enrolled destinationAccount and authenticate matching Spot/Futures IIBAN relationship.","product wallet identity")
        else:native(["/fromAccount" if option=="fromAccount" else "/toAccount"],"main/funding/future/futures/cashAccount→cash; multiCollateralMarginAccount/multiCollateral/multiCollateralMargin→flex; unified inverse market→fi_suffix and linear→fv_suffix. cash→spot uses separate withdrawal operation. Independently bind custody and native accountUid.","derivative wallet identity")
    elif option=="leverage":native(["/maxLeverage"],"Set exact maximum leverage for resolved contract through separate fresh configuration approval; financial plans cannot authorize an administrative preference change.","leverage ratio")
    elif option in ("opened","increased","decreased","reversed","no_change","trades","funding_realization","settlement","continuation_token") and method=="fetch_positions_history":native(["/"+option],"Copy exact closed native history selector or opaque continuation token; do not infer exhaustive history from one page.","history event selector / opaque cursor")
    elif option=="trades" and method=="fetch_order_trades":native(["/txid"],"Extract exact trade IDs from original order or trades array; comma-join each bounded query batch of 20 and filter parsed rows against original order.","native trade IDs")
    elif option=="trigger" and not spot:result.update(kind="native_endpoint_choice",transform="true/params.stop chooses typed history-get-triggers; false chooses typed history-get-orders. Both retain exact accountUid and time/count bounds.",nativePointers=["/since","/before","/count"],finBehavior="caller_endpoint_choice_required")
    elif option=="method" and method=="fetch_trades" and not spot:result.update(kind="native_endpoint_choice",transform="Only historyGetMarketSymbolExecutions or publicGetHistory; select the corresponding typed operation. Never pass an arbitrary method/path or merge distinct timestamp units.",nativePointers=["/tradeable","/symbol"],finBehavior="caller_endpoint_choice_required")
    else:
        native_fields={field for op in operations for field in contracts.get(op,{}).get("schema",{}).get("properties",{})}
        if option in native_fields:native(["/"+option],"Copy only this explicit operation-specific native field; keep identity/unit from the source schema.")
        else:unavailable("No exact closed native pointer or implemented caller behavior was established for this source option. It must remain explicit incomplete, never arbitrary params passthrough.")
    # Only return pointers actually accepted by at least one exact mapped native
    # operation. Endpoint-choice mappings retain their distinct operation choice.
    valid=[];missing=[]
    for pointer in result["nativePointers"]:
        field=pointer.split("/")[1]
        owning=[op for op in operations if field in contracts.get(op,{}).get("schema",{}).get("properties",{})]
        if owning:valid.append({"pointer":pointer,"operations":owning})
        else:missing.append(pointer)
    result["nativeMappings"]=valid
    if missing:result["unavailableNativePointers"]=missing
    if result["kind"]=="native_input" and not valid:result["finBehavior"]="incomplete_native_mapping"
    return result


def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--ccxt-source",type=Path)
    args=parser.parse_args()
    source=args.ccxt_source
    if source is None:
        candidates=list((Path.home()/".cargo/registry/src").glob("*/ccxt-base-4.5.85/src"))
        if len(candidates)!=1:raise SystemExit("Pass --ccxt-source for the exact pinned ccxt-base 4.5.85/src directory")
        source=candidates[0]
    contract_list=json.loads((DOCS/"contracts.json").read_text())["endpoints"]
    contracts={entry["operation"]:entry for entry in contract_list if entry.get("operation")}
    by_native={(entry["client"],entry["method"]):entry["operation"] for entry in contract_list if entry.get("operation")}
    base_text=(source/"exchange_generated.rs").read_text();base_functions=functions(base_text)
    output={"schemaVersion":1,"ccxtVersion":"4.5.85","snapshotDate":"2026-10-10","scope":"Explicit source option translation inventory. Fin exposes closed native operations, not a raw CCXT object/params API; caller transformations and unsupported aliases are labeled separately.","sources":[],"overrides":[],"inheritedAndEmulated":[],"corrections":[]}
    for client in ("kraken","krakenfutures"):
        metadata=json.loads((DOCS/f"{client}-ccxt.json").read_text())
        text=(source/"exchanges"/f"{client}.rs").read_text()
        digest=hashlib.sha256(text.encode()).hexdigest()
        if digest!=metadata["sourceSha256"]:raise SystemExit(f"{client} source SHA differs from the inspected pinned version")
        local=functions(text)
        output["sources"].append({"client":client,"file":f"ccxt-base-4.5.85/src/exchanges/{client}.rs","sha256":digest})
        def build(method,binding,inherited=False,capability=None):
            body=local.get(method,base_functions.get(method))
            declarations=list(binding.get("ccxtParameterDeclarations",[]))
            if inherited and body:
                declarations=re.findall(r"@param[^\n]+",body["text"])
            params=[parameter(value) for value in declarations];params=[p for p in params if p]
            if body:
                head=body["text"].split("\n",1)[0]
                arguments=re.findall(r"(?:mut\s+)?(\w+):\s*Value",head)
                arguments+=re.findall(r"let mut (\w+)\s*=\s*get_arg\(optional_args",body["text"])
                for arg in arguments:
                    arg="type" if arg=="type_var" else arg
                    if arg not in {p["name"] for p in params}:
                        params.append({"name":arg,"type":"source-signature","optional":arg not in re.findall(r"(?:mut\s+)?(\w+):\s*Value",head),"declaration":"Source Rust signature/optional_args: "+arg,"description":"Explicit positional source argument omitted from the nearby declaration block."})
            native=list(binding.get("nativeMethods",[]));operations=list(binding.get("operations",[]))
            if method=="transfer_out":
                native=["private_post_wallet_transfer"] if client=="kraken" else ["private_post_withdrawal"]
                operations=[by_native[(client,native[0])]]
            if client=="krakenfutures" and method in ("fetch_trading_fees","fetch_trading_fee"):
                operations=[by_native[("kraken","private_post_trade_volume")]]
            entry={"client":client,"method":method,"capability":capability,"sourceLine":body["line"] if body else None,"sourceFile":"exchange_generated.rs" if method not in local else f"exchanges/{client}.rs","operations":operations,"pinnedCcxtNativeMethods":native,"unifiedAliasExposure":"not_a_public_unified_input_schema; explicit closed Fin operation equivalent","productIdentity":"Spot/margin account with selected authenticated wallet ID and explicit tokenized denomination" if client=="kraken" else "Derivatives native accountUid/custody wallet/contracts; separately verified related Spot IIBAN key for current fees or cross-product movement","parameters":[]}
            mapping_method=method
            if inherited and binding.get("delegates"):
                mapping_method=binding["delegates"][0]
            if method=="transfer_out":mapping_method="transfer"
            for p in params:entry["parameters"].append({**p,**map_option(client,mapping_method,p["name"],operations,contracts)})
            if method=="transfer_out":
                entry["fixedNativeInputs"]={"from":"Spot Wallet","to":"Futures Wallet"} if client=="kraken" else {"fromWallet":"cash","destinationProduct":"Spot"}
            if inherited:
                fixed={"create_market_order":{"orderType":"mkt"},"create_post_only_order":{"orderType":"post"},"create_reduce_only_order":{"reduceOnly":True},"create_stop_limit_order":{"orderType":"stp","limitPrice":"exact positional price"},"create_stop_market_order":{"orderType":"stp"},"fetch_index_ohlcv":{"tick_type":"spot"},"fetch_mark_ohlcv":{"tick_type":"mark"}}
                entry["fixedNativeInputs"]=fixed.get(method,{})
                if client=="kraken":
                    entry["fixedNativeInputs"]={"create_stop_limit_order":{"ordertype":"stop-loss-limit","price":"exact triggerPrice","price2":"exact positional price"},"create_stop_market_order":{"ordertype":"stop-loss","price":"exact triggerPrice"}}.get(method,{})
                if client=="kraken" and method in ("create_stop_order","create_stop_limit_order","create_stop_market_order"):
                    entry["upstreamSourceGap"]="Base helper forwards stopPrice but pinned Spot order_request has triggerPrice=false and does not translate stopPrice; use explicit native stop-loss family rather than claiming generic CCXT stop alias execution."
            if client=="krakenfutures" and method in ("fetch_trading_fees","fetch_trading_fee"):
                entry["currentFinancialFeeAuthority"]={"operation":by_native[("kraken","private_post_trade_volume")],"input":{"pair":[{"asset":"exact resolved Futures instrument ID","aclass":"futures_contract"}],"fee_schedule":True},"relationship":"Authenticated Spot/Futures IIBAN relationship with independently authenticated native Futures accountUid; no same-name or physical-ID inference","legacyRead":"Pinned public fee schedules/volumes remain explicit deprecated informational reads; they do not authorize financial fee bounds."}
            if body:
                option_tokens=sorted(set(re.findall(r'(?:safe_\w+\([^\n]*?params[^\n]*?|handle_option_\w+_and_params\(params[^\n]*?)"([A-Za-z_][A-Za-z0-9_]*)"',body["text"])))
                helper="order_request" if client=="kraken" else "create_order_request"
                if method in ("create_order","create_orders") and helper in local:
                    option_tokens+=re.findall(r'safe_\w+\([^\n]*?params[^\n]*?"([A-Za-z_][A-Za-z0-9_]*)"',local[helper]["text"])
                option_tokens+=re.findall(r'params\w*[^\n]*?(?:get|contains_key)\("([A-Za-z_][A-Za-z0-9_]*)"',body["text"])
                if method in ("create_order","create_orders") and "handle_post_only" in (local.get(helper,{"text":""})["text"]):
                    option_tokens += ["postOnly","timeInForce"]
                entry["processedUndeclaredOptions"]=[{"name":"params."+name,**map_option(client,method,"params."+name,operations,contracts)} for name in sorted(set(option_tokens)) if name not in ("fetchOHLCV","fetchWithdrawals","fetchTrades") and "params."+name not in {p["name"] for p in params}]
            return entry
        for method,binding in metadata["unifiedBindings"].items():
            output["overrides"].append(build(method,binding))
        for capability,binding in metadata["capabilityBindings"].items():
            if binding.get("unifiedMethod"):
                if binding.get("implementation")=="emulated":
                    output["inheritedAndEmulated"].append({"client":client,"capability":capability,"method":binding["unifiedMethod"],"overrideReference":client+"."+binding["unifiedMethod"],"implementation":"emulated","behavior":"Use the explicit override parameter mappings; response normalization/filtering remains caller-owned unless separately implemented."})
                continue
            method=snake(capability)
            base_entry=build(method,binding,True,capability)
            base_entry["delegates"]=binding.get("delegates",[]);base_entry["implementation"]=binding["implementation"]
            output["inheritedAndEmulated"].append(base_entry)
    output["baseSource"]={"file":"ccxt-base-4.5.85/src/exchange_generated.rs","sha256":hashlib.sha256(base_text.encode()).hexdigest()}
    output["corrections"]=[{"scope":"krakenfutures.fetchIndexOHLCV","mapping":"params.price=index maps to native tick_type=spot, not index","source":"KrakenFutures fetch_ohlcv explicit native venue rename"},{"scope":"Spot/Futures.transfer_out","mapping":"The source delegates to transfer; empty prior nativeMethods is not an unmapped capability"},{"scope":"krakenfutures.fetchTradingFee/fetchTradingFees","mapping":"Current financial fee authority uses verified related Spot GetTradeVolume pair with aclass=futures_contract and fee_schedule=true, never deprecated Futures fee schedule."}]
    output["totals"]={"overrideMethods":len(output["overrides"]),"overrideDeclaredAndSignatureParameters":sum(len(e["parameters"]) for e in output["overrides"]),"inheritedAndEmulatedCapabilities":len(output["inheritedAndEmulated"])}
    (DOCS/"unified-options.json").write_text(json.dumps(output,indent=2)+"\n")
    lines=["# Kraken unified option translation inventory","",output["scope"],"","Every declared parameter in the 71 pinned overrides is listed in the machine inventory, along with source-signature arguments omitted from nearby comments, processed source options and inherited/emulated capability arguments. Pointers are checked against actual closed Fin input definitions. Quantities use explicit native units; Fin never applies CCXT precision rounding or treats arbitrary params as authority.","","Finite native request mapping is implemented separately from CCXT aliases, response normalization and automatic pagination. Native equivalent fields are marked explicit_native_input_required. Response filtering/pagination/endpoint composition is caller-side-required. Incomplete or unavailable native pointers remain explicit; this inventory does not claim those aliases are executable broker inputs.","","Current Futures fees require the related authenticated Spot GetTradeVolume route with futures_contract pair class. Deprecated Futures schedules are informational reads. Index candles use native spot. Transfer-out delegates preserve the product's exact direction and separately verified destination custody.","","```sh","python3 scripts/audit-kraken-unified-options.py","```","","| Client/method | Declared/signature option | Native pointer or caller behavior | Fin behavior | Units |","| --- | --- | --- | --- | --- |"]
    for entry in output["overrides"]+output["inheritedAndEmulated"]:
        for param in entry.get("parameters",[])+entry.get("processedUndeclaredOptions",[]):
            mappings=", ".join(m["pointer"] for m in param.get("nativeMappings",[])) or param["kind"]
            lines.append(f"| `{entry['client']}.{entry['method']}` | `{param['name']}` | {mappings}; {param['transform'].replace('|','/')} | `{param['finBehavior']}` | {param['units']} |")
    lines += ["","The JSON includes every expanded closed native field for generic params declarations, exact source file hashes/line references, identity requirements and remaining alias limitations. These documents are source audit evidence; read fixtures and financial-family tests separately establish executable behavior. Read-only live and native Touch ID evidence do not follow from this audit."]
    (DOCS/"unified-options.md").write_text("\n".join(lines)+"\n")
    print(json.dumps(output["totals"]))


if __name__=="__main__":main()
