// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use crate::io_extras::sink;
use crate::prelude::*;
use core::ops::Deref;

use bitcoin::blockdata::constants::WITNESS_SCALE_FACTOR;
use bitcoin::consensus::Encodable;
use bitcoin::policy::MAX_STANDARD_TX_WEIGHT;
use bitcoin::absolute::LockTime as AbsoluteLockTime;
use bitcoin::{OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut};

use crate::chain::chaininterface::fee_for_weight;
use crate::events::bump_transaction::{BASE_INPUT_WEIGHT, EMPTY_SCRIPT_SIG_WEIGHT};
use crate::ln::channel::TOTAL_BITCOIN_SUPPLY_SATOSHIS;
use crate::ln::msgs::SerialId;
use crate::ln::{msgs, ChannelId};
use crate::sign::EntropySource;
use crate::util::ser::TransactionU16LenLimited;

/// The number of received `tx_add_input` messages during a negotiation at which point the
/// negotiation MUST be failed.
const MAX_RECEIVED_TX_ADD_INPUT_COUNT: u16 = 4096;

/// The number of received `tx_add_output` messages during a negotiation at which point the
/// negotiation MUST be failed.
const MAX_RECEIVED_TX_ADD_OUTPUT_COUNT: u16 = 4096;

/// The number of inputs or outputs that the state machine can have, before it MUST fail the
/// negotiation.
const MAX_INPUTS_OUTPUTS_COUNT: usize = 252;

trait SerialIdExt {
	fn is_for_initiator(&self) -> bool;
	fn is_for_non_initiator(&self) -> bool;
}

impl SerialIdExt for SerialId {
	fn is_for_initiator(&self) -> bool {
		self % 2 == 0
	}

	fn is_for_non_initiator(&self) -> bool {
		!self.is_for_initiator()
	}
}

#[derive(Debug, Clone, PartialEq)]
pub enum AbortReason {
	InvalidStateTransition,
	UnexpectedCounterpartyMessage,
	ReceivedTooManyTxAddInputs,
	ReceivedTooManyTxAddOutputs,
	IncorrectInputSequenceValue,
	IncorrectSerialIdParity,
	SerialIdUnknown,
	DuplicateSerialId,
	PrevTxOutInvalid,
	ExceededMaximumSatsAllowed,
	ExceededNumberOfInputsOrOutputs,
	TransactionTooLarge,
	BelowDustLimit,
	InvalidOutputScript,
	InsufficientFees,
	OutputsValueExceedsInputsValue,
	InvalidTx,
}

#[derive(Debug)]
pub struct TxInputWithPrevOutput {
	input: TxIn,
	prev_output: TxOut,
}

/// Represents a shared input, composed of funds belonging to both sides.
/// In a splicing transaction, the previous funding transaction is such a shared input.
#[derive(Debug, Clone)]
pub struct SharedFundingInput {
	txin: TxIn,
	txout: TxOut,
	to_remote_value_satoshis: u64,
	to_local_value_satoshis: u64,
}

#[derive(Debug)]
struct NegotiationContext {
	holder_is_initiator: bool,
	received_tx_add_input_count: u16,
	received_tx_add_output_count: u16,
	/// If this negotiation is part of a splice, then this field will be Some and used as a shared
	/// input.
	shared_funding_input: Option<SharedFundingInput>,
	/// How much the holder is contributing to the shared funding output.
	///
	/// NOTE: Can be negative in the case of a splice-out.
	local_contribution_satoshis: i64,
	/// How much the counterparty is contributing to the shared funding output.
	///
	/// NOTE: Can be negative in the case of a splice-out.
	remote_contribution_satoshis: i64,
	/// The output intended to be the new funding output.
	new_funding_output: TxOut,
	/// The inputs to be contributed by the holder.
	inputs: HashMap<SerialId, InteractiveTxInput>,
	prevtx_outpoints: HashSet<OutPoint>,
	/// The outputs to be contributed by the holder.
	outputs: HashMap<SerialId, InteractiveTxOutput>,
	/// The locktime of the funding transaction.
	tx_locktime: AbsoluteLockTime,
	/// The fee rate used for the transaction
	feerate_sat_per_kw: u32,
}

impl NegotiationContext {
	fn new_funding_output_value_satoshis(&self) -> u64 {
		let shared_funding_input_value =
			self.shared_funding_input.as_ref().map(|input| input.txout.value).unwrap_or(0);
		let contribution =
			self.local_contribution_satoshis.saturating_add(self.remote_contribution_satoshis);

		if contribution < 0 {
			shared_funding_input_value.saturating_sub(contribution.saturating_abs().unsigned_abs())
		} else {
			shared_funding_input_value.saturating_add(contribution.saturating_abs().unsigned_abs())
		}
	}

	fn funding_output_remote_value_satoshis(&self) -> u64 {
		let remote_shared_input_value = self
			.shared_funding_input
			.as_ref()
			.map(|input| input.to_remote_value_satoshis)
			.unwrap_or(0);

		if self.remote_contribution_satoshis < 0 {
			remote_shared_input_value
				.saturating_sub(self.remote_contribution_satoshis.saturating_abs().unsigned_abs())
		} else {
			remote_shared_input_value
				.saturating_add(self.remote_contribution_satoshis.saturating_abs().unsigned_abs())
		}
	}

	fn funding_output_local_value_satoshis(&self) -> u64 {
		let local_shared_input_value = self
			.shared_funding_input
			.as_ref()
			.map(|input| input.to_local_value_satoshis)
			.unwrap_or(0);

		if self.local_contribution_satoshis < 0 {
			local_shared_input_value
				.saturating_sub(self.local_contribution_satoshis.saturating_abs().unsigned_abs())
		} else {
			local_shared_input_value
				.saturating_add(self.local_contribution_satoshis.saturating_abs().unsigned_abs())
		}
	}

	fn is_serial_id_valid_for_counterparty(&self, serial_id: &SerialId) -> bool {
		// A received `SerialId`'s parity must match the role of the counterparty.
		self.holder_is_initiator == serial_id.is_for_non_initiator()
	}

	fn total_input_and_output_count(&self) -> usize {
		self.inputs.len().saturating_add(self.outputs.len())
	}

	fn counterparty_inputs_contributed(&self) -> impl Iterator<Item = &InteractiveTxInput> + Clone {
		self.inputs
			.iter()
			.filter(move |(serial_id, _)| self.is_serial_id_valid_for_counterparty(serial_id))
			.map(|(_, input_with_prevout)| input_with_prevout)
	}

	fn counterparty_outputs_contributed(
		&self,
	) -> impl Iterator<Item = &InteractiveTxOutput> + Clone {
		self.outputs
			.iter()
			.filter(move |(serial_id, _)| self.is_serial_id_valid_for_counterparty(serial_id))
			.map(|(_, output)| output)
	}

	fn received_tx_add_input(&mut self, msg: &msgs::TxAddInput) -> Result<(), AbortReason> {
		// The interactive-txs spec calls for us to fail negotiation if the `prevtx` we receive is
		// invalid. However, we would not need to account for this explicit negotiation failure
		// mode here since `PeerManager` would already disconnect the peer if the `prevtx` is
		// invalid; implicitly ending the negotiation.

		if !self.is_serial_id_valid_for_counterparty(&msg.serial_id) {
			// The receiving node:
			//  - MUST fail the negotiation if:
			//     - the `serial_id` has the wrong parity
			return Err(AbortReason::IncorrectSerialIdParity);
		}

		self.received_tx_add_input_count += 1;
		if self.received_tx_add_input_count > MAX_RECEIVED_TX_ADD_INPUT_COUNT {
			// The receiving node:
			//  - MUST fail the negotiation if:
			//     - if has received 4096 `tx_add_input` messages during this negotiation
			return Err(AbortReason::ReceivedTooManyTxAddInputs);
		}

		if msg.sequence >= 0xFFFFFFFE {
			// The receiving node:
			//  - MUST fail the negotiation if:
			//    - `sequence` is set to `0xFFFFFFFE` or `0xFFFFFFFF`
			return Err(AbortReason::IncorrectInputSequenceValue);
		}

		let transaction = msg.prevtx.as_transaction();
		let txid = msg.shared_input_txid.unwrap_or(transaction.txid());

		if let Some(tx_out) = transaction.output.get(msg.prevtx_out as usize) {
			if !tx_out.script_pubkey.is_witness_program() {
				// The receiving node:
				//  - MUST fail the negotiation if:
				//     - the `scriptPubKey` is not a witness program
				return Err(AbortReason::PrevTxOutInvalid);
			}

			if !self.prevtx_outpoints.insert(OutPoint { txid, vout: msg.prevtx_out }) {
				// The receiving node:
				//  - MUST fail the negotiation if:
				//     - the `prevtx` and `prevtx_vout` are identical to a previously added
				//       (and not removed) input's
				return Err(AbortReason::PrevTxOutInvalid);
			}
		} else if msg.shared_input_txid.is_none() {
			// The receiving node:
			//  - MUST fail the negotiation if:
			//     - `prevtx_vout` is greater or equal to the number of outputs on `prevtx`
			return Err(AbortReason::PrevTxOutInvalid);
		}

		let prev_outpoint = OutPoint { txid, vout: msg.prevtx_out };
		match self.inputs.entry(msg.serial_id) {
			hash_map::Entry::Occupied(_) => {
				// The receiving node:
				//  - MUST fail the negotiation if:
				//    - the `serial_id` is already included in the transaction
				return Err(AbortReason::DuplicateSerialId);
			},
			hash_map::Entry::Vacant(entry) => {
				let txin = TxIn {
					previous_output: prev_outpoint.clone(),
					sequence: Sequence(msg.sequence),
					..Default::default()
				};
				let vout = txin.previous_output.vout as usize;
				let input = if msg
					.shared_input_txid
					.and_then(|txid| {
						self.shared_funding_input
							.as_ref()
							.map(|input| txid == input.txin.previous_output.txid)
					})
					.unwrap_or(false)
				{
					InteractiveTxInput::Shared(SharedInput {
						serial_id: msg.serial_id,
						txin,
						prevout_value: self
							.shared_funding_input
							.as_ref()
							.map(|input| input.txout.value)
							.unwrap_or(0),
						to_remote_value_satoshis: self
							.shared_funding_input
							.as_ref()
							.map(|input| input.to_remote_value_satoshis)
							.unwrap_or(0),
						to_local_value_satoshis: self
							.shared_funding_input
							.as_ref()
							.map(|input| input.to_local_value_satoshis)
							.unwrap_or(0),
					})
				} else {
					InteractiveTxInput::Remote(RemoteInput {
						serial_id: msg.serial_id,
						txin,
						prev_tx: msg.prevtx.clone(),
						prevout_value: msg
							.prevtx
							.as_transaction()
							.output
							.get(vout)
							.ok_or(AbortReason::PrevTxOutInvalid)?
							.value,
					})
				};
				entry.insert(input);
			},
		}
		self.prevtx_outpoints.insert(prev_outpoint);
		Ok(())
	}

	fn received_tx_remove_input(&mut self, msg: &msgs::TxRemoveInput) -> Result<(), AbortReason> {
		if !self.is_serial_id_valid_for_counterparty(&msg.serial_id) {
			return Err(AbortReason::IncorrectSerialIdParity);
		}

		self.inputs
			.remove(&msg.serial_id)
			// The receiving node:
			//  - MUST fail the negotiation if:
			//    - the input or output identified by the `serial_id` was not added by the sender
			//    - the `serial_id` does not correspond to a currently added input
			.ok_or(AbortReason::SerialIdUnknown)
			.map(|_| ())
	}

	fn received_tx_add_output(&mut self, msg: &msgs::TxAddOutput) -> Result<(), AbortReason> {
		// The receiving node:
		//  - MUST fail the negotiation if:
		//     - the serial_id has the wrong parity
		if !self.is_serial_id_valid_for_counterparty(&msg.serial_id) {
			return Err(AbortReason::IncorrectSerialIdParity);
		}

		self.received_tx_add_output_count += 1;
		if self.received_tx_add_output_count > MAX_RECEIVED_TX_ADD_OUTPUT_COUNT {
			// The receiving node:
			//  - MUST fail the negotiation if:
			//     - if has received 4096 `tx_add_output` messages during this negotiation
			return Err(AbortReason::ReceivedTooManyTxAddOutputs);
		}

		if msg.sats < msg.script.dust_value().to_sat() {
			// The receiving node:
			// - MUST fail the negotiation if:
			//		- the sats amount is less than the dust_limit
			return Err(AbortReason::BelowDustLimit);
		}

		// Check that adding this output would not cause the total output value to exceed the total
		// bitcoin supply.
		let mut outputs_value: u64 = 0;
		for output in self.outputs.iter() {
			outputs_value = outputs_value.saturating_add(output.1.value());
		}
		if outputs_value.saturating_add(msg.sats) > TOTAL_BITCOIN_SUPPLY_SATOSHIS {
			// The receiving node:
			// - MUST fail the negotiation if:
			//		- the sats amount is greater than 2,100,000,000,000,000 (TOTAL_BITCOIN_SUPPLY_SATOSHIS)
			return Err(AbortReason::ExceededMaximumSatsAllowed);
		}

		// The receiving node:
		//   - MUST accept P2WSH, P2WPKH, P2TR scripts
		//   - MAY fail the negotiation if script is non-standard
		//
		// We can actually be a bit looser than the above as only witness version 0 has special
		// length-based standardness constraints to match similar consensus rules. All witness scripts
		// with witness versions V1 and up are always considered standard. Yes, the scripts can be
		// anyone-can-spend-able, but if our counterparty wants to add an output like that then it's none
		// of our concern really ¯\_(ツ)_/¯
		//
		// TODO: The last check would be simplified when https://github.com/rust-bitcoin/rust-bitcoin/commit/1656e1a09a1959230e20af90d20789a4a8f0a31b
		// hits the next release of rust-bitcoin.
		if !(msg.script.is_v0_p2wpkh()
			|| msg.script.is_v0_p2wsh()
			|| (msg.script.is_witness_program()
				&& msg.script.witness_version().map(|v| v.to_num() >= 1).unwrap_or(false)))
		{
			return Err(AbortReason::InvalidOutputScript);
		}

		let output = if msg.script == self.new_funding_output.script_pubkey {
			InteractiveTxOutput::Shared(SharedOutput {
				serial_id: msg.serial_id,
				txout: TxOut { value: msg.sats, script_pubkey: msg.script.clone() },
				to_remote_value_satoshis: self.funding_output_remote_value_satoshis(),
				to_local_value_satoshis: self.funding_output_local_value_satoshis(),
			})
		} else {
			InteractiveTxOutput::Remote(RemoteOutput {
				serial_id: msg.serial_id,
				txout: TxOut { value: msg.sats, script_pubkey: msg.script.clone() },
			})
		};
		match self.outputs.entry(msg.serial_id) {
			hash_map::Entry::Occupied(_) => {
				// The receiving node:
				//  - MUST fail the negotiation if:
				//    - the `serial_id` is already included in the transaction
				return Err(AbortReason::DuplicateSerialId);
			},
			hash_map::Entry::Vacant(entry) => {
				entry.insert(output);
			},
		};
		Ok(())
	}

	fn received_tx_remove_output(&mut self, msg: &msgs::TxRemoveOutput) -> Result<(), AbortReason> {
		if !self.is_serial_id_valid_for_counterparty(&msg.serial_id) {
			return Err(AbortReason::IncorrectSerialIdParity);
		}
		if let Some(_) = self.outputs.remove(&msg.serial_id) {
			Ok(())
		} else {
			// The receiving node:
			//  - MUST fail the negotiation if:
			//    - the input or output identified by the `serial_id` was not added by the sender
			//    - the `serial_id` does not correspond to a currently added input
			Err(AbortReason::SerialIdUnknown)
		}
	}

	fn sent_tx_add_input(&mut self, msg: &msgs::TxAddInput) -> Result<(), AbortReason> {
		let tx = msg.prevtx.as_transaction();
		let txin = TxIn {
			previous_output: OutPoint { txid: tx.txid(), vout: msg.prevtx_out },
			sequence: Sequence(msg.sequence),
			..Default::default()
		};
		if !self.prevtx_outpoints.insert(txin.previous_output.clone()) {
			// We have added an input that already exists
			return Err(AbortReason::PrevTxOutInvalid);
		}
		let vout = txin.previous_output.vout as usize;
		let input = if let Some(_) = msg.shared_input_txid {
			InteractiveTxInput::Shared(SharedInput {
				serial_id: msg.serial_id,
				txin: self
					.shared_funding_input
					.as_ref()
					.map(|input| input.txin.clone())
					.unwrap_or(txin),
				prevout_value: self
					.shared_funding_input
					.as_ref()
					.map(|input| input.txout.value)
					.unwrap_or(0),
				to_remote_value_satoshis: self
					.shared_funding_input
					.as_ref()
					.map(|input| input.to_remote_value_satoshis)
					.unwrap_or(0),
				to_local_value_satoshis: self
					.shared_funding_input
					.as_ref()
					.map(|input| input.to_local_value_satoshis)
					.unwrap_or(0),
			})
		} else {
			InteractiveTxInput::Local(LocalInput {
				serial_id: msg.serial_id,
				txin,
				prev_tx: msg.prevtx.clone(),
				prevout_value: msg
					.prevtx
					.as_transaction()
					.output
					.get(vout)
					.ok_or(AbortReason::PrevTxOutInvalid)?
					.value,
			})
		};
		self.inputs.insert(msg.serial_id, input);
		Ok(())
	}

	fn sent_tx_add_output(&mut self, msg: &msgs::TxAddOutput) -> Result<(), AbortReason> {
		let txout = TxOut { value: msg.sats, script_pubkey: msg.script.clone() };
		let output = if msg.script == self.new_funding_output.script_pubkey {
			InteractiveTxOutput::Shared(SharedOutput {
				serial_id: msg.serial_id,
				txout,
				to_remote_value_satoshis: self.funding_output_remote_value_satoshis(),
				to_local_value_satoshis: self.funding_output_local_value_satoshis(),
			})
		} else {
			InteractiveTxOutput::Local(LocalOutput { serial_id: msg.serial_id, txout })
		};
		self.outputs.insert(msg.serial_id, output);
		Ok(())
	}

	fn sent_tx_remove_input(&mut self, msg: &msgs::TxRemoveInput) -> Result<(), AbortReason> {
		self.inputs.remove(&msg.serial_id);
		Ok(())
	}

	fn sent_tx_remove_output(&mut self, msg: &msgs::TxRemoveOutput) -> Result<(), AbortReason> {
		self.outputs.remove(&msg.serial_id);
		Ok(())
	}

	fn build_transaction(self) -> Result<Transaction, AbortReason> {
		// The receiving node:
		// MUST fail the negotiation if:

		// - the peer's total input satoshis with its part of any shared input is less than their outputs
		//   and proportion of any shared output
		let mut counterparty_value_in: u64 = 0;
		let mut counterparty_value_out: u64 = 0;
		for input in self.counterparty_inputs_contributed() {
			let value = match input {
				InteractiveTxInput::Local(input) => input.prevout_value,
				InteractiveTxInput::Remote(input) => input.prevout_value,
				InteractiveTxInput::Shared(input) => input.to_remote_value_satoshis,
			};
			counterparty_value_in = counterparty_value_in.saturating_add(value);
		}
		for output in self.counterparty_outputs_contributed() {
			let value = match output {
				InteractiveTxOutput::Local(output) => output.txout.value,
				InteractiveTxOutput::Remote(output) => output.txout.value,
				InteractiveTxOutput::Shared(output) => output.to_remote_value_satoshis,
			};
			counterparty_value_out = counterparty_value_out.saturating_add(value);
		}
		if counterparty_value_in < counterparty_value_out {
			return Err(AbortReason::OutputsValueExceedsInputsValue);
		}

		// - there are more than 252 inputs
		// - there are more than 252 outputs
		if self.inputs.len() > MAX_INPUTS_OUTPUTS_COUNT
			|| self.outputs.len() > MAX_INPUTS_OUTPUTS_COUNT
		{
			return Err(AbortReason::ExceededNumberOfInputsOrOutputs);
		}

		// TODO: How do we enforce their fees cover the witness without knowing its expected length?
		const INPUT_WEIGHT: u64 = BASE_INPUT_WEIGHT + EMPTY_SCRIPT_SIG_WEIGHT;

		// - the peer's paid feerate does not meet or exceed the agreed feerate (based on the minimum fee).
		let mut counterparty_weight_contributed: u64 = self
			.counterparty_outputs_contributed()
			.map(|output| {
				(8 /* value */ + output.txout().script_pubkey.consensus_encode(&mut sink()).unwrap() as u64)
					* WITNESS_SCALE_FACTOR as u64
			})
			.sum();
		counterparty_weight_contributed +=
			self.counterparty_inputs_contributed().count() as u64 * INPUT_WEIGHT;
		let counterparty_fees_contributed =
			counterparty_value_in.saturating_sub(counterparty_value_out);
		let mut required_counterparty_contribution_fee =
			fee_for_weight(self.feerate_sat_per_kw, counterparty_weight_contributed);
		if !self.holder_is_initiator {
			// if is the non-initiator:
			// 	- the initiator's fees do not cover the common fields (version, segwit marker + flag,
			// 		input count, output count, locktime)
			let tx_common_fields_weight =
		        (4 /* version */ + 4 /* locktime */ + 1 /* input count */ + 1 /* output count */) *
		            WITNESS_SCALE_FACTOR as u64 + 2 /* segwit marker + flag */;
			let tx_common_fields_fee =
				fee_for_weight(self.feerate_sat_per_kw, tx_common_fields_weight);
			required_counterparty_contribution_fee += tx_common_fields_fee;
		}
		if counterparty_fees_contributed < required_counterparty_contribution_fee {
			return Err(AbortReason::InsufficientFees);
		}

		// Inputs and outputs must be sorted by serial_id
		let mut inputs = self.inputs.into_iter().collect::<Vec<_>>();
		let mut outputs = self.outputs.into_iter().collect::<Vec<_>>();
		inputs.sort_unstable_by_key(|(serial_id, _)| *serial_id);
		outputs.sort_unstable_by_key(|(serial_id, _)| *serial_id);

		let tx_to_validate = Transaction {
			version: 2,
			lock_time: self.tx_locktime,
			input: inputs
				.into_iter()
				.map(|(_, input)| match input {
					InteractiveTxInput::Local(input) => input.txin,
					InteractiveTxInput::Remote(input) => input.txin,
					InteractiveTxInput::Shared(input) => input.txin,
				})
				.collect(),
			output: outputs
				.into_iter()
				.map(|(_, output)| match output {
					InteractiveTxOutput::Local(output) => output.txout,
					InteractiveTxOutput::Remote(output) => output.txout,
					InteractiveTxOutput::Shared(output) => output.txout,
				})
				.collect(),
		};
		if tx_to_validate.weight().to_wu() > MAX_STANDARD_TX_WEIGHT as u64 {
			return Err(AbortReason::TransactionTooLarge);
		}

		Ok(tx_to_validate)
	}
}

// The interactive transaction construction protocol allows two peers to collaboratively build a
// transaction for broadcast.
//
// The protocol is turn-based, so we define different states here that we store depending on whose
// turn it is to send the next message. The states are defined so that their types ensure we only
// perform actions (only send messages) via defined state transitions that do not violate the
// protocol.
//
// An example of a full negotiation and associated states follows:
//
//     +------------+                         +------------------+---- Holder state after message sent/received ----+
//     |            |--(1)- tx_add_input ---->|                  |                  SentChangeMsg                   +
//     |            |<-(2)- tx_complete ------|                  |                ReceivedTxComplete                +
//     |            |--(3)- tx_add_output --->|                  |                  SentChangeMsg                   +
//     |            |<-(4)- tx_complete ------|                  |                ReceivedTxComplete                +
//     |            |--(5)- tx_add_input ---->|                  |                  SentChangeMsg                   +
//     |   Holder   |<-(6)- tx_add_input -----|   Counterparty   |                ReceivedChangeMsg                 +
//     |            |--(7)- tx_remove_output >|                  |                  SentChangeMsg                   +
//     |            |<-(8)- tx_add_output ----|                  |                ReceivedChangeMsg                 +
//     |            |--(9)- tx_complete ----->|                  |                  SentTxComplete                  +
//     |            |<-(10) tx_complete ------|                  |                NegotiationComplete               +
//     +------------+                         +------------------+--------------------------------------------------+

/// Negotiation states that can send & receive `tx_(add|remove)_(input|output)` and `tx_complete`
trait State {}

/// Category of states where we have sent some message to the counterparty, and we are waiting for
/// a response.
trait SentMsgState: State {
	fn into_negotiation_context(self) -> NegotiationContext;
}

/// Category of states that our counterparty has put us in after we receive a message from them.
trait ReceivedMsgState: State {
	fn into_negotiation_context(self) -> NegotiationContext;
}

// This macro is a helper for implementing the above state traits for various states subsequently
// defined below the macro.
macro_rules! define_state {
	(SENT_MSG_STATE, $state: ident, $doc: expr) => {
		define_state!($state, NegotiationContext, $doc);
		impl SentMsgState for $state {
			fn into_negotiation_context(self) -> NegotiationContext {
				self.0
			}
		}
	};
	(RECEIVED_MSG_STATE, $state: ident, $doc: expr) => {
		define_state!($state, NegotiationContext, $doc);
		impl ReceivedMsgState for $state {
			fn into_negotiation_context(self) -> NegotiationContext {
				self.0
			}
		}
	};
	($state: ident, $inner: ident, $doc: expr) => {
		#[doc = $doc]
		#[derive(Debug)]
		struct $state($inner);
		impl State for $state {}
	};
}

define_state!(
	SENT_MSG_STATE,
	SentChangeMsg,
	"We have sent a message to the counterparty that has affected our negotiation state."
);
define_state!(
	SENT_MSG_STATE,
	SentTxComplete,
	"We have sent a `tx_complete` message and are awaiting the counterparty's."
);
define_state!(
	RECEIVED_MSG_STATE,
	ReceivedChangeMsg,
	"We have received a message from the counterparty that has affected our negotiation state."
);
define_state!(
	RECEIVED_MSG_STATE,
	ReceivedTxComplete,
	"We have received a `tx_complete` message and the counterparty is awaiting ours."
);
define_state!(NegotiationComplete, Transaction, "We have exchanged consecutive `tx_complete` messages with the counterparty and the transaction negotiation is complete.");
define_state!(
	NegotiationAborted,
	AbortReason,
	"The negotiation has failed and cannot be continued."
);

type StateTransitionResult<S> = Result<S, AbortReason>;

trait StateTransition<NewState: State, TransitionData> {
	fn transition(self, data: TransitionData) -> StateTransitionResult<NewState>;
}

// This macro helps define the legal transitions between the states above by implementing
// the `StateTransition` trait for each of the states that follow this declaration.
macro_rules! define_state_transitions {
	(SENT_MSG_STATE, [$(DATA $data: ty, TRANSITION $transition: ident),+]) => {
		$(
			impl<S: SentMsgState> StateTransition<ReceivedChangeMsg, $data> for S {
				fn transition(self, data: $data) -> StateTransitionResult<ReceivedChangeMsg> {
					let mut context = self.into_negotiation_context();
					context.$transition(data)?;
					Ok(ReceivedChangeMsg(context))
				}
			}
		 )*
	};
	(RECEIVED_MSG_STATE, [$(DATA $data: ty, TRANSITION $transition: ident),+]) => {
		$(
			impl<S: ReceivedMsgState> StateTransition<SentChangeMsg, $data> for S {
				fn transition(self, data: $data) -> StateTransitionResult<SentChangeMsg> {
					let mut context = self.into_negotiation_context();
					context.$transition(data)?;
					Ok(SentChangeMsg(context))
				}
			}
		 )*
	};
	(TX_COMPLETE, $from_state: ident, $tx_complete_state: ident) => {
		impl StateTransition<NegotiationComplete, &msgs::TxComplete> for $tx_complete_state {
			fn transition(self, _data: &msgs::TxComplete) -> StateTransitionResult<NegotiationComplete> {
				let context = self.into_negotiation_context();
				let tx = context.build_transaction()?;
				Ok(NegotiationComplete(tx))
			}
		}

		impl StateTransition<$tx_complete_state, &msgs::TxComplete> for $from_state {
			fn transition(self, _data: &msgs::TxComplete) -> StateTransitionResult<$tx_complete_state> {
				Ok($tx_complete_state(self.into_negotiation_context()))
			}
		}
	};
}

// State transitions when we have sent our counterparty some messages and are waiting for them
// to respond.
define_state_transitions!(SENT_MSG_STATE, [
	DATA &msgs::TxAddInput, TRANSITION received_tx_add_input,
	DATA &msgs::TxRemoveInput, TRANSITION received_tx_remove_input,
	DATA &msgs::TxAddOutput, TRANSITION received_tx_add_output,
	DATA &msgs::TxRemoveOutput, TRANSITION received_tx_remove_output
]);
// State transitions when we have received some messages from our counterparty and we should
// respond.
define_state_transitions!(RECEIVED_MSG_STATE, [
	DATA &msgs::TxAddInput, TRANSITION sent_tx_add_input,
	DATA &msgs::TxRemoveInput, TRANSITION sent_tx_remove_input,
	DATA &msgs::TxAddOutput, TRANSITION sent_tx_add_output,
	DATA &msgs::TxRemoveOutput, TRANSITION sent_tx_remove_output
]);
define_state_transitions!(TX_COMPLETE, SentChangeMsg, ReceivedTxComplete);
define_state_transitions!(TX_COMPLETE, ReceivedChangeMsg, SentTxComplete);

#[derive(Debug)]
enum StateMachine {
	Indeterminate,
	SentChangeMsg(SentChangeMsg),
	ReceivedChangeMsg(ReceivedChangeMsg),
	SentTxComplete(SentTxComplete),
	ReceivedTxComplete(ReceivedTxComplete),
	NegotiationComplete(NegotiationComplete),
	NegotiationAborted(NegotiationAborted),
}

impl Default for StateMachine {
	fn default() -> Self {
		Self::Indeterminate
	}
}

// The `StateMachine` internally executes the actual transition between two states and keeps
// track of the current state. This macro defines _how_ those state transitions happen to
// update the internal state.
macro_rules! define_state_machine_transitions {
	($transition: ident, $msg: ty, [$(FROM $from_state: ident, TO $to_state: ident),+]) => {
		fn $transition(self, msg: $msg) -> StateMachine {
			match self {
				$(
					Self::$from_state(s) => match s.transition(msg) {
						Ok(new_state) => StateMachine::$to_state(new_state),
						Err(abort_reason) => StateMachine::NegotiationAborted(NegotiationAborted(abort_reason)),
					}
				 )*
				_ => StateMachine::NegotiationAborted(NegotiationAborted(AbortReason::UnexpectedCounterpartyMessage)),
			}
		}
	};
}

impl StateMachine {
	fn new(
		feerate_sat_per_kw: u32, is_initiator: bool, tx_locktime: AbsoluteLockTime,
		shared_funding_input: Option<SharedFundingInput>, new_funding_output: TxOut,
		remote_contribution_satoshis: i64, local_contribution_satoshis: i64,
	) -> Self {
		let context = NegotiationContext {
			tx_locktime,
			holder_is_initiator: is_initiator,
			received_tx_add_input_count: 0,
			received_tx_add_output_count: 0,
			inputs: new_hash_map(),
			prevtx_outpoints: new_hash_set(),
			outputs: new_hash_map(),
			feerate_sat_per_kw,
			shared_funding_input,
			local_contribution_satoshis,
			remote_contribution_satoshis,
			new_funding_output,
		};
		if is_initiator {
			Self::ReceivedChangeMsg(ReceivedChangeMsg(context))
		} else {
			Self::SentChangeMsg(SentChangeMsg(context))
		}
	}

	// TxAddInput
	define_state_machine_transitions!(sent_tx_add_input, &msgs::TxAddInput, [
		FROM ReceivedChangeMsg, TO SentChangeMsg,
		FROM ReceivedTxComplete, TO SentChangeMsg
	]);
	define_state_machine_transitions!(received_tx_add_input, &msgs::TxAddInput, [
		FROM SentChangeMsg, TO ReceivedChangeMsg,
		FROM SentTxComplete, TO ReceivedChangeMsg
	]);

	// TxAddOutput
	define_state_machine_transitions!(sent_tx_add_output, &msgs::TxAddOutput, [
		FROM ReceivedChangeMsg, TO SentChangeMsg,
		FROM ReceivedTxComplete, TO SentChangeMsg
	]);
	define_state_machine_transitions!(received_tx_add_output, &msgs::TxAddOutput, [
		FROM SentChangeMsg, TO ReceivedChangeMsg,
		FROM SentTxComplete, TO ReceivedChangeMsg
	]);

	// TxRemoveInput
	define_state_machine_transitions!(sent_tx_remove_input, &msgs::TxRemoveInput, [
		FROM ReceivedChangeMsg, TO SentChangeMsg,
		FROM ReceivedTxComplete, TO SentChangeMsg
	]);
	define_state_machine_transitions!(received_tx_remove_input, &msgs::TxRemoveInput, [
		FROM SentChangeMsg, TO ReceivedChangeMsg,
		FROM SentTxComplete, TO ReceivedChangeMsg
	]);

	// TxRemoveOutput
	define_state_machine_transitions!(sent_tx_remove_output, &msgs::TxRemoveOutput, [
		FROM ReceivedChangeMsg, TO SentChangeMsg,
		FROM ReceivedTxComplete, TO SentChangeMsg
	]);
	define_state_machine_transitions!(received_tx_remove_output, &msgs::TxRemoveOutput, [
		FROM SentChangeMsg, TO ReceivedChangeMsg,
		FROM SentTxComplete, TO ReceivedChangeMsg
	]);

	// TxComplete
	define_state_machine_transitions!(sent_tx_complete, &msgs::TxComplete, [
		FROM ReceivedChangeMsg, TO SentTxComplete,
		FROM ReceivedTxComplete, TO NegotiationComplete
	]);
	define_state_machine_transitions!(received_tx_complete, &msgs::TxComplete, [
		FROM SentChangeMsg, TO ReceivedTxComplete,
		FROM SentTxComplete, TO NegotiationComplete
	]);
}

#[derive(Debug)]
pub struct LocalInput {
	serial_id: SerialId,
	txin: TxIn,
	prev_tx: TransactionU16LenLimited,
	prevout_value: u64,
}

#[derive(Debug)]
pub struct RemoteInput {
	serial_id: SerialId,
	txin: TxIn,
	prev_tx: TransactionU16LenLimited,
	prevout_value: u64,
}

#[derive(Debug)]
pub struct SharedInput {
	serial_id: SerialId,
	txin: TxIn,
	prevout_value: u64,
	to_remote_value_satoshis: u64,
	to_local_value_satoshis: u64,
}

#[derive(Debug)]
pub enum InteractiveTxInput {
	Local(LocalInput),
	Remote(RemoteInput),
	Shared(SharedInput),
}

#[derive(Debug)]
pub struct LocalOutput {
	serial_id: SerialId,
	txout: TxOut,
}

#[derive(Debug)]
pub struct RemoteOutput {
	serial_id: SerialId,
	txout: TxOut,
}

#[derive(Debug)]
pub struct SharedOutput {
	serial_id: SerialId,
	txout: TxOut,
	to_remote_value_satoshis: u64,
	to_local_value_satoshis: u64,
}

impl InteractiveTxInput {
	pub fn serial_id(&self) -> SerialId {
		match self {
			InteractiveTxInput::Local(input) => input.serial_id,
			InteractiveTxInput::Remote(input) => input.serial_id,
			InteractiveTxInput::Shared(input) => input.serial_id,
		}
	}

	pub fn txin(&self) -> &TxIn {
		match self {
			InteractiveTxInput::Local(input) => &input.txin,
			InteractiveTxInput::Remote(input) => &input.txin,
			InteractiveTxInput::Shared(input) => &input.txin,
		}
	}

	pub fn value(&self) -> Result<u64, AbortReason> {
		Ok(match self {
			InteractiveTxInput::Local(input) => input.prevout_value,
			InteractiveTxInput::Remote(input) => input.prevout_value,
			InteractiveTxInput::Shared(input) => input.prevout_value,
		})
	}
}

impl InteractiveTxOutput {
	pub fn serial_id(&self) -> SerialId {
		match self {
			InteractiveTxOutput::Local(output) => output.serial_id,
			InteractiveTxOutput::Remote(output) => output.serial_id,
			InteractiveTxOutput::Shared(output) => output.serial_id,
		}
	}

	pub fn txout(&self) -> &TxOut {
		match self {
			InteractiveTxOutput::Local(output) => &output.txout,
			InteractiveTxOutput::Remote(output) => &output.txout,
			InteractiveTxOutput::Shared(output) => &output.txout,
		}
	}

	pub fn value(&self) -> u64 { self.txout().value }

	pub fn script_pubkey(&self) -> &ScriptBuf { &self.txout().script_pubkey }
}

#[derive(Debug)]
pub enum InteractiveTxOutput {
	Local(LocalOutput),
	Remote(RemoteOutput),
	Shared(SharedOutput),
}

pub struct InteractiveTxConstructor {
	state_machine: StateMachine,
	channel_id: ChannelId,
	inputs_to_contribute: Vec<InteractiveTxInput>,
	outputs_to_contribute: Vec<InteractiveTxOutput>,
}

pub enum InteractiveTxMessageSend {
	TxAddInput(msgs::TxAddInput),
	TxAddOutput(msgs::TxAddOutput),
	TxComplete(msgs::TxComplete),
}

// This macro executes a state machine transition based on a provided action.
macro_rules! do_state_transition {
	($self: ident, $transition: ident, $msg: expr) => {{
		let state_machine = core::mem::take(&mut $self.state_machine);
		$self.state_machine = state_machine.$transition($msg);
		match &$self.state_machine {
			StateMachine::NegotiationAborted(state) => Err(state.0.clone()),
			_ => Ok(()),
		}
	}};
}

fn generate_holder_serial_id<ES: Deref>(entropy_source: &ES, is_initiator: bool) -> SerialId
where
	ES::Target: EntropySource,
{
	let rand_bytes = entropy_source.get_secure_random_bytes();
	let mut serial_id_bytes = [0u8; 8];
	serial_id_bytes.copy_from_slice(&rand_bytes[..8]);
	let mut serial_id = u64::from_be_bytes(serial_id_bytes);
	if serial_id.is_for_initiator() != is_initiator {
		serial_id ^= 1;
	}
	serial_id
}

pub enum HandleTxCompleteValue {
	SendTxMessage(InteractiveTxMessageSend),
	SendTxComplete(InteractiveTxMessageSend, Transaction),
	NegotiationComplete(Transaction),
}

impl InteractiveTxConstructor {
	/// Instantiates a new `InteractiveTxConstructor`.
	///
	/// If this is for a dual_funded channel then the `to_remote_value_satoshis` parameter should be set
	/// to zero.
	///
	/// A tuple is returned containing the newly instantiate `InteractiveTxConstructor` and optionally
	/// an initial wrapped `Tx_` message which the holder needs to send to the counterparty.
	pub fn new<ES: Deref>(
		entropy_source: &ES, channel_id: ChannelId, feerate_sat_per_kw: u32, is_initiator: bool,
		funding_tx_locktime: AbsoluteLockTime,
		inputs_to_contribute: Vec<(TxIn, TransactionU16LenLimited)>,
		outputs_to_contribute: Vec<TxOut>, shared_funding_input: Option<SharedFundingInput>,
		new_funding_output: TxOut, remote_contribution_satoshis: i64,
		local_contribution_satoshis: i64,
	) -> Result<(Self, Option<InteractiveTxMessageSend>), String>
	// TODO: Better error
	where
		ES::Target: EntropySource,
	{
		let state_machine = StateMachine::new(
			feerate_sat_per_kw,
			is_initiator,
			funding_tx_locktime,
			shared_funding_input.clone(),
			new_funding_output,
			remote_contribution_satoshis,
			local_contribution_satoshis,
		);
		let mut inputs: Vec<InteractiveTxInput> = Vec::with_capacity(inputs_to_contribute.len());
		for input in inputs_to_contribute {
			let (txin, prev_tx) = input;
			let serial_id = generate_holder_serial_id(entropy_source, is_initiator);
			let prevout_value = prev_tx
				.as_transaction()
				.output
				.get(txin.previous_output.vout as usize)
				.ok_or(format!(
					"The previous output's vout ({}) out of range of previous tx's ({}) {} outputs",
					txin.previous_output.vout,
					prev_tx.as_transaction().txid(),
					prev_tx.as_transaction().input.len()
				))?
				.value;
			inputs.push(InteractiveTxInput::Local(LocalInput {
				serial_id,
				txin,
				prev_tx,
				prevout_value,
			}));
		}
		if is_initiator {
			if let Some(shared_funding_input) = shared_funding_input {
				inputs.push(InteractiveTxInput::Shared(SharedInput {
					serial_id: generate_holder_serial_id(entropy_source, is_initiator),
					txin: shared_funding_input.txin,
					prevout_value: shared_funding_input.txout.value,
					to_remote_value_satoshis: shared_funding_input.to_remote_value_satoshis,
					to_local_value_satoshis: shared_funding_input.to_local_value_satoshis,
				}))
			}
		}
		// We'll sort by the randomly generated serial IDs, effectively shuffling the order of the inputs
		// as the user passed them to us to avoid leaking any potential categorization of transactions
		// before we pass any of the inputs to the counterparty.
		inputs.sort_unstable_by_key(|input| input.serial_id());
		let mut outputs_to_contribute: Vec<InteractiveTxOutput> = outputs_to_contribute
			.into_iter()
			.map(|txout| {
				let serial_id = generate_holder_serial_id(entropy_source, is_initiator);
				InteractiveTxOutput::Local(LocalOutput { serial_id, txout })
			})
			.collect();
		// In the same manner and for the same rationale as the inputs above, we'll shuffle the outputs.
		outputs_to_contribute.sort_unstable_by_key(|output| output.serial_id());
		let mut constructor =
			Self { state_machine, channel_id, inputs_to_contribute: inputs, outputs_to_contribute };
		let message_send = if is_initiator {
			match constructor.maybe_send_message() {
				Ok(msg_send) => Some(msg_send),
				Err(_) => {
					debug_assert!(
						false,
						"We should always be able to start our state machine successfully"
					);
					None
				},
			}
		} else {
			None
		};
		Ok((constructor, message_send))
	}

	fn maybe_send_message(&mut self) -> Result<InteractiveTxMessageSend, AbortReason> {
		// We first attempt to send inputs we want to add, then outputs. Once we are done sending
		// them both, then we always send tx_complete.
		if let Some(input) = self.inputs_to_contribute.pop() {
			let (serial_id, prevtx, txin, shared_input_txid) = match input {
				InteractiveTxInput::Local(i) => (i.serial_id, i.prev_tx, i.txin, None),
				InteractiveTxInput::Remote(i) => (i.serial_id, i.prev_tx, i.txin, None),
				InteractiveTxInput::Shared(i) => {
					let txid = i.txin.previous_output.txid;
					(
						i.serial_id,
						TransactionU16LenLimited::new(Transaction {
							version: 2,
							lock_time: AbsoluteLockTime::from_height(1)
								.map_err(|_| AbortReason::PrevTxOutInvalid)?,
							input: vec![],
							output: vec![],
						})
						.map_err(|_| AbortReason::PrevTxOutInvalid)?,
						i.txin,
						Some(txid),
					)
				},
			};
			let msg = msgs::TxAddInput {
				channel_id: self.channel_id,
				serial_id,
				prevtx,
				prevtx_out: txin.previous_output.vout,
				sequence: txin.sequence.to_consensus_u32(),
				shared_input_txid,
			};
			do_state_transition!(self, sent_tx_add_input, &msg)?;
			Ok(InteractiveTxMessageSend::TxAddInput(msg))
		} else if let Some(output) = self.outputs_to_contribute.pop() {
			let (serial_id, txout) = match output {
				InteractiveTxOutput::Local(o) => (o.serial_id, o.txout),
				InteractiveTxOutput::Remote(o) => (o.serial_id, o.txout),
				InteractiveTxOutput::Shared(o) => (o.serial_id, o.txout),
			};
			let msg = msgs::TxAddOutput {
				channel_id: self.channel_id,
				serial_id,
				sats: txout.value,
				script: txout.script_pubkey,
			};
			do_state_transition!(self, sent_tx_add_output, &msg)?;
			Ok(InteractiveTxMessageSend::TxAddOutput(msg))
		} else {
			let msg = msgs::TxComplete { channel_id: self.channel_id };
			do_state_transition!(self, sent_tx_complete, &msg)?;
			Ok(InteractiveTxMessageSend::TxComplete(msg))
		}
	}

	pub fn handle_tx_add_input(
		&mut self, msg: &msgs::TxAddInput,
	) -> Result<InteractiveTxMessageSend, AbortReason> {
		do_state_transition!(self, received_tx_add_input, msg)?;
		self.maybe_send_message()
	}

	pub fn handle_tx_remove_input(
		&mut self, msg: &msgs::TxRemoveInput,
	) -> Result<InteractiveTxMessageSend, AbortReason> {
		do_state_transition!(self, received_tx_remove_input, msg)?;
		self.maybe_send_message()
	}

	pub fn handle_tx_add_output(
		&mut self, msg: &msgs::TxAddOutput,
	) -> Result<InteractiveTxMessageSend, AbortReason> {
		do_state_transition!(self, received_tx_add_output, msg)?;
		self.maybe_send_message()
	}

	pub fn handle_tx_remove_output(
		&mut self, msg: &msgs::TxRemoveOutput,
	) -> Result<InteractiveTxMessageSend, AbortReason> {
		do_state_transition!(self, received_tx_remove_output, msg)?;
		self.maybe_send_message()
	}

	pub fn handle_tx_complete(
		&mut self, msg: &msgs::TxComplete,
	) -> Result<HandleTxCompleteValue, AbortReason> {
		do_state_transition!(self, received_tx_complete, msg)?;
		match &self.state_machine {
			StateMachine::ReceivedTxComplete(_) => {
				let msg_send = self.maybe_send_message()?;
				return match &self.state_machine {
					StateMachine::NegotiationComplete(s) => {
						Ok(HandleTxCompleteValue::SendTxComplete(msg_send, s.0.clone()))
					},
					StateMachine::SentChangeMsg(_) => {
						Ok(HandleTxCompleteValue::SendTxMessage(msg_send))
					}, // We either had an input or output to contribute.
					_ => {
						debug_assert!(false, "We cannot transition to any other states after receiving `tx_complete` and responding");
						return Err(AbortReason::InvalidStateTransition);
					},
				};
			},
			StateMachine::NegotiationComplete(s) => {
				Ok(HandleTxCompleteValue::NegotiationComplete(s.0.clone()))
			},
			_ => {
				debug_assert!(
					false,
					"We cannot transition to any other states after receiving `tx_complete`"
				);
				Err(AbortReason::InvalidStateTransition)
			},
		}
	}
}

#[cfg(test)]
mod tests {
	use crate::chain::chaininterface::FEERATE_FLOOR_SATS_PER_KW;
	use crate::ln::channel::TOTAL_BITCOIN_SUPPLY_SATOSHIS;
	use crate::ln::interactivetxs::{
		generate_holder_serial_id, AbortReason, HandleTxCompleteValue, InteractiveTxConstructor,
		InteractiveTxMessageSend, MAX_INPUTS_OUTPUTS_COUNT, MAX_RECEIVED_TX_ADD_INPUT_COUNT,
		MAX_RECEIVED_TX_ADD_OUTPUT_COUNT,
	};
	use crate::ln::ChannelId;
	use crate::sign::EntropySource;
	use crate::util::atomic_counter::AtomicCounter;
	use crate::util::ser::TransactionU16LenLimited;
	use bitcoin::blockdata::opcodes;
	use bitcoin::blockdata::script::Builder;
	use bitcoin::{
		absolute::LockTime as AbsoluteLockTime, OutPoint, Sequence, Transaction, TxIn, TxOut,
	};
	use core::ops::Deref;

	use super::SharedFundingInput;

	// A simple entropy source that works based on an atomic counter.
	struct TestEntropySource(AtomicCounter);
	impl EntropySource for TestEntropySource {
		fn get_secure_random_bytes(&self) -> [u8; 32] {
			let mut res = [0u8; 32];
			let increment = self.0.get_increment();
			for i in 0..32 {
				// Rotate the increment value by 'i' bits to the right, to avoid clashes
				// when `generate_local_serial_id` does a parity flip on consecutive calls for the
				// same party.
				let rotated_increment = increment.rotate_right(i as u32);
				res[i] = (rotated_increment & 0xff) as u8;
			}
			res
		}
	}

	// An entropy source that deliberately returns you the same seed every time. We use this
	// to test if the constructor would catch inputs/outputs that are attempting to be added
	// with duplicate serial ids.
	struct DuplicateEntropySource;
	impl EntropySource for DuplicateEntropySource {
		fn get_secure_random_bytes(&self) -> [u8; 32] {
			let mut res = [0u8; 32];
			let count = 1u64;
			res[0..8].copy_from_slice(&count.to_be_bytes());
			res
		}
	}

	#[derive(Debug, PartialEq, Eq)]
	enum ErrorCulprit {
		NodeA,
		NodeB,
		// Some error values are only checked at the end of the negotiation and are not easy to attribute
		// to a particular party. Both parties would indicate an `AbortReason` in this case.
		// e.g. Exceeded max inputs and outputs after negotiation.
		Indeterminate,
	}

	struct TestSession {
		inputs_a: Vec<(TxIn, TransactionU16LenLimited)>,
		outputs_a: Vec<TxOut>,
		inputs_b: Vec<(TxIn, TransactionU16LenLimited)>,
		outputs_b: Vec<TxOut>,
		shared_funding_input: Option<SharedFundingInput>,
		new_funding_output: TxOut,
		b_funding_satoshis: i64,
		a_funding_satoshis: i64,
		expect_error: Option<(AbortReason, ErrorCulprit)>,
		description: String,
	}

	fn do_test_interactive_tx_constructor(session: TestSession) {
		let entropy_source = TestEntropySource(AtomicCounter::new());
		do_test_interactive_tx_constructor_internal(session, &&entropy_source);
	}

	fn do_test_interactive_tx_constructor_with_entropy_source<ES: Deref>(
		session: TestSession, entropy_source: ES,
	) where
		ES::Target: EntropySource,
	{
		do_test_interactive_tx_constructor_internal(session, &entropy_source);
	}

	fn do_test_interactive_tx_constructor_internal<ES: Deref>(
		session: TestSession, entropy_source: &ES,
	) where
		ES::Target: EntropySource,
	{
		let channel_id = ChannelId(entropy_source.get_secure_random_bytes());
		let tx_locktime = AbsoluteLockTime::from_height(1337).unwrap();

		let (mut constructor_a, first_message_a) = InteractiveTxConstructor::new(
			entropy_source,
			channel_id,
			FEERATE_FLOOR_SATS_PER_KW * 10,
			true,
			tx_locktime,
			session.inputs_a,
			session.outputs_a,
			session.shared_funding_input.clone(),
			session.new_funding_output.clone(),
			session.b_funding_satoshis,
			session.a_funding_satoshis,
		)
		.unwrap();
		let (mut constructor_b, first_message_b) = InteractiveTxConstructor::new(
			entropy_source,
			channel_id,
			FEERATE_FLOOR_SATS_PER_KW * 10,
			false,
			tx_locktime,
			session.inputs_b,
			session.outputs_b,
			session.shared_funding_input,
			session.new_funding_output,
			session.b_funding_satoshis,
			session.a_funding_satoshis,
		)
		.unwrap();

		let handle_message_send =
			|msg: InteractiveTxMessageSend, for_constructor: &mut InteractiveTxConstructor| {
				match msg {
					InteractiveTxMessageSend::TxAddInput(msg) => for_constructor
						.handle_tx_add_input(&msg)
						.map(|msg_send| (Some(msg_send), None)),
					InteractiveTxMessageSend::TxAddOutput(msg) => for_constructor
						.handle_tx_add_output(&msg)
						.map(|msg_send| (Some(msg_send), None)),
					InteractiveTxMessageSend::TxComplete(msg) => {
						for_constructor.handle_tx_complete(&msg).map(|value| match value {
							HandleTxCompleteValue::SendTxMessage(msg_send) => {
								(Some(msg_send), None)
							},
							HandleTxCompleteValue::SendTxComplete(msg_send, tx) => {
								(Some(msg_send), Some(tx))
							},
							HandleTxCompleteValue::NegotiationComplete(tx) => (None, Some(tx)),
						})
					},
				}
			};

		assert!(first_message_b.is_none());
		let mut message_send_a = first_message_a;
		let mut message_send_b = None;
		let mut final_tx_a = None;
		let mut final_tx_b = None;
		while final_tx_a.is_none() || final_tx_b.is_none() {
			if let Some(message_send_a) = message_send_a.take() {
				match handle_message_send(message_send_a, &mut constructor_b) {
					Ok((msg_send, final_tx)) => {
						message_send_b = msg_send;
						final_tx_b = final_tx;
					},
					Err(abort_reason) => {
						let error_culprit = match abort_reason {
							AbortReason::ExceededNumberOfInputsOrOutputs => {
								ErrorCulprit::Indeterminate
							},
							_ => ErrorCulprit::NodeA,
						};
						assert_eq!(
							Some((abort_reason, error_culprit)),
							session.expect_error,
							"Test: {}",
							session.description
						);
						assert!(message_send_b.is_none(), "Test: {}", session.description);
						return;
					},
				}
			}
			if let Some(message_send_b) = message_send_b.take() {
				match handle_message_send(message_send_b, &mut constructor_a) {
					Ok((msg_send, final_tx)) => {
						message_send_a = msg_send;
						final_tx_a = final_tx;
					},
					Err(abort_reason) => {
						let error_culprit = match abort_reason {
							AbortReason::ExceededNumberOfInputsOrOutputs => {
								ErrorCulprit::Indeterminate
							},
							_ => ErrorCulprit::NodeB,
						};
						assert_eq!(
							Some((abort_reason, error_culprit)),
							session.expect_error,
							"Test: {}",
							session.description
						);
						assert!(message_send_a.is_none(), "Test: {}", session.description);
						return;
					},
				}
			}
		}
		assert!(message_send_a.is_none());
		assert!(message_send_b.is_none());
		assert_eq!(final_tx_a, final_tx_b);
		assert!(session.expect_error.is_none());
	}

	fn generate_tx(values: &[u64]) -> Transaction {
		generate_tx_with_locktime(values, 1337)
	}

	fn generate_tx_with_locktime(values: &[u64], locktime: u32) -> Transaction {
		Transaction {
			version: 2,
			lock_time: AbsoluteLockTime::from_height(locktime).unwrap(),
			input: vec![TxIn { ..Default::default() }],
			output: values
				.iter()
				.map(|value| TxOut {
					value: *value,
					script_pubkey: Builder::new()
						.push_opcode(opcodes::OP_TRUE)
						.into_script()
						.to_v0_p2wsh(),
				})
				.collect(),
		}
	}

	fn generate_shared_input(value: u64, to_remote: u64, to_local: u64) -> SharedFundingInput {
		let tx = generate_tx_with_locktime(&[value], 1111);
		SharedFundingInput {
			txin: TxIn {
				previous_output: OutPoint { txid: tx.txid(), vout: 0u32 },
				script_sig: Default::default(),
				sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
				witness: Default::default(),
			},
			txout: TxOut { value, script_pubkey: Default::default() },
			to_remote_value_satoshis: to_remote,
			to_local_value_satoshis: to_local,
		}
	}

	fn generate_inputs(values: &[u64]) -> Vec<(TxIn, TransactionU16LenLimited)> {
		let tx = generate_tx(values);
		let txid = tx.txid();
		tx.output
			.iter()
			.enumerate()
			.map(|(idx, _)| {
				let input = TxIn {
					previous_output: OutPoint { txid, vout: idx as u32 },
					script_sig: Default::default(),
					sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
					witness: Default::default(),
				};
				(input, TransactionU16LenLimited::new(tx.clone()).unwrap())
			})
			.collect()
	}

	fn generate_outputs(values: &[u64]) -> Vec<TxOut> {
		values
			.iter()
			.map(|value| TxOut {
				value: *value,
				script_pubkey: Builder::new()
					.push_opcode(opcodes::OP_TRUE)
					.into_script()
					.to_v0_p2wsh(),
			})
			.collect()
	}

	fn generate_fixed_number_of_inputs(count: u16) -> Vec<(TxIn, TransactionU16LenLimited)> {
		// Generate transactions with a total `count` number of outputs such that no transaction has a
		// serialized length greater than u16::MAX.
		let max_outputs_per_prevtx = 1_500;
		let mut remaining = count;
		let mut inputs: Vec<(TxIn, TransactionU16LenLimited)> = Vec::with_capacity(count as usize);

		while remaining > 0 {
			let tx_output_count = remaining.min(max_outputs_per_prevtx);
			remaining -= tx_output_count;

			// Use unique locktime for each tx so outpoints are different across transactions
			let tx = generate_tx_with_locktime(
				&vec![1_000_000; tx_output_count as usize],
				(1337 + remaining).into(),
			);
			let txid = tx.txid();

			let mut temp: Vec<(TxIn, TransactionU16LenLimited)> = tx
				.output
				.iter()
				.enumerate()
				.map(|(idx, _)| {
					let input = TxIn {
						previous_output: OutPoint { txid, vout: idx as u32 },
						script_sig: Default::default(),
						sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
						witness: Default::default(),
					};
					(input, TransactionU16LenLimited::new(tx.clone()).unwrap())
				})
				.collect();

			inputs.append(&mut temp);
		}

		inputs
	}

	fn generate_fixed_number_of_outputs(count: u16) -> Vec<TxOut> {
		// Set a constant value for each TxOut
		generate_outputs(&vec![1_000_000; count as usize])
	}

	fn generate_non_witness_output(value: u64) -> TxOut {
		TxOut {
			value,
			script_pubkey: Builder::new().push_opcode(opcodes::OP_TRUE).into_script().to_p2sh(),
		}
	}

	#[test]
	fn test_interactive_tx_constructor() {
		// No contributions.
		do_test_interactive_tx_constructor(TestSession {
			description: "No contributions".into(),
			inputs_a: vec![],
			outputs_a: vec![],
			inputs_b: vec![],
			outputs_b: vec![],
			expect_error: Some((AbortReason::InsufficientFees, ErrorCulprit::NodeA)),
			shared_funding_input: None,
			new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
			b_funding_satoshis: 0,
			a_funding_satoshis: 0,
		});
		// Single contribution, no initiator inputs.
		do_test_interactive_tx_constructor(TestSession {
			description: "Single contribution, no initiator inputs".into(),
			inputs_a: vec![],
			outputs_a: generate_outputs(&[1_000_000]),
			inputs_b: vec![],
			outputs_b: vec![],
			expect_error: Some((AbortReason::OutputsValueExceedsInputsValue, ErrorCulprit::NodeA)),
			shared_funding_input: None,
			new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
			b_funding_satoshis: 0,
			a_funding_satoshis: 0,
		});
		// Single contribution, no initiator outputs.
		do_test_interactive_tx_constructor(TestSession {
			description: "Single contribution, no initiator outputs".into(),
			inputs_a: generate_inputs(&[1_000_000]),
			outputs_a: vec![],
			inputs_b: vec![],
			outputs_b: vec![],
			expect_error: None,
			shared_funding_input: None,
			new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
			b_funding_satoshis: 0,
			a_funding_satoshis: 0,
		});
		// Single contribution, insufficient fees.
		do_test_interactive_tx_constructor(TestSession {
			description: "Single contribution, insufficient fees".into(),
			inputs_a: generate_inputs(&[1_000_000]),
			outputs_a: generate_outputs(&[1_000_000]),
			inputs_b: vec![],
			outputs_b: vec![],
			expect_error: Some((AbortReason::InsufficientFees, ErrorCulprit::NodeA)),
			shared_funding_input: None,
			new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
			b_funding_satoshis: 0,
			a_funding_satoshis: 0,
		});
		// Initiator contributes sufficient fees, but non-initiator does not.
		do_test_interactive_tx_constructor(TestSession {
			description: "Initiator contributes sufficient fees, but non-initiator does not".into(),
			inputs_a: generate_inputs(&[1_000_000]),
			outputs_a: vec![],
			inputs_b: generate_inputs(&[100_000]),
			outputs_b: generate_outputs(&[100_000]),
			expect_error: Some((AbortReason::InsufficientFees, ErrorCulprit::NodeB)),
			shared_funding_input: None,
			new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
			b_funding_satoshis: 0,
			a_funding_satoshis: 0,
		});
		// Multi-input-output contributions from both sides.
		do_test_interactive_tx_constructor(TestSession {
			description: "Multi-input-output contributions from both sides".into(),
			inputs_a: generate_inputs(&[1_000_000, 1_000_000]),
			outputs_a: generate_outputs(&[1_000_000, 200_000]),
			inputs_b: generate_inputs(&[1_000_000, 500_000]),
			outputs_b: generate_outputs(&[1_000_000, 400_000]),
			expect_error: None,
			shared_funding_input: None,
			new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
			b_funding_satoshis: 0,
			a_funding_satoshis: 0,
		});

		// Prevout from initiator is not a witness program
		let non_segwit_output_tx = {
			let mut tx = generate_tx(&[1_000_000]);
			tx.output.push(TxOut {
				script_pubkey: Builder::new()
					.push_opcode(opcodes::all::OP_RETURN)
					.into_script()
					.to_p2sh(),
				..Default::default()
			});

			TransactionU16LenLimited::new(tx).unwrap()
		};
		let non_segwit_input = TxIn {
			previous_output: OutPoint {
				txid: non_segwit_output_tx.as_transaction().txid(),
				vout: 1,
			},
			sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
			..Default::default()
		};
		do_test_interactive_tx_constructor(TestSession {
			description: "Prevout from initiator is not a witness program".into(),
			inputs_a: vec![(non_segwit_input, non_segwit_output_tx)],
			outputs_a: vec![],
			inputs_b: vec![],
			outputs_b: vec![],
			expect_error: Some((AbortReason::PrevTxOutInvalid, ErrorCulprit::NodeA)),
			shared_funding_input: None,
			new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
			b_funding_satoshis: 0,
			a_funding_satoshis: 0,
		});

		// Invalid input sequence from initiator.
		let tx = TransactionU16LenLimited::new(generate_tx(&[1_000_000])).unwrap();
		let invalid_sequence_input = TxIn {
			previous_output: OutPoint { txid: tx.as_transaction().txid(), vout: 0 },
			..Default::default()
		};
		do_test_interactive_tx_constructor(TestSession {
			description: "Invalid input sequence from initiator".into(),
			inputs_a: vec![(invalid_sequence_input, tx.clone())],
			outputs_a: generate_outputs(&[1_000_000]),
			inputs_b: vec![],
			outputs_b: vec![],
			expect_error: Some((AbortReason::IncorrectInputSequenceValue, ErrorCulprit::NodeA)),
			shared_funding_input: None,
			new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
			b_funding_satoshis: 0,
			a_funding_satoshis: 0,
		});
		// Duplicate prevout from initiator.
		let duplicate_input = TxIn {
			previous_output: OutPoint { txid: tx.as_transaction().txid(), vout: 0 },
			sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
			..Default::default()
		};
		do_test_interactive_tx_constructor(TestSession {
			description: "Duplicate prevout from initiator".into(),
			inputs_a: vec![(duplicate_input.clone(), tx.clone()), (duplicate_input, tx.clone())],
			outputs_a: generate_outputs(&[1_000_000]),
			inputs_b: vec![],
			outputs_b: vec![],
			expect_error: Some((AbortReason::PrevTxOutInvalid, ErrorCulprit::NodeB)),
			shared_funding_input: None,
			new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
			b_funding_satoshis: 0,
			a_funding_satoshis: 0,
		});
		// Non-initiator uses same prevout as initiator.
		let duplicate_input = TxIn {
			previous_output: OutPoint { txid: tx.as_transaction().txid(), vout: 0 },
			sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
			..Default::default()
		};
		do_test_interactive_tx_constructor(TestSession {
			description: "Non-initiator uses same prevout as initiator".into(),
			inputs_a: vec![(duplicate_input.clone(), tx.clone())],
			outputs_a: generate_outputs(&[1_000_000]),
			inputs_b: vec![(duplicate_input.clone(), tx.clone())],
			outputs_b: vec![],
			expect_error: Some((AbortReason::PrevTxOutInvalid, ErrorCulprit::NodeA)),
			shared_funding_input: None,
			new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
			b_funding_satoshis: 95_000,
			a_funding_satoshis: 0,
		});
		// Initiator sends too many TxAddInputs
		do_test_interactive_tx_constructor(TestSession {
			description: "Initiator sends too many TxAddInputs".into(),
			inputs_a: generate_fixed_number_of_inputs(MAX_RECEIVED_TX_ADD_INPUT_COUNT + 1),
			outputs_a: vec![],
			inputs_b: vec![],
			outputs_b: vec![],
			expect_error: Some((AbortReason::ReceivedTooManyTxAddInputs, ErrorCulprit::NodeA)),
			shared_funding_input: None,
			new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
			b_funding_satoshis: 0,
			a_funding_satoshis: 0,
		});
		// Attempt to queue up two inputs with duplicate serial ids. We use a deliberately bad
		// entropy source, `DuplicateEntropySource` to simulate this.
		do_test_interactive_tx_constructor_with_entropy_source(
			TestSession {
				description: "Attempt to queue up two inputs with duplicate serial ids".into(),
				inputs_a: generate_fixed_number_of_inputs(2),
				outputs_a: vec![],
				inputs_b: vec![],
				outputs_b: vec![],
				expect_error: Some((AbortReason::DuplicateSerialId, ErrorCulprit::NodeA)),
				shared_funding_input: None,
				new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
				b_funding_satoshis: 0,
				a_funding_satoshis: 0,
			},
			&DuplicateEntropySource,
		);
		// Initiator sends too many TxAddOutputs.
		do_test_interactive_tx_constructor(TestSession {
			description: "Initiator sends too many TxAddOutputs".into(),
			inputs_a: vec![],
			outputs_a: generate_fixed_number_of_outputs(MAX_RECEIVED_TX_ADD_OUTPUT_COUNT + 1),
			inputs_b: vec![],
			outputs_b: vec![],
			expect_error: Some((AbortReason::ReceivedTooManyTxAddOutputs, ErrorCulprit::NodeA)),
			shared_funding_input: None,
			new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
			b_funding_satoshis: 0,
			a_funding_satoshis: 0,
		});
		// Initiator sends an output below dust value.
		do_test_interactive_tx_constructor(TestSession {
			description: "Initiator sends an output below dust value".into(),
			inputs_a: vec![],
			outputs_a: generate_outputs(&[1]),
			inputs_b: vec![],
			outputs_b: vec![],
			expect_error: Some((AbortReason::BelowDustLimit, ErrorCulprit::NodeA)),
			shared_funding_input: None,
			new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
			b_funding_satoshis: 0,
			a_funding_satoshis: 0,
		});
		// Initiator sends an output above maximum sats allowed.
		do_test_interactive_tx_constructor(TestSession {
			description: "Initiator sends an output above maximum sats allowed".into(),
			inputs_a: vec![],
			outputs_a: generate_outputs(&[TOTAL_BITCOIN_SUPPLY_SATOSHIS + 1]),
			inputs_b: vec![],
			outputs_b: vec![],
			expect_error: Some((AbortReason::ExceededMaximumSatsAllowed, ErrorCulprit::NodeA)),
			shared_funding_input: None,
			new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
			b_funding_satoshis: 0,
			a_funding_satoshis: 0,
		});
		// Initiator sends an output without a witness program.
		do_test_interactive_tx_constructor(TestSession {
			description: "Initiator sends an output without a witness program".into(),
			inputs_a: vec![],
			outputs_a: vec![generate_non_witness_output(1_000_000)],
			inputs_b: vec![],
			outputs_b: vec![],
			expect_error: Some((AbortReason::InvalidOutputScript, ErrorCulprit::NodeA)),
			shared_funding_input: None,
			new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
			b_funding_satoshis: 0,
			a_funding_satoshis: 0,
		});
		// Attempt to queue up two outputs with duplicate serial ids. We use a deliberately bad
		// entropy source, `DuplicateEntropySource` to simulate this.
		do_test_interactive_tx_constructor_with_entropy_source(
			TestSession {
				description: "Attempt to queue up two outputs with duplicate serial ids".into(),
				inputs_a: vec![],
				outputs_a: generate_fixed_number_of_outputs(2),
				inputs_b: vec![],
				outputs_b: vec![],
				expect_error: Some((AbortReason::DuplicateSerialId, ErrorCulprit::NodeA)),
				shared_funding_input: None,
				new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
				b_funding_satoshis: 0,
				a_funding_satoshis: 0,
			},
			&DuplicateEntropySource,
		);

		// Peer contributed more output value than inputs
		do_test_interactive_tx_constructor(TestSession {
			description: "Peer contributed more output value than inputs".into(),
			inputs_a: generate_inputs(&[100_000]),
			outputs_a: generate_outputs(&[1_000_000]),
			inputs_b: vec![],
			outputs_b: vec![],
			expect_error: Some((AbortReason::OutputsValueExceedsInputsValue, ErrorCulprit::NodeA)),
			shared_funding_input: None,
			new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
			b_funding_satoshis: 0,
			a_funding_satoshis: 0,
		});

		// Peer contributed more than allowed number of inputs.
		do_test_interactive_tx_constructor(TestSession {
			description: "Peer contributed more than allowed number of inputs".into(),
			inputs_a: generate_fixed_number_of_inputs(MAX_INPUTS_OUTPUTS_COUNT as u16 + 1),
			outputs_a: vec![],
			inputs_b: vec![],
			outputs_b: vec![],
			expect_error: Some((
				AbortReason::ExceededNumberOfInputsOrOutputs,
				ErrorCulprit::Indeterminate,
			)),
			shared_funding_input: None,
			new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
			b_funding_satoshis: 0,
			a_funding_satoshis: 0,
		});
		// Peer contributed more than allowed number of outputs.
		do_test_interactive_tx_constructor(TestSession {
			description: "Peer contributed more than allowed number of outputs".into(),
			inputs_a: generate_inputs(&[TOTAL_BITCOIN_SUPPLY_SATOSHIS]),
			outputs_a: generate_fixed_number_of_outputs(MAX_INPUTS_OUTPUTS_COUNT as u16 + 1),
			inputs_b: vec![],
			outputs_b: vec![],
			expect_error: Some((
				AbortReason::ExceededNumberOfInputsOrOutputs,
				ErrorCulprit::Indeterminate,
			)),
			shared_funding_input: None,
			new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
			b_funding_satoshis: 0,
			a_funding_satoshis: 0,
		});

		// During a splice-out, with peer providing more output value than input value
		// but still pays enough fees due to their to_remote_value_satoshis portion in
		// the shared input.
		do_test_interactive_tx_constructor(TestSession {
			description: "Splice out with sufficient initiator balance".into(),
			inputs_a: generate_inputs(&[100_000]),
			outputs_a: generate_outputs(&[120_000]),
			inputs_b: vec![],
			outputs_b: vec![],
			expect_error: None,
			shared_funding_input: Some(generate_shared_input(100_000, 50_000, 50_000)),
			new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
			b_funding_satoshis: 0,
			a_funding_satoshis: -20_000,
		});

		// During a splice-out, with peer providing more output value than input value
		// and the to_remote_value_satoshis portion in
		// the shared input cannot cover fees
		do_test_interactive_tx_constructor(TestSession {
			description: "Splice out with insufficient initiator balance".into(),
			inputs_a: generate_inputs(&[100_000]),
			outputs_a: generate_outputs(&[120_000]),
			inputs_b: vec![],
			outputs_b: vec![],
			expect_error: Some((AbortReason::OutputsValueExceedsInputsValue, ErrorCulprit::NodeA)),
			shared_funding_input: Some(generate_shared_input(100_000, 15_000, 85_000)),
			new_funding_output: TxOut { value: 0, script_pubkey: Default::default() },
			b_funding_satoshis: 0,
			a_funding_satoshis: -10_000,
		});
	}

	#[test]
	fn test_generate_local_serial_id() {
		let entropy_source = TestEntropySource(AtomicCounter::new());

		// Initiators should have even serial id, non-initiators should have odd serial id.
		assert_eq!(generate_holder_serial_id(&&entropy_source, true) % 2, 0);
		assert_eq!(generate_holder_serial_id(&&entropy_source, false) % 2, 1)
	}
}
