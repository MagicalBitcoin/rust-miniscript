// Miniscript
// Written in 2018 by
//     Andrew Poelstra <apoelstra@wpsoftware.net>
//
// To the extent possible under law, the author(s) have dedicated all
// copyright and related and neighboring rights to this software to
// the public domain worldwide. This software is distributed without
// any warranty.
//
// You should have received a copy of the CC0 Public Domain Dedication
// along with this software.
// If not, see <http://creativecommons.org/publicdomain/zero/1.0/>.
//

//! # Output Descriptors
//!
//! Tools for representing Bitcoin output's scriptPubKeys as abstract spending
//! policies known as "output descriptors". These include a Miniscript which
//! describes the actual signing policy, as well as the blockchain format (P2SH,
//! Segwit v0, etc.)
//!
//! The format represents EC public keys abstractly to allow wallets to replace
//! these with BIP32 paths, pay-to-contract instructions, etc.
//!

use std::collections::HashMap;
use std::fmt;
use std::str::{self, FromStr};

use bitcoin::blockdata::{opcodes, script};
use bitcoin::hashes::hash160;
use bitcoin::hashes::hex::FromHex;
use bitcoin::secp256k1;
use bitcoin::util::bip32;
use bitcoin::{self, Script};

#[cfg(feature = "serde")]
use serde::{de, ser};

use expression;
use miniscript;
use miniscript::context::ScriptContextError;
use miniscript::{Legacy, Miniscript, Segwitv0};
use Error;
use MiniscriptKey;
use Satisfier;
use ToPublicKey;

mod create_descriptor;
mod satisfied_constraints;

pub use self::create_descriptor::from_txin_with_witness_stack;
pub use self::satisfied_constraints::Error as InterpreterError;
pub use self::satisfied_constraints::SatisfiedConstraint;
pub use self::satisfied_constraints::SatisfiedConstraints;
pub use self::satisfied_constraints::Stack;

pub type KeyMap = HashMap<DescriptorPublicKey, DescriptorSecretKey>;

/// Script descriptor
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Descriptor<Pk: MiniscriptKey> {
    /// A raw scriptpubkey (including pay-to-pubkey) under Legacy context
    Bare(Miniscript<Pk, Legacy>),
    /// Pay-to-Pubkey
    Pk(Pk),
    /// Pay-to-PubKey-Hash
    Pkh(Pk),
    /// Pay-to-Witness-PubKey-Hash
    Wpkh(Pk),
    /// Pay-to-Witness-PubKey-Hash inside P2SH
    ShWpkh(Pk),
    /// Pay-to-ScriptHash with Legacy context
    Sh(Miniscript<Pk, Legacy>),
    /// Pay-to-Witness-ScriptHash with Segwitv0 context
    Wsh(Miniscript<Pk, Segwitv0>),
    /// P2SH-P2WSH with Segwitv0 context
    ShWsh(Miniscript<Pk, Segwitv0>),
}

#[derive(Debug, Eq, PartialEq, Clone, Ord, PartialOrd, Hash)]
pub enum DescriptorPublicKey {
    SinglePub(DescriptorSinglePub),
    XPub(DescriptorXKey<bip32::ExtendedPubKey>),
}

#[derive(Debug, Eq, PartialEq, Clone, Ord, PartialOrd, Hash)]
pub struct DescriptorSinglePub {
    pub origin: Option<(bip32::Fingerprint, bip32::DerivationPath)>,
    pub key: bitcoin::PublicKey,
}

#[derive(Debug)]
pub struct DescriptorSinglePriv {
    pub origin: Option<(bip32::Fingerprint, bip32::DerivationPath)>,
    pub key: bitcoin::PrivateKey,
}

#[derive(Debug)]
pub enum DescriptorSecretKey {
    SinglePriv(DescriptorSinglePriv),
    XPrv(DescriptorXKey<bip32::ExtendedPrivKey>),
}

impl fmt::Display for DescriptorSecretKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &DescriptorSecretKey::SinglePriv(ref sk) => {
                maybe_fmt_master_id(f, &sk.origin)?;
                sk.key.fmt(f)?;
                Ok(())
            }
            &DescriptorSecretKey::XPrv(ref xprv) => {
                maybe_fmt_master_id(f, &xprv.origin)?;
                xprv.xkey.fmt(f)?;
                fmt_derivation_path(f, &xprv.derivation_path)?;
                if xprv.is_wildcard {
                    write!(f, "/*")?;
                }
                Ok(())
            }
        }
    }
}

pub trait InnerXKey: fmt::Display + str::FromStr {
    fn xkey_fingerprint(&self) -> bip32::Fingerprint;
    fn can_derive_hardened() -> bool;
}

impl InnerXKey for bip32::ExtendedPubKey {
    fn xkey_fingerprint(&self) -> bip32::Fingerprint {
        self.fingerprint()
    }

    fn can_derive_hardened() -> bool {
        false
    }
}

impl InnerXKey for bip32::ExtendedPrivKey {
    fn xkey_fingerprint(&self) -> bip32::Fingerprint {
        self.fingerprint(&secp256k1::Secp256k1::signing_only())
    }

    fn can_derive_hardened() -> bool {
        true
    }
}

/// Instance of an extended key with origin and derivation path
#[derive(Debug, Eq, PartialEq, Clone, Ord, PartialOrd, Hash)]
pub struct DescriptorXKey<K: InnerXKey> {
    pub origin: Option<(bip32::Fingerprint, bip32::DerivationPath)>,
    pub xkey: K,
    pub derivation_path: bip32::DerivationPath,
    pub is_wildcard: bool,
}

impl DescriptorSinglePriv {
    fn as_public(&self) -> Result<DescriptorSinglePub, DescriptorKeyParseError> {
        let secp = secp256k1::Secp256k1::new();

        let pub_key = self.key.public_key(&secp);

        Ok(DescriptorSinglePub {
            origin: self.origin.clone(),
            key: pub_key,
        })
    }
}

impl DescriptorXKey<bip32::ExtendedPrivKey> {
    fn as_public(&self) -> Result<DescriptorXKey<bip32::ExtendedPubKey>, DescriptorKeyParseError> {
        let secp = secp256k1::Secp256k1::new();

        let path_len = (&self.derivation_path).as_ref().len();
        let public_suffix_len = (&self.derivation_path)
            .into_iter()
            .rev()
            .take_while(|c| c.is_normal())
            .count();

        let derivation_path = &self.derivation_path[(path_len - public_suffix_len)..];
        let deriv_on_hardened = &self.derivation_path[..(path_len - public_suffix_len)];

        let derived_xprv = self
            .xkey
            .derive_priv(&secp, &deriv_on_hardened)
            .map_err(|_| DescriptorKeyParseError("Unable to derive the hardened steps"))?;
        let xpub = bip32::ExtendedPubKey::from_private(&secp, &derived_xprv);

        let origin = match &self.origin {
            &Some((fingerprint, ref origin_path)) => Some((
                fingerprint,
                origin_path
                    .into_iter()
                    .chain(deriv_on_hardened.into_iter())
                    .cloned()
                    .collect(),
            )),
            &None if !deriv_on_hardened.as_ref().is_empty() => {
                Some((self.xkey.fingerprint(&secp), deriv_on_hardened.into()))
            }
            _ => self.origin.clone(),
        };

        Ok(DescriptorXKey {
            origin,
            xkey: xpub,
            derivation_path: derivation_path.into(),
            is_wildcard: self.is_wildcard,
        })
    }
}

#[derive(Debug, PartialEq)]
pub struct DescriptorKeyParseError(&'static str);

impl fmt::Display for DescriptorKeyParseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl fmt::Display for DescriptorPublicKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            DescriptorPublicKey::SinglePub(ref pk) => {
                maybe_fmt_master_id(f, &pk.origin)?;
                pk.key.fmt(f)?;
                Ok(())
            }
            DescriptorPublicKey::XPub(ref xpub) => {
                maybe_fmt_master_id(f, &xpub.origin)?;
                xpub.xkey.fmt(f)?;
                fmt_derivation_path(f, &xpub.derivation_path)?;
                if xpub.is_wildcard {
                    write!(f, "/*")?;
                }
                Ok(())
            }
        }
    }
}

impl DescriptorSecretKey {
    pub fn as_public(&self) -> Result<DescriptorPublicKey, DescriptorKeyParseError> {
        Ok(match self {
            &DescriptorSecretKey::SinglePriv(ref sk) => {
                DescriptorPublicKey::SinglePub(sk.as_public()?)
            }
            &DescriptorSecretKey::XPrv(ref xprv) => DescriptorPublicKey::XPub(xprv.as_public()?),
        })
    }
}

/// Writes the fingerprint of the origin, if there is one.
fn maybe_fmt_master_id(
    f: &mut fmt::Formatter,
    origin: &Option<(bip32::Fingerprint, bip32::DerivationPath)>,
) -> fmt::Result {
    if let Some((ref master_id, ref master_deriv)) = *origin {
        fmt::Formatter::write_str(f, "[")?;
        for byte in master_id.into_bytes().iter() {
            write!(f, "{:02x}", byte)?;
        }
        fmt_derivation_path(f, master_deriv)?;
        fmt::Formatter::write_str(f, "]")?;
    }

    Ok(())
}

/// Writes a derivation path to the formatter, no leading 'm'
fn fmt_derivation_path(f: &mut fmt::Formatter, path: &bip32::DerivationPath) -> fmt::Result {
    for child in path {
        write!(f, "/{}", child)?;
    }
    Ok(())
}

impl FromStr for DescriptorPublicKey {
    type Err = DescriptorKeyParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // A "raw" public key without any origin is the least we accept.
        if s.len() < 66 {
            return Err(DescriptorKeyParseError(
                "Key too short (<66 char), doesn't match any format",
            ));
        }

        let (key_part, origin) = DescriptorXKey::<bip32::ExtendedPubKey>::parse_xkey_origin(s)?;

        if key_part.contains("pub") {
            let (xpub, derivation_path, is_wildcard) =
                DescriptorXKey::<bip32::ExtendedPubKey>::parse_xkey_deriv(key_part)?;

            Ok(DescriptorPublicKey::XPub(DescriptorXKey {
                origin,
                xkey: xpub,
                derivation_path,
                is_wildcard,
            }))
        } else {
            let key = bitcoin::PublicKey::from_str(key_part)
                .map_err(|_| DescriptorKeyParseError("Error while parsing simple public key"))?;
            Ok(DescriptorPublicKey::SinglePub(DescriptorSinglePub {
                key,
                origin,
            }))
        }
    }
}

impl DescriptorPublicKey {
    /// Derives the specified child key if self is a wildcard xpub. Otherwise returns self.
    ///
    /// Panics if given a hardened child number
    pub fn derive(self, child_number: bip32::ChildNumber) -> DescriptorPublicKey {
        debug_assert!(child_number.is_normal());

        match self {
            DescriptorPublicKey::SinglePub(_) => self,
            DescriptorPublicKey::XPub(xpub) => {
                if xpub.is_wildcard {
                    DescriptorPublicKey::XPub(DescriptorXKey {
                        origin: xpub.origin,
                        xkey: xpub.xkey,
                        derivation_path: xpub.derivation_path.into_child(child_number),
                        is_wildcard: false,
                    })
                } else {
                    DescriptorPublicKey::XPub(xpub)
                }
            }
        }
    }
}

impl FromStr for DescriptorSecretKey {
    type Err = DescriptorKeyParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (key_part, origin) = DescriptorXKey::<bip32::ExtendedPubKey>::parse_xkey_origin(s)?;

        if key_part.len() <= 52 {
            let sk = bitcoin::PrivateKey::from_str(key_part)
                .map_err(|_| DescriptorKeyParseError("Error while parsing a WIF private key"))?;
            Ok(DescriptorSecretKey::SinglePriv(DescriptorSinglePriv {
                key: sk,
                origin: None,
            }))
        } else {
            let (xprv, derivation_path, is_wildcard) =
                DescriptorXKey::<bip32::ExtendedPrivKey>::parse_xkey_deriv(key_part)?;
            Ok(DescriptorSecretKey::XPrv(DescriptorXKey {
                origin,
                xkey: xprv,
                derivation_path,
                is_wildcard,
            }))
        }
    }
}

impl<K: InnerXKey> DescriptorXKey<K> {
    fn parse_xkey_origin(
        s: &str,
    ) -> Result<(&str, Option<(bip32::Fingerprint, bip32::DerivationPath)>), DescriptorKeyParseError>
    {
        for ch in s.as_bytes() {
            if *ch < 20 || *ch > 127 {
                return Err(DescriptorKeyParseError(
                    "Encountered an unprintable character",
                ));
            }
        }

        if s.is_empty() {
            return Err(DescriptorKeyParseError("Empty key"));
        }
        let mut parts = s[1..].split(']');

        if let Some('[') = s.chars().next() {
            let mut raw_origin = parts
                .next()
                .ok_or(DescriptorKeyParseError("Unclosed '['"))?
                .split('/');

            let origin_id_hex = raw_origin.next().ok_or(DescriptorKeyParseError(
                "No master fingerprint found after '['",
            ))?;

            if origin_id_hex.len() != 8 {
                return Err(DescriptorKeyParseError(
                    "Master fingerprint should be 8 characters long",
                ));
            }
            let parent_fingerprint = bip32::Fingerprint::from_hex(origin_id_hex).map_err(|_| {
                DescriptorKeyParseError("Malformed master fingerprint, expected 8 hex chars")
            })?;

            let origin_path = raw_origin
                .map(|p| bip32::ChildNumber::from_str(p))
                .collect::<Result<bip32::DerivationPath, bip32::Error>>()
                .map_err(|_| {
                    DescriptorKeyParseError("Error while parsing master derivation path")
                })?;

            let key = parts
                .next()
                .ok_or(DescriptorKeyParseError("No key after origin."))?;

            Ok((key, Some((parent_fingerprint, origin_path))))
        } else {
            Ok((s, None))
        }
    }

    /// Parse an extended key concatenated to a derivation path.
    fn parse_xkey_deriv(
        key_deriv: &str,
    ) -> Result<(K, bip32::DerivationPath, bool), DescriptorKeyParseError> {
        let mut key_deriv = key_deriv.split('/');
        let xkey_str = key_deriv.next().ok_or(DescriptorKeyParseError(
            "No key found after origin description",
        ))?;
        let xkey = K::from_str(xkey_str)
            .map_err(|_| DescriptorKeyParseError("Error while parsing xkey."))?;

        let mut is_wildcard = false;
        let derivation_path = key_deriv
            .filter_map(|p| {
                if !is_wildcard && p == "*" {
                    is_wildcard = true;
                    None
                } else if !is_wildcard && p == "*'" {
                    Some(Err(DescriptorKeyParseError(
                        "Hardened derivation is currently not supported.",
                    )))
                } else if is_wildcard {
                    Some(Err(DescriptorKeyParseError(
                        "'*' may only appear as last element in a derivation path.",
                    )))
                } else {
                    Some(bip32::ChildNumber::from_str(p).map_err(|_| {
                        DescriptorKeyParseError("Error while parsing key derivation path")
                    }))
                }
            })
            .collect::<Result<bip32::DerivationPath, _>>()?;

        if !K::can_derive_hardened() && !(&derivation_path).into_iter().all(|c| c.is_normal()) {
            Err(DescriptorKeyParseError(
                "Hardened derivation is currently not supported.",
            ))
        } else {
            Ok((xkey, derivation_path, is_wildcard))
        }
    }

    pub fn matches(
        &self,
        fingerprint: bip32::Fingerprint,
        path: &bip32::DerivationPath,
    ) -> Option<bip32::DerivationPath> {
        let (compare_fingerprint, compare_path) = match &self.origin {
            &Some((fingerprint, ref path)) => (
                fingerprint.clone(),
                path.into_iter()
                    .chain(self.derivation_path.into_iter())
                    .cloned()
                    .collect(),
            ),
            &None => (self.xkey.xkey_fingerprint(), self.derivation_path.clone()),
        };

        let path_excluding_wildcard = if self.is_wildcard && path.as_ref().len() > 0 {
            path.into_iter()
                .take(path.as_ref().len() - 1)
                .cloned()
                .collect()
        } else {
            path.clone()
        };

        if compare_fingerprint == fingerprint && compare_path == path_excluding_wildcard {
            Some(path_excluding_wildcard)
        } else {
            None
        }
    }
}

impl MiniscriptKey for DescriptorPublicKey {
    // This allows us to be able to derive public keys even for PkH s
    type Hash = Self;

    fn to_pubkeyhash(&self) -> Self {
        self.clone()
    }
}

impl ToPublicKey for DescriptorPublicKey {
    fn to_public_key(&self) -> bitcoin::PublicKey {
        match *self {
            DescriptorPublicKey::SinglePub(ref spub) => spub.key.to_public_key(),
            DescriptorPublicKey::XPub(ref xpub) => {
                let ctx = secp256k1::Secp256k1::verification_only();
                xpub.xkey
                    .derive_pub(&ctx, &xpub.derivation_path)
                    .expect("Shouldn't fail, only normal derivations")
                    .public_key
            }
        }
    }

    fn hash_to_hash160(hash: &Self::Hash) -> hash160::Hash {
        hash.to_public_key().to_pubkeyhash()
    }
}

impl<Pk: MiniscriptKey> Descriptor<Pk> {
    /// Convert a descriptor using abstract keys to one using specific keys
    /// This will panic if translatefpk returns an uncompressed key when
    /// converting to a Segwit descriptor. To prevent this panic, ensure
    /// translatefpk returns an error in this case instead.
    pub fn translate_pk<Fpk, Fpkh, Q, E>(
        &self,
        mut translatefpk: Fpk,
        mut translatefpkh: Fpkh,
    ) -> Result<Descriptor<Q>, E>
    where
        Fpk: FnMut(&Pk) -> Result<Q, E>,
        Fpkh: FnMut(&Pk::Hash) -> Result<Q::Hash, E>,
        Q: MiniscriptKey,
    {
        match *self {
            Descriptor::Bare(ref ms) => Ok(Descriptor::Bare(
                ms.translate_pk(&mut translatefpk, &mut translatefpkh)?,
            )),
            Descriptor::Pk(ref pk) => translatefpk(pk).map(Descriptor::Pk),
            Descriptor::Pkh(ref pk) => translatefpk(pk).map(Descriptor::Pkh),
            Descriptor::Wpkh(ref pk) => {
                if pk.is_uncompressed() {
                    panic!("Uncompressed pubkeys are not allowed in segwit v0 scripts");
                }
                translatefpk(pk).map(Descriptor::Wpkh)
            }
            Descriptor::ShWpkh(ref pk) => {
                if pk.is_uncompressed() {
                    panic!("Uncompressed pubkeys are not allowed in segwit v0 scripts");
                }
                translatefpk(pk).map(Descriptor::ShWpkh)
            }
            Descriptor::Sh(ref ms) => Ok(Descriptor::Sh(
                ms.translate_pk(&mut translatefpk, &mut translatefpkh)?,
            )),
            Descriptor::Wsh(ref ms) => Ok(Descriptor::Wsh(
                ms.translate_pk(&mut translatefpk, &mut translatefpkh)?,
            )),
            Descriptor::ShWsh(ref ms) => Ok(Descriptor::ShWsh(
                ms.translate_pk(&mut translatefpk, &mut translatefpkh)?,
            )),
        }
    }
}

impl<Pk: MiniscriptKey + ToPublicKey> Descriptor<Pk> {
    /// Computes the Bitcoin address of the descriptor, if one exists
    pub fn address(&self, network: bitcoin::Network) -> Option<bitcoin::Address> {
        match *self {
            Descriptor::Bare(..) => None,
            Descriptor::Pk(..) => None,
            Descriptor::Pkh(ref pk) => Some(bitcoin::Address::p2pkh(&pk.to_public_key(), network)),
            Descriptor::Wpkh(ref pk) => Some(
                bitcoin::Address::p2wpkh(&pk.to_public_key(), network)
                    .expect("wpkh descriptors have compressed keys"),
            ),
            Descriptor::ShWpkh(ref pk) => Some(
                bitcoin::Address::p2shwpkh(&pk.to_public_key(), network)
                    .expect("shwpkh descriptors have compressed keys"),
            ),
            Descriptor::Sh(ref miniscript) => {
                Some(bitcoin::Address::p2sh(&miniscript.encode(), network))
            }
            Descriptor::Wsh(ref miniscript) => {
                Some(bitcoin::Address::p2wsh(&miniscript.encode(), network))
            }
            Descriptor::ShWsh(ref miniscript) => {
                Some(bitcoin::Address::p2shwsh(&miniscript.encode(), network))
            }
        }
    }

    /// Computes the scriptpubkey of the descriptor
    pub fn script_pubkey(&self) -> Script {
        match *self {
            Descriptor::Bare(ref d) => d.encode(),
            Descriptor::Pk(ref pk) => script::Builder::new()
                .push_key(&pk.to_public_key())
                .push_opcode(opcodes::all::OP_CHECKSIG)
                .into_script(),
            Descriptor::Pkh(ref pk) => {
                let addr = bitcoin::Address::p2pkh(&pk.to_public_key(), bitcoin::Network::Bitcoin);
                addr.script_pubkey()
            }
            Descriptor::Wpkh(ref pk) => {
                let addr = bitcoin::Address::p2wpkh(&pk.to_public_key(), bitcoin::Network::Bitcoin)
                    .expect("wpkh descriptors have compressed keys");
                addr.script_pubkey()
            }
            Descriptor::ShWpkh(ref pk) => {
                let addr =
                    bitcoin::Address::p2shwpkh(&pk.to_public_key(), bitcoin::Network::Bitcoin)
                        .expect("shwpkh descriptors have compressed keys");
                addr.script_pubkey()
            }
            Descriptor::Sh(ref miniscript) => miniscript.encode().to_p2sh(),
            Descriptor::Wsh(ref miniscript) => miniscript.encode().to_v0_p2wsh(),
            Descriptor::ShWsh(ref miniscript) => miniscript.encode().to_v0_p2wsh().to_p2sh(),
        }
    }

    /// Computes the scriptSig that will be in place for an unsigned
    /// input spending an output with this descriptor. For pre-segwit
    /// descriptors, which use the scriptSig for signatures, this
    /// returns the empty script.
    ///
    /// This is used in Segwit transactions to produce an unsigned
    /// transaction whose txid will not change during signing (since
    /// only the witness data will change).
    pub fn unsigned_script_sig(&self) -> Script {
        match *self {
            // non-segwit
            Descriptor::Bare(..)
            | Descriptor::Pk(..)
            | Descriptor::Pkh(..)
            | Descriptor::Sh(..) => Script::new(),
            // pure segwit, empty scriptSig
            Descriptor::Wsh(..) | Descriptor::Wpkh(..) => Script::new(),
            // segwit+p2sh
            Descriptor::ShWpkh(ref pk) => {
                let addr = bitcoin::Address::p2wpkh(&pk.to_public_key(), bitcoin::Network::Bitcoin)
                    .expect("wpkh descriptors have compressed keys");
                let redeem_script = addr.script_pubkey();
                script::Builder::new()
                    .push_slice(&redeem_script[..])
                    .into_script()
            }
            Descriptor::ShWsh(ref d) => {
                let witness_script = d.encode();
                script::Builder::new()
                    .push_slice(&witness_script.to_v0_p2wsh()[..])
                    .into_script()
            }
        }
    }

    /// Computes the "witness script" of the descriptor, i.e. the underlying
    /// script before any hashing is done. For `Bare`, `Pkh` and `Wpkh` this
    /// is the scriptPubkey; for `ShWpkh` and `Sh` this is the redeemScript;
    /// for the others it is the witness script.
    pub fn witness_script(&self) -> Script {
        match *self {
            Descriptor::Bare(..)
            | Descriptor::Pk(..)
            | Descriptor::Pkh(..)
            | Descriptor::Wpkh(..) => self.script_pubkey(),
            Descriptor::ShWpkh(ref pk) => {
                let addr = bitcoin::Address::p2wpkh(&pk.to_public_key(), bitcoin::Network::Bitcoin)
                    .expect("shwpkh descriptors have compressed keys");
                addr.script_pubkey()
            }
            Descriptor::Sh(ref d) => d.encode(),
            Descriptor::Wsh(ref d) | Descriptor::ShWsh(ref d) => d.encode(),
        }
    }

    /// Returns satisfying witness and scriptSig to spend an
    /// output controlled by the given descriptor if it possible to
    /// construct one using the satisfier.
    pub fn get_satisfication<S: Satisfier<Pk>>(
        &self,
        satisfier: S,
    ) -> Result<(Vec<Vec<u8>>, Script), Error> {
        fn witness_to_scriptsig(witness: &[Vec<u8>]) -> Script {
            let mut b = script::Builder::new();
            for wit in witness {
                if let Ok(n) = script::read_scriptint(wit) {
                    b = b.push_int(n);
                } else {
                    b = b.push_slice(wit);
                }
            }
            b.into_script()
        }

        match *self {
            Descriptor::Bare(ref d) => {
                let wit = match d.satisfy(satisfier) {
                    Some(wit) => wit,
                    None => return Err(Error::CouldNotSatisfy),
                };
                let script_sig = witness_to_scriptsig(&wit);
                let witness = vec![];
                Ok((witness, script_sig))
            }
            Descriptor::Pk(ref pk) => {
                if let Some(sig) = satisfier.lookup_sig(pk) {
                    let mut sig_vec = sig.0.serialize_der().to_vec();
                    sig_vec.push(sig.1.as_u32() as u8);
                    let script_sig = script::Builder::new()
                        .push_slice(&sig_vec[..])
                        .into_script();
                    let witness = vec![];
                    Ok((witness, script_sig))
                } else {
                    Err(Error::MissingSig(pk.to_public_key()))
                }
            }
            Descriptor::Pkh(ref pk) => {
                if let Some(sig) = satisfier.lookup_sig(pk) {
                    let mut sig_vec = sig.0.serialize_der().to_vec();
                    sig_vec.push(sig.1.as_u32() as u8);
                    let script_sig = script::Builder::new()
                        .push_slice(&sig_vec[..])
                        .push_key(&pk.to_public_key())
                        .into_script();
                    let witness = vec![];
                    Ok((witness, script_sig))
                } else {
                    Err(Error::MissingSig(pk.to_public_key()))
                }
            }
            Descriptor::Wpkh(ref pk) => {
                if let Some(sig) = satisfier.lookup_sig(pk) {
                    let mut sig_vec = sig.0.serialize_der().to_vec();
                    sig_vec.push(sig.1.as_u32() as u8);
                    let script_sig = Script::new();
                    let witness = vec![sig_vec, pk.to_public_key().to_bytes()];
                    Ok((witness, script_sig))
                } else {
                    Err(Error::MissingSig(pk.to_public_key()))
                }
            }
            Descriptor::ShWpkh(ref pk) => {
                if let Some(sig) = satisfier.lookup_sig(pk) {
                    let mut sig_vec = sig.0.serialize_der().to_vec();
                    sig_vec.push(sig.1.as_u32() as u8);
                    let addr =
                        bitcoin::Address::p2wpkh(&pk.to_public_key(), bitcoin::Network::Bitcoin)
                            .expect("wpkh descriptors have compressed keys");
                    let redeem_script = addr.script_pubkey();

                    let script_sig = script::Builder::new()
                        .push_slice(&redeem_script[..])
                        .into_script();
                    let witness = vec![sig_vec, pk.to_public_key().to_bytes()];
                    Ok((witness, script_sig))
                } else {
                    Err(Error::MissingSig(pk.to_public_key()))
                }
            }
            Descriptor::Sh(ref d) => {
                let mut script_witness = match d.satisfy(satisfier) {
                    Some(wit) => wit,
                    None => return Err(Error::CouldNotSatisfy),
                };
                script_witness.push(d.encode().into_bytes());
                let script_sig = witness_to_scriptsig(&script_witness);
                let witness = vec![];
                Ok((witness, script_sig))
            }
            Descriptor::Wsh(ref d) => {
                let mut witness = match d.satisfy(satisfier) {
                    Some(wit) => wit,
                    None => return Err(Error::CouldNotSatisfy),
                };
                witness.push(d.encode().into_bytes());
                let script_sig = Script::new();
                Ok((witness, script_sig))
            }
            Descriptor::ShWsh(ref d) => {
                let witness_script = d.encode();
                let script_sig = script::Builder::new()
                    .push_slice(&witness_script.to_v0_p2wsh()[..])
                    .into_script();

                let mut witness = match d.satisfy(satisfier) {
                    Some(wit) => wit,
                    None => return Err(Error::CouldNotSatisfy),
                };
                witness.push(witness_script.into_bytes());
                Ok((witness, script_sig))
            }
        }
    }
    /// Attempts to produce a satisfying witness and scriptSig to spend an
    /// output controlled by the given descriptor; add the data to a given
    /// `TxIn` output.
    pub fn satisfy<S: Satisfier<Pk>>(
        &self,
        txin: &mut bitcoin::TxIn,
        satisfier: S,
    ) -> Result<(), Error> {
        let (witness, script_sig) = self.get_satisfication(satisfier)?;
        txin.witness = witness;
        txin.script_sig = script_sig;
        Ok(())
    }

    /// Computes an upper bound on the weight of a satisfying witness to the
    /// transaction. Assumes all signatures are 73 bytes, including push opcode
    /// and sighash suffix. Includes the weight of the VarInts encoding the
    /// scriptSig and witness stack length.
    pub fn max_satisfaction_weight(&self) -> usize {
        fn varint_len(n: usize) -> usize {
            bitcoin::VarInt(n as u64).len()
        }

        match *self {
            Descriptor::Bare(ref ms) => {
                let scriptsig_len = ms.max_satisfaction_size(1);
                4 * (varint_len(scriptsig_len) + scriptsig_len)
            }
            Descriptor::Pk(..) => 4 * (1 + 73),
            Descriptor::Pkh(ref pk) => 4 * (1 + 73 + pk.serialized_len()),
            Descriptor::Wpkh(ref pk) => 4 + 1 + 73 + pk.serialized_len(),
            Descriptor::ShWpkh(ref pk) => 4 * 24 + 1 + 73 + pk.serialized_len(),
            Descriptor::Sh(ref ms) => {
                let ss = ms.script_size();
                let push_size = if ss < 76 {
                    1
                } else if ss < 0x100 {
                    2
                } else if ss < 0x10000 {
                    3
                } else {
                    5
                };

                let scriptsig_len = push_size + ss + ms.max_satisfaction_size(1);
                4 * (varint_len(scriptsig_len) + scriptsig_len)
            }
            Descriptor::Wsh(ref ms) => {
                let script_size = ms.script_size();
                4 +  // scriptSig length byte
                    varint_len(script_size) +
                    script_size +
                    varint_len(ms.max_satisfaction_witness_elements()) +
                    ms.max_satisfaction_size(2)
            }
            Descriptor::ShWsh(ref ms) => {
                let script_size = ms.script_size();
                4 * 36
                    + varint_len(script_size)
                    + script_size
                    + varint_len(ms.max_satisfaction_witness_elements())
                    + ms.max_satisfaction_size(2)
            }
        }
    }

    /// Get the `scriptCode` of a transaction output.
    ///
    /// The `scriptCode` is the Script of the previous transaction output being serialized in the
    /// sighash when evaluating a `CHECKSIG` & co. OP code.
    pub fn script_code(&self) -> Script {
        match *self {
            // For "legacy" non-P2SH outputs, it is defined as the txo's scriptPubKey.
            Descriptor::Bare(..) | Descriptor::Pk(..) | Descriptor::Pkh(..) => self.script_pubkey(),
            // For "legacy" P2SH outputs, it is defined as the txo's redeemScript.
            Descriptor::Sh(ref d) => d.encode(),
            // For SegWit outputs, it is defined by bip-0143 (quoted below) and is different from
            // the previous txo's scriptPubKey.
            // The item 5:
            //     - For P2WPKH witness program, the scriptCode is `0x1976a914{20-byte-pubkey-hash}88ac`.
            Descriptor::Wpkh(ref pk) | Descriptor::ShWpkh(ref pk) => {
                let addr = bitcoin::Address::p2pkh(&pk.to_public_key(), bitcoin::Network::Bitcoin);
                addr.script_pubkey()
            }
            //     - For P2WSH witness program, if the witnessScript does not contain any `OP_CODESEPARATOR`,
            //       the `scriptCode` is the `witnessScript` serialized as scripts inside CTxOut.
            Descriptor::Wsh(ref d) | Descriptor::ShWsh(ref d) => d.encode(),
        }
    }
}

impl Descriptor<DescriptorPublicKey> {
    /// Derives all wildcard keys in the descriptor using the supplied `child_number`
    pub fn derive(&self, child_number: bip32::ChildNumber) -> Descriptor<DescriptorPublicKey> {
        self.translate_pk(
            |pk| Result::Ok::<DescriptorPublicKey, ()>(pk.clone().derive(child_number)),
            |pk| Result::Ok::<DescriptorPublicKey, ()>(pk.clone().derive(child_number)),
        )
        .expect("Translation fn can't fail.")
    }

    pub fn parse_secret(s: &str) -> Result<(Descriptor<DescriptorPublicKey>, KeyMap), Error> {
        fn parse_key(
            s: &String,
            keymap: &mut KeyMap,
        ) -> Result<DescriptorPublicKey, DescriptorKeyParseError> {
            let (public_key, secret_key) = match DescriptorSecretKey::from_str(s) {
                Ok(sk) => (sk.as_public()?, Some(sk)),
                Err(_) => (DescriptorPublicKey::from_str(s)?, None),
            };

            if let Some(secret_key) = secret_key {
                keymap.insert(public_key.clone(), secret_key);
            }

            Ok(public_key)
        }

        let mut keymap_pk = KeyMap::new();
        let mut keymap_pkh = KeyMap::new();

        let descriptor = Descriptor::<String>::from_str(s)?;
        let descriptor = descriptor
            .translate_pk(
                |pk| parse_key(pk, &mut keymap_pk),
                |pkh| parse_key(pkh, &mut keymap_pkh),
            )
            .map_err(|e| Error::Unexpected(e.to_string()))?;

        keymap_pk.extend(keymap_pkh.into_iter());

        Ok((descriptor, keymap_pk))
    }

    pub fn to_string_with_secret(&self, key_map: &KeyMap) -> String {
        fn key_to_string(pk: &DescriptorPublicKey, key_map: &KeyMap) -> Result<String, ()> {
            Ok(match key_map.get(pk) {
                Some(secret) => secret.to_string(),
                None => pk.to_string(),
            })
        }

        let descriptor = self
            .translate_pk::<_, _, String, ()>(
                |pk| key_to_string(pk, key_map),
                |pkh| key_to_string(pkh, key_map),
            )
            .expect("Translation to string cannot fail");

        descriptor.to_string()
    }
}

impl<Pk> expression::FromTree for Descriptor<Pk>
where
    Pk: MiniscriptKey,
    <Pk as FromStr>::Err: ToString,
    <<Pk as MiniscriptKey>::Hash as str::FromStr>::Err: ToString,
{
    /// Parse an expression tree into a descriptor
    fn from_tree(top: &expression::Tree) -> Result<Descriptor<Pk>, Error> {
        match (top.name, top.args.len() as u32) {
            ("pk", 1) => {
                expression::terminal(&top.args[0], |pk| Pk::from_str(pk).map(Descriptor::Pk))
            }
            ("pkh", 1) => {
                expression::terminal(&top.args[0], |pk| Pk::from_str(pk).map(Descriptor::Pkh))
            }
            ("wpkh", 1) => {
                let wpkh = expression::terminal(&top.args[0], |pk| Pk::from_str(pk))?;
                if wpkh.is_uncompressed() {
                    Err(Error::ContextError(ScriptContextError::CompressedOnly))
                } else {
                    Ok(Descriptor::Wpkh(wpkh))
                }
            }
            ("sh", 1) => {
                let newtop = &top.args[0];
                match (newtop.name, newtop.args.len()) {
                    ("wsh", 1) => {
                        let sub = Miniscript::from_tree(&newtop.args[0])?;
                        if sub.ty.corr.base != miniscript::types::Base::B {
                            Err(Error::NonTopLevel(format!("{:?}", sub)))
                        } else {
                            Ok(Descriptor::ShWsh(sub))
                        }
                    }
                    ("wpkh", 1) => {
                        let wpkh = expression::terminal(&newtop.args[0], |pk| Pk::from_str(pk))?;
                        if wpkh.is_uncompressed() {
                            Err(Error::ContextError(ScriptContextError::CompressedOnly))
                        } else {
                            Ok(Descriptor::ShWpkh(wpkh))
                        }
                    }
                    _ => {
                        let sub = Miniscript::from_tree(&top.args[0])?;
                        if sub.ty.corr.base != miniscript::types::Base::B {
                            Err(Error::NonTopLevel(format!("{:?}", sub)))
                        } else {
                            Ok(Descriptor::Sh(sub))
                        }
                    }
                }
            }
            ("wsh", 1) => {
                let sub = Miniscript::from_tree(&top.args[0])?;
                if sub.ty.corr.base != miniscript::types::Base::B {
                    Err(Error::NonTopLevel(format!("{:?}", sub)))
                } else {
                    Ok(Descriptor::Wsh(sub))
                }
            }
            _ => {
                let sub = Miniscript::from_tree(&top)?;
                if sub.ty.corr.base != miniscript::types::Base::B {
                    Err(Error::NonTopLevel(format!("{:?}", sub)))
                } else {
                    Ok(Descriptor::Bare(sub))
                }
            }
        }
    }
}

impl<Pk> FromStr for Descriptor<Pk>
where
    Pk: MiniscriptKey,
    <Pk as FromStr>::Err: ToString,
    <<Pk as MiniscriptKey>::Hash as str::FromStr>::Err: ToString,
{
    type Err = Error;

    fn from_str(s: &str) -> Result<Descriptor<Pk>, Error> {
        for ch in s.as_bytes() {
            if *ch < 20 || *ch > 127 {
                return Err(Error::Unprintable(*ch));
            }
        }

        let top = expression::Tree::from_str(s)?;
        expression::FromTree::from_tree(&top)
    }
}

impl<Pk: MiniscriptKey> fmt::Debug for Descriptor<Pk> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Descriptor::Bare(ref sub) => write!(f, "{:?}", sub),
            Descriptor::Pk(ref p) => write!(f, "pk({:?})", p),
            Descriptor::Pkh(ref p) => write!(f, "pkh({:?})", p),
            Descriptor::Wpkh(ref p) => write!(f, "wpkh({:?})", p),
            Descriptor::ShWpkh(ref p) => write!(f, "sh(wpkh({:?}))", p),
            Descriptor::Sh(ref sub) => write!(f, "sh({:?})", sub),
            Descriptor::Wsh(ref sub) => write!(f, "wsh({:?})", sub),
            Descriptor::ShWsh(ref sub) => write!(f, "sh(wsh({:?}))", sub),
        }
    }
}

impl<Pk: MiniscriptKey> fmt::Display for Descriptor<Pk> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Descriptor::Bare(ref sub) => write!(f, "{}", sub),
            Descriptor::Pk(ref p) => write!(f, "pk({})", p),
            Descriptor::Pkh(ref p) => write!(f, "pkh({})", p),
            Descriptor::Wpkh(ref p) => write!(f, "wpkh({})", p),
            Descriptor::ShWpkh(ref p) => write!(f, "sh(wpkh({}))", p),
            Descriptor::Sh(ref sub) => write!(f, "sh({})", sub),
            Descriptor::Wsh(ref sub) => write!(f, "wsh({})", sub),
            Descriptor::ShWsh(ref sub) => write!(f, "sh(wsh({}))", sub),
        }
    }
}

serde_string_impl_pk!(Descriptor, "a script descriptor");

#[cfg(test)]
mod tests {
    use super::DescriptorKeyParseError;

    use bitcoin::blockdata::opcodes::all::{OP_CLTV, OP_CSV};
    use bitcoin::blockdata::script::Instruction;
    use bitcoin::blockdata::{opcodes, script};
    use bitcoin::hashes::hex::FromHex;
    use bitcoin::hashes::{hash160, sha256};
    use bitcoin::util::bip32;
    use bitcoin::{self, secp256k1, PublicKey};
    use descriptor::{
        DescriptorPublicKey, DescriptorSecretKey, DescriptorSinglePub, DescriptorXKey,
    };
    use miniscript::satisfy::BitcoinSig;
    use std::cmp;
    use std::collections::HashMap;
    use std::str::FromStr;
    use {Descriptor, DummyKey, Miniscript, Satisfier};

    #[cfg(feature = "compiler")]
    use policy;

    type StdDescriptor = Descriptor<PublicKey>;
    const TEST_PK: &'static str =
        "pk(020000000000000000000000000000000000000000000000000000000000000002)";

    impl cmp::PartialEq for DescriptorSecretKey {
        fn eq(&self, other: &Self) -> bool {
            match (self, other) {
                (
                    &DescriptorSecretKey::SinglePriv(ref a),
                    &DescriptorSecretKey::SinglePriv(ref b),
                ) => a.origin == b.origin && a.key == b.key,
                (&DescriptorSecretKey::XPrv(ref a), &DescriptorSecretKey::XPrv(ref b)) => {
                    a.origin == b.origin
                        && a.xkey == b.xkey
                        && a.derivation_path == b.derivation_path
                        && a.is_wildcard == b.is_wildcard
                }
                _ => false,
            }
        }
    }

    fn roundtrip_descriptor(s: &str) {
        let desc = Descriptor::<DummyKey>::from_str(&s).unwrap();
        let output = desc.to_string();
        let normalize_aliases = s.replace("c:pk_k(", "pk(").replace("c:pk_h(", "pkh(");
        assert_eq!(normalize_aliases, output);
    }

    #[test]
    fn desc_rtt_tests() {
        roundtrip_descriptor("c:pk_k()");
        roundtrip_descriptor("wsh(pk())");
        roundtrip_descriptor("wsh(c:pk_k())");
        roundtrip_descriptor("c:pk_h()");
    }
    #[test]
    fn parse_descriptor() {
        StdDescriptor::from_str("(").unwrap_err();
        StdDescriptor::from_str("(x()").unwrap_err();
        StdDescriptor::from_str("(\u{7f}()3").unwrap_err();
        StdDescriptor::from_str("pk()").unwrap_err();
        StdDescriptor::from_str("nl:0").unwrap_err(); //issue 63

        StdDescriptor::from_str(TEST_PK).unwrap();

        let uncompressed_pk =
        "0414fc03b8df87cd7b872996810db8458d61da8448e531569c8517b469a119d267be5645686309c6e6736dbd93940707cc9143d3cf29f1b877ff340e2cb2d259cf";

        // Context tests
        StdDescriptor::from_str(&format!("pk({})", uncompressed_pk)).unwrap();
        StdDescriptor::from_str(&format!("pkh({})", uncompressed_pk)).unwrap();
        StdDescriptor::from_str(&format!("sh(pk({}))", uncompressed_pk)).unwrap();
        StdDescriptor::from_str(&format!("wpkh({})", uncompressed_pk)).unwrap_err();
        StdDescriptor::from_str(&format!("sh(wpkh({}))", uncompressed_pk)).unwrap_err();
        StdDescriptor::from_str(&format!("wsh(pk{})", uncompressed_pk)).unwrap_err();
        StdDescriptor::from_str(&format!("sh(wsh(pk{}))", uncompressed_pk)).unwrap_err();
    }

    #[test]
    pub fn script_pubkey() {
        let bare = StdDescriptor::from_str("older(1000)").unwrap();
        assert_eq!(
            bare.script_pubkey(),
            bitcoin::Script::from(vec![0x02, 0xe8, 0x03, 0xb2])
        );
        assert_eq!(bare.address(bitcoin::Network::Bitcoin), None);

        let pk = StdDescriptor::from_str(TEST_PK).unwrap();
        assert_eq!(
            pk.script_pubkey(),
            bitcoin::Script::from(vec![
                0x21, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xac,
            ])
        );

        let pkh = StdDescriptor::from_str(
            "pkh(\
             020000000000000000000000000000000000000000000000000000000000000002\
             )",
        )
        .unwrap();
        assert_eq!(
            pkh.script_pubkey(),
            script::Builder::new()
                .push_opcode(opcodes::all::OP_DUP)
                .push_opcode(opcodes::all::OP_HASH160)
                .push_slice(
                    &hash160::Hash::from_hex("84e9ed95a38613f0527ff685a9928abe2d4754d4",).unwrap()
                        [..]
                )
                .push_opcode(opcodes::all::OP_EQUALVERIFY)
                .push_opcode(opcodes::all::OP_CHECKSIG)
                .into_script()
        );
        assert_eq!(
            pkh.address(bitcoin::Network::Bitcoin).unwrap().to_string(),
            "1D7nRvrRgzCg9kYBwhPH3j3Gs6SmsRg3Wq"
        );

        let wpkh = StdDescriptor::from_str(
            "wpkh(\
             020000000000000000000000000000000000000000000000000000000000000002\
             )",
        )
        .unwrap();
        assert_eq!(
            wpkh.script_pubkey(),
            script::Builder::new()
                .push_opcode(opcodes::all::OP_PUSHBYTES_0)
                .push_slice(
                    &hash160::Hash::from_hex("84e9ed95a38613f0527ff685a9928abe2d4754d4",).unwrap()
                        [..]
                )
                .into_script()
        );
        assert_eq!(
            wpkh.address(bitcoin::Network::Bitcoin).unwrap().to_string(),
            "bc1qsn57m9drscflq5nl76z6ny52hck5w4x5wqd9yt"
        );

        let shwpkh = StdDescriptor::from_str(
            "sh(wpkh(\
             020000000000000000000000000000000000000000000000000000000000000002\
             ))",
        )
        .unwrap();
        assert_eq!(
            shwpkh.script_pubkey(),
            script::Builder::new()
                .push_opcode(opcodes::all::OP_HASH160)
                .push_slice(
                    &hash160::Hash::from_hex("f1c3b9a431134cb90a500ec06e0067cfa9b8bba7",).unwrap()
                        [..]
                )
                .push_opcode(opcodes::all::OP_EQUAL)
                .into_script()
        );
        assert_eq!(
            shwpkh
                .address(bitcoin::Network::Bitcoin)
                .unwrap()
                .to_string(),
            "3PjMEzoveVbvajcnDDuxcJhsuqPHgydQXq"
        );

        let sh = StdDescriptor::from_str(
            "sh(c:pk_k(\
             020000000000000000000000000000000000000000000000000000000000000002\
             ))",
        )
        .unwrap();
        assert_eq!(
            sh.script_pubkey(),
            script::Builder::new()
                .push_opcode(opcodes::all::OP_HASH160)
                .push_slice(
                    &hash160::Hash::from_hex("aa5282151694d3f2f32ace7d00ad38f927a33ac8",).unwrap()
                        [..]
                )
                .push_opcode(opcodes::all::OP_EQUAL)
                .into_script()
        );
        assert_eq!(
            sh.address(bitcoin::Network::Bitcoin).unwrap().to_string(),
            "3HDbdvM9CQ6ASnQFUkWw6Z4t3qNwMesJE9"
        );

        let wsh = StdDescriptor::from_str(
            "wsh(c:pk_k(\
             020000000000000000000000000000000000000000000000000000000000000002\
             ))",
        )
        .unwrap();
        assert_eq!(
            wsh.script_pubkey(),
            script::Builder::new()
                .push_opcode(opcodes::all::OP_PUSHBYTES_0)
                .push_slice(
                    &sha256::Hash::from_hex(
                        "\
                         f9379edc8983152dc781747830075bd5\
                         3896e4b0ce5bff73777fd77d124ba085\
                         "
                    )
                    .unwrap()[..]
                )
                .into_script()
        );
        assert_eq!(
            wsh.address(bitcoin::Network::Bitcoin).unwrap().to_string(),
            "bc1qlymeahyfsv2jm3upw3urqp6m65ufde9seedl7umh0lth6yjt5zzsk33tv6"
        );

        let shwsh = StdDescriptor::from_str(
            "sh(wsh(c:pk_k(\
             020000000000000000000000000000000000000000000000000000000000000002\
             )))",
        )
        .unwrap();
        assert_eq!(
            shwsh.script_pubkey(),
            script::Builder::new()
                .push_opcode(opcodes::all::OP_HASH160)
                .push_slice(
                    &hash160::Hash::from_hex("4bec5d7feeed99e1d0a23fe32a4afe126a7ff07e",).unwrap()
                        [..]
                )
                .push_opcode(opcodes::all::OP_EQUAL)
                .into_script()
        );
        assert_eq!(
            shwsh
                .address(bitcoin::Network::Bitcoin)
                .unwrap()
                .to_string(),
            "38cTksiyPT2b1uGRVbVqHdDhW9vKs84N6Z"
        );
    }

    #[test]
    fn satisfy() {
        let secp = secp256k1::Secp256k1::new();
        let sk =
            secp256k1::SecretKey::from_slice(&b"sally was a secret key, she said"[..]).unwrap();
        let pk = bitcoin::PublicKey {
            key: secp256k1::PublicKey::from_secret_key(&secp, &sk),
            compressed: true,
        };
        let msg = secp256k1::Message::from_slice(&b"michael was a message, amusingly"[..])
            .expect("32 bytes");
        let sig = secp.sign(&msg, &sk);
        let mut sigser = sig.serialize_der().to_vec();
        sigser.push(0x01); // sighash_all

        struct SimpleSat {
            sig: secp256k1::Signature,
            pk: bitcoin::PublicKey,
        };

        impl Satisfier<bitcoin::PublicKey> for SimpleSat {
            fn lookup_sig(&self, pk: &bitcoin::PublicKey) -> Option<BitcoinSig> {
                if *pk == self.pk {
                    Some((self.sig, bitcoin::SigHashType::All))
                } else {
                    None
                }
            }
        }

        let satisfier = SimpleSat { sig, pk };
        let ms = ms_str!("c:pk_k({})", pk);

        let mut txin = bitcoin::TxIn {
            previous_output: bitcoin::OutPoint::default(),
            script_sig: bitcoin::Script::new(),
            sequence: 100,
            witness: vec![],
        };
        let bare = Descriptor::Bare(ms.clone());

        bare.satisfy(&mut txin, &satisfier).expect("satisfaction");
        assert_eq!(
            txin,
            bitcoin::TxIn {
                previous_output: bitcoin::OutPoint::default(),
                script_sig: script::Builder::new().push_slice(&sigser[..]).into_script(),
                sequence: 100,
                witness: vec![],
            }
        );
        assert_eq!(bare.unsigned_script_sig(), bitcoin::Script::new());

        let pkh = Descriptor::Pkh(pk);
        pkh.satisfy(&mut txin, &satisfier).expect("satisfaction");
        assert_eq!(
            txin,
            bitcoin::TxIn {
                previous_output: bitcoin::OutPoint::default(),
                script_sig: script::Builder::new()
                    .push_slice(&sigser[..])
                    .push_key(&pk)
                    .into_script(),
                sequence: 100,
                witness: vec![],
            }
        );
        assert_eq!(pkh.unsigned_script_sig(), bitcoin::Script::new());

        let wpkh = Descriptor::Wpkh(pk);
        wpkh.satisfy(&mut txin, &satisfier).expect("satisfaction");
        assert_eq!(
            txin,
            bitcoin::TxIn {
                previous_output: bitcoin::OutPoint::default(),
                script_sig: bitcoin::Script::new(),
                sequence: 100,
                witness: vec![sigser.clone(), pk.to_bytes(),],
            }
        );
        assert_eq!(wpkh.unsigned_script_sig(), bitcoin::Script::new());

        let shwpkh = Descriptor::ShWpkh(pk);
        shwpkh.satisfy(&mut txin, &satisfier).expect("satisfaction");
        let redeem_script = script::Builder::new()
            .push_opcode(opcodes::all::OP_PUSHBYTES_0)
            .push_slice(
                &hash160::Hash::from_hex("d1b2a1faf62e73460af885c687dee3b7189cd8ab").unwrap()[..],
            )
            .into_script();
        assert_eq!(
            txin,
            bitcoin::TxIn {
                previous_output: bitcoin::OutPoint::default(),
                script_sig: script::Builder::new()
                    .push_slice(&redeem_script[..])
                    .into_script(),
                sequence: 100,
                witness: vec![sigser.clone(), pk.to_bytes(),],
            }
        );
        assert_eq!(
            shwpkh.unsigned_script_sig(),
            script::Builder::new()
                .push_slice(&redeem_script[..])
                .into_script()
        );

        let sh = Descriptor::Sh(ms.clone());
        sh.satisfy(&mut txin, &satisfier).expect("satisfaction");
        assert_eq!(
            txin,
            bitcoin::TxIn {
                previous_output: bitcoin::OutPoint::default(),
                script_sig: script::Builder::new()
                    .push_slice(&sigser[..])
                    .push_slice(&ms.encode()[..])
                    .into_script(),
                sequence: 100,
                witness: vec![],
            }
        );
        assert_eq!(sh.unsigned_script_sig(), bitcoin::Script::new());

        let ms = ms_str!("c:pk_k({})", pk);

        let wsh = Descriptor::Wsh(ms.clone());
        wsh.satisfy(&mut txin, &satisfier).expect("satisfaction");
        assert_eq!(
            txin,
            bitcoin::TxIn {
                previous_output: bitcoin::OutPoint::default(),
                script_sig: bitcoin::Script::new(),
                sequence: 100,
                witness: vec![sigser.clone(), ms.encode().into_bytes(),],
            }
        );
        assert_eq!(wsh.unsigned_script_sig(), bitcoin::Script::new());

        let shwsh = Descriptor::ShWsh(ms.clone());
        shwsh.satisfy(&mut txin, &satisfier).expect("satisfaction");
        assert_eq!(
            txin,
            bitcoin::TxIn {
                previous_output: bitcoin::OutPoint::default(),
                script_sig: script::Builder::new()
                    .push_slice(&ms.encode().to_v0_p2wsh()[..])
                    .into_script(),
                sequence: 100,
                witness: vec![sigser.clone(), ms.encode().into_bytes(),],
            }
        );
        assert_eq!(
            shwsh.unsigned_script_sig(),
            script::Builder::new()
                .push_slice(&ms.encode().to_v0_p2wsh()[..])
                .into_script()
        );
    }

    #[test]
    fn after_is_cltv() {
        let descriptor = Descriptor::<bitcoin::PublicKey>::from_str("wsh(after(1000))").unwrap();
        let script = descriptor.witness_script();

        let actual_instructions: Vec<_> = script.instructions().collect();
        let check = actual_instructions.last().unwrap();

        assert_eq!(check, &Ok(Instruction::Op(OP_CLTV)))
    }

    #[test]
    fn older_is_csv() {
        let descriptor = Descriptor::<bitcoin::PublicKey>::from_str("wsh(older(1000))").unwrap();
        let script = descriptor.witness_script();

        let actual_instructions: Vec<_> = script.instructions().collect();
        let check = actual_instructions.last().unwrap();

        assert_eq!(check, &Ok(Instruction::Op(OP_CSV)))
    }

    #[test]
    fn roundtrip_tests() {
        let descriptor = Descriptor::<bitcoin::PublicKey>::from_str("multi");
        assert_eq!(
            descriptor.unwrap_err().to_string(),
            "unexpected «no arguments given»"
        )
    }

    #[test]
    fn empty_thresh() {
        let descriptor = Descriptor::<bitcoin::PublicKey>::from_str("thresh");
        assert_eq!(
            descriptor.unwrap_err().to_string(),
            "unexpected «no arguments given»"
        )
    }

    #[test]
    fn witness_stack_for_andv_is_arranged_in_correct_order() {
        // arrange
        let a = bitcoin::PublicKey::from_str(
            "02937402303919b3a2ee5edd5009f4236f069bf75667b8e6ecf8e5464e20116a0e",
        )
        .unwrap();
        let sig_a = secp256k1::Signature::from_str("3045022100a7acc3719e9559a59d60d7b2837f9842df30e7edcd754e63227e6168cec72c5d022066c2feba4671c3d99ea75d9976b4da6c86968dbf3bab47b1061e7a1966b1778c").unwrap();

        let b = bitcoin::PublicKey::from_str(
            "02eb64639a17f7334bb5a1a3aad857d6fec65faef439db3de72f85c88bc2906ad3",
        )
        .unwrap();
        let sig_b = secp256k1::Signature::from_str("3044022075b7b65a7e6cd386132c5883c9db15f9a849a0f32bc680e9986398879a57c276022056d94d12255a4424f51c700ac75122cb354895c9f2f88f0cbb47ba05c9c589ba").unwrap();

        let descriptor = Descriptor::<bitcoin::PublicKey>::from_str(&format!(
            "wsh(and_v(v:pk({A}),pk({B})))",
            A = a,
            B = b
        ))
        .unwrap();

        let mut txin = bitcoin::TxIn {
            previous_output: bitcoin::OutPoint::default(),
            script_sig: bitcoin::Script::new(),
            sequence: 0,
            witness: vec![],
        };
        let satisfier = {
            let mut satisfier = HashMap::with_capacity(2);

            satisfier.insert(a, (sig_a.clone(), ::bitcoin::SigHashType::All));
            satisfier.insert(b, (sig_b.clone(), ::bitcoin::SigHashType::All));

            satisfier
        };

        // act
        descriptor.satisfy(&mut txin, &satisfier).unwrap();

        // assert
        let witness0 = &txin.witness[0];
        let witness1 = &txin.witness[1];

        let sig0 = secp256k1::Signature::from_der(&witness0[..witness0.len() - 1]).unwrap();
        let sig1 = secp256k1::Signature::from_der(&witness1[..witness1.len() - 1]).unwrap();

        // why are we asserting this way?
        // The witness stack is evaluated from top to bottom. Given an `and` instruction, the left arm of the and is going to evaluate first,
        // meaning the next witness element (on a three element stack, that is the middle one) needs to be the signature for the left side of the `and`.
        // The left side of the `and` performs a CHECKSIG against public key `a` so `sig1` needs to be `sig_a` and `sig0` needs to be `sig_b`.
        assert_eq!(sig1, sig_a);
        assert_eq!(sig0, sig_b);
    }

    #[test]
    fn test_scriptcode() {
        // P2WPKH (from bip143 test vectors)
        let descriptor = Descriptor::<PublicKey>::from_str(
            "wpkh(025476c2e83188368da1ff3e292e7acafcdb3566bb0ad253f62fc70f07aeee6357)",
        )
        .unwrap();
        assert_eq!(
            *descriptor.script_code().as_bytes(),
            Vec::<u8>::from_hex("76a9141d0f172a0ecb48aee1be1f2687d2963ae33f71a188ac").unwrap()[..]
        );

        // P2SH-P2WPKH (from bip143 test vectors)
        let descriptor = Descriptor::<PublicKey>::from_str(
            "sh(wpkh(03ad1d8e89212f0b92c74d23bb710c00662ad1470198ac48c43f7d6f93a2a26873))",
        )
        .unwrap();
        assert_eq!(
            *descriptor.script_code().as_bytes(),
            Vec::<u8>::from_hex("76a91479091972186c449eb1ded22b78e40d009bdf008988ac").unwrap()[..]
        );

        // P2WSH (from bitcoind's `createmultisig`)
        let descriptor = Descriptor::<PublicKey>::from_str(
            "wsh(multi(2,03789ed0bb717d88f7d321a368d905e7430207ebbd82bd342cf11ae157a7ace5fd,03dbc6764b8884a92e871274b87583e6d5c2a58819473e17e107ef3f6aa5a61626))",
        )
        .unwrap();
        assert_eq!(
            *descriptor
                .script_code()
                .as_bytes(),
            Vec::<u8>::from_hex("522103789ed0bb717d88f7d321a368d905e7430207ebbd82bd342cf11ae157a7ace5fd2103dbc6764b8884a92e871274b87583e6d5c2a58819473e17e107ef3f6aa5a6162652ae").unwrap()[..]
        );

        // P2SH-P2WSH (from bitcoind's `createmultisig`)
        let descriptor = Descriptor::<PublicKey>::from_str("sh(wsh(multi(2,03789ed0bb717d88f7d321a368d905e7430207ebbd82bd342cf11ae157a7ace5fd,03dbc6764b8884a92e871274b87583e6d5c2a58819473e17e107ef3f6aa5a61626)))").unwrap();
        assert_eq!(
            *descriptor
                .script_code()
                .as_bytes(),
            Vec::<u8>::from_hex("522103789ed0bb717d88f7d321a368d905e7430207ebbd82bd342cf11ae157a7ace5fd2103dbc6764b8884a92e871274b87583e6d5c2a58819473e17e107ef3f6aa5a6162652ae")
                .unwrap()[..]
        );
    }

    #[test]
    fn parse_descriptor_key() {
        // With a wildcard
        let key = "[78412e3a/44'/0'/0']xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL/1/*";
        let expected = DescriptorPublicKey::XPub(DescriptorXKey {
            origin: Some((
                bip32::Fingerprint::from(&[0x78, 0x41, 0x2e, 0x3a][..]),
                (&[
                    bip32::ChildNumber::from_hardened_idx(44).unwrap(),
                    bip32::ChildNumber::from_hardened_idx(0).unwrap(),
                    bip32::ChildNumber::from_hardened_idx(0).unwrap(),
                ][..])
                .into(),
            )),
            xkey: bip32::ExtendedPubKey::from_str("xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL").unwrap(),
            derivation_path: (&[bip32::ChildNumber::from_normal_idx(1).unwrap()][..]).into(),
            is_wildcard: true,
        });
        assert_eq!(expected, key.parse().unwrap());
        assert_eq!(format!("{}", expected), key);

        // Without origin
        let key = "xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL/1";
        let expected = DescriptorPublicKey::XPub(DescriptorXKey {
            origin: None,
            xkey: bip32::ExtendedPubKey::from_str("xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL").unwrap(),
            derivation_path: (&[bip32::ChildNumber::from_normal_idx(1).unwrap()][..]).into(),
            is_wildcard: false,
        });
        assert_eq!(expected, key.parse().unwrap());
        assert_eq!(format!("{}", expected), key);

        // Testnet tpub
        let key = "tpubD6NzVbkrYhZ4YqYr3amYH15zjxHvBkUUeadieW8AxTZC7aY2L8aPSk3tpW6yW1QnWzXAB7zoiaNMfwXPPz9S68ZCV4yWvkVXjdeksLskCed/1";
        let expected = DescriptorPublicKey::XPub(DescriptorXKey {
            origin: None,
            xkey: bip32::ExtendedPubKey::from_str("tpubD6NzVbkrYhZ4YqYr3amYH15zjxHvBkUUeadieW8AxTZC7aY2L8aPSk3tpW6yW1QnWzXAB7zoiaNMfwXPPz9S68ZCV4yWvkVXjdeksLskCed").unwrap(),
            derivation_path: (&[bip32::ChildNumber::from_normal_idx(1).unwrap()][..]).into(),
            is_wildcard: false,
        });
        assert_eq!(expected, key.parse().unwrap());
        assert_eq!(format!("{}", expected), key);

        // Without derivation path
        let key = "xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL";
        let expected = DescriptorPublicKey::XPub(DescriptorXKey {
            origin: None,
            xkey: bip32::ExtendedPubKey::from_str("xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL").unwrap(),
            derivation_path: bip32::DerivationPath::from(&[][..]),
            is_wildcard: false,
        });
        assert_eq!(expected, key.parse().unwrap());
        assert_eq!(format!("{}", expected), key);

        // Raw (compressed) pubkey
        let key = "03f28773c2d975288bc7d1d205c3748651b075fbc6610e58cddeeddf8f19405aa8";
        let expected = DescriptorPublicKey::SinglePub(DescriptorSinglePub {
            key: bitcoin::PublicKey::from_str(
                "03f28773c2d975288bc7d1d205c3748651b075fbc6610e58cddeeddf8f19405aa8",
            )
            .unwrap(),
            origin: None,
        });
        assert_eq!(expected, key.parse().unwrap());
        assert_eq!(format!("{}", expected), key);

        // Raw (uncompressed) pubkey
        let key = "04f5eeb2b10c944c6b9fbcfff94c35bdeecd93df977882babc7f3a2cf7f5c81d3b09a68db7f0e04f21de5d4230e75e6dbe7ad16eefe0d4325a62067dc6f369446a";
        let expected = DescriptorPublicKey::SinglePub(DescriptorSinglePub {
            key: bitcoin::PublicKey::from_str(
                "04f5eeb2b10c944c6b9fbcfff94c35bdeecd93df977882babc7f3a2cf7f5c81d3b09a68db7f0e04f21de5d4230e75e6dbe7ad16eefe0d4325a62067dc6f369446a",
            )
            .unwrap(),
            origin: None,
        });
        assert_eq!(expected, key.parse().unwrap());
        assert_eq!(format!("{}", expected), key);

        // Raw pubkey with origin
        let desc =
            "[78412e3a/0'/42/0']0231c7d3fc85c148717848033ce276ae2b464a4e2c367ed33886cc428b8af48ff8";
        let expected = DescriptorPublicKey::SinglePub(DescriptorSinglePub {
            key: bitcoin::PublicKey::from_str(
                "0231c7d3fc85c148717848033ce276ae2b464a4e2c367ed33886cc428b8af48ff8",
            )
            .unwrap(),
            origin: Some((
                bip32::Fingerprint::from(&[0x78, 0x41, 0x2e, 0x3a][..]),
                (&[
                    bip32::ChildNumber::from_hardened_idx(0).unwrap(),
                    bip32::ChildNumber::from_normal_idx(42).unwrap(),
                    bip32::ChildNumber::from_hardened_idx(0).unwrap(),
                ][..])
                    .into(),
            )),
        });
        assert_eq!(expected, desc.parse().expect("Parsing desc"));
        assert_eq!(format!("{}", expected), desc);
    }

    #[test]
    fn parse_descriptor_key_errors() {
        // We refuse creating descriptors which claim to be able to derive hardened childs
        let desc = "[78412e3a/44'/0'/0']xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL/1/42'/*";
        assert_eq!(
            DescriptorPublicKey::from_str(desc),
            Err(DescriptorKeyParseError(
                "Hardened derivation is currently not supported."
            ))
        );

        // And even if they they claim it for the wildcard!
        let desc = "[78412e3a/44'/0'/0']xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL/1/42/*'";
        assert_eq!(
            DescriptorPublicKey::from_str(desc),
            Err(DescriptorKeyParseError(
                "Hardened derivation is currently not supported."
            ))
        );

        // And ones with misplaced wildcard
        let desc = "[78412e3a/44'/0'/0']xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL/1/*/44";
        assert_eq!(
            DescriptorPublicKey::from_str(desc),
            Err(DescriptorKeyParseError(
                "\'*\' may only appear as last element in a derivation path."
            ))
        );

        // And ones with invalid fingerprints
        let desc = "[NonHexor]xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL/1/*";
        assert_eq!(
            DescriptorPublicKey::from_str(desc),
            Err(DescriptorKeyParseError(
                "Malformed master fingerprint, expected 8 hex chars"
            ))
        );

        // And ones with invalid xpubs..
        let desc = "[78412e3a]xpub1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaLcgJvLJuZZvRcEL/1/*";
        assert_eq!(
            DescriptorPublicKey::from_str(desc),
            Err(DescriptorKeyParseError("Error while parsing xkey."))
        );

        // ..or invalid raw keys
        let desc = "[78412e3a]0208a117f3897c3a13c9384b8695eed98dc31bc2500feb19a1af424cd47a5d83/1/*";
        assert_eq!(
            DescriptorPublicKey::from_str(desc),
            Err(DescriptorKeyParseError(
                "Error while parsing simple public key"
            ))
        );

        // ..or invalid separators
        let desc = "[78412e3a]]03f28773c2d975288bc7d1d205c3748651b075fbc6610e58cddeeddf8f19405aa8";
        assert_eq!(
            DescriptorPublicKey::from_str(desc),
            Err(DescriptorKeyParseError(
                "Error while parsing simple public key"
            ))
        );
    }

    #[test]
    fn parse_descriptor_secret_key_error() {
        // Xpubs are invalid
        let secret_key = "xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL";
        assert_eq!(
            DescriptorSecretKey::from_str(secret_key),
            Err(DescriptorKeyParseError("Error while parsing xkey."))
        );

        // And ones with invalid fingerprints
        let desc = "[NonHexor]tprv8ZgxMBicQKsPcwcD4gSnMti126ZiETsuX7qwrtMypr6FBwAP65puFn4v6c3jrN9VwtMRMph6nyT63NrfUL4C3nBzPcduzVSuHD7zbX2JKVc/1/*";
        assert_eq!(
            DescriptorSecretKey::from_str(desc),
            Err(DescriptorKeyParseError(
                "Malformed master fingerprint, expected 8 hex chars"
            ))
        );

        // ..or invalid raw keys
        let desc = "[78412e3a]L32jTfVLei6BYTPUpwpJSkrHx8iL9GZzeErVS8y4Y/1/*";
        assert_eq!(
            DescriptorSecretKey::from_str(desc),
            Err(DescriptorKeyParseError(
                "Error while parsing a WIF private key"
            ))
        );
    }

    #[test]
    fn test_deriv_on_xprv() {
        let secret_key = DescriptorSecretKey::from_str("tprv8ZgxMBicQKsPcwcD4gSnMti126ZiETsuX7qwrtMypr6FBwAP65puFn4v6c3jrN9VwtMRMph6nyT63NrfUL4C3nBzPcduzVSuHD7zbX2JKVc/0'/1'/2").unwrap();
        let public_key = secret_key.as_public().unwrap();
        assert_eq!(public_key.to_string(), "[2cbe2a6d/0'/1']tpubDBrgjcxBxnXyL575sHdkpKohWu5qHKoQ7TJXKNrYznh5fVEGBv89hA8ENW7A8MFVpFUSvgLqc4Nj1WZcpePX6rrxviVtPowvMuGF5rdT2Vi/2");

        let secret_key = DescriptorSecretKey::from_str("tprv8ZgxMBicQKsPcwcD4gSnMti126ZiETsuX7qwrtMypr6FBwAP65puFn4v6c3jrN9VwtMRMph6nyT63NrfUL4C3nBzPcduzVSuHD7zbX2JKVc/0'/1'/2'").unwrap();
        let public_key = secret_key.as_public().unwrap();
        assert_eq!(public_key.to_string(), "[2cbe2a6d/0'/1'/2']tpubDDPuH46rv4dbFtmF6FrEtJEy1CvLZonyBoVxF6xsesHdYDdTBrq2mHhm8AbsPh39sUwL2nZyxd6vo4uWNTU9v4t893CwxjqPnwMoUACLvMV");

        let secret_key = DescriptorSecretKey::from_str("tprv8ZgxMBicQKsPcwcD4gSnMti126ZiETsuX7qwrtMypr6FBwAP65puFn4v6c3jrN9VwtMRMph6nyT63NrfUL4C3nBzPcduzVSuHD7zbX2JKVc/0/1/2").unwrap();
        let public_key = secret_key.as_public().unwrap();
        assert_eq!(public_key.to_string(), "tpubD6NzVbkrYhZ4WQdzxL7NmJN7b85ePo4p6RSj9QQHF7te2RR9iUeVSGgnGkoUsB9LBRosgvNbjRv9bcsJgzgBd7QKuxDm23ZewkTRzNSLEDr/0/1/2");

        let secret_key = DescriptorSecretKey::from_str("[aabbccdd]tprv8ZgxMBicQKsPcwcD4gSnMti126ZiETsuX7qwrtMypr6FBwAP65puFn4v6c3jrN9VwtMRMph6nyT63NrfUL4C3nBzPcduzVSuHD7zbX2JKVc/0/1/2").unwrap();
        let public_key = secret_key.as_public().unwrap();
        assert_eq!(public_key.to_string(), "[aabbccdd]tpubD6NzVbkrYhZ4WQdzxL7NmJN7b85ePo4p6RSj9QQHF7te2RR9iUeVSGgnGkoUsB9LBRosgvNbjRv9bcsJgzgBd7QKuxDm23ZewkTRzNSLEDr/0/1/2");

        let secret_key = DescriptorSecretKey::from_str("[aabbccdd/90']tprv8ZgxMBicQKsPcwcD4gSnMti126ZiETsuX7qwrtMypr6FBwAP65puFn4v6c3jrN9VwtMRMph6nyT63NrfUL4C3nBzPcduzVSuHD7zbX2JKVc/0'/1'/2").unwrap();
        let public_key = secret_key.as_public().unwrap();
        assert_eq!(public_key.to_string(), "[aabbccdd/90'/0'/1']tpubDBrgjcxBxnXyL575sHdkpKohWu5qHKoQ7TJXKNrYznh5fVEGBv89hA8ENW7A8MFVpFUSvgLqc4Nj1WZcpePX6rrxviVtPowvMuGF5rdT2Vi/2");
    }

    #[test]
    fn test_parse_secret() {
        let (descriptor, key_map) = Descriptor::parse_secret("wpkh(tprv8ZgxMBicQKsPcwcD4gSnMti126ZiETsuX7qwrtMypr6FBwAP65puFn4v6c3jrN9VwtMRMph6nyT63NrfUL4C3nBzPcduzVSuHD7zbX2JKVc/44'/0'/0'/0/*)").unwrap();
        assert_eq!(descriptor.to_string(), "wpkh([2cbe2a6d/44'/0'/0']tpubDCvNhURocXGZsLNqWcqD3syHTqPXrMSTwi8feKVwAcpi29oYKsDD3Vex7x2TDneKMVN23RbLprfxB69v94iYqdaYHsVz3kPR37NQXeqouVz/0/*)");
        assert_eq!(key_map.len(), 1);
    }

    #[test]
    #[cfg(feature = "compiler")]
    fn parse_and_derive() {
        let descriptor_str = "thresh(2,\
pk([d34db33f/44'/0'/0']xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL/1/*),\
pk(xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL/1),\
pk(03f28773c2d975288bc7d1d205c3748651b075fbc6610e58cddeeddf8f19405aa8))";
        let policy: policy::concrete::Policy<DescriptorPublicKey> = descriptor_str.parse().unwrap();
        let descriptor = Descriptor::Sh(policy.compile().unwrap());
        let derived_descriptor =
            descriptor.derive(bip32::ChildNumber::from_normal_idx(42).unwrap());

        let res_descriptor_str = "thresh(2,\
pk([d34db33f/44'/0'/0']xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL/1/42),\
pk(xpub6ERApfZwUNrhLCkDtcHTcxd75RbzS1ed54G1LkBUHQVHQKqhMkhgbmJbZRkrgZw4koxb5JaHWkY4ALHY2grBGRjaDMzQLcgJvLJuZZvRcEL/1),\
pk(03f28773c2d975288bc7d1d205c3748651b075fbc6610e58cddeeddf8f19405aa8))";
        let res_policy: policy::concrete::Policy<DescriptorPublicKey> =
            res_descriptor_str.parse().unwrap();
        let res_descriptor = Descriptor::Sh(res_policy.compile().unwrap());

        assert_eq!(res_descriptor, derived_descriptor);
    }

    #[test]
    fn parse_with_secrets() {
        let descriptor_str = "wpkh(xprv9s21ZrQH143K4CTb63EaMxja1YiTnSEWKMbn23uoEnAzxjdUJRQkazCAtzxGm4LSoTSVTptoV9RbchnKPW9HxKtZumdyxyikZFDLhogJ5Uj/44'/0'/0'/0/*)";
        let (descriptor, keymap) =
            Descriptor::<DescriptorPublicKey>::parse_secret(descriptor_str).unwrap();

        let expected = "wpkh([a12b02f4/44'/0'/0']xpub6BzhLAQUDcBUfHRQHZxDF2AbcJqp4Kaeq6bzJpXrjrWuK26ymTFwkEFbxPra2bJ7yeZKbDjfDeFwxe93JMqpo5SsPJH6dZdvV9kMzJkAZ69/0/*)";
        assert_eq!(expected, descriptor.to_string());
        assert_eq!(keymap.len(), 1);

        // try to turn it back into a string with the secrets
        assert_eq!(descriptor_str, descriptor.to_string_with_secret(&keymap));
    }
}
