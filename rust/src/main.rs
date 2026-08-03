use bitcoincore_rpc::{Auth, Client as BitcoinClient, RpcApi};
use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::fs;
use std::thread;
use std::time::Duration;

/// Call Alice's Lightning node via CLN REST API on port 3010
fn call_alice_ln(method: &str, params: Value) -> Result<Value, Box<dyn std::error::Error>> {
    let rune = std::env::var("ALICE_RUNE")?;
    let url = format!("http://localhost:3010/v1/{}", method);

    let client = Client::new();
    let response = client
        .post(&url)
        .json(&params)
        .header("Rune", rune)
        .send()?
        .json::<Value>()?;

    Ok(response)
}

/// Call Bob's Lightning node via CLN REST API on port 3011
fn call_bob_ln(method: &str, params: Value) -> Result<Value, Box<dyn std::error::Error>> {
    let rune = std::env::var("BOB_RUNE")?;
    let url = format!("http://localhost:3011/v1/{}", method);

    let client = Client::new();
    let response = client
        .post(&url)
        .json(&params)
        .header("Rune", rune)
        .send()?
        .json::<Value>()?;

    Ok(response)
}

fn string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str, Box<dyn std::error::Error>> {
    value[field]
        .as_str()
        .ok_or_else(|| format!("missing string field '{field}' in {value}").into())
}

// CLN serializes msat as numbers, strings, or objects depending on version.
fn msat(value: &Value) -> Result<u64, Box<dyn std::error::Error>> {
    if let Some(number) = value.as_u64() {
        return Ok(number);
    }
    if let Some(text) = value.as_str() {
        return Ok(text.trim_end_matches("msat").parse()?);
    }
    if let Some(number) = value.get("msat").and_then(Value::as_u64) {
        return Ok(number);
    }
    Err(format!("invalid millisatoshi value: {value}").into())
}

fn peer_channel<'a>(response: &'a Value, peer_id: &str) -> Option<&'a Value> {
    response["channels"]
        .as_array()?
        .iter()
        .find(|channel| channel["peer_id"].as_str() == Some(peer_id))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Bitcoin RPC client
    let bitcoin_rpc = BitcoinClient::new(
        "http://localhost:18443",
        Auth::UserPass("alice".to_string(), "password".to_string()),
    )?;

    println!("Blockchain Info: {:?}", bitcoin_rpc.get_blockchain_info()?);

    // Get both node IDs
    let alice_info = call_alice_ln("getinfo", Value::Null)?;
    println!("Alice Node Info: {:?}", alice_info);
    let bob_info = call_bob_ln("getinfo", Value::Null)?;
    println!("Bob Node Info: {:?}", bob_info);
    let alice_id = string_field(&alice_info, "id")?.to_owned();
    let bob_id = string_field(&bob_info, "id")?.to_owned();

    // Connect Alice to Bob as a peer
    if let Err(error) = call_alice_ln(
        "connect",
        json!({"id": bob_id, "host": "bob", "port": 9735}),
    ) {
        let peers = call_alice_ln("listpeers", json!({"id": bob_id}))?;
        let connected = peers["peers"]
            .as_array()
            .is_some_and(|items| items.iter().any(|peer| peer["connected"] == true));
        if !connected {
            return Err(error);
        }
    }

    // Create or load a mining wallet
    let loaded: Vec<String> = bitcoin_rpc.call("listwallets", &[])?;
    if !loaded.iter().any(|wallet| wallet == "miner") {
        let wallets: Value = bitcoin_rpc.call("listwalletdir", &[])?;
        let exists = wallets["wallets"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["name"] == "miner"));
        if exists {
            let _: Value = bitcoin_rpc.call("loadwallet", &[json!("miner")])?;
        } else {
            let _: Value = bitcoin_rpc.call("createwallet", &[json!("miner")])?;
        }
    }
    let miner = BitcoinClient::new(
        "http://localhost:18443/wallet/miner",
        Auth::UserPass("alice".to_string(), "password".to_string()),
    )?;
    let mining_address: String = miner.call("getnewaddress", &[])?;

    // Coinbase rewards mature after 100 confirmations, so mine to block 101
    // before the first mined coin is spendable.
    let height = bitcoin_rpc.get_block_count()?;
    if height < 101 {
        let _: Value = miner.call(
            "generatetoaddress",
            &[json!(101 - height), json!(mining_address)],
        )?;
    }
    println!("Miner balance: {}", miner.get_balance(None, None)?);

    // Create an on-chain address for Alice and send 1 BTC from the mining wallet
    let address_response = call_alice_ln("newaddr", json!({"addresstype": "bech32"}))?;
    let alice_address = address_response["bech32"]
        .as_str()
        .or_else(|| address_response["p2tr"].as_str())
        .ok_or("CLN newaddr returned no address")?;
    let _: String = miner.call("sendtoaddress", &[json!(alice_address), json!(1.0)])?;
    // 6 blocks confirm the funding transaction
    let _: Value = miner.call("generatetoaddress", &[json!(6), json!(mining_address)])?;

    // Wait for Alice to recognize the confirmed deposit
    let mut funded = false;
    for _ in 0..30 {
        let funds = call_alice_ln("listfunds", json!({}))?;
        funded = funds["outputs"].as_array().is_some_and(|outputs| {
            outputs.iter().any(|output| {
                output["status"] == "confirmed"
                    && msat(&output["amount_msat"]).is_ok_and(|amount| amount >= 500_000_000)
            })
        });
        if funded {
            break;
        }
        thread::sleep(Duration::from_secs(1));
    }
    if !funded {
        return Err("Alice did not recognize the confirmed on-chain funds".into());
    }

    // Open a 500,000 satoshi payment channel from Alice to Bob
    call_alice_ln("fundchannel", json!({"id": bob_id, "amount": 500000}))?;
    // Mine 6 blocks to confirm the channel funding transaction
    let _: Value = miner.call("generatetoaddress", &[json!(6), json!(mining_address)])?;

    // Poll until the channel reaches CHANNELD_NORMAL on both sides
    let (alice_channel, bob_channel) = loop {
        let alice_channels = call_alice_ln("listpeerchannels", json!({"id": bob_id}))?;
        let bob_channels = call_bob_ln("listpeerchannels", json!({"id": alice_id}))?;
        let a = peer_channel(&alice_channels, &bob_id).cloned();
        let b = peer_channel(&bob_channels, &alice_id).cloned();
        if let (Some(a), Some(b)) = (a, b) {
            if a["state"] == "CHANNELD_NORMAL" && b["state"] == "CHANNELD_NORMAL" {
                break (a, b);
            }
        }
        thread::sleep(Duration::from_secs(1));
    };

    // Peer counts from both perspectives
    let alice_peers = call_alice_ln("listpeers", json!({}))?;
    let bob_peers = call_bob_ln("listpeers", json!({}))?;
    let alice_peer_count = alice_peers["peers"].as_array().map_or(0, Vec::len);
    let bob_peer_count = bob_peers["peers"].as_array().map_or(0, Vec::len);

    // Channel details
    let channel_id = string_field(&alice_channel, "channel_id")?;
    let funding_txid = string_field(&alice_channel, "funding_txid")?;
    if bob_channel["funding_txid"] != funding_txid {
        return Err("Alice and Bob report different funding transactions".into());
    }
    let total_msat = msat(&alice_channel["total_msat"])?;
    let alice_balance_msat = msat(&alice_channel["to_us_msat"])?;
    let bob_balance_msat = msat(&bob_channel["to_us_msat"])?;

    // Write to out.txt (cargo runs in rust/, so ../ is the project root)
    let report = format!(
        "{alice_id}\n{bob_id}\n{alice_peer_count}\n{bob_peer_count}\n{channel_id}\n{funding_txid}\nCHANNELD_NORMAL\nCHANNELD_NORMAL\n{total_msat}\n{alice_balance_msat}\n{bob_balance_msat}\n"
    );
    fs::write("../out.txt", &report)?;
    fs::write("../output.txt", report)?;
    println!("Report written to out.txt");

    Ok(())
}
