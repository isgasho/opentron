//! Replacement of zcash_primitives::transaction::builder::Builder.

use bellman::groth16::Proof;
use ff::{Field, PrimeField};
use keys::Address;
use lazy_static::lazy_static;
use pairing::bls12_381::{Bls12, Fr, FrRepr};
use primitive_types::U256;
use rand::{rngs::OsRng, seq::SliceRandom, CryptoRng, RngCore};
use sha2::{Digest, Sha256};
use zcash_primitives::jubjub::edwards;
use zcash_primitives::jubjub::fs::{Fs, FsRepr};
use zcash_primitives::jubjub::Unknown;
use zcash_primitives::keys::{ExpandedSpendingKey, FullViewingKey, OutgoingViewingKey};
use zcash_primitives::merkle_tree::MerklePath;
use zcash_primitives::note_encryption::{Memo, SaplingNoteEncryption};
use zcash_primitives::primitives::{Diversifier, Note, PaymentAddress};
use zcash_primitives::prover::TxProver;
use zcash_primitives::redjubjub::PrivateKey;
use zcash_primitives::redjubjub::{PublicKey, Signature};
use zcash_primitives::sapling;
use zcash_primitives::sapling::Node;
use zcash_primitives::transaction::components::{Amount, GROTH_PROOF_SIZE};
use zcash_primitives::JUBJUB;
use zcash_proofs::prover::LocalTxProver;
use zcash_proofs::sapling::SaplingProvingContext;

use crate::keys::ZAddress;

// pub use zcash_primitives::transaction::builder::Error;

lazy_static! {
    pub static ref TX_PROVER: LocalTxProver = {
        use std::path::Path;

        let spend_path = "../ztron-params/sapling-spend.params";
        let output_path = "../ztron-params/sapling-output.params";

        eprintln!("loading local tx prover");

        LocalTxProver::new(Path::new(spend_path), Path::new(output_path))
    };
}

#[derive(Debug, PartialEq)]
pub enum Error {
    AnchorMismatch,
    BindingSig,
    ChangeIsNegative(Amount),
    InvalidAddress,
    InvalidAmount,
    NoChangeAddress,
    SpendProof,
    InvalidTransaction(&'static str),
}

impl ::std::fmt::Display for Error {
    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for Error {}

#[derive(Debug, PartialEq, Eq)]
pub enum TransactionType {
    Mint,
    Transfer,
    Burn,
}

struct TransparentInput {
    amount: U256,
}

struct TransparentOutput {
    address: Address,
    amount: U256,
}

struct SaplingSpend {
    expsk: ExpandedSpendingKey<Bls12>,
    diversifier: Diversifier,
    note: Note<Bls12>,
    alpha: Fs,
    merkle_path: MerklePath<Node>,
}

pub struct SpendDescription {
    pub cv: edwards::Point<Bls12, Unknown>,
    pub anchor: Fr,
    pub nullifier: [u8; 32],
    pub rk: PublicKey<Bls12>,
    pub zkproof: [u8; GROTH_PROOF_SIZE],
    pub spend_auth_sig: Option<Signature>,
}

impl SpendDescription {
    fn generate_spend_sig(&mut self, spend: &SaplingSpend, sighash: &[u8; 32]) {
        let mut rng = rand::rngs::OsRng;

        let spend_sig = sapling::spend_sig(PrivateKey(spend.expsk.ask), spend.alpha, sighash, &mut rng, &JUBJUB);
        self.spend_auth_sig = Some(spend_sig);
    }
}

impl SaplingSpend {
    fn generate_spend_proof<P: TxProver>(&self, ctx: &mut P::SaplingProvingContext, prover: &P) -> SpendDescription {
        let fvk = FullViewingKey::from_expanded_spending_key(&self.expsk, &JUBJUB);
        let nf = {
            let mut raw = [0u8; 32];
            raw.copy_from_slice(&self.note.nf(&fvk.vk, self.merkle_path.position, &JUBJUB));
            raw
        };
        let proof_generation_key = self.expsk.proof_generation_key(&JUBJUB);
        let anchor = self.merkle_path.root(Node::new(self.note.cm(&JUBJUB).into())).into();

        let (zkproof, cv, rk) = prover
            .spend_proof(
                ctx,
                proof_generation_key,
                self.diversifier,
                self.note.r,
                self.alpha,
                self.note.value,
                anchor,
                self.merkle_path.clone(),
            )
            .expect("proving should not fail");

        SpendDescription {
            cv,
            anchor,
            nullifier: nf,
            rk,
            zkproof,
            spend_auth_sig: None,
        }
    }
}

pub struct SaplingOutput {
    ovk: OutgoingViewingKey,
    to: PaymentAddress<Bls12>,
    note: Note<Bls12>,
    memo: Memo,
}

pub struct OutputDescription {
    pub cv: edwards::Point<Bls12, Unknown>,
    pub cmu: Fr,
    pub ephemeral_key: edwards::Point<Bls12, Unknown>,
    pub enc_ciphertext: [u8; 580],
    pub out_ciphertext: [u8; 80],
    pub zkproof: [u8; GROTH_PROOF_SIZE],
}

impl SaplingOutput {
    pub fn new<R: RngCore + CryptoRng>(
        rng: &mut R,
        ovk: OutgoingViewingKey,
        to: PaymentAddress<Bls12>,
        value: Amount,
        memo: Option<Memo>,
    ) -> Result<Self, Error> {
        let g_d = match to.g_d(&JUBJUB) {
            Some(g_d) => g_d,
            None => return Err(Error::InvalidAddress),
        };
        if value.is_negative() {
            return Err(Error::InvalidAmount);
        }

        let rcm = Fs::random(rng);

        let note = Note {
            g_d,
            pk_d: to.pk_d().clone(),
            value: value.into(),
            r: rcm,
        };

        Ok(SaplingOutput {
            ovk,
            to,
            note,
            memo: memo.unwrap_or_default(),
        })
    }

    fn generate_output_proof<P: TxProver>(&self, ctx: &mut P::SaplingProvingContext, prover: &P) -> OutputDescription {
        let mut rng = rand::rngs::OsRng;

        let cmu = self.note.cm(&JUBJUB); // note commitment

        let enc = SaplingNoteEncryption::new(
            self.ovk,
            self.note.clone(),
            self.to.clone(),
            self.memo.clone(),
            &mut rng,
        );

        let c_enc = enc.encrypt_note_plaintext();

        let epk = enc.epk().clone();

        // zkproof, value_commitment
        let (zkproof, cv) = prover.output_proof(ctx, *enc.esk(), self.to.clone(), self.note.r, self.note.value);

        let c_out = enc.encrypt_outgoing_plaintext(&cv, &self.note.cm(&JUBJUB));

        OutputDescription {
            cv,
            cmu,
            ephemeral_key: epk.into(),
            enc_ciphertext: c_enc,
            out_ciphertext: c_out,
            zkproof,
        }
    }
}

fn abi_encode_transfer(spends: &[SpendDescription], outputs: &[OutputDescription], binding_sig: &Signature) -> Vec<u8> {
    use ethabi::Token;

    //input: nf, anchor, cv, rk, proof
    //output: cm, cv, epk, proof
    // transfer(
    //    bytes32[10][] input,
    //    bytes32[2][] spendAuthoritySignature,
    //    bytes32[9][] output,
    //    bytes32[2] bindingSignature,
    //    bytes32[21][] c
    // )

    let input = Token::Array(
        spends
            .iter()
            .map(|spend_desc| {
                let mut raw = Vec::with_capacity(10 * 32);
                raw.extend_from_slice(&spend_desc.nullifier[..]);
                raw.extend_from_slice(spend_desc.anchor.to_repr().as_ref());
                spend_desc.cv.write(&mut raw).unwrap();
                spend_desc.rk.write(&mut raw).unwrap();
                raw.extend_from_slice(&spend_desc.zkproof[..]);
                Token::FixedBytes(raw)
            })
            .collect(),
    );
    let spend_auth_sig = Token::Array(
        spends
            .iter()
            .map(|spend| {
                let mut raw = Vec::with_capacity(64);
                spend.spend_auth_sig.as_ref().unwrap().write(&mut raw).unwrap();
                Token::FixedBytes(raw)
            })
            .collect(),
    );
    let output = Token::Array(
        outputs
            .iter()
            .map(|output_desc| {
                let mut raw = Vec::with_capacity(9 * 32);
                raw.extend_from_slice(output_desc.cmu.to_repr().as_ref());
                output_desc.cv.write(&mut raw).unwrap();
                output_desc.ephemeral_key.write(&mut raw).unwrap();
                raw.extend_from_slice(&output_desc.zkproof[..]);
                Token::FixedBytes(raw)
            })
            .collect(),
    );
    let binding_signature = {
        let mut raw = Vec::with_capacity(64);
        binding_sig.write(&mut raw).unwrap();
        Token::FixedBytes(raw)
    };

    let c = Token::Array(
        outputs
            .iter()
            .map(|output_desc| {
                let mut raw = Vec::with_capacity(21 * 32);
                raw.extend_from_slice(&output_desc.enc_ciphertext[..]);
                raw.extend_from_slice(&output_desc.out_ciphertext[..]);
                Token::FixedBytes(raw)
            })
            .collect(),
    );
    let parameters = [input, spend_auth_sig, output, binding_signature, c];

    ethabi::encode(&parameters)
}

/// Generates a Transaction from its inputs and outputs.
pub struct Builder<R: RngCore + CryptoRng> {
    rng: R,
    contract_address: Address,
    scaling_factor: U256,
    value_balance: Amount,
    anchor: Option<Fr>,
    spends: Vec<SaplingSpend>,
    outputs: Vec<SaplingOutput>,
    transparent_input: Option<TransparentInput>,
    transparent_output: Option<TransparentOutput>,
    // change_address: Option<(OutgoingViewingKey, PaymentAddress<Bls12>)>,
}

impl Builder<OsRng> {
    pub fn new(contract_address: Address, scaling_exponent: u8) -> Self {
        Builder::new_with_rng(contract_address, scaling_exponent, OsRng)
    }
}

impl<R: RngCore + CryptoRng> Builder<R> {
    pub fn new_with_rng(contract_address: Address, scaling_exponent: u8, rng: R) -> Self {
        Builder {
            rng,
            contract_address,
            scaling_factor: U256::exp10(scaling_exponent as usize),
            value_balance: Amount::zero(),
            anchor: None,
            spends: vec![],
            outputs: vec![],
            transparent_input: None,
            transparent_output: None,
        }
    }

    /// Adds a Sapling note to be spent in this transaction.
    pub fn add_sapling_spend(
        &mut self,
        expsk: ExpandedSpendingKey<Bls12>,
        diversifier: Diversifier,
        note: Note<Bls12>,
        merkle_path: MerklePath<Node>,
    ) -> Result<(), Error> {
        if self.spends.len() >= 2 {
            return Err(Error::InvalidTransaction("too many sapling spends"));
        }

        // Consistency check: all anchors must equal the first one
        let cm = Node::new(note.cm(&JUBJUB).into());
        if let Some(anchor) = self.anchor {
            let path_root: Fr = merkle_path.root(cm).into();
            if path_root != anchor {
                return Err(Error::AnchorMismatch);
            }
        } else {
            self.anchor = Some(merkle_path.root(cm).into())
        }

        let alpha = Fs::random(&mut self.rng);

        self.value_balance += Amount::from_u64(note.value).map_err(|_| Error::InvalidAmount)?;

        self.spends.push(SaplingSpend {
            expsk,
            diversifier,
            note,
            alpha,
            merkle_path,
        });

        Ok(())
    }

    /// Adds a Sapling address to send funds to.
    pub fn add_sapling_output(
        &mut self,
        ovk: OutgoingViewingKey,
        to: ZAddress,
        value: Amount,
        memo: Option<Memo>,
    ) -> Result<(), Error> {
        if self.outputs.len() >= 2 {
            return Err(Error::InvalidTransaction("too many sapling output"));
        }

        let output = SaplingOutput::new(&mut self.rng, ovk, to.0, value, memo)?;
        self.value_balance -= value;
        self.outputs.push(output);

        Ok(())
    }

    /// Adds a transparent coin to be spent in this transaction.
    pub fn add_transparent_input(&mut self, value: U256) -> Result<(), Error> {
        if self.transparent_input.is_some() {
            return Err(Error::InvalidTransaction("mint can only have one transparent input"));
        }
        let input = TransparentInput { amount: value };
        self.transparent_input = Some(input);
        Ok(())
    }

    /// Adds a transparent address to send funds to.
    pub fn add_transparent_output(&mut self, to: &Address, value: U256) -> Result<(), Error> {
        if self.transparent_output.is_some() {
            return Err(Error::InvalidTransaction("burn can only have one transparent output"));
        }
        let output = TransparentOutput {
            address: to.clone(),
            amount: value,
        };
        self.transparent_output = Some(output);

        Ok(())
    }

    fn transaction_type(&self) -> Result<TransactionType, Error> {
        if self.transparent_input.is_some() {
            if self.outputs.len() == 1 && self.spends.is_empty() && self.transparent_output.is_none() {
                return Ok(TransactionType::Mint);
            }
            return Err(Error::InvalidTransaction(
                "mint must be a transaction to 1 shielded output",
            ));
        } else if self.transparent_output.is_some() {
            if self.spends.len() == 1 && self.outputs.len() <= 1 && self.transparent_input.is_none() {
                return Ok(TransactionType::Burn);
            }
            return Err(Error::InvalidTransaction(
                "burn must be a transaction from 1 shielded output, to max 1 shielded output",
            ));
        } else {
            if self.spends.len() >= 1 && self.outputs.len() >= 1 {
                return Ok(TransactionType::Transfer);
            }
            return Err(Error::InvalidTransaction("invalid mint, burn, or transfer"));
        }
    }

    fn build_mint(self, prover: &impl TxProver) -> Result<Vec<u8>, Error> {
        if self.value_balance.is_positive() {
            return Err(Error::InvalidAmount);
        }
        let transparent_input_value = self.transparent_input.as_ref().unwrap().amount;
        let shielded_output_value = -i64::from(self.value_balance);
        if U256::from(shielded_output_value) * self.scaling_factor != transparent_input_value {
            return Err(Error::InvalidTransaction("input & output amount mismatch"));
        }

        let mut ctx = prover.new_sapling_proving_context();

        let output_desc = self.outputs[0].generate_output_proof(&mut ctx, prover);

        let mut transaction_data = Vec::with_capacity(1024);
        transaction_data.extend_from_slice(self.contract_address.as_tvm_bytes());
        // receive note value
        transaction_data.extend_from_slice(&shielded_output_value.to_be_bytes()[..]);
        // encodeReceiveDescriptionWithoutC
        transaction_data.extend_from_slice(output_desc.cmu.to_repr().as_ref());
        output_desc.cv.write(&mut transaction_data).unwrap();
        output_desc.ephemeral_key.write(&mut transaction_data).unwrap();
        transaction_data.extend_from_slice(&output_desc.zkproof[..]);
        // encodeCencCout
        transaction_data.extend_from_slice(&output_desc.enc_ciphertext[..]);
        transaction_data.extend_from_slice(&output_desc.out_ciphertext[..]);
        transaction_data.extend(&[0u8; 12]);

        let sighash = {
            let mut hasher = Sha256::new();
            hasher.update(&transaction_data);
            hasher.finalize()
        };
        let binding_sig = prover
            .binding_sig(&mut ctx, self.value_balance, sighash.as_ref())
            .map_err(|()| Error::BindingSig)?;

        let mut parameter = vec![0u8; 32];

        let raw_value = U256::exp10(18); // value * scaleFactor
        raw_value.to_big_endian(&mut parameter[..32]);

        parameter.extend_from_slice(output_desc.cmu.to_repr().as_ref());
        output_desc.cv.write(&mut parameter).unwrap();
        output_desc.ephemeral_key.write(&mut parameter).unwrap();
        parameter.extend_from_slice(&output_desc.zkproof[..]);

        binding_sig.write(&mut parameter).unwrap();

        parameter.extend_from_slice(&output_desc.enc_ciphertext[..]);
        parameter.extend_from_slice(&output_desc.out_ciphertext[..]);
        parameter.extend(&[0u8; 12]);

        Ok(parameter)
    }

    fn build_transfer(self, prover: &impl TxProver) -> Result<Vec<u8>, Error> {
        println!("val bal => {:?}", self.value_balance);
        if self.value_balance != Amount::zero() {
            return Err(Error::InvalidAmount);
        }

        let mut ctx = prover.new_sapling_proving_context();

        println!("generating proofs...");

        let mut spend_descs: Vec<_> = self
            .spends
            .iter()
            .map(|output| output.generate_spend_proof(&mut ctx, prover))
            .collect();

        let output_descs: Vec<_> = self
            .outputs
            .iter()
            .map(|output| output.generate_output_proof(&mut ctx, prover))
            .collect();

        println!("generating proofs... done");

        let mut transaction_data = Vec::with_capacity(1024);
        transaction_data.extend_from_slice(self.contract_address.as_tvm_bytes());
        for spend in &spend_descs {
            // encodeSpendDescriptionWithoutSpendAuthSig
            transaction_data.extend_from_slice(&spend.nullifier[..]);
            transaction_data.extend_from_slice(spend.anchor.to_repr().as_ref());
            spend.cv.write(&mut transaction_data).unwrap();
            spend.rk.write(&mut transaction_data).unwrap();
            transaction_data.extend_from_slice(&spend.zkproof[..]);
        }

        for output in &output_descs {
            // encodeReceiveDescriptionWithoutC
            transaction_data.extend_from_slice(output.cmu.to_repr().as_ref());
            output.cv.write(&mut transaction_data).unwrap();
            output.ephemeral_key.write(&mut transaction_data).unwrap();
            transaction_data.extend_from_slice(&output.zkproof[..]);
        }

        for output in &output_descs {
            // encodeCencCout
            transaction_data.extend_from_slice(&output.enc_ciphertext[..]);
            transaction_data.extend_from_slice(&output.out_ciphertext[..]);
            transaction_data.extend(&[0u8; 12]);
        }

        let sighash = {
            let mut hasher = Sha256::new();
            hasher.update(&transaction_data);
            hasher.finalize()
        };

        println!("sighash => {:?}", hex::encode(&sighash));

        for (desc, spend) in spend_descs.iter_mut().zip(self.spends.iter()) {
            desc.generate_spend_sig(spend, sighash.as_ref());
        }
        for desc in &spend_descs {
            println!("!!! => {:?}", desc.spend_auth_sig);
        }

        let binding_sig = prover
            .binding_sig(&mut ctx, self.value_balance, sighash.as_ref())
            .map_err(|_| Error::BindingSig)?;

        Ok(abi_encode_transfer(&spend_descs, &output_descs, &binding_sig))
    }

    fn build_burn(self, prover: &impl TxProver) -> Result<Vec<u8>, Error> {
        println!("val bal => {:?}", self.value_balance);
        if self.value_balance.is_negative() {
            return Err(Error::InvalidAmount);
        }
        let transparent_output_value = self.transparent_output.as_ref().unwrap().amount;
        let shielded_input_value = i64::from(self.value_balance);
        if U256::from(shielded_input_value) * self.scaling_factor != transparent_output_value {
            return Err(Error::InvalidTransaction("input & output amount mismatch"));
        }

        unimplemented!()

    }

    pub fn build(self, prover: &impl TxProver) -> Result<(TransactionType, Vec<u8>), Error> {
        // also check validity
        let txn_type = self.transaction_type()?;

        let ret = match txn_type {
            TransactionType::Mint => self.build_mint(prover)?,
            TransactionType::Transfer => self.build_transfer(prover)?,
            TransactionType::Burn => self.build_burn(prover)?,
        };
        Ok((txn_type, ret))
    }
}