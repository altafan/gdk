use bitcoin::blockdata::script::{Builder, Script};
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::hash_types::PubkeyHash;
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::{self, All, Message, Secp256k1};
use bitcoin::util::address::Address;
use bitcoin::util::bip143::SighashComponents;
use bitcoin::util::bip32::{ChildNumber, DerivationPath, ExtendedPrivKey, ExtendedPubKey};
use bitcoin::PublicKey;
use elements;
use gdk_common::model::{Balances, GetTransactionsOpt};
use hex;
use log::{debug, info};
use rand::Rng;

use gdk_common::mnemonic::Mnemonic;
use gdk_common::model::{AddressPointer, CreateTransaction, Settings, TransactionMeta};
use gdk_common::network::{ElementsNetwork, Network, NetworkId};
use gdk_common::util::p2shwpkh_script;
use gdk_common::wally::*;

use crate::db::*;
use crate::error::*;
use crate::model::*;

use elements::confidential::{Asset, Nonce, Value};
use gdk_common::be::*;
use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use std::str::FromStr;

#[derive(Debug)]
pub struct WalletCtx {
    pub secp: Secp256k1<All>,
    pub network: Network,
    pub mnemonic: Mnemonic,
    pub db: Forest,
    pub xprv: ExtendedPrivKey,
    pub xpub: ExtendedPubKey,
    pub master_blinding: Option<MasterBlindingKey>,
    pub change_max_deriv: u32,
}

#[derive(Clone)]
pub enum ElectrumUrl {
    Tls(String, bool),
    Plaintext(String),
}

#[derive(Debug)]
pub struct UTXOInfo {
    pub asset: String,
    pub value: u64,
    pub script: Script,
}

impl UTXOInfo {
    fn new(asset: String, value: u64, script: Script) -> Self {
        UTXOInfo {
            asset,
            value,
            script,
        }
    }
}

pub struct WalletData {
    pub utxos: Vec<(BEOutPoint, UTXOInfo)>,
    pub all_txs: BETransactions,
    pub spent: HashSet<BEOutPoint>,
    pub all_scripts: HashSet<Script>,
    pub all_unblinded: HashMap<elements::OutPoint, Unblinded>,
}

impl WalletCtx {
    pub fn new(
        db: Forest,
        mnemonic: Mnemonic,
        network: Network,
        xprv: ExtendedPrivKey,
        xpub: ExtendedPubKey,
        master_blinding: Option<MasterBlindingKey>,
    ) -> Result<Self, Error> {
        Ok(WalletCtx {
            mnemonic,
            db,
            network, // TODO: from db
            secp: Secp256k1::gen_new(),
            xprv,
            xpub,
            master_blinding,
            change_max_deriv: 0,
        })
    }

    pub fn get_mnemonic(&self) -> &Mnemonic {
        &self.mnemonic
    }

    fn derive_address(&self, xpub: &ExtendedPubKey, path: &[u32; 2]) -> Result<BEAddress, Error> {
        let path: Vec<ChildNumber> = path
            .iter()
            .map(|x| ChildNumber::Normal {
                index: *x,
            })
            .collect();
        let derived = xpub.derive_pub(&self.secp, &path)?;
        if self.network.liquid {}
        match self.network.id() {
            NetworkId::Bitcoin(network) => {
                Ok(BEAddress::Bitcoin(Address::p2shwpkh(&derived.public_key, network)))
            }
            NetworkId::Elements(network) => {
                let master_blinding_key = self
                    .master_blinding
                    .as_ref()
                    .expect("we are in elements but master blinding is None");
                let script = p2shwpkh_script(&derived.public_key);
                let blinding_key =
                    asset_blinding_key_to_ec_private_key(&master_blinding_key, &script);
                let public_key = ec_public_key_from_private_key(blinding_key);
                let blinder = Some(public_key);
                let addr = elements::Address::p2shwpkh(
                    &derived.public_key,
                    blinder,
                    address_params(network),
                );

                Ok(BEAddress::Elements(addr))
            }
        }
    }

    pub fn get_settings(&self) -> Result<Settings, Error> {
        Ok(self.db.get_settings()?.unwrap_or_default())
    }

    pub fn change_settings(&self, settings: &Settings) -> Result<(), Error> {
        self.db.insert_settings(settings)
    }

    pub fn list_tx(&self, opt: &GetTransactionsOpt) -> Result<Vec<TransactionMeta>, Error> {
        info!("start list_tx");
        let (_, all_txs) = self.db.get_all_spent_and_txs()?;
        let all_scripts = self.db.get_all_scripts()?;
        let all_unblinded = self.db.get_all_unblinded()?; // empty map if not liquid

        let mut txs = vec![];
        let mut my_txids = self.db.get_my()?;
        my_txids.sort_by(|a, b| b.1.unwrap_or(std::u32::MAX).cmp(&a.1.unwrap_or(std::u32::MAX)));

        for (tx_id, height) in my_txids.iter().skip(opt.first).take(opt.count) {
            info!("tx_id {}", tx_id);

            let tx = all_txs.get(tx_id).ok_or_else(fn_err("no tx"))?;
            let header = height
                .map(|h| self.db.get_header(h)?.ok_or_else(fn_err("no header")))
                .transpose()?;

            let fee = tx.fee(&all_txs, &all_unblinded);
            let satoshi = tx.my_balances(
                &all_txs,
                &all_scripts,
                &all_unblinded,
                self.network.policy_asset.as_ref(),
            );

            let negatives = satoshi.iter().filter(|(_, v)| **v < 0).count();
            let positives = satoshi.iter().filter(|(_, v)| **v > 0).count();
            let type_ = match (positives > negatives, tx.is_redeposit(&all_scripts)) {
                (_, true) => "redeposit",
                (true, false ) => "incoming",
                (false, false ) => "outgoing",
            };

            let tx_meta = TransactionMeta::new(
                tx.clone(),
                *height,
                header.map(|h| h.time()),
                satoshi,
                fee,
                self.network.id().get_bitcoin_network().unwrap_or(bitcoin::Network::Bitcoin),
                type_.to_string(),
            );

            txs.push(tx_meta);
        }

        Ok(txs)
    }

    fn utxos(&self) -> Result<WalletData, Error> {
        info!("start utxos");
        let (spent, all_txs) = self.db.get_all_spent_and_txs()?;
        let all_scripts = self.db.get_all_scripts()?;
        let all_unblinded = self.db.get_all_unblinded()?; // empty map if not liquid

        let mut utxos = vec![];
        for tx_id in self.db.get_only_txids()? {
            let tx = all_txs.get(&tx_id).ok_or_else(fn_err("no tx"))?;
            let tx_utxos: Vec<(BEOutPoint, UTXOInfo)> = match tx {
                BETransaction::Bitcoin(tx) => tx
                    .output
                    .clone()
                    .into_iter()
                    .enumerate()
                    .map(|(vout, output)| (BEOutPoint::new_bitcoin(tx.txid(), vout as u32), output))
                    .filter(|(_, output)| all_scripts.contains(&output.script_pubkey))
                    .filter(|(outpoint, _)| !spent.contains(&outpoint))
                    .map(|(outpoint, output)| {
                        (
                            outpoint,
                            UTXOInfo::new("btc".to_string(), output.value, output.script_pubkey),
                        )
                    })
                    .collect(),
                BETransaction::Elements(tx) => tx
                    .output
                    .clone()
                    .into_iter()
                    .enumerate()
                    .map(|(vout, output)| {
                        (BEOutPoint::new_elements(tx.txid(), vout as u32), output)
                    })
                    .filter(|(_, output)| all_scripts.contains(&output.script_pubkey))
                    .filter(|(outpoint, _)| !spent.contains(&outpoint))
                    .filter_map(|(outpoint, output)| {
                        if let BEOutPoint::Elements(el_outpoint) = outpoint {
                            if let Some(unblinded) = all_unblinded.get(&el_outpoint) {
                                return Some((
                                    outpoint,
                                    UTXOInfo::new(
                                        unblinded.asset_hex(self.network.policy_asset.as_ref()),
                                        unblinded.value,
                                        output.script_pubkey,
                                    ),
                                ));
                            }
                        }
                        None
                    })
                    .collect(),
            };
            utxos.extend(tx_utxos);
        }
        utxos.sort_by(|a, b| (b.1).value.cmp(&(a.1).value));

        let result = WalletData {
            utxos,
            all_unblinded,
            all_txs,
            all_scripts,
            spent,
        };
        Ok(result)
    }

    pub fn balance(&self) -> Result<Balances, Error> {
        info!("start balance");
        let mut result = HashMap::new();
        result.entry("btc".to_string()).or_insert(0);
        for (_, info) in self.utxos()?.utxos.iter() {
            let asset_btc = if Some(&info.asset) == self.network.policy_asset.as_ref() {
                "btc".to_string()
            } else {
                info.asset.clone()
            };
            *result.entry(asset_btc).or_default() += info.value as i64;
        }
        Ok(result)
    }

    pub fn create_tx(&self, request: &mut CreateTransaction) -> Result<TransactionMeta, Error> {
        info!("create_tx {:?}", request);

        // eagerly check for address validity
        for address in request.addressees.iter().map(|a| &a.address) {
            match self.network.id() {
                NetworkId::Bitcoin(_) => bitcoin::Address::from_str(address)
                    .map_err(|_| Error::InvalidAddress)
                    .map(|_| ())?,
                NetworkId::Elements(_) => elements::Address::from_str(address)
                    .map_err(|_| Error::InvalidAddress)
                    .map(|_| ())?,
            }
        }

        if request.addressees.is_empty() {
            return Err(Error::EmptyAddressees);
        }

        if !request.send_all.unwrap_or(false) && request.addressees.iter().any(|a| a.satoshi == 0) {
            return Err(Error::InvalidAmount);
        }

        // convert from satoshi/kbyte to satoshi/byte
        let fee_rate = (request.fee_rate.unwrap_or(1000) as f64) / 1000.0;
        info!("target fee_rate {:?} satoshi/byte", fee_rate);

        let wallet_data = self.utxos()?;
        let utxos = wallet_data.utxos;
        info!("utxos len:{}", utxos.len());

        if request.send_all.unwrap_or(false) {
            info!("send_all calculating total_amount");
            if request.addressees.len() != 1 {
                return Err(Error::SendAll);
            }
            let mut test_request = request.clone();
            let address_amount = test_request.addressees[0].clone();
            let asset = address_amount.asset_tag.as_deref().unwrap_or("btc");
            let total_amount: u64 =
                utxos.iter().filter(|(_, i)| i.asset == asset).map(|(_, i)| i.value).sum();
            info!("asset: {} total_amount:{}", asset, total_amount);

            test_request.send_all = Some(false);
            test_request.addressees[0].satoshi = total_amount;
            loop {
                let mut r = test_request.clone();
                match self.create_tx(&mut r) {
                    Err(Error::InsufficientFunds) => {
                        // cannot use deterministic step otherwise the fee will identify the wallet
                        // note that this value is ok because under the dust value and we will create no change
                        let step: u64 = rand::thread_rng().gen_range(25, 75);
                        test_request.addressees[0].satoshi = test_request.addressees[0]
                            .satoshi
                            .checked_sub(step)
                            .ok_or_else(|| Error::SendAll)?
                    }
                    _ => break,
                }
            }
            request.addressees[0].satoshi = test_request.addressees[0].satoshi;
        }

        let mut tx = BETransaction::new(self.network.id());
        let policy_asset = self.network.policy_asset().ok();

        let mut fee_val = match self.network.id() {
            // last output is not consider for fee dynamic calculation, consider it here
            NetworkId::Bitcoin(_) => (70.0 * fee_rate) as u64,
            NetworkId::Elements(_) => (1200.0 * fee_rate) as u64,
        };

        let mut outgoing_map: HashMap<String, u64> = HashMap::new();
        let mut remap: HashMap<String, AssetId> = HashMap::new();
        if let Some(btc_asset) = policy_asset {
            remap.insert("btc".to_string(), btc_asset);
        }
        outgoing_map.insert("btc".to_string(), 0);

        let calc_fee_bytes = |bytes| ((bytes as f64) * fee_rate) as u64;
        fee_val += calc_fee_bytes(tx.get_weight() / 4);

        for out in request.addressees.iter() {
            let asset = out.asset().or(policy_asset);
            let len = tx
                .add_output(&out.address, out.satoshi, asset)
                .map_err(|_| Error::InvalidAddress)?;
            fee_val += calc_fee_bytes(len);

            let asset_hex = if asset == policy_asset {
                "btc".to_string()
            } else {
                out.asset_tag.as_ref().unwrap_or(&"btc".to_string()).to_string()
            };
            *outgoing_map.entry(asset_hex.clone()).or_default() += out.satoshi;
            if let Some(asset) = asset {
                remap.insert(asset_hex, asset);
            }
        }
        info!("{:?}", outgoing_map);

        let mut outgoing: Vec<(String, u64)> = outgoing_map.into_iter().collect();
        outgoing.sort_by(|a, b| b.0.len().cmp(&a.0.len())); // just want "btc" as last
        info!("outgoing sorted:{:?}", outgoing);
        for (asset, outgoing) in outgoing.iter() {
            info!("doing {} out:{}", asset, outgoing);
            let mut utxos: Vec<&(BEOutPoint, UTXOInfo)> =
                utxos.iter().filter(|(_, i)| &i.asset == asset).collect();
            utxos.sort_by(|a, b| (a.1).value.cmp(&(b.1).value));
            info!("filtered {} utxos:{:?}", asset, utxos);

            let mut selected_amount = 0u64;
            let mut needed = if asset == "btc" {
                *outgoing + fee_val
            } else {
                *outgoing
            };
            while selected_amount < needed {
                info!(
                    "selected_amount:{} outgoing:{} fee_val:{}",
                    selected_amount, outgoing, fee_val
                );
                let option = utxos.pop();

                info!("pop is: {:?}", option);
                let utxo = option.ok_or(Error::InsufficientFunds)?;
                info!("popped out utxo: {:?}", utxo);

                // UTXO with same script should be spent together
                let mut same_script_utxo = vec![];
                for other_utxo in utxos.iter() {
                    if (other_utxo.1).script == (utxo.1).script {
                        same_script_utxo.push(other_utxo.clone());
                    }
                }
                utxos.retain(|(_, i)| i.script != utxo.1.script);
                same_script_utxo.push(utxo);

                for (outpoint, info) in same_script_utxo {
                    let len = tx.add_input(outpoint.clone());
                    fee_val += calc_fee_bytes(len + 70); // TODO: adjust 70 based on the signature size

                    selected_amount += info.value;
                    needed = if asset == "btc" {
                        *outgoing + fee_val
                    } else {
                        *outgoing
                    };
                }
            }
            info!("selected_amount {} outgoing {} fee_val {}", selected_amount, outgoing, fee_val);
            let mut change_val = selected_amount - outgoing;
            if asset == "btc" {
                change_val -= fee_val;
            }
            info!("change val for {} is {}", asset, change_val);
            let min_change = match self.network.id() {
                NetworkId::Bitcoin(_) => 546,
                NetworkId::Elements(_) => {
                    if asset == "btc" {
                        // from a purely privacy perspective could make sense to always create the change output in liquid, so min change = 0
                        // however elements core use the dust anyway for 2 reasons: rebasing from core and economical considerations
                        // another reason, specific to this wallet, is that the send_all algorithm could reason in steps greater than 1, making it not too slow
                        546
                    } else {
                        // Assets should always create change, cause 1 satoshi could represent 1 house
                        0
                    }
                }
            };
            if change_val > min_change {
                if request.send_all.unwrap_or(false)
                    && asset == request.addressees[0].asset_tag.as_deref().unwrap_or("btc")
                {
                    return Err(Error::SendAll);
                }
                let change_index = self.db.get_index(Index::Internal)? + 1;
                let change_address =
                    self.derive_address(&self.xpub, &[1, change_index])?.to_string();
                info!("adding change {:?}", change_address);

                let len = tx.add_output(&change_address, change_val, remap.get(asset).cloned())?;
                fee_val += calc_fee_bytes(len);
            }
        }

        tx.scramble();

        let fee_val = tx.fee(&wallet_data.all_txs, &wallet_data.all_unblinded); // recompute exact fee_val from built tx

        tx.add_fee_if_elements(fee_val, policy_asset);

        let len = tx.serialize().len();
        let rate = fee_val as f64 / len as f64;
        info!("created tx fee {:?} size: {} rate: {}", fee_val, len, rate);

        let mut satoshi = tx.my_balances(
            &wallet_data.all_txs,
            &wallet_data.all_scripts,
            &wallet_data.all_unblinded,
            self.network.policy_asset.as_ref(),
        );
        for (_, v) in satoshi.iter_mut() {
            *v = v.abs();
        }

        let mut created_tx = TransactionMeta::new(
            tx,
            None,
            None,
            satoshi,
            fee_val,
            self.network.id().get_bitcoin_network().unwrap_or(bitcoin::Network::Bitcoin),
            "outgoing".to_string(),
        );
        created_tx.create_transaction = Some(request.clone());
        info!("returning: {:?}", created_tx);

        Ok(created_tx)
    }

    // TODO when we can serialize psbt
    //pub fn sign(&self, psbt: PartiallySignedTransaction) -> Result<PartiallySignedTransaction, Error> { Err(Error::Generic("NotImplemented".to_string())) }

    fn internal_sign(
        &self,
        tx: &Transaction,
        input_index: usize,
        path: &DerivationPath,
        value: u64,
    ) -> (PublicKey, Vec<u8>) {
        let privkey = self.xprv.derive_priv(&self.secp, &path).unwrap();
        let pubkey = ExtendedPubKey::from_private(&self.secp, &privkey);

        let witness_script = Address::p2pkh(&pubkey.public_key, pubkey.network).script_pubkey();

        let hash =
            SighashComponents::new(tx).sighash_all(&tx.input[input_index], &witness_script, value);

        let signature = self
            .secp
            .sign(&Message::from_slice(&hash.into_inner()[..]).unwrap(), &privkey.private_key.key);

        //let mut signature = signature.serialize_der().to_vec();
        let mut signature = hex::decode(&format!("{:?}", signature)).unwrap();
        signature.push(0x01 as u8); // TODO how to properly do this?

        (pubkey.public_key, signature)
    }

    pub fn sign(&self, request: &TransactionMeta) -> Result<TransactionMeta, Error> {
        info!("sign");

        match &request.transaction {
            BETransaction::Bitcoin(tx) => {
                let mut out_tx = tx.clone();

                for i in 0..tx.input.len() {
                    let prev_output = tx.input[i].previous_output.clone();
                    info!("input#{} prev_output:{:?}", i, prev_output);
                    let prev_tx = self
                        .db
                        .get_bitcoin_tx(&prev_output.txid)?
                        .ok_or_else(|| Error::Generic("cannot find tx in db".into()))?;
                    let out = prev_tx.output[prev_output.vout as usize].clone();
                    let derivation_path = self
                        .db
                        .get_path(&out.script_pubkey)?
                        .ok_or_else(|| Error::Generic("can't find derivation path".into()))?
                        .to_derivation_path()?;
                    info!(
                        "input#{} prev_output:{:?} derivation_path:{:?}",
                        i, prev_output, derivation_path
                    );

                    let (pk, sig) = self.internal_sign(&tx, i, &derivation_path, out.value);
                    let script_sig = script_sig(&pk);
                    let witness = vec![sig, pk.to_bytes()];

                    out_tx.input[i].script_sig = script_sig;
                    out_tx.input[i].witness = witness;
                }
                let tx = BETransaction::Bitcoin(out_tx);
                info!("transaction final size is {}", tx.serialize().len());
                let wgtx: TransactionMeta = tx.into();
                self.db.increment_index(Index::Internal)?;

                Ok(wgtx)
            }
            BETransaction::Elements(tx) => {
                let mut out_tx = tx.clone();
                self.blind_tx(&mut out_tx)?;

                for idx in 0..out_tx.input.len() {
                    let prev_output = out_tx.input[idx].previous_output.clone();
                    info!("input#{} prev_output:{:?}", idx, prev_output);
                    let prev_tx = self
                        .db
                        .get_liquid_tx(&prev_output.txid)?
                        .ok_or_else(|| Error::Generic("cannot find tx in db".into()))?;
                    let out = prev_tx.output[prev_output.vout as usize].clone();
                    let derivation_path = self
                        .db
                        .get_path(&out.script_pubkey)?
                        .ok_or_else(|| Error::Generic("can't find derivation path".into()))?
                        .to_derivation_path()?;

                    let privkey = self.xprv.derive_priv(&self.secp, &derivation_path).unwrap();
                    let pubkey = ExtendedPubKey::from_private(&self.secp, &privkey);
                    let el_net = self.network.id().get_elements_network().unwrap();
                    let script_code =
                        elements::Address::p2pkh(&pubkey.public_key, None, address_params(el_net))
                            .script_pubkey();
                    let sighash = tx_get_elements_signature_hash(
                        &out_tx,
                        idx,
                        &script_code,
                        &out.value,
                        bitcoin::SigHashType::All.as_u32(),
                        true, // segwit
                    );
                    let msg = secp256k1::Message::from_slice(&sighash[..]).unwrap();
                    let mut signature =
                        self.secp.sign(&msg, &privkey.private_key.key).serialize_der().to_vec();
                    signature.push(0x01);

                    let redeem_script = script_sig(&pubkey.public_key);
                    out_tx.input[idx].script_sig = redeem_script;
                    out_tx.input[idx].witness.script_witness =
                        vec![signature, pubkey.public_key.to_bytes()];
                }
                let fee: u64 =
                    out_tx.output.iter().filter(|o| o.is_fee()).map(|o| o.minimum_value()).sum();
                let tx = BETransaction::Elements(out_tx);
                info!("transaction final size is {} fee is {}", tx.serialize().len(), fee);
                let wgtx: TransactionMeta = tx.into();
                self.db.increment_index(Index::Internal)?;

                Ok(wgtx)
            }
        }
    }

    fn blind_tx(&self, tx: &mut elements::Transaction) -> Result<(), Error> {
        info!("blind_tx {}", tx.txid());
        let mut input_assets = vec![];
        let mut input_abfs = vec![];
        let mut input_vbfs = vec![];
        let mut input_ags = vec![];
        let mut input_values = vec![];
        for input in tx.input.iter() {
            info!("input {:?}", input);

            let unblinded = self
                .db
                .get_unblinded(&input.previous_output)?
                .ok_or_else(|| Error::Generic("cannot find unblinded values".into()))?;
            info!(
                "unblinded value: {} asset:{}",
                unblinded.value,
                hex::encode(&unblinded.asset[..])
            );

            input_values.push(unblinded.value);
            input_assets.extend(unblinded.asset.to_vec());
            input_abfs.extend(unblinded.abf.to_vec());
            input_vbfs.extend(unblinded.vbf.to_vec());
            let input_asset = asset_generator_from_bytes(&unblinded.asset, &unblinded.abf);
            input_ags.extend(elements::encode::serialize(&input_asset));
        }

        let random_bytes = rand::thread_rng().gen::<[u8; 32]>().clone();
        //let random_bytes = [11u8; 32];
        let min_value = 1;
        let ct_exp = 0;
        let ct_bits = 52;

        let mut output_blinded_values = vec![];
        for output in tx.output.iter() {
            if !output.is_fee() {
                output_blinded_values.push(output.minimum_value());
            }
        }
        info!("output_blinded_values {:?}", output_blinded_values);
        let mut all_values = vec![];
        all_values.extend(input_values);
        all_values.extend(output_blinded_values);
        let in_num = tx.input.len();
        let out_num = tx.output.len();

        let output_abfs: Vec<Vec<u8>> = (0..out_num - 1).map(|_| random_bytes.to_vec()).collect();
        let mut output_vbfs: Vec<Vec<u8>> =
            (0..out_num - 2).map(|_| random_bytes.to_vec()).collect();

        let mut all_abfs = vec![];
        all_abfs.extend(input_abfs.to_vec());
        all_abfs.extend(output_abfs.iter().cloned().flatten().collect::<Vec<u8>>());

        let mut all_vbfs = vec![];
        all_vbfs.extend(input_vbfs.to_vec());
        all_vbfs.extend(output_vbfs.iter().cloned().flatten().collect::<Vec<u8>>());

        let last_vbf = asset_final_vbf(all_values, in_num as u32, all_abfs, all_vbfs);
        output_vbfs.push(last_vbf.to_vec());

        for (i, mut output) in tx.output.iter_mut().enumerate() {
            info!("output {:?}", output);
            if !output.is_fee() {
                match (output.value, output.asset, output.nonce) {
                    (Value::Explicit(value), Asset::Explicit(asset), Nonce::Confidential(_, _)) => {
                        info!("value: {}", value);
                        let nonce = elements::encode::serialize(&output.nonce);
                        let blinding_pubkey = PublicKey::from_slice(&nonce).unwrap();
                        let blinding_key = asset_blinding_key_to_ec_private_key(
                            self.master_blinding.as_ref().unwrap(),
                            &output.script_pubkey,
                        );
                        let blinding_public_key = ec_public_key_from_private_key(blinding_key);
                        let mut output_abf = [0u8; 32];
                        output_abf.copy_from_slice(&(&output_abfs[i])[..]);
                        let mut output_vbf = [0u8; 32];
                        output_vbf.copy_from_slice(&(&output_vbfs[i])[..]);
                        let asset = asset.clone().into_inner();

                        let output_generator = asset_generator_from_bytes(&asset, &output_abf);
                        let output_value_commitment =
                            asset_value_commitment(value, output_vbf, output_generator);

                        let rangeproof = asset_rangeproof(
                            value,
                            blinding_pubkey.key,
                            blinding_key,
                            asset,
                            output_abf,
                            output_vbf,
                            output_value_commitment,
                            &output.script_pubkey,
                            output_generator,
                            min_value,
                            ct_exp,
                            ct_bits,
                        );
                        debug!("asset: {}", hex::encode(&asset));
                        debug!("output_abf: {}", hex::encode(&output_abf));
                        debug!(
                            "output_generator: {}",
                            hex::encode(&elements::encode::serialize(&output_generator))
                        );
                        debug!("random_bytes: {}", hex::encode(&random_bytes));
                        debug!("input_assets: {}", hex::encode(&input_assets));
                        debug!("input_abfs: {}", hex::encode(&input_abfs));
                        debug!("input_ags: {}", hex::encode(&input_ags));
                        debug!("in_num: {}", in_num);

                        let surjectionproof = asset_surjectionproof(
                            asset,
                            output_abf,
                            output_generator,
                            random_bytes,
                            &input_assets,
                            &input_abfs,
                            &input_ags,
                            in_num,
                        );
                        debug!("surjectionproof: {}", hex::encode(&surjectionproof));

                        let bytes = blinding_public_key.serialize();
                        let byte32: [u8; 32] = bytes[1..].as_ref().try_into().unwrap();
                        output.nonce =
                            elements::confidential::Nonce::Confidential(bytes[0], byte32);
                        output.asset = output_generator;
                        output.value = output_value_commitment;
                        output.witness.surjection_proof = surjectionproof;
                        output.witness.rangeproof = rangeproof;
                    }
                    _ => panic!("create_tx created things not right"),
                }
            }
        }
        Ok(())
    }

    pub fn validate_address(&self, _address: Address) -> Result<bool, Error> {
        // if we managed to get here it means that the address is already valid.
        // only other thing we can check is if it the network is right.

        // TODO implement for both Liquid and Bitcoin address
        //Ok(address.network == self.network)
        unimplemented!("validate not implemented");
    }

    pub fn poll(&self, _xpub: WGExtendedPubKey) -> Result<(), Error> {
        Ok(())
    }

    pub fn get_address(&self) -> Result<AddressPointer, Error> {
        let pointer = self.db.increment_index(Index::External)?;
        let address = self.derive_address(&self.xpub, &[0, pointer])?.to_string();
        Ok(AddressPointer {
            address,
            pointer,
        })
    }
    pub fn xpub_from_xprv(&self, xprv: WGExtendedPrivKey) -> Result<WGExtendedPubKey, Error> {
        Ok(WGExtendedPubKey {
            xpub: ExtendedPubKey::from_private(&self.secp, &xprv.xprv),
        })
    }

    pub fn generate_xprv(&self) -> Result<WGExtendedPrivKey, Error> {
        let random_bytes = rand::thread_rng().gen::<[u8; 32]>();

        Ok(WGExtendedPrivKey {
            xprv: ExtendedPrivKey::new_master(
                self.network.id().get_bitcoin_network().unwrap(),
                &random_bytes,
            )?, // TODO support LIQUID
        })
    }

    pub fn get_asset_icons(&self) -> Result<Option<serde_json::Value>, Error> {
        self.db.get_asset_icons()
    }
    pub fn get_asset_registry(&self) -> Result<Option<serde_json::Value>, Error> {
        self.db.get_asset_registry()
    }
}

fn address_params(net: ElementsNetwork) -> &'static elements::AddressParams {
    match net {
        ElementsNetwork::Liquid => &elements::AddressParams::LIQUID,
        ElementsNetwork::ElementsRegtest => &elements::AddressParams::ELEMENTS,
    }
}

fn script_sig(public_key: &PublicKey) -> Script {
    let internal = Builder::new()
        .push_int(0)
        .push_slice(&PubkeyHash::hash(&public_key.to_bytes())[..])
        .into_script();
    Builder::new().push_slice(internal.as_bytes()).into_script()
}

#[cfg(test)]
mod test {
    use crate::interface::script_sig;
    use bitcoin::consensus::deserialize;
    use bitcoin::hashes::hash160;
    use bitcoin::hashes::Hash;
    use bitcoin::secp256k1::{All, Message, Secp256k1, SecretKey};
    use bitcoin::util::bip143::SighashComponents;
    use bitcoin::util::bip32::{ChildNumber, ExtendedPrivKey, ExtendedPubKey};
    use bitcoin::util::key::PrivateKey;
    use bitcoin::util::key::PublicKey;
    use bitcoin::Script;
    use bitcoin::{Address, Network, Transaction};
    use std::str::FromStr;

    fn p2pkh_hex(pk: &str) -> (PublicKey, Script) {
        let pk = hex::decode(pk).unwrap();
        let pk = PublicKey::from_slice(pk.as_slice()).unwrap();
        let witness_script = Address::p2pkh(&pk, Network::Bitcoin).script_pubkey();
        (pk, witness_script)
    }

    #[test]
    fn test_bip() {
        let secp: Secp256k1<All> = Secp256k1::gen_new();

        // https://github.com/bitcoin/bips/blob/master/bip-0143.mediawiki#p2sh-p2wpkh
        let tx_bytes = hex::decode("0100000001db6b1b20aa0fd7b23880be2ecbd4a98130974cf4748fb66092ac4d3ceb1a54770100000000feffffff02b8b4eb0b000000001976a914a457b684d7f0d539a46a45bbc043f35b59d0d96388ac0008af2f000000001976a914fd270b1ee6abcaea97fea7ad0402e8bd8ad6d77c88ac92040000").unwrap();
        let tx: Transaction = deserialize(&tx_bytes).unwrap();

        let private_key_bytes =
            hex::decode("eb696a065ef48a2192da5b28b694f87544b30fae8327c4510137a922f32c6dcf")
                .unwrap();

        let key = SecretKey::from_slice(&private_key_bytes).unwrap();
        let private_key = PrivateKey {
            compressed: true,
            network: Network::Testnet,
            key,
        };

        let (public_key, witness_script) =
            p2pkh_hex("03ad1d8e89212f0b92c74d23bb710c00662ad1470198ac48c43f7d6f93a2a26873");
        assert_eq!(
            hex::encode(witness_script.to_bytes()),
            "76a91479091972186c449eb1ded22b78e40d009bdf008988ac"
        );
        let value = 1_000_000_000;
        let comp = SighashComponents::new(&tx);
        let hash = comp.sighash_all(&tx.input[0], &witness_script, value).into_inner();

        assert_eq!(
            &hash[..],
            &hex::decode("64f3b0f4dd2bb3aa1ce8566d220cc74dda9df97d8490cc81d89d735c92e59fb6")
                .unwrap()[..],
        );

        let signature = secp.sign(&Message::from_slice(&hash[..]).unwrap(), &private_key.key);

        //let mut signature = signature.serialize_der().to_vec();
        let signature_hex = format!("{:?}01", signature); // add sighash type at the end
        assert_eq!(signature_hex, "3044022047ac8e878352d3ebbde1c94ce3a10d057c24175747116f8288e5d794d12d482f0220217f36a485cae903c713331d877c1f64677e3622ad4010726870540656fe9dcb01");

        let script_sig = script_sig(&public_key);

        assert_eq!(
            format!("{}", hex::encode(script_sig.as_bytes())),
            "16001479091972186c449eb1ded22b78e40d009bdf0089"
        );
    }

    #[test]
    fn test_my_tx() {
        let secp: Secp256k1<All> = Secp256k1::gen_new();
        let xprv = ExtendedPrivKey::from_str("tprv8jdzkeuCYeH5hi8k2JuZXJWV8sPNK62ashYyUVD9Euv5CPVr2xUbRFEM4yJBB1yBHZuRKWLeWuzH4ptmvSgjLj81AvPc9JhV4i8wEfZYfPb").unwrap();
        let xpub = ExtendedPubKey::from_private(&secp, &xprv);
        let private_key = xprv.private_key;
        let public_key = xpub.public_key;
        let public_key_bytes = public_key.to_bytes();
        let public_key_str = format!("{}", hex::encode(&public_key_bytes));

        let address = Address::p2shwpkh(&public_key, Network::Testnet);
        assert_eq!(format!("{}", address), "2NCEMwNagVAbbQWNfu7M7DNGxkknVTzhooC");

        assert_eq!(
            public_key_str,
            "0386fe0922d694cef4fa197f9040da7e264b0a0ff38aa2e647545e5a6d6eab5bfc"
        );
        let tx_hex = "020000000001010e73b361dd0f0320a33fd4c820b0c7ac0cae3b593f9da0f0509cc35de62932eb01000000171600141790ee5e7710a06ce4a9250c8677c1ec2843844f0000000002881300000000000017a914cc07bc6d554c684ea2b4af200d6d988cefed316e87a61300000000000017a914fda7018c5ee5148b71a767524a22ae5d1afad9a9870247304402206675ed5fb86d7665eb1f7950e69828d0aa9b41d866541cedcedf8348563ba69f022077aeabac4bd059148ff41a36d5740d83163f908eb629784841e52e9c79a3dbdb01210386fe0922d694cef4fa197f9040da7e264b0a0ff38aa2e647545e5a6d6eab5bfc00000000";

        let tx_bytes = hex::decode(tx_hex).unwrap();
        let tx: Transaction = deserialize(&tx_bytes).unwrap();

        let (_, witness_script) = p2pkh_hex(&public_key_str);
        assert_eq!(
            hex::encode(witness_script.to_bytes()),
            "76a9141790ee5e7710a06ce4a9250c8677c1ec2843844f88ac"
        );
        let value = 10_202;
        let comp = SighashComponents::new(&tx);
        let hash = comp.sighash_all(&tx.input[0], &witness_script, value);

        assert_eq!(
            &hash.into_inner()[..],
            &hex::decode("58b15613fc1701b2562430f861cdc5803531d08908df531082cf1828cd0b8995")
                .unwrap()[..],
        );

        let signature = secp.sign(&Message::from_slice(&hash[..]).unwrap(), &private_key.key);

        //let mut signature = signature.serialize_der().to_vec();
        let signature_hex = format!("{:?}01", signature); // add sighash type at the end
        let signature = hex::decode(&signature_hex).unwrap();

        assert_eq!(signature_hex, "304402206675ed5fb86d7665eb1f7950e69828d0aa9b41d866541cedcedf8348563ba69f022077aeabac4bd059148ff41a36d5740d83163f908eb629784841e52e9c79a3dbdb01");
        assert_eq!(tx.input[0].witness[0], signature);
        assert_eq!(tx.input[0].witness[1], public_key_bytes);

        let script_sig = script_sig(&public_key);
        assert_eq!(tx.input[0].script_sig, script_sig);
    }
}