// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

#![crate_name = "lightning_types"]

//! Various types which are used in the lightning network.
//!
//! See the `lightning` crate for usage of these.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]
#![forbid(unsafe_code)]
#![deny(rustdoc::broken_intra_doc_links)]
#![deny(rustdoc::private_intra_doc_links)]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

extern crate alloc;
extern crate core;

use alloc::vec::Vec;

use core::borrow::Borrow;

use bitcoin::hashes::{
	Hash as _,
	sha256::Hash as Sha256,
};

// TODO: Once we switch to rust-bitcoin 0.32, import this as bitcoin::hex
use hex_conservative::display::impl_fmt_traits;

/// The payment hash is the hash of the [`PaymentPreimage`] which is the value used to lock funds
/// in HTLCs while they transit the lightning network.
///
/// This is not exported to bindings users as we just use [u8; 32] directly
#[derive(Hash, Copy, Clone, PartialEq, Eq, Ord, PartialOrd)]
pub struct PaymentHash(pub [u8; 32]);

impl Borrow<[u8]> for PaymentHash {
	fn borrow(&self) -> &[u8] {
		&self.0[..]
	}
}

impl_fmt_traits! {
	impl fmt_traits for PaymentHash {
		const LENGTH: usize = 32;
	}
}

/// The payment preimage is the "secret key" which is used to claim the funds of an HTLC on-chain
/// or in a lightning channel.
///
/// This is not exported to bindings users as we just use [u8; 32] directly
#[derive(Hash, Copy, Clone, PartialEq, Eq, Ord, PartialOrd)]
pub struct PaymentPreimage(pub [u8; 32]);

impl Borrow<[u8]> for PaymentPreimage {
	fn borrow(&self) -> &[u8] {
		&self.0[..]
	}
}

impl_fmt_traits! {
	impl fmt_traits for PaymentPreimage {
		const LENGTH: usize = 32;
	}
}

/// Converts a `PaymentPreimage` into a `PaymentHash` by hashing the preimage with SHA256.
impl From<PaymentPreimage> for PaymentHash {
	fn from(value: PaymentPreimage) -> Self {
		PaymentHash(Sha256::hash(&value.0).to_byte_array())
	}
}

/// The payment secret is used to authenticate the sender of an HTLC to the recipient and tie
/// multi-part HTLCs together into a single payment.
///
/// This is not exported to bindings users as we just use [u8; 32] directly
#[derive(Hash, Copy, Clone, PartialEq, Eq, Ord, PartialOrd)]
pub struct PaymentSecret(pub [u8; 32]);

impl Borrow<[u8]> for PaymentSecret {
	fn borrow(&self) -> &[u8] {
		&self.0[..]
	}
}

impl_fmt_traits! {
	impl fmt_traits for PaymentSecret {
		const LENGTH: usize = 32;
	}
}

use bech32::{Base32Len, FromBase32, ToBase32, WriteBase32, u5};

impl FromBase32 for PaymentSecret {
	type Err = bech32::Error;

	fn from_base32(field_data: &[u5]) -> Result<PaymentSecret, bech32::Error> {
		if field_data.len() != 52 {
			return Err(bech32::Error::InvalidLength)
		} else {
			let data_bytes = Vec::<u8>::from_base32(field_data)?;
			let mut payment_secret = [0; 32];
			payment_secret.copy_from_slice(&data_bytes);
			Ok(PaymentSecret(payment_secret))
		}
	}
}

impl ToBase32 for PaymentSecret {
	fn write_base32<W: WriteBase32>(&self, writer: &mut W) -> Result<(), <W as WriteBase32>::Err> {
		(&self.0[..]).write_base32(writer)
	}
}

impl Base32Len for PaymentSecret {
	fn base32_len(&self) -> usize {
		52
	}
}
