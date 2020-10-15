// Magical Bitcoin Library
// Written in 2020 by
//     Alekos Filini <alekos.filini@gmail.com>
//
// Copyright (c) 2020 Magical Bitcoin
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! Wallet
//!
//! This module defines the [`Wallet`] structure.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::{BTreeMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use bitcoin::consensus::encode::serialize;
use bitcoin::util::bip32::ChildNumber;
use bitcoin::util::psbt::PartiallySignedTransaction as PSBT;
use bitcoin::{Address, Network, OutPoint, Script, SigHashType, Transaction, TxOut, Txid};

use miniscript::descriptor::DescriptorPublicKey;

#[allow(unused_imports)]
use log::{debug, error, info, trace};

pub mod address_validator;
pub mod coin_selection;
pub mod export;
mod rbf;
pub mod signer;
pub mod time;
pub mod tx_builder;
pub(crate) mod utils;

pub use utils::IsDust;

use address_validator::AddressValidator;
use signer::{Signer, SignerId, SignerOrdering, SignersContainer};
use tx_builder::TxBuilder;
use utils::{After, Older};

use crate::blockchain::{Blockchain, BlockchainMarker, OfflineBlockchain, Progress};
use crate::database::{BatchDatabase, BatchOperations, DatabaseUtils};
use crate::descriptor::{
    get_checksum, DescriptorMeta, DescriptorScripts, ExtendedDescriptor, ExtractPolicy, Policy,
    ToWalletDescriptor,
};
use crate::error::Error;
use crate::psbt::PSBTUtils;
use crate::types::*;

const CACHE_ADDR_BATCH_SIZE: u32 = 100;

/// Type alias for a [`Wallet`] that uses [`OfflineBlockchain`]
pub type OfflineWallet<D> = Wallet<OfflineBlockchain, D>;

/// A Bitcoin wallet
///
/// A wallet takes descriptors, a [`database`](crate::database) and a
/// [`blockchain`](crate::blockchain) and implements the basic functions that a Bitcoin wallets
/// needs to operate, like [generating addresses](Wallet::get_new_address), [returning the balance](Wallet::get_balance),
/// [creating transactions](Wallet::create_tx), etc.
///
/// A wallet can be either "online" if the [`blockchain`](crate::blockchain) type provided
/// implements [`Blockchain`], or "offline" [`OfflineBlockchain`] is used. Offline wallets only expose
/// methods that don't need any interaction with the blockchain to work.
pub struct Wallet<B: BlockchainMarker, D: BatchDatabase> {
    descriptor: ExtendedDescriptor,
    change_descriptor: Option<ExtendedDescriptor>,

    signers: Arc<SignersContainer<DescriptorPublicKey>>,
    change_signers: Arc<SignersContainer<DescriptorPublicKey>>,

    address_validators: Vec<Arc<Box<dyn AddressValidator>>>,

    network: Network,

    current_height: Option<u32>,

    client: Option<B>,
    database: RefCell<D>,
}

// offline actions, always available
impl<B, D> Wallet<B, D>
where
    B: BlockchainMarker,
    D: BatchDatabase,
{
    /// Create a new "offline" wallet
    pub fn new_offline<E: ToWalletDescriptor>(
        descriptor: E,
        change_descriptor: Option<E>,
        network: Network,
        mut database: D,
    ) -> Result<Self, Error> {
        let (descriptor, keymap) = descriptor.to_wallet_descriptor(network)?;
        database.check_descriptor_checksum(
            ScriptType::External,
            get_checksum(&descriptor.to_string())?.as_bytes(),
        )?;
        let signers = Arc::new(SignersContainer::from(keymap));
        let (change_descriptor, change_signers) = match change_descriptor {
            Some(desc) => {
                let (change_descriptor, change_keymap) = desc.to_wallet_descriptor(network)?;
                database.check_descriptor_checksum(
                    ScriptType::Internal,
                    get_checksum(&change_descriptor.to_string())?.as_bytes(),
                )?;

                let change_signers = Arc::new(SignersContainer::from(change_keymap));
                // if !parsed.same_structure(descriptor.as_ref()) {
                //     return Err(Error::DifferentDescriptorStructure);
                // }

                (Some(change_descriptor), change_signers)
            }
            None => (None, Arc::new(SignersContainer::new())),
        };

        Ok(Wallet {
            descriptor,
            change_descriptor,
            signers,
            change_signers,
            address_validators: Vec::new(),

            network,

            current_height: None,

            client: None,
            database: RefCell::new(database),
        })
    }

    /// Return a newly generated address using the external descriptor
    pub fn get_new_address(&self) -> Result<Address, Error> {
        let index = self.fetch_and_increment_index(ScriptType::External)?;

        self.descriptor
            .derive(&[ChildNumber::from_normal_idx(index).unwrap()])
            .address(self.network)
            .ok_or(Error::ScriptDoesntHaveAddressForm)
    }

    /// Return whether or not a `script` is part of this wallet (either internal or external)
    pub fn is_mine(&self, script: &Script) -> Result<bool, Error> {
        self.database.borrow().is_mine(script)
    }

    /// Return the list of unspent outputs of this wallet
    ///
    /// Note that this methods only operate on the internal database, which first needs to be
    /// [`Wallet::sync`] manually.
    pub fn list_unspent(&self) -> Result<Vec<UTXO>, Error> {
        self.database.borrow().iter_utxos()
    }

    /// Return the list of transactions made and received by the wallet
    ///
    /// Optionally fill the [`TransactionDetails::transaction`] field with the raw transaction if
    /// `include_raw` is `true`.
    ///
    /// Note that this methods only operate on the internal database, which first needs to be
    /// [`Wallet::sync`] manually.
    pub fn list_transactions(&self, include_raw: bool) -> Result<Vec<TransactionDetails>, Error> {
        self.database.borrow().iter_txs(include_raw)
    }

    /// Return the balance, meaning the sum of this wallet's unspent outputs' values
    ///
    /// Note that this methods only operate on the internal database, which first needs to be
    /// [`Wallet::sync`] manually.
    pub fn get_balance(&self) -> Result<u64, Error> {
        Ok(self
            .list_unspent()?
            .iter()
            .fold(0, |sum, i| sum + i.txout.value))
    }

    /// Add an external signer
    ///
    /// See [the `signer` module](signer) for an example.
    pub fn add_signer(
        &mut self,
        script_type: ScriptType,
        id: SignerId<DescriptorPublicKey>,
        ordering: SignerOrdering,
        signer: Arc<Box<dyn Signer>>,
    ) {
        let signers = match script_type {
            ScriptType::External => Arc::make_mut(&mut self.signers),
            ScriptType::Internal => Arc::make_mut(&mut self.change_signers),
        };

        signers.add_external(id, ordering, signer);
    }

    /// Add an address validator
    ///
    /// See [the `address_validator` module](address_validator) for an example.
    pub fn add_address_validator(&mut self, validator: Arc<Box<dyn AddressValidator>>) {
        self.address_validators.push(validator);
    }

    /// Create a new transaction following the options specified in the `builder`
    ///
    /// ## Example
    ///
    /// ```no_run
    /// # use std::str::FromStr;
    /// # use bitcoin::*;
    /// # use bdk::*;
    /// # use bdk::database::*;
    /// # let descriptor = "wpkh(tpubD6NzVbkrYhZ4Xferm7Pz4VnjdcDPFyjVu5K4iZXQ4pVN8Cks4pHVowTBXBKRhX64pkRyJZJN5xAKj4UDNnLPb5p2sSKXhewoYx5GbTdUFWq/*)";
    /// # let wallet: OfflineWallet<_> = Wallet::new_offline(descriptor, None, Network::Testnet, MemoryDatabase::default())?;
    /// # let to_address = Address::from_str("2N4eQYCbKUHCCTUjBJeHcJp9ok6J2GZsTDt").unwrap();
    /// let (psbt, details) = wallet.create_tx(
    ///     TxBuilder::with_recipients(vec![(to_address.script_pubkey(), 50_000)])
    /// )?;
    /// // sign and broadcast ...
    /// # Ok::<(), bdk::Error>(())
    /// ```
    pub fn create_tx<Cs: coin_selection::CoinSelectionAlgorithm>(
        &self,
        builder: TxBuilder<Cs>,
    ) -> Result<(PSBT, TransactionDetails), Error> {
        if builder.recipients.is_empty() {
            return Err(Error::NoAddressees);
        }

        // TODO: fetch both internal and external policies
        let policy = self
            .descriptor
            .extract_policy(Arc::clone(&self.signers))?
            .unwrap();
        if policy.requires_path() && builder.policy_path.is_none() {
            return Err(Error::SpendingPolicyRequired);
        }
        let requirements =
            policy.get_condition(builder.policy_path.as_ref().unwrap_or(&BTreeMap::new()))?;
        debug!("requirements: {:?}", requirements);

        let version = match builder.version {
            Some(tx_builder::Version(0)) => {
                return Err(Error::Generic("Invalid version `0`".into()))
            }
            Some(tx_builder::Version(1)) if requirements.csv.is_some() => {
                return Err(Error::Generic(
                    "TxBuilder requested version `1`, but at least `2` is needed to use OP_CSV"
                        .into(),
                ))
            }
            Some(tx_builder::Version(x)) => x,
            None if requirements.csv.is_some() => 2,
            _ => 1,
        };

        let lock_time = match builder.locktime {
            None => requirements.timelock.unwrap_or(0),
            Some(x) if requirements.timelock.is_none() => x,
            Some(x) if requirements.timelock.unwrap() <= x => x,
            Some(x) => return Err(Error::Generic(format!("TxBuilder requested timelock of `{}`, but at least `{}` is required to spend from this script", x, requirements.timelock.unwrap())))
        };

        let n_sequence = match (builder.rbf, requirements.csv) {
            (None, Some(csv)) => csv,
            (Some(rbf), Some(csv)) if rbf < csv => return Err(Error::Generic(format!("Cannot enable RBF with nSequence `{}`, since at least `{}` is required to spend with OP_CSV", rbf, csv))),
            (None, _) if requirements.timelock.is_some() => 0xFFFFFFFE,
            (Some(rbf), _) if rbf >= 0xFFFFFFFE => return Err(Error::Generic("Cannot enable RBF with a nSequence >= 0xFFFFFFFE".into())),
            (Some(rbf), _) => rbf,
            (None, _) => 0xFFFFFFFF,
        };

        let mut tx = Transaction {
            version,
            lock_time,
            input: vec![],
            output: vec![],
        };

        let fee_rate = builder.fee_rate.unwrap_or_default();
        if builder.send_all && builder.recipients.len() != 1 {
            return Err(Error::SendAllMultipleOutputs);
        }

        // we keep it as a float while we accumulate it, and only round it at the end
        let mut fee_amount: f32 = 0.0;
        let mut outgoing: u64 = 0;
        let mut received: u64 = 0;

        let calc_fee_bytes = |wu| (wu as f32) * fee_rate.as_sat_vb() / 4.0;
        fee_amount += calc_fee_bytes(tx.get_weight());

        for (index, (script_pubkey, satoshi)) in builder.recipients.iter().enumerate() {
            let value = match builder.send_all {
                true => 0,
                false if satoshi.is_dust() => return Err(Error::OutputBelowDustLimit(index)),
                false => *satoshi,
            };

            if self.is_mine(script_pubkey)? {
                received += value;
            }

            let new_out = TxOut {
                script_pubkey: script_pubkey.clone(),
                value,
            };
            fee_amount += calc_fee_bytes(serialize(&new_out).len() * 4);

            tx.output.push(new_out);

            outgoing += value;
        }

        // TODO: use the right weight instead of the maximum, and only fall-back to it if the
        // script is unknown in the database
        let input_witness_weight = std::cmp::max(
            self.get_descriptor_for_script_type(ScriptType::Internal)
                .0
                .max_satisfaction_weight(),
            self.get_descriptor_for_script_type(ScriptType::External)
                .0
                .max_satisfaction_weight(),
        );

        if builder.change_policy != tx_builder::ChangeSpendPolicy::ChangeAllowed
            && self.change_descriptor.is_none()
        {
            return Err(Error::Generic(
                "The `change_policy` can be set only if the wallet has a change_descriptor".into(),
            ));
        }

        let (available_utxos, use_all_utxos) = self.get_available_utxos(
            builder.change_policy,
            &builder.utxos,
            &builder.unspendable,
            builder.send_all,
        )?;
        let coin_selection::CoinSelectionResult {
            txin,
            selected_amount,
            mut fee_amount,
        } = builder.coin_selection.coin_select(
            available_utxos,
            use_all_utxos,
            fee_rate,
            outgoing,
            input_witness_weight,
            fee_amount,
        )?;
        let (mut txin, prev_script_pubkeys): (Vec<_>, Vec<_>) = txin.into_iter().unzip();
        // map that allows us to lookup the prev_script_pubkey for a given previous_output
        let prev_script_pubkeys = txin
            .iter()
            .zip(prev_script_pubkeys.into_iter())
            .map(|(txin, script)| (txin.previous_output, script))
            .collect::<HashMap<_, _>>();

        txin.iter_mut().for_each(|i| i.sequence = n_sequence);
        tx.input = txin;

        // prepare the change output
        let change_output = match builder.send_all {
            true => None,
            false => {
                let change_script = self.get_change_address()?;
                let change_output = TxOut {
                    script_pubkey: change_script,
                    value: 0,
                };

                // take the change into account for fees
                fee_amount += calc_fee_bytes(serialize(&change_output).len() * 4);
                Some(change_output)
            }
        };

        let mut fee_amount = fee_amount.ceil() as u64;
        let change_val = selected_amount - outgoing - fee_amount;
        if !builder.send_all && !change_val.is_dust() {
            let mut change_output = change_output.unwrap();
            change_output.value = change_val;
            received += change_val;

            tx.output.push(change_output);
        } else if builder.send_all && !change_val.is_dust() {
            // there's only one output, send everything to it
            tx.output[0].value = change_val;

            // send_all to our address
            if self.is_mine(&tx.output[0].script_pubkey)? {
                received = change_val;
            }
        } else if !builder.send_all && change_val.is_dust() {
            // skip the change output because it's dust, this adds up to the fees
            fee_amount += change_val;
        } else if builder.send_all {
            // send_all but the only output would be below dust limit
            return Err(Error::InsufficientFunds); // TODO: or OutputBelowDustLimit?
        }

        // sort input/outputs according to the chosen algorithm
        builder.ordering.sort_tx(&mut tx);

        let txid = tx.txid();
        let psbt = self.complete_transaction(tx, prev_script_pubkeys, builder)?;

        let transaction_details = TransactionDetails {
            transaction: None,
            txid,
            timestamp: time::get_timestamp(),
            received,
            sent: selected_amount,
            fees: fee_amount,
            height: None,
        };

        Ok((psbt, transaction_details))
    }

    /// Bump the fee of a transaction following the options specified in the `builder`
    ///
    /// Return an error if the transaction is already confirmed or doesn't explicitly signal RBF.
    ///
    /// **NOTE**: if the original transaction was made with [`TxBuilder::send_all`], the same
    /// option must be enabled when bumping its fees to correctly reduce the only output's value to
    /// increase the fees.
    ///
    /// ## Example
    ///
    /// ```no_run
    /// # use std::str::FromStr;
    /// # use bitcoin::*;
    /// # use bdk::*;
    /// # use bdk::database::*;
    /// # let descriptor = "wpkh(tpubD6NzVbkrYhZ4Xferm7Pz4VnjdcDPFyjVu5K4iZXQ4pVN8Cks4pHVowTBXBKRhX64pkRyJZJN5xAKj4UDNnLPb5p2sSKXhewoYx5GbTdUFWq/*)";
    /// # let wallet: OfflineWallet<_> = Wallet::new_offline(descriptor, None, Network::Testnet, MemoryDatabase::default())?;
    /// let txid = Txid::from_str("faff0a466b70f5d5f92bd757a92c1371d4838bdd5bc53a06764e2488e51ce8f8").unwrap();
    /// let (psbt, details) = wallet.bump_fee(
    ///     &txid,
    ///     TxBuilder::new().fee_rate(FeeRate::from_sat_per_vb(5.0)),
    /// )?;
    /// // sign and broadcast ...
    /// # Ok::<(), bdk::Error>(())
    /// ```
    // TODO: support for merging multiple transactions while bumping the fees
    // TODO: option to force addition of an extra output? seems bad for privacy to update the
    // change
    pub fn bump_fee<Cs: coin_selection::CoinSelectionAlgorithm>(
        &self,
        txid: &Txid,
        builder: TxBuilder<Cs>,
    ) -> Result<(PSBT, TransactionDetails), Error> {
        let mut details = match self.database.borrow().get_tx(&txid, true)? {
            None => return Err(Error::TransactionNotFound),
            Some(tx) if tx.transaction.is_none() => return Err(Error::TransactionNotFound),
            Some(tx) if tx.height.is_some() => return Err(Error::TransactionConfirmed),
            Some(tx) => tx,
        };
        let mut tx = details.transaction.take().unwrap();
        if !tx.input.iter().any(|txin| txin.sequence <= 0xFFFFFFFD) {
            return Err(Error::IrreplaceableTransaction);
        }

        // the new tx must "pay for its bandwidth"
        let vbytes = tx.get_weight() as f32 / 4.0;
        let required_feerate = FeeRate::from_sat_per_vb(details.fees as f32 / vbytes + 1.0);
        let new_feerate = builder.fee_rate.unwrap_or_default();

        if new_feerate < required_feerate {
            return Err(Error::FeeRateTooLow {
                required: required_feerate,
            });
        }
        let mut fee_difference =
            (new_feerate.as_sat_vb() * tx.get_weight() as f32 / 4.0).ceil() as u64 - details.fees;

        if builder.send_all && tx.output.len() > 1 {
            return Err(Error::SendAllMultipleOutputs);
        }

        // find the index of the output that we can update. either the change or the only one if
        // it's `send_all`
        let updatable_output = if builder.send_all {
            0
        } else {
            let mut change_output = None;
            for (index, txout) in tx.output.iter().enumerate() {
                // look for an output that we know and that has the right ScriptType. We use
                // `get_deget_descriptor_for` to find what's the ScriptType for `Internal`
                // addresses really is, because if there's no change_descriptor it's actually equal
                // to "External"
                let (_, change_type) = self.get_descriptor_for_script_type(ScriptType::Internal);
                match self
                    .database
                    .borrow()
                    .get_path_from_script_pubkey(&txout.script_pubkey)?
                {
                    Some((script_type, _)) if script_type == change_type => {
                        change_output = Some(index);
                        break;
                    }
                    _ => {}
                }
            }

            // we need a change output, add one here and take into account the extra fees for it
            if change_output.is_none() {
                let change_script = self.get_change_address()?;
                let change_txout = TxOut {
                    script_pubkey: change_script,
                    value: 0,
                };
                fee_difference +=
                    (serialize(&change_txout).len() as f32 * new_feerate.as_sat_vb()).ceil() as u64;
                tx.output.push(change_txout);

                change_output = Some(tx.output.len() - 1);
            }

            change_output.unwrap()
        };

        // if `builder.utxos` is Some(_) we have to add inputs and we skip down to the last branch
        match tx.output[updatable_output]
            .value
            .checked_sub(fee_difference)
        {
            Some(new_value) if !new_value.is_dust() && builder.utxos.is_none() => {
                // try to reduce the "updatable output" amount
                tx.output[updatable_output].value = new_value;
                if self.is_mine(&tx.output[updatable_output].script_pubkey)? {
                    details.received -= fee_difference;
                }

                details.fees += fee_difference;
            }
            _ if builder.send_all && builder.utxos.is_none() => {
                // if the tx is "send_all" it doesn't make sense to either remove the only output
                // or add more inputs
                return Err(Error::InsufficientFunds);
            }
            _ => {
                // initially always remove the change output
                let mut removed_change_output = tx.output.remove(updatable_output);
                if self.is_mine(&removed_change_output.script_pubkey)? {
                    details.received -= removed_change_output.value;
                }

                // we want to add more inputs if:
                // - builder.utxos tells us to do so
                // - the removed change value is lower than the fee_difference we want to add
                let needs_more_inputs =
                    builder.utxos.is_some() || removed_change_output.value <= fee_difference;
                let added_amount = if needs_more_inputs {
                    // TODO: use the right weight instead of the maximum, and only fall-back to it if the
                    // script is unknown in the database
                    let input_witness_weight = std::cmp::max(
                        self.get_descriptor_for_script_type(ScriptType::Internal)
                            .0
                            .max_satisfaction_weight(),
                        self.get_descriptor_for_script_type(ScriptType::External)
                            .0
                            .max_satisfaction_weight(),
                    );

                    let (available_utxos, use_all_utxos) = self.get_available_utxos(
                        builder.change_policy,
                        &builder.utxos,
                        &builder.unspendable,
                        false,
                    )?;
                    let available_utxos = rbf::filter_available(
                        self.database.borrow().deref(),
                        available_utxos.into_iter(),
                    )?;
                    let coin_selection::CoinSelectionResult {
                        txin,
                        selected_amount,
                        fee_amount,
                    } = builder.coin_selection.coin_select(
                        available_utxos,
                        use_all_utxos,
                        new_feerate,
                        fee_difference
                            .checked_sub(removed_change_output.value)
                            .unwrap_or(0),
                        input_witness_weight,
                        0.0,
                    )?;
                    fee_difference += fee_amount.ceil() as u64;

                    // add the new inputs
                    let (mut txin, _): (Vec<_>, Vec<_>) = txin.into_iter().unzip();

                    // TODO: use tx_builder.sequence ??
                    // copy the n_sequence from the inputs that were already in the transaction
                    txin.iter_mut()
                        .for_each(|i| i.sequence = tx.input[0].sequence);
                    tx.input.extend_from_slice(&mut txin);

                    details.sent += selected_amount;
                    selected_amount
                } else {
                    // otherwise just remove the output and add 0 new coins
                    0
                };

                match (removed_change_output.value + added_amount).checked_sub(fee_difference) {
                    None => return Err(Error::InsufficientFunds),
                    Some(new_value) if new_value.is_dust() => {
                        // the change would be dust, add that to fees
                        details.fees += fee_difference + new_value;
                    }
                    Some(new_value) => {
                        // add the change back
                        removed_change_output.value = new_value;
                        tx.output.push(removed_change_output);

                        details.received += new_value;
                        details.fees += fee_difference;
                    }
                }
            }
        };

        // clear witnesses
        for input in &mut tx.input {
            input.script_sig = Script::default();
            input.witness = vec![];
        }

        // sort input/outputs according to the chosen algorithm
        builder.ordering.sort_tx(&mut tx);

        // TODO: check that we are not replacing more than 100 txs from mempool

        details.txid = tx.txid();
        details.timestamp = time::get_timestamp();

        let prev_script_pubkeys = tx
            .input
            .iter()
            .map(|txin| {
                Ok((
                    txin.previous_output.clone(),
                    self.database
                        .borrow()
                        .get_previous_output(&txin.previous_output)?,
                ))
            })
            .collect::<Result<Vec<_>, Error>>()?
            .into_iter()
            .filter_map(|(outpoint, txout)| match txout {
                Some(txout) => Some((outpoint, txout.script_pubkey)),
                None => None,
            })
            .collect();
        let psbt = self.complete_transaction(tx, prev_script_pubkeys, builder)?;

        Ok((psbt, details))
    }

    /// Sign a transaction with all the wallet's signers, in the order specified by every signer's
    /// [`SignerOrdering`]
    ///
    /// ## Example
    ///
    /// ```no_run
    /// # use std::str::FromStr;
    /// # use bitcoin::*;
    /// # use bdk::*;
    /// # use bdk::database::*;
    /// # let descriptor = "wpkh(tpubD6NzVbkrYhZ4Xferm7Pz4VnjdcDPFyjVu5K4iZXQ4pVN8Cks4pHVowTBXBKRhX64pkRyJZJN5xAKj4UDNnLPb5p2sSKXhewoYx5GbTdUFWq/*)";
    /// # let wallet: OfflineWallet<_> = Wallet::new_offline(descriptor, None, Network::Testnet, MemoryDatabase::default())?;
    /// # let (psbt, _) = wallet.create_tx(TxBuilder::new())?;
    /// let (signed_psbt, finalized) = wallet.sign(psbt, None)?;
    /// # Ok::<(), bdk::Error>(())
    pub fn sign(&self, mut psbt: PSBT, assume_height: Option<u32>) -> Result<(PSBT, bool), Error> {
        // this helps us doing our job later
        self.add_input_hd_keypaths(&mut psbt)?;

        for signer in self
            .signers
            .signers()
            .iter()
            .chain(self.change_signers.signers().iter())
        {
            if signer.sign_whole_tx() {
                signer.sign(&mut psbt, None)?;
            } else {
                for index in 0..psbt.inputs.len() {
                    signer.sign(&mut psbt, Some(index))?;
                }
            }
        }

        // attempt to finalize
        self.finalize_psbt(psbt, assume_height)
    }

    /// Return the spending policies for the wallet's descriptor
    pub fn policies(&self, script_type: ScriptType) -> Result<Option<Policy>, Error> {
        match (script_type, self.change_descriptor.as_ref()) {
            (ScriptType::External, _) => {
                Ok(self.descriptor.extract_policy(Arc::clone(&self.signers))?)
            }
            (ScriptType::Internal, None) => Ok(None),
            (ScriptType::Internal, Some(desc)) => {
                Ok(desc.extract_policy(Arc::clone(&self.change_signers))?)
            }
        }
    }

    /// Return the "public" version of the wallet's descriptor, meaning a new descriptor that has
    /// the same structure but with every secret key removed
    ///
    /// This can be used to build a watch-only version of a wallet
    pub fn public_descriptor(
        &self,
        script_type: ScriptType,
    ) -> Result<Option<ExtendedDescriptor>, Error> {
        match (script_type, self.change_descriptor.as_ref()) {
            (ScriptType::External, _) => Ok(Some(self.descriptor.clone())),
            (ScriptType::Internal, None) => Ok(None),
            (ScriptType::Internal, Some(desc)) => Ok(Some(desc.clone())),
        }
    }

    /// Try to finalize a PSBT
    pub fn finalize_psbt(
        &self,
        mut psbt: PSBT,
        assume_height: Option<u32>,
    ) -> Result<(PSBT, bool), Error> {
        let mut tx = psbt.global.unsigned_tx.clone();

        for (n, (input, psbt_input)) in tx.input.iter_mut().zip(psbt.inputs.iter()).enumerate() {
            // if the height is None in the database it means it's still unconfirmed, so consider
            // that as a very high value
            let create_height = self
                .database
                .borrow()
                .get_tx(&input.previous_output.txid, false)?
                .and_then(|tx| Some(tx.height.unwrap_or(std::u32::MAX)));
            let current_height = assume_height.or(self.current_height);

            debug!(
                "Input #{} - {}, using `create_height` = {:?}, `current_height` = {:?}",
                n, input.previous_output, create_height, current_height
            );

            // - Try to derive the descriptor by looking at the txout. If it's in our database, we
            //   know exactly which `script_type` to use, and which derivation index it is
            // - If that fails, try to derive it by looking at the psbt input: the complete logic
            //   is in `src/descriptor/mod.rs`, but it will basically look at `hd_keypaths`,
            //   `redeem_script` and `witness_script` to determine the right derivation
            // - If that also fails, it will try it on the internal descriptor, if present
            let desc = if let Some(desc) = psbt
                .get_utxo_for(n)
                .map(|txout| self.get_descriptor_for_txout(&txout))
                .transpose()?
                .flatten()
            {
                desc
            } else if let Some(desc) = self
                .descriptor
                .derive_from_psbt_input(psbt_input, psbt.get_utxo_for(n))
            {
                desc
            } else if let Some(desc) = self
                .change_descriptor
                .as_ref()
                .and_then(|desc| desc.derive_from_psbt_input(psbt_input, psbt.get_utxo_for(n)))
            {
                desc
            } else {
                debug!("Couldn't find the right derived descriptor for input {}", n);
                return Ok((psbt, false));
            };

            match desc.satisfy(
                input,
                (
                    psbt_input.clone(),
                    After::new(current_height, false),
                    Older::new(current_height, create_height, false),
                ),
            ) {
                Ok(_) => continue,
                Err(e) => {
                    debug!("satisfy error {:?} for input {}", e, n);
                    return Ok((psbt, false));
                }
            }
        }

        // consume tx to extract its input's script_sig and witnesses and move them into the psbt
        for (input, psbt_input) in tx.input.into_iter().zip(psbt.inputs.iter_mut()) {
            psbt_input.final_script_sig = Some(input.script_sig);
            psbt_input.final_script_witness = Some(input.witness);
        }

        Ok((psbt, true))
    }

    // Internals

    fn get_descriptor_for_script_type(
        &self,
        script_type: ScriptType,
    ) -> (&ExtendedDescriptor, ScriptType) {
        let desc = match script_type {
            ScriptType::Internal if self.change_descriptor.is_some() => (
                self.change_descriptor.as_ref().unwrap(),
                ScriptType::Internal,
            ),
            _ => (&self.descriptor, ScriptType::External),
        };

        desc
    }

    fn get_descriptor_for_txout(&self, txout: &TxOut) -> Result<Option<ExtendedDescriptor>, Error> {
        Ok(self
            .database
            .borrow()
            .get_path_from_script_pubkey(&txout.script_pubkey)?
            .map(|(script_type, child)| (self.get_descriptor_for_script_type(script_type).0, child))
            .map(|(desc, child)| desc.derive(&[ChildNumber::from_normal_idx(child).unwrap()])))
    }

    fn get_change_address(&self) -> Result<Script, Error> {
        let (desc, script_type) = self.get_descriptor_for_script_type(ScriptType::Internal);
        let index = self.fetch_and_increment_index(script_type)?;

        Ok(desc
            .derive(&[ChildNumber::from_normal_idx(index).unwrap()])
            .script_pubkey())
    }

    fn fetch_and_increment_index(&self, script_type: ScriptType) -> Result<u32, Error> {
        let (descriptor, script_type) = self.get_descriptor_for_script_type(script_type);
        let index = match descriptor.is_fixed() {
            true => 0,
            false => self
                .database
                .borrow_mut()
                .increment_last_index(script_type)?,
        };

        if self
            .database
            .borrow()
            .get_script_pubkey_from_path(script_type, index)?
            .is_none()
        {
            self.cache_addresses(script_type, index, CACHE_ADDR_BATCH_SIZE)?;
        }

        let hd_keypaths = descriptor.get_hd_keypaths(index)?;
        let script = descriptor
            .derive(&[ChildNumber::from_normal_idx(index).unwrap()])
            .script_pubkey();
        for validator in &self.address_validators {
            validator.validate(script_type, &hd_keypaths, &script)?;
        }

        Ok(index)
    }

    fn cache_addresses(
        &self,
        script_type: ScriptType,
        from: u32,
        mut count: u32,
    ) -> Result<(), Error> {
        let (descriptor, script_type) = self.get_descriptor_for_script_type(script_type);
        if descriptor.is_fixed() {
            if from > 0 {
                return Ok(());
            }

            count = 1;
        }

        let mut address_batch = self.database.borrow().begin_batch();

        let start_time = time::Instant::new();
        for i in from..(from + count) {
            address_batch.set_script_pubkey(
                &descriptor
                    .derive(&[ChildNumber::from_normal_idx(i).unwrap()])
                    .script_pubkey(),
                script_type,
                i,
            )?;
        }

        info!(
            "Derivation of {} addresses from {} took {} ms",
            count,
            from,
            start_time.elapsed().as_millis()
        );

        self.database.borrow_mut().commit_batch(address_batch)?;

        Ok(())
    }

    fn get_available_utxos(
        &self,
        change_policy: tx_builder::ChangeSpendPolicy,
        utxo: &Option<Vec<OutPoint>>,
        unspendable: &Option<Vec<OutPoint>>,
        send_all: bool,
    ) -> Result<(Vec<UTXO>, bool), Error> {
        let unspendable_set = match unspendable {
            None => HashSet::new(),
            Some(vec) => vec.into_iter().collect(),
        };

        match utxo {
            // with manual coin selection we always want to spend all the selected utxos, no matter
            // what (even if they are marked as unspendable)
            Some(raw_utxos) => {
                let full_utxos = raw_utxos
                    .iter()
                    .map(|u| self.database.borrow().get_utxo(&u))
                    .collect::<Result<Vec<_>, _>>()?;
                if !full_utxos.iter().all(|u| u.is_some()) {
                    return Err(Error::UnknownUTXO);
                }

                Ok((full_utxos.into_iter().map(|x| x.unwrap()).collect(), true))
            }
            // otherwise limit ourselves to the spendable utxos for the selected policy, and the `send_all` setting
            None => {
                let utxos = self.list_unspent()?.into_iter();
                let utxos = change_policy.filter_utxos(utxos).into_iter();

                Ok((
                    utxos
                        .filter(|u| !unspendable_set.contains(&u.outpoint))
                        .collect(),
                    send_all,
                ))
            }
        }
    }

    fn complete_transaction<Cs: coin_selection::CoinSelectionAlgorithm>(
        &self,
        tx: Transaction,
        prev_script_pubkeys: HashMap<OutPoint, Script>,
        builder: TxBuilder<Cs>,
    ) -> Result<PSBT, Error> {
        let mut psbt = PSBT::from_unsigned_tx(tx)?;

        // add metadata for the inputs
        for (psbt_input, input) in psbt
            .inputs
            .iter_mut()
            .zip(psbt.global.unsigned_tx.input.iter())
        {
            let prev_script = match prev_script_pubkeys.get(&input.previous_output) {
                Some(prev_script) => prev_script,
                None => continue,
            };

            // Add sighash, default is obviously "ALL"
            psbt_input.sighash_type = builder.sighash.or(Some(SigHashType::All));

            // Try to find the prev_script in our db to figure out if this is internal or external,
            // and the derivation index
            let (script_type, child) = match self
                .database
                .borrow()
                .get_path_from_script_pubkey(&prev_script)?
            {
                Some(x) => x,
                None => continue,
            };

            let (desc, _) = self.get_descriptor_for_script_type(script_type);
            psbt_input.hd_keypaths = desc.get_hd_keypaths(child)?;
            let derived_descriptor = desc.derive(&[ChildNumber::from_normal_idx(child).unwrap()]);

            psbt_input.redeem_script = derived_descriptor.psbt_redeem_script();
            psbt_input.witness_script = derived_descriptor.psbt_witness_script();

            let prev_output = input.previous_output;
            if let Some(prev_tx) = self.database.borrow().get_raw_tx(&prev_output.txid)? {
                if derived_descriptor.is_witness() {
                    psbt_input.witness_utxo =
                        Some(prev_tx.output[prev_output.vout as usize].clone());
                }
                if !derived_descriptor.is_witness() || builder.force_non_witness_utxo {
                    psbt_input.non_witness_utxo = Some(prev_tx);
                }
            }
        }

        // probably redundant but it doesn't hurt...
        self.add_input_hd_keypaths(&mut psbt)?;

        // add metadata for the outputs
        for (psbt_output, tx_output) in psbt
            .outputs
            .iter_mut()
            .zip(psbt.global.unsigned_tx.output.iter())
        {
            if let Some((script_type, child)) = self
                .database
                .borrow()
                .get_path_from_script_pubkey(&tx_output.script_pubkey)?
            {
                let (desc, _) = self.get_descriptor_for_script_type(script_type);
                psbt_output.hd_keypaths = desc.get_hd_keypaths(child)?;
            }
        }

        Ok(psbt)
    }

    fn add_input_hd_keypaths(&self, psbt: &mut PSBT) -> Result<(), Error> {
        let mut input_utxos = Vec::with_capacity(psbt.inputs.len());
        for n in 0..psbt.inputs.len() {
            input_utxos.push(psbt.get_utxo_for(n).clone());
        }

        // try to add hd_keypaths if we've already seen the output
        for (psbt_input, out) in psbt.inputs.iter_mut().zip(input_utxos.iter()) {
            if let Some(out) = out {
                if let Some((script_type, child)) = self
                    .database
                    .borrow()
                    .get_path_from_script_pubkey(&out.script_pubkey)?
                {
                    debug!("Found descriptor {:?}/{}", script_type, child);

                    // merge hd_keypaths
                    let (desc, _) = self.get_descriptor_for_script_type(script_type);
                    let mut hd_keypaths = desc.get_hd_keypaths(child)?;
                    psbt_input.hd_keypaths.append(&mut hd_keypaths);
                }
            }
        }

        Ok(())
    }
}

impl<B, D> Wallet<B, D>
where
    B: Blockchain,
    D: BatchDatabase,
{
    /// Create a new "online" wallet
    #[maybe_async]
    pub fn new<E: ToWalletDescriptor>(
        descriptor: E,
        change_descriptor: Option<E>,
        network: Network,
        database: D,
        client: B,
    ) -> Result<Self, Error> {
        let mut wallet = Self::new_offline(descriptor, change_descriptor, network, database)?;

        wallet.current_height = Some(maybe_await!(client.get_height())? as u32);
        wallet.client = Some(client);

        Ok(wallet)
    }

    /// Sync the internal database with the blockchain
    #[maybe_async]
    pub fn sync<P: 'static + Progress>(
        &self,
        progress_update: P,
        max_address_param: Option<u32>,
    ) -> Result<(), Error> {
        debug!("Begin sync...");

        let mut run_setup = false;

        let max_address = match self.descriptor.is_fixed() {
            true => 0,
            false => max_address_param.unwrap_or(CACHE_ADDR_BATCH_SIZE),
        };
        if self
            .database
            .borrow()
            .get_script_pubkey_from_path(ScriptType::External, max_address)?
            .is_none()
        {
            run_setup = true;
            self.cache_addresses(ScriptType::External, 0, max_address)?;
        }

        if let Some(change_descriptor) = &self.change_descriptor {
            let max_address = match change_descriptor.is_fixed() {
                true => 0,
                false => max_address_param.unwrap_or(CACHE_ADDR_BATCH_SIZE),
            };

            if self
                .database
                .borrow()
                .get_script_pubkey_from_path(
                    ScriptType::Internal,
                    max_address.checked_sub(1).unwrap_or(0),
                )?
                .is_none()
            {
                run_setup = true;
                self.cache_addresses(ScriptType::Internal, 0, max_address)?;
            }
        }

        // TODO: what if i generate an address first and cache some addresses?
        // TODO: we should sync if generating an address triggers a new batch to be stored
        if run_setup {
            maybe_await!(self.client.as_ref().ok_or(Error::OfflineClient)?.setup(
                None,
                self.database.borrow_mut().deref_mut(),
                progress_update,
            ))
        } else {
            maybe_await!(self.client.as_ref().ok_or(Error::OfflineClient)?.sync(
                None,
                self.database.borrow_mut().deref_mut(),
                progress_update,
            ))
        }
    }

    /// Return a reference to the internal blockchain client
    pub fn client(&self) -> Option<&B> {
        self.client.as_ref()
    }

    /// Broadcast a transaction to the network
    #[maybe_async]
    pub fn broadcast(&self, tx: Transaction) -> Result<Txid, Error> {
        maybe_await!(self
            .client
            .as_ref()
            .ok_or(Error::OfflineClient)?
            .broadcast(&tx))?;

        Ok(tx.txid())
    }
}

#[cfg(test)]
mod test {
    use std::str::FromStr;

    use bitcoin::{
        consensus::deserialize,
        util::psbt::{serialize::Serialize, PartiallySignedTransaction},
        Network,
    };

    use crate::database::memory::MemoryDatabase;
    use crate::database::Database;
    use crate::types::ScriptType;

    use super::*;

    #[test]
    fn test_cache_addresses_fixed() {
        let db = MemoryDatabase::new();
        let wallet: OfflineWallet<_> = Wallet::new_offline(
            "wpkh(L5EZftvrYaSudiozVRzTqLcHLNDoVn7H5HSfM9BAN6tMJX8oTWz6)",
            None,
            Network::Testnet,
            db,
        )
        .unwrap();

        assert_eq!(
            wallet.get_new_address().unwrap().to_string(),
            "tb1qj08ys4ct2hzzc2hcz6h2hgrvlmsjynaw43s835"
        );
        assert_eq!(
            wallet.get_new_address().unwrap().to_string(),
            "tb1qj08ys4ct2hzzc2hcz6h2hgrvlmsjynaw43s835"
        );

        assert!(wallet
            .database
            .borrow_mut()
            .get_script_pubkey_from_path(ScriptType::External, 0)
            .unwrap()
            .is_some());
        assert!(wallet
            .database
            .borrow_mut()
            .get_script_pubkey_from_path(ScriptType::Internal, 0)
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_cache_addresses() {
        let db = MemoryDatabase::new();
        let wallet: OfflineWallet<_> = Wallet::new_offline("wpkh(tpubEBr4i6yk5nf5DAaJpsi9N2pPYBeJ7fZ5Z9rmN4977iYLCGco1VyjB9tvvuvYtfZzjD5A8igzgw3HeWeeKFmanHYqksqZXYXGsw5zjnj7KM9/*)", None, Network::Testnet, db).unwrap();

        assert_eq!(
            wallet.get_new_address().unwrap().to_string(),
            "tb1q6yn66vajcctph75pvylgkksgpp6nq04ppwct9a"
        );
        assert_eq!(
            wallet.get_new_address().unwrap().to_string(),
            "tb1q4er7kxx6sssz3q7qp7zsqsdx4erceahhax77d7"
        );

        assert!(wallet
            .database
            .borrow_mut()
            .get_script_pubkey_from_path(ScriptType::External, CACHE_ADDR_BATCH_SIZE - 1)
            .unwrap()
            .is_some());
        assert!(wallet
            .database
            .borrow_mut()
            .get_script_pubkey_from_path(ScriptType::External, CACHE_ADDR_BATCH_SIZE)
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_cache_addresses_refill() {
        let db = MemoryDatabase::new();
        let wallet: OfflineWallet<_> = Wallet::new_offline("wpkh(tpubEBr4i6yk5nf5DAaJpsi9N2pPYBeJ7fZ5Z9rmN4977iYLCGco1VyjB9tvvuvYtfZzjD5A8igzgw3HeWeeKFmanHYqksqZXYXGsw5zjnj7KM9/*)", None, Network::Testnet, db).unwrap();

        assert_eq!(
            wallet.get_new_address().unwrap().to_string(),
            "tb1q6yn66vajcctph75pvylgkksgpp6nq04ppwct9a"
        );
        assert!(wallet
            .database
            .borrow_mut()
            .get_script_pubkey_from_path(ScriptType::External, CACHE_ADDR_BATCH_SIZE - 1)
            .unwrap()
            .is_some());

        for _ in 0..CACHE_ADDR_BATCH_SIZE {
            wallet.get_new_address().unwrap();
        }

        assert!(wallet
            .database
            .borrow_mut()
            .get_script_pubkey_from_path(ScriptType::External, CACHE_ADDR_BATCH_SIZE * 2 - 1)
            .unwrap()
            .is_some());
    }

    pub(crate) fn get_test_wpkh() -> &'static str {
        "wpkh(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW)"
    }

    pub(crate) fn get_test_single_sig_csv() -> &'static str {
        // and(pk(Alice),older(6))
        "wsh(and_v(v:pk(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW),older(6)))"
    }

    pub(crate) fn get_test_single_sig_cltv() -> &'static str {
        // and(pk(Alice),after(100000))
        "wsh(and_v(v:pk(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW),after(100000)))"
    }

    pub(crate) fn get_funded_wallet(
        descriptor: &str,
    ) -> (
        OfflineWallet<MemoryDatabase>,
        (String, Option<String>),
        bitcoin::Txid,
    ) {
        let descriptors = testutils!(@descriptors (descriptor));
        let wallet: OfflineWallet<_> = Wallet::new_offline(
            &descriptors.0,
            None,
            Network::Regtest,
            MemoryDatabase::new(),
        )
        .unwrap();

        let txid = wallet.database.borrow_mut().received_tx(
            testutils! {
                @tx ( (@external descriptors, 0) => 50_000 ) (@confirmations 1)
            },
            Some(100),
        );

        (wallet, descriptors, txid)
    }

    macro_rules! assert_fee_rate {
        ($tx:expr, $fees:expr, $fee_rate:expr $( ,@dust_change $( $dust_change:expr )* )* $( ,@add_signature $( $add_signature:expr )* )* ) => ({
            let mut tx = $tx.clone();
            $(
                $( $add_signature )*
                for txin in &mut tx.input {
                    txin.witness.push([0x00; 108].to_vec()); // fake signature
                }
            )*

            #[allow(unused_mut)]
            #[allow(unused_assignments)]
            let mut dust_change = false;
            $(
                $( $dust_change )*
                dust_change = true;
            )*

            let tx_fee_rate = $fees as f32 / (tx.get_weight() as f32 / 4.0);
            let fee_rate = $fee_rate.as_sat_vb();

            if !dust_change {
                assert!((tx_fee_rate - fee_rate).abs() < 0.5, format!("Expected fee rate of {}, the tx has {}", fee_rate, tx_fee_rate));
            } else {
                assert!(tx_fee_rate >= fee_rate, format!("Expected fee rate of at least {}, the tx has {}", fee_rate, tx_fee_rate));
            }
        });
    }

    #[test]
    #[should_panic(expected = "NoAddressees")]
    fn test_create_tx_empty_recipients() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        wallet
            .create_tx(TxBuilder::with_recipients(vec![]).version(0))
            .unwrap();
    }

    #[test]
    #[should_panic(expected = "Invalid version `0`")]
    fn test_create_tx_version_0() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        wallet
            .create_tx(TxBuilder::with_recipients(vec![(addr.script_pubkey(), 25_000)]).version(0))
            .unwrap();
    }

    #[test]
    #[should_panic(
        expected = "TxBuilder requested version `1`, but at least `2` is needed to use OP_CSV"
    )]
    fn test_create_tx_version_1_csv() {
        let (wallet, _, _) = get_funded_wallet(get_test_single_sig_csv());
        let addr = wallet.get_new_address().unwrap();
        wallet
            .create_tx(TxBuilder::with_recipients(vec![(addr.script_pubkey(), 25_000)]).version(1))
            .unwrap();
    }

    #[test]
    fn test_create_tx_custom_version() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(addr.script_pubkey(), 25_000)]).version(42))
            .unwrap();

        assert_eq!(psbt.global.unsigned_tx.version, 42);
    }

    #[test]
    fn test_create_tx_default_locktime() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(
                addr.script_pubkey(),
                25_000,
            )]))
            .unwrap();

        assert_eq!(psbt.global.unsigned_tx.lock_time, 0);
    }

    #[test]
    fn test_create_tx_default_locktime_cltv() {
        let (wallet, _, _) = get_funded_wallet(get_test_single_sig_cltv());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(
                addr.script_pubkey(),
                25_000,
            )]))
            .unwrap();

        assert_eq!(psbt.global.unsigned_tx.lock_time, 100_000);
    }

    #[test]
    fn test_create_tx_custom_locktime() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 25_000)]).nlocktime(630_000),
            )
            .unwrap();

        assert_eq!(psbt.global.unsigned_tx.lock_time, 630_000);
    }

    #[test]
    fn test_create_tx_custom_locktime_compatible_with_cltv() {
        let (wallet, _, _) = get_funded_wallet(get_test_single_sig_cltv());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 25_000)]).nlocktime(630_000),
            )
            .unwrap();

        assert_eq!(psbt.global.unsigned_tx.lock_time, 630_000);
    }

    #[test]
    #[should_panic(
        expected = "TxBuilder requested timelock of `50000`, but at least `100000` is required to spend from this script"
    )]
    fn test_create_tx_custom_locktime_incompatible_with_cltv() {
        let (wallet, _, _) = get_funded_wallet(get_test_single_sig_cltv());
        let addr = wallet.get_new_address().unwrap();
        wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 25_000)]).nlocktime(50000),
            )
            .unwrap();
    }

    #[test]
    fn test_create_tx_no_rbf_csv() {
        let (wallet, _, _) = get_funded_wallet(get_test_single_sig_csv());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(
                addr.script_pubkey(),
                25_000,
            )]))
            .unwrap();

        assert_eq!(psbt.global.unsigned_tx.input[0].sequence, 6);
    }

    #[test]
    fn test_create_tx_with_default_rbf_csv() {
        let (wallet, _, _) = get_funded_wallet(get_test_single_sig_csv());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 25_000)]).enable_rbf(),
            )
            .unwrap();

        assert_eq!(psbt.global.unsigned_tx.input[0].sequence, 0xFFFFFFFD);
    }

    #[test]
    #[should_panic(
        expected = "Cannot enable RBF with nSequence `3`, since at least `6` is required to spend with OP_CSV"
    )]
    fn test_create_tx_with_custom_rbf_csv() {
        let (wallet, _, _) = get_funded_wallet(get_test_single_sig_csv());
        let addr = wallet.get_new_address().unwrap();
        wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 25_000)])
                    .enable_rbf_with_sequence(3),
            )
            .unwrap();
    }

    #[test]
    fn test_create_tx_no_rbf_cltv() {
        let (wallet, _, _) = get_funded_wallet(get_test_single_sig_cltv());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(
                addr.script_pubkey(),
                25_000,
            )]))
            .unwrap();

        assert_eq!(psbt.global.unsigned_tx.input[0].sequence, 0xFFFFFFFE);
    }

    #[test]
    #[should_panic(expected = "Cannot enable RBF with a nSequence >= 0xFFFFFFFE")]
    fn test_create_tx_invalid_rbf_sequence() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 25_000)])
                    .enable_rbf_with_sequence(0xFFFFFFFE),
            )
            .unwrap();
    }

    #[test]
    fn test_create_tx_custom_rbf_sequence() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 25_000)])
                    .enable_rbf_with_sequence(0xDEADBEEF),
            )
            .unwrap();

        assert_eq!(psbt.global.unsigned_tx.input[0].sequence, 0xDEADBEEF);
    }

    #[test]
    fn test_create_tx_default_sequence() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(
                addr.script_pubkey(),
                25_000,
            )]))
            .unwrap();

        assert_eq!(psbt.global.unsigned_tx.input[0].sequence, 0xFFFFFFFF);
    }

    #[test]
    #[should_panic(
        expected = "The `change_policy` can be set only if the wallet has a change_descriptor"
    )]
    fn test_create_tx_change_policy_no_internal() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 25_000)])
                    .do_not_spend_change(),
            )
            .unwrap();
    }

    #[test]
    #[should_panic(expected = "SendAllMultipleOutputs")]
    fn test_create_tx_send_all_multiple_outputs() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        wallet
            .create_tx(
                TxBuilder::with_recipients(vec![
                    (addr.script_pubkey(), 25_000),
                    (addr.script_pubkey(), 10_000),
                ])
                .send_all(),
            )
            .unwrap();
    }

    #[test]
    fn test_create_tx_send_all() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, details) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(addr.script_pubkey(), 0)]).send_all())
            .unwrap();

        assert_eq!(psbt.global.unsigned_tx.output.len(), 1);
        assert_eq!(
            psbt.global.unsigned_tx.output[0].value,
            50_000 - details.fees
        );
    }

    #[test]
    fn test_create_tx_default_fee_rate() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, details) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(addr.script_pubkey(), 0)]).send_all())
            .unwrap();

        assert_fee_rate!(psbt.extract_tx(), details.fees, FeeRate::default(), @add_signature);
    }

    #[test]
    fn test_create_tx_custom_fee_rate() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, details) = wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 0)])
                    .fee_rate(FeeRate::from_sat_per_vb(5.0))
                    .send_all(),
            )
            .unwrap();

        assert_fee_rate!(psbt.extract_tx(), details.fees, FeeRate::from_sat_per_vb(5.0), @add_signature);
    }

    #[test]
    fn test_create_tx_add_change() {
        use super::tx_builder::TxOrdering;

        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, details) = wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 25_000)])
                    .ordering(TxOrdering::Untouched),
            )
            .unwrap();

        assert_eq!(psbt.global.unsigned_tx.output.len(), 2);
        assert_eq!(psbt.global.unsigned_tx.output[0].value, 25_000);
        assert_eq!(
            psbt.global.unsigned_tx.output[1].value,
            25_000 - details.fees
        );
    }

    #[test]
    fn test_create_tx_skip_change_dust() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(
                addr.script_pubkey(),
                49_800,
            )]))
            .unwrap();

        assert_eq!(psbt.global.unsigned_tx.output.len(), 1);
        assert_eq!(psbt.global.unsigned_tx.output[0].value, 49_800);
    }

    #[test]
    #[should_panic(expected = "InsufficientFunds")]
    fn test_create_tx_send_all_dust_amount() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        // very high fee rate, so that the only output would be below dust
        wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 0)])
                    .send_all()
                    .fee_rate(crate::FeeRate::from_sat_per_vb(453.0)),
            )
            .unwrap();
    }

    #[test]
    fn test_create_tx_ordering_respected() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, details) = wallet
            .create_tx(
                TxBuilder::with_recipients(vec![
                    (addr.script_pubkey(), 30_000),
                    (addr.script_pubkey(), 10_000),
                ])
                .ordering(super::tx_builder::TxOrdering::BIP69Lexicographic),
            )
            .unwrap();

        assert_eq!(psbt.global.unsigned_tx.output.len(), 3);
        assert_eq!(
            psbt.global.unsigned_tx.output[0].value,
            10_000 - details.fees
        );
        assert_eq!(psbt.global.unsigned_tx.output[1].value, 10_000);
        assert_eq!(psbt.global.unsigned_tx.output[2].value, 30_000);
    }

    #[test]
    fn test_create_tx_default_sighash() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(
                addr.script_pubkey(),
                30_000,
            )]))
            .unwrap();

        assert_eq!(psbt.inputs[0].sighash_type, Some(bitcoin::SigHashType::All));
    }

    #[test]
    fn test_create_tx_custom_sighash() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 30_000)])
                    .sighash(bitcoin::SigHashType::Single),
            )
            .unwrap();

        assert_eq!(
            psbt.inputs[0].sighash_type,
            Some(bitcoin::SigHashType::Single)
        );
    }

    #[test]
    fn test_create_tx_input_hd_keypaths() {
        use bitcoin::util::bip32::{DerivationPath, Fingerprint};
        use std::str::FromStr;

        let (wallet, _, _) = get_funded_wallet("wpkh([d34db33f/44'/0'/0']tpubDEnoLuPdBep9bzw5LoGYpsxUQYheRQ9gcgrJhJEcdKFB9cWQRyYmkCyRoTqeD4tJYiVVgt6A3rN6rWn9RYhR9sBsGxji29LYWHuKKbdb1ev/0/*)");
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(addr.script_pubkey(), 0)]).send_all())
            .unwrap();

        assert_eq!(psbt.inputs[0].hd_keypaths.len(), 1);
        assert_eq!(
            psbt.inputs[0].hd_keypaths.values().nth(0).unwrap(),
            &(
                Fingerprint::from_str("d34db33f").unwrap(),
                DerivationPath::from_str("m/44'/0'/0'/0/0").unwrap()
            )
        );
    }

    #[test]
    fn test_create_tx_output_hd_keypaths() {
        use bitcoin::util::bip32::{DerivationPath, Fingerprint};
        use std::str::FromStr;

        let (wallet, descriptors, _) = get_funded_wallet("wpkh([d34db33f/44'/0'/0']tpubDEnoLuPdBep9bzw5LoGYpsxUQYheRQ9gcgrJhJEcdKFB9cWQRyYmkCyRoTqeD4tJYiVVgt6A3rN6rWn9RYhR9sBsGxji29LYWHuKKbdb1ev/0/*)");
        // cache some addresses
        wallet.get_new_address().unwrap();

        let addr = testutils!(@external descriptors, 5);
        let (psbt, _) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(addr.script_pubkey(), 0)]).send_all())
            .unwrap();

        assert_eq!(psbt.outputs[0].hd_keypaths.len(), 1);
        assert_eq!(
            psbt.outputs[0].hd_keypaths.values().nth(0).unwrap(),
            &(
                Fingerprint::from_str("d34db33f").unwrap(),
                DerivationPath::from_str("m/44'/0'/0'/0/5").unwrap()
            )
        );
    }

    #[test]
    fn test_create_tx_set_redeem_script_p2sh() {
        use bitcoin::hashes::hex::FromHex;

        let (wallet, _, _) =
            get_funded_wallet("sh(pk(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW))");
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(addr.script_pubkey(), 0)]).send_all())
            .unwrap();

        assert_eq!(
            psbt.inputs[0].redeem_script,
            Some(Script::from(
                Vec::<u8>::from_hex(
                    "21032b0558078bec38694a84933d659303e2575dae7e91685911454115bfd64487e3ac"
                )
                .unwrap()
            ))
        );
        assert_eq!(psbt.inputs[0].witness_script, None);
    }

    #[test]
    fn test_create_tx_set_witness_script_p2wsh() {
        use bitcoin::hashes::hex::FromHex;

        let (wallet, _, _) =
            get_funded_wallet("wsh(pk(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW))");
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(addr.script_pubkey(), 0)]).send_all())
            .unwrap();

        assert_eq!(psbt.inputs[0].redeem_script, None);
        assert_eq!(
            psbt.inputs[0].witness_script,
            Some(Script::from(
                Vec::<u8>::from_hex(
                    "21032b0558078bec38694a84933d659303e2575dae7e91685911454115bfd64487e3ac"
                )
                .unwrap()
            ))
        );
    }

    #[test]
    fn test_create_tx_set_redeem_witness_script_p2wsh_p2sh() {
        use bitcoin::hashes::hex::FromHex;

        let (wallet, _, _) =
            get_funded_wallet("sh(wsh(pk(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW)))");
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(addr.script_pubkey(), 0)]).send_all())
            .unwrap();

        let script = Script::from(
            Vec::<u8>::from_hex(
                "21032b0558078bec38694a84933d659303e2575dae7e91685911454115bfd64487e3ac",
            )
            .unwrap(),
        );

        assert_eq!(psbt.inputs[0].redeem_script, Some(script.to_v0_p2wsh()));
        assert_eq!(psbt.inputs[0].witness_script, Some(script));
    }

    #[test]
    fn test_create_tx_non_witness_utxo() {
        let (wallet, _, _) =
            get_funded_wallet("sh(pk(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW))");
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(addr.script_pubkey(), 0)]).send_all())
            .unwrap();

        assert!(psbt.inputs[0].non_witness_utxo.is_some());
        assert!(psbt.inputs[0].witness_utxo.is_none());
    }

    #[test]
    fn test_create_tx_only_witness_utxo() {
        let (wallet, _, _) =
            get_funded_wallet("wsh(pk(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW))");
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(addr.script_pubkey(), 0)]).send_all())
            .unwrap();

        assert!(psbt.inputs[0].non_witness_utxo.is_none());
        assert!(psbt.inputs[0].witness_utxo.is_some());
    }

    #[test]
    fn test_create_tx_both_non_witness_utxo_and_witness_utxo() {
        let (wallet, _, _) =
            get_funded_wallet("wsh(pk(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW))");
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 0)])
                    .force_non_witness_utxo()
                    .send_all(),
            )
            .unwrap();

        assert!(psbt.inputs[0].non_witness_utxo.is_some());
        assert!(psbt.inputs[0].witness_utxo.is_some());
    }

    #[test]
    #[should_panic(expected = "IrreplaceableTransaction")]
    fn test_bump_fee_irreplaceable_tx() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, mut details) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(
                addr.script_pubkey(),
                25_000,
            )]))
            .unwrap();
        let tx = psbt.extract_tx();
        let txid = tx.txid();
        // skip saving the utxos, we know they can't be used anyways
        details.transaction = Some(tx);
        wallet.database.borrow_mut().set_tx(&details).unwrap();

        wallet.bump_fee(&txid, TxBuilder::new()).unwrap();
    }

    #[test]
    #[should_panic(expected = "TransactionConfirmed")]
    fn test_bump_fee_confirmed_tx() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, mut details) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(
                addr.script_pubkey(),
                25_000,
            )]))
            .unwrap();
        let tx = psbt.extract_tx();
        let txid = tx.txid();
        // skip saving the utxos, we know they can't be used anyways
        details.transaction = Some(tx);
        details.height = Some(42);
        wallet.database.borrow_mut().set_tx(&details).unwrap();

        wallet.bump_fee(&txid, TxBuilder::new()).unwrap();
    }

    #[test]
    #[should_panic(expected = "FeeRateTooLow")]
    fn test_bump_fee_low_fee_rate() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = wallet.get_new_address().unwrap();
        let (psbt, mut details) = wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 25_000)]).enable_rbf(),
            )
            .unwrap();
        let tx = psbt.extract_tx();
        let txid = tx.txid();
        // skip saving the utxos, we know they can't be used anyways
        details.transaction = Some(tx);
        wallet.database.borrow_mut().set_tx(&details).unwrap();

        wallet
            .bump_fee(
                &txid,
                TxBuilder::new().fee_rate(FeeRate::from_sat_per_vb(1.0)),
            )
            .unwrap();
    }

    #[test]
    fn test_bump_fee_reduce_change() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = Address::from_str("2N1Ffz3WaNzbeLFBb51xyFMHYSEUXcbiSoX").unwrap();
        let (psbt, mut original_details) = wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 25_000)]).enable_rbf(),
            )
            .unwrap();
        let mut tx = psbt.extract_tx();
        let txid = tx.txid();
        // skip saving the new utxos, we know they can't be used anyways
        for txin in &mut tx.input {
            txin.witness.push([0x00; 108].to_vec()); // fake signature
            wallet
                .database
                .borrow_mut()
                .del_utxo(&txin.previous_output)
                .unwrap();
        }
        original_details.transaction = Some(tx);
        wallet
            .database
            .borrow_mut()
            .set_tx(&original_details)
            .unwrap();

        let (psbt, details) = wallet
            .bump_fee(
                &txid,
                TxBuilder::new().fee_rate(FeeRate::from_sat_per_vb(2.5)),
            )
            .unwrap();

        assert_eq!(details.sent, original_details.sent);
        assert_eq!(
            details.received + details.fees,
            original_details.received + original_details.fees
        );
        assert!(details.fees > original_details.fees);

        let tx = &psbt.global.unsigned_tx;
        assert_eq!(tx.output.len(), 2);
        assert_eq!(
            tx.output
                .iter()
                .find(|txout| txout.script_pubkey == addr.script_pubkey())
                .unwrap()
                .value,
            25_000
        );
        assert_eq!(
            tx.output
                .iter()
                .find(|txout| txout.script_pubkey != addr.script_pubkey())
                .unwrap()
                .value,
            details.received
        );

        assert_fee_rate!(psbt.extract_tx(), details.fees, FeeRate::from_sat_per_vb(2.5), @add_signature);
    }

    #[test]
    fn test_bump_fee_reduce_send_all() {
        let (wallet, _, _) = get_funded_wallet(get_test_wpkh());
        let addr = Address::from_str("2N1Ffz3WaNzbeLFBb51xyFMHYSEUXcbiSoX").unwrap();
        let (psbt, mut original_details) = wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 0)])
                    .send_all()
                    .enable_rbf(),
            )
            .unwrap();
        let mut tx = psbt.extract_tx();
        let txid = tx.txid();
        for txin in &mut tx.input {
            txin.witness.push([0x00; 108].to_vec()); // fake signature
            wallet
                .database
                .borrow_mut()
                .del_utxo(&txin.previous_output)
                .unwrap();
        }
        original_details.transaction = Some(tx);
        wallet
            .database
            .borrow_mut()
            .set_tx(&original_details)
            .unwrap();

        let (psbt, details) = wallet
            .bump_fee(
                &txid,
                TxBuilder::new()
                    .send_all()
                    .fee_rate(FeeRate::from_sat_per_vb(2.5)),
            )
            .unwrap();

        assert_eq!(details.sent, original_details.sent);
        assert!(details.fees > original_details.fees);

        let tx = &psbt.global.unsigned_tx;
        assert_eq!(tx.output.len(), 1);
        assert_eq!(tx.output[0].value + details.fees, details.sent);

        assert_fee_rate!(psbt.extract_tx(), details.fees, FeeRate::from_sat_per_vb(2.5), @add_signature);
    }

    #[test]
    #[should_panic(expected = "InsufficientFunds")]
    fn test_bump_fee_remove_send_all_output() {
        let (wallet, descriptors, _) = get_funded_wallet(get_test_wpkh());
        // receive an extra tx, to make sure that in case of "send_all" we get an error and it
        // doesn't try to pick more inputs
        let incoming_txid = wallet.database.borrow_mut().received_tx(
            testutils! (@tx ( (@external descriptors, 0) => 25_000 ) (@confirmations 1)),
            Some(100),
        );
        let addr = Address::from_str("2N1Ffz3WaNzbeLFBb51xyFMHYSEUXcbiSoX").unwrap();
        let (psbt, mut original_details) = wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 0)])
                    .utxos(vec![OutPoint {
                        txid: incoming_txid,
                        vout: 0,
                    }])
                    .send_all()
                    .enable_rbf(),
            )
            .unwrap();
        let mut tx = psbt.extract_tx();
        let txid = tx.txid();
        for txin in &mut tx.input {
            txin.witness.push([0x00; 108].to_vec()); // fake signature
            wallet
                .database
                .borrow_mut()
                .del_utxo(&txin.previous_output)
                .unwrap();
        }
        original_details.transaction = Some(tx);
        wallet
            .database
            .borrow_mut()
            .set_tx(&original_details)
            .unwrap();
        assert_eq!(original_details.sent, 25_000);

        wallet
            .bump_fee(
                &txid,
                TxBuilder::new()
                    .send_all()
                    .fee_rate(FeeRate::from_sat_per_vb(225.0)),
            )
            .unwrap();
    }

    #[test]
    fn test_bump_fee_add_input() {
        let (wallet, descriptors, _) = get_funded_wallet(get_test_wpkh());
        wallet.database.borrow_mut().received_tx(
            testutils! (@tx ( (@external descriptors, 0) => 25_000 ) (@confirmations 1)),
            Some(100),
        );

        let addr = Address::from_str("2N1Ffz3WaNzbeLFBb51xyFMHYSEUXcbiSoX").unwrap();
        let (psbt, mut original_details) = wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 45_000)]).enable_rbf(),
            )
            .unwrap();
        let mut tx = psbt.extract_tx();
        let txid = tx.txid();
        // skip saving the new utxos, we know they can't be used anyways
        for txin in &mut tx.input {
            txin.witness.push([0x00; 108].to_vec()); // fake signature
            wallet
                .database
                .borrow_mut()
                .del_utxo(&txin.previous_output)
                .unwrap();
        }
        original_details.transaction = Some(tx);
        wallet
            .database
            .borrow_mut()
            .set_tx(&original_details)
            .unwrap();

        let (psbt, details) = wallet
            .bump_fee(
                &txid,
                TxBuilder::new().fee_rate(FeeRate::from_sat_per_vb(50.0)),
            )
            .unwrap();

        assert_eq!(details.sent, original_details.sent + 25_000);
        assert_eq!(details.fees + details.received, 30_000);

        let tx = &psbt.global.unsigned_tx;
        assert_eq!(tx.input.len(), 2);
        assert_eq!(tx.output.len(), 2);
        assert_eq!(
            tx.output
                .iter()
                .find(|txout| txout.script_pubkey == addr.script_pubkey())
                .unwrap()
                .value,
            45_000
        );
        assert_eq!(
            tx.output
                .iter()
                .find(|txout| txout.script_pubkey != addr.script_pubkey())
                .unwrap()
                .value,
            details.received
        );

        assert_fee_rate!(psbt.extract_tx(), details.fees, FeeRate::from_sat_per_vb(50.0), @add_signature);
    }

    #[test]
    fn test_bump_fee_no_change_add_input_and_change() {
        let (wallet, descriptors, _) = get_funded_wallet(get_test_wpkh());
        let incoming_txid = wallet.database.borrow_mut().received_tx(
            testutils! (@tx ( (@external descriptors, 0) => 25_000 ) (@confirmations 1)),
            Some(100),
        );

        let addr = Address::from_str("2N1Ffz3WaNzbeLFBb51xyFMHYSEUXcbiSoX").unwrap();
        let (psbt, mut original_details) = wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 0)])
                    .send_all()
                    .add_utxo(OutPoint {
                        txid: incoming_txid,
                        vout: 0,
                    })
                    .enable_rbf(),
            )
            .unwrap();
        let mut tx = psbt.extract_tx();
        let txid = tx.txid();
        // skip saving the new utxos, we know they can't be used anyways
        for txin in &mut tx.input {
            txin.witness.push([0x00; 108].to_vec()); // fake signature
            wallet
                .database
                .borrow_mut()
                .del_utxo(&txin.previous_output)
                .unwrap();
        }
        original_details.transaction = Some(tx);
        wallet
            .database
            .borrow_mut()
            .set_tx(&original_details)
            .unwrap();

        // NOTE: we don't set "send_all" here. so we have a transaction with only one input, but
        // here we are allowed to add more, and we will also have to add a change
        let (psbt, details) = wallet
            .bump_fee(
                &txid,
                TxBuilder::new().fee_rate(FeeRate::from_sat_per_vb(50.0)),
            )
            .unwrap();

        let original_send_all_amount = original_details.sent - original_details.fees;
        assert_eq!(details.sent, original_details.sent + 50_000);
        assert_eq!(
            details.received,
            75_000 - original_send_all_amount - details.fees
        );

        let tx = &psbt.global.unsigned_tx;
        assert_eq!(tx.input.len(), 2);
        assert_eq!(tx.output.len(), 2);
        assert_eq!(
            tx.output
                .iter()
                .find(|txout| txout.script_pubkey == addr.script_pubkey())
                .unwrap()
                .value,
            original_send_all_amount
        );
        assert_eq!(
            tx.output
                .iter()
                .find(|txout| txout.script_pubkey != addr.script_pubkey())
                .unwrap()
                .value,
            75_000 - original_send_all_amount - details.fees
        );

        assert_fee_rate!(psbt.extract_tx(), details.fees, FeeRate::from_sat_per_vb(50.0), @add_signature);
    }

    #[test]
    fn test_bump_fee_add_input_change_dust() {
        let (wallet, descriptors, _) = get_funded_wallet(get_test_wpkh());
        wallet.database.borrow_mut().received_tx(
            testutils! (@tx ( (@external descriptors, 0) => 25_000 ) (@confirmations 1)),
            Some(100),
        );

        let addr = Address::from_str("2N1Ffz3WaNzbeLFBb51xyFMHYSEUXcbiSoX").unwrap();
        let (psbt, mut original_details) = wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 45_000)]).enable_rbf(),
            )
            .unwrap();
        let mut tx = psbt.extract_tx();
        assert_eq!(tx.input.len(), 1);
        assert_eq!(tx.output.len(), 2);
        let txid = tx.txid();
        // skip saving the new utxos, we know they can't be used anyways
        for txin in &mut tx.input {
            txin.witness.push([0x00; 108].to_vec()); // fake signature
            wallet
                .database
                .borrow_mut()
                .del_utxo(&txin.previous_output)
                .unwrap();
        }
        original_details.transaction = Some(tx);
        wallet
            .database
            .borrow_mut()
            .set_tx(&original_details)
            .unwrap();

        let (psbt, details) = wallet
            .bump_fee(
                &txid,
                TxBuilder::new().fee_rate(FeeRate::from_sat_per_vb(140.0)),
            )
            .unwrap();

        assert_eq!(original_details.received, 5_000 - original_details.fees);

        assert_eq!(details.sent, original_details.sent + 25_000);
        assert_eq!(details.fees, 30_000);
        assert_eq!(details.received, 0);

        let tx = &psbt.global.unsigned_tx;
        assert_eq!(tx.input.len(), 2);
        assert_eq!(tx.output.len(), 1);
        assert_eq!(
            tx.output
                .iter()
                .find(|txout| txout.script_pubkey == addr.script_pubkey())
                .unwrap()
                .value,
            45_000
        );

        assert_fee_rate!(psbt.extract_tx(), details.fees, FeeRate::from_sat_per_vb(140.0), @dust_change, @add_signature);
    }

    #[test]
    fn test_bump_fee_force_add_input() {
        let (wallet, descriptors, _) = get_funded_wallet(get_test_wpkh());
        let incoming_txid = wallet.database.borrow_mut().received_tx(
            testutils! (@tx ( (@external descriptors, 0) => 25_000 ) (@confirmations 1)),
            Some(100),
        );

        let addr = Address::from_str("2N1Ffz3WaNzbeLFBb51xyFMHYSEUXcbiSoX").unwrap();
        let (psbt, mut original_details) = wallet
            .create_tx(
                TxBuilder::with_recipients(vec![(addr.script_pubkey(), 45_000)]).enable_rbf(),
            )
            .unwrap();
        let mut tx = psbt.extract_tx();
        let txid = tx.txid();
        // skip saving the new utxos, we know they can't be used anyways
        for txin in &mut tx.input {
            txin.witness.push([0x00; 108].to_vec()); // fake signature
            wallet
                .database
                .borrow_mut()
                .del_utxo(&txin.previous_output)
                .unwrap();
        }
        original_details.transaction = Some(tx);
        wallet
            .database
            .borrow_mut()
            .set_tx(&original_details)
            .unwrap();

        // the new fee_rate is low enough that just reducing the change would be fine, but we force
        // the addition of an extra input with `add_utxo()`
        let (psbt, details) = wallet
            .bump_fee(
                &txid,
                TxBuilder::new()
                    .add_utxo(OutPoint {
                        txid: incoming_txid,
                        vout: 0,
                    })
                    .fee_rate(FeeRate::from_sat_per_vb(5.0)),
            )
            .unwrap();

        assert_eq!(details.sent, original_details.sent + 25_000);
        assert_eq!(details.fees + details.received, 30_000);

        let tx = &psbt.global.unsigned_tx;
        assert_eq!(tx.input.len(), 2);
        assert_eq!(tx.output.len(), 2);
        assert_eq!(
            tx.output
                .iter()
                .find(|txout| txout.script_pubkey == addr.script_pubkey())
                .unwrap()
                .value,
            45_000
        );
        assert_eq!(
            tx.output
                .iter()
                .find(|txout| txout.script_pubkey != addr.script_pubkey())
                .unwrap()
                .value,
            details.received
        );

        assert_fee_rate!(psbt.extract_tx(), details.fees, FeeRate::from_sat_per_vb(5.0), @add_signature);
    }

    #[test]
    fn test_sign_single_xprv() {
        let (wallet, _, _) = get_funded_wallet("wpkh(tprv8ZgxMBicQKsPd3EupYiPRhaMooHKUHJxNsTfYuScep13go8QFfHdtkG9nRkFGb7busX4isf6X9dURGCoKgitaApQ6MupRhZMcELAxTBRJgS/*)");
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(addr.script_pubkey(), 0)]).send_all())
            .unwrap();

        let (signed_psbt, finalized) = wallet.sign(psbt, None).unwrap();
        assert_eq!(finalized, true);

        let extracted = signed_psbt.extract_tx();
        assert_eq!(extracted.input[0].witness.len(), 2);
    }

    #[test]
    fn test_sign_single_xprv_bip44_path() {
        let (wallet, _, _) = get_funded_wallet("wpkh(tprv8ZgxMBicQKsPd3EupYiPRhaMooHKUHJxNsTfYuScep13go8QFfHdtkG9nRkFGb7busX4isf6X9dURGCoKgitaApQ6MupRhZMcELAxTBRJgS/44'/0'/0'/0/*)");
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(addr.script_pubkey(), 0)]).send_all())
            .unwrap();

        let (signed_psbt, finalized) = wallet.sign(psbt, None).unwrap();
        assert_eq!(finalized, true);

        let extracted = signed_psbt.extract_tx();
        assert_eq!(extracted.input[0].witness.len(), 2);
    }

    #[test]
    fn test_sign_single_xprv_sh_wpkh() {
        let (wallet, _, _) = get_funded_wallet("sh(wpkh(tprv8ZgxMBicQKsPd3EupYiPRhaMooHKUHJxNsTfYuScep13go8QFfHdtkG9nRkFGb7busX4isf6X9dURGCoKgitaApQ6MupRhZMcELAxTBRJgS/*))");
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(addr.script_pubkey(), 0)]).send_all())
            .unwrap();

        let (signed_psbt, finalized) = wallet.sign(psbt, None).unwrap();
        assert_eq!(finalized, true);

        let extracted = signed_psbt.extract_tx();
        assert_eq!(extracted.input[0].witness.len(), 2);
    }

    #[test]
    fn test_sign_single_wif() {
        let (wallet, _, _) =
            get_funded_wallet("wpkh(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW)");
        let addr = wallet.get_new_address().unwrap();
        let (psbt, _) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(addr.script_pubkey(), 0)]).send_all())
            .unwrap();

        let (signed_psbt, finalized) = wallet.sign(psbt, None).unwrap();
        assert_eq!(finalized, true);

        let extracted = signed_psbt.extract_tx();
        assert_eq!(extracted.input[0].witness.len(), 2);
    }

    #[test]
    fn test_sign_single_xprv_no_hd_keypaths() {
        let (wallet, _, _) = get_funded_wallet("wpkh(tprv8ZgxMBicQKsPd3EupYiPRhaMooHKUHJxNsTfYuScep13go8QFfHdtkG9nRkFGb7busX4isf6X9dURGCoKgitaApQ6MupRhZMcELAxTBRJgS/*)");
        let addr = wallet.get_new_address().unwrap();
        let (mut psbt, _) = wallet
            .create_tx(TxBuilder::with_recipients(vec![(addr.script_pubkey(), 0)]).send_all())
            .unwrap();

        psbt.inputs[0].hd_keypaths.clear();
        assert_eq!(psbt.inputs[0].hd_keypaths.len(), 0);

        let (signed_psbt, finalized) = wallet.sign(psbt, None).unwrap();
        assert_eq!(finalized, true);

        let extracted = signed_psbt.extract_tx();
        assert_eq!(extracted.input[0].witness.len(), 2);
    }

    /// replicating a 3 0f 7 p2wsh p2sh wallet from electrum
    #[test]
    fn test_sign_multi_p2wsh_p2sh() {
        let pubkeys = vec![
            "020e1e9e13a2c6178c0e3cba7f6a6bb18c5363e74f35b7572a620ae72b90685680",
            "02264f891c836d99272164b962e511a017e1a1b835d5f30bdc8691c42634e59351",
            "02e0bbf72ffa17d4c0654515c8541df02958c3ef82bc09a825c1c55aa8a892afbb",
            "031f26b9d6ac186f65ef0a1b093371e99e8e17aa6d3636f7634920479df5ae5a77",
            "031f5bd798116f9701794da6904ca0d26d06e24ba920785e48671c91c35dad756e",
            "036eb794bc2b512233ec9fc8cd60e2e4ba31eb1f49796cb380ecd9898147a9ca86",
            "03a0e5c6c0212cf6d0c60867b9107d461c5e5790b032f6a5d33b3319332f1d9130",
        ];
        let priv_key = "KxUXnwf3w7y7L7bQYk8ZyUrjKacTHpZKRdo71HnnsFs2Rbwe95QZ";
        let desc = format!(
            "sh(wsh(multi(3,{},{},{},{},{},{},{})))",
            pubkeys[0], priv_key, pubkeys[2], pubkeys[3], pubkeys[4], pubkeys[5], pubkeys[6]
        );
        let wallet: OfflineWallet<_> =
            Wallet::new_offline(&desc, None, Network::Bitcoin, MemoryDatabase::default()).unwrap();

        assert_eq!(
            "325g8XuPdyYav4bZK8k4dA62hQpExMQT6C",
            wallet.get_new_address().unwrap().to_string()
        );

        let unsigned_tx = "0200000001bbc8d87eba45f99fa950c4cb513f23c07b0941e1f9058a4d1fcb52c98595c2ec0100000000000000000210270000000000001976a914bda5fbf75d67de006aa4768970113ca8a1d0727688acec7700000000000017a91404490b65f9dc2e67dbb3e5f4d7835d4582fcc4d98700000000";
        let psbt = hex::decode(unsigned_tx).unwrap();
        let psbt = deserialize(&psbt).unwrap();
        let psbt = PartiallySignedTransaction::from_unsigned_tx(psbt).unwrap();

        let (signed_psbt, finalized) = wallet.sign(psbt, None).unwrap();
        assert_eq!(finalized, false);
        assert_ne!(
            hex::encode(signed_psbt.extract_tx().serialize()),
            unsigned_tx
        );
    }
}
