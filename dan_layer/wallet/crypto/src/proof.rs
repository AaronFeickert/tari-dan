//   Copyright 2024 The Tari Project
//   SPDX-License-Identifier: BSD-3-Clause

use blake2::Blake2b;
use chacha20poly1305::{
    aead,
    aead::{generic_array::GenericArray, OsRng},
    consts::U32,
    AeadCore,
    AeadInPlace,
    KeyInit,
    Tag,
    XChaCha20Poly1305,
    XNonce,
};
use digest::FixedOutput;
use tari_crypto::{
    commitment::{ExtensionDegree, HomomorphicCommitmentFactory},
    errors::RangeProofError,
    extended_range_proof::ExtendedRangeProofService,
    hashing::DomainSeparatedHasher,
    keys::{PublicKey, SecretKey},
    ristretto::{
        bulletproofs_plus::{RistrettoExtendedMask, RistrettoExtendedWitness},
        pedersen::PedersenCommitment,
        RistrettoPublicKey,
        RistrettoSchnorr,
        RistrettoSecretKey,
    },
    tari_utilities::ByteArray,
};
use tari_engine_types::confidential::{challenges, get_commitment_factory, get_range_proof_service};
use tari_hashing::TransactionSecureNonceKdfDomain;
use tari_template_lib::{
    crypto::RistrettoPublicKeyBytes,
    models::{
        Amount,
        ConfidentialOutputStatement,
        ConfidentialStatement,
        EncryptedData,
        ViewableBalanceProof,
        ViewableBalanceProofChallengeFields,
    },
};
use tari_utilities::safe_array::SafeArray;
use zeroize::{Zeroize, Zeroizing};

use crate::{
    byte_utils::copy_fixed,
    error::ConfidentialProofError,
    kdfs::EncryptedDataKey,
    ConfidentialProofStatement,
};

pub fn create_confidential_output_statement(
    output_statement: Option<&ConfidentialProofStatement>,
    output_revealed_amount: Amount,
    change_statement: Option<&ConfidentialProofStatement>,
    change_revealed_amount: Amount,
) -> Result<ConfidentialOutputStatement, ConfidentialProofError> {
    let proof_change_statement = change_statement
        .as_ref()
        .map(|stmt| -> Result<_, ConfidentialProofError> {
            let change_commitment = stmt.to_commitment();
            Ok(ConfidentialStatement {
                commitment: copy_fixed(change_commitment.as_bytes()),
                sender_public_nonce: RistrettoPublicKeyBytes::from_bytes(stmt.sender_public_nonce.as_bytes())
                    .expect("[generate_confidential_proof] change nonce"),
                encrypted_data: stmt.encrypted_data.clone(),
                minimum_value_promise: stmt.minimum_value_promise,
                viewable_balance_proof: stmt.resource_view_key.as_ref().map(|view_key| {
                    create_viewable_balance_proof(
                        &stmt.mask,
                        stmt.amount.as_u64_checked().unwrap(),
                        &change_commitment,
                        view_key,
                    )
                }),
            })
        })
        .transpose()?;
    let confidential_output_value = output_statement
        .as_ref()
        .map(|o| o.amount)
        .unwrap_or_default()
        .as_u64_checked()
        .ok_or(ConfidentialProofError::NegativeAmount)?;

    let proof_output_statement = output_statement.as_ref().map(|stmt| {
        let commitment = stmt.to_commitment();
        ConfidentialStatement {
            commitment: copy_fixed(commitment.as_bytes()),
            sender_public_nonce: copy_fixed(stmt.sender_public_nonce.as_bytes()),
            encrypted_data: stmt.encrypted_data.clone(),
            minimum_value_promise: stmt.minimum_value_promise,
            viewable_balance_proof: stmt.resource_view_key.as_ref().map(|view_key| {
                create_viewable_balance_proof(&stmt.mask, confidential_output_value, &commitment, view_key)
            }),
        }
    });

    let output_range_proof = generate_extended_bullet_proof(output_statement, change_statement)?;

    Ok(ConfidentialOutputStatement {
        output_statement: proof_output_statement,
        change_statement: proof_change_statement,
        range_proof: output_range_proof,
        output_revealed_amount,
        change_revealed_amount,
    })
}

fn inner_encrypted_data_kdf_aead(
    encryption_key: &RistrettoSecretKey,
    commitment: &PedersenCommitment,
) -> EncryptedDataKey {
    let mut aead_key = EncryptedDataKey::from(SafeArray::default());
    DomainSeparatedHasher::<Blake2b<U32>, TransactionSecureNonceKdfDomain>::new_with_label("encrypted_value_and_mask")
        .chain(encryption_key.as_bytes())
        .chain(commitment.as_bytes())
        .finalize_into(GenericArray::from_mut_slice(aead_key.reveal_mut()));
    aead_key
}

pub fn create_viewable_balance_proof(
    mask: &RistrettoSecretKey,
    output_amount: u64,
    commitment: &PedersenCommitment,
    view_key: &RistrettoPublicKey,
) -> ViewableBalanceProof {
    let (elgamal_secret_nonce, elgamal_public_nonce) = RistrettoPublicKey::random_keypair(&mut OsRng);
    let r = &elgamal_secret_nonce;
    let value_as_secret = RistrettoSecretKey::from(output_amount);

    // E = v.G + rP
    let elgamal_encrypted = RistrettoPublicKey::from_secret_key(&value_as_secret) + r * view_key;

    // Nonces
    let x_v = RistrettoSecretKey::random(&mut OsRng);
    let x_m = RistrettoSecretKey::random(&mut OsRng);
    let x_r = RistrettoSecretKey::random(&mut OsRng);

    // C' = x_m.G + x_v.H
    let c_prime = get_commitment_factory().commit(&x_m, &x_v);
    // E' = x_v.G + x_r.P
    let e_prime = RistrettoPublicKey::from_secret_key(&x_v) + &x_r * view_key;
    // R' = x_r.G
    let r_prime = RistrettoPublicKey::from_secret_key(&x_r);

    // Create challenge
    let elgamal_encrypted = copy_fixed(elgamal_encrypted.as_bytes());
    let elgamal_public_nonce = copy_fixed(elgamal_public_nonce.as_bytes());
    let c_prime = copy_fixed(c_prime.as_bytes());
    let e_prime = copy_fixed(e_prime.as_bytes());
    let r_prime = copy_fixed(r_prime.as_bytes());

    let challenge_fields = ViewableBalanceProofChallengeFields {
        elgamal_encrypted: &elgamal_encrypted,
        elgamal_public_nonce: &elgamal_public_nonce,
        c_prime: &c_prime,
        e_prime: &e_prime,
        r_prime: &r_prime,
    };

    let e = &challenges::viewable_balance_proof_challenge64(commitment, view_key, challenge_fields);

    // Generate signatures
    // TODO: sign_raw_uniform should take a [u8; 64] for the challenge so that length mismatches are caught at compile
    //       time. The challenge is never a secret (in all current usages), so non-zeroed memory is not an issue.

    // sv = ev + x_v
    let s_v = RistrettoSchnorr::sign_raw_uniform(&value_as_secret, x_v, e)
        .expect("INVARIANT VIOLATION: sv RistrettoSchnorr::sign_raw_uniform and challenge hash output length mismatch");
    // sm = em + x_m
    let s_m = RistrettoSchnorr::sign_raw_uniform(mask, x_m, e)
        .expect("INVARIANT VIOLATION: sm RistrettoSchnorr::sign_raw_uniform and challenge hash output length mismatch");
    // sr = er + x_r
    let s_r = RistrettoSchnorr::sign_raw_uniform(r, x_r, e)
        .expect("INVARIANT VIOLATION: sr RistrettoSchnorr::sign_raw_uniform and challenge hash output length mismatch");

    ViewableBalanceProof {
        elgamal_encrypted,
        elgamal_public_nonce,
        c_prime,
        e_prime,
        r_prime,
        s_v: copy_fixed(s_v.get_signature().as_bytes()),
        s_m: copy_fixed(s_m.get_signature().as_bytes()),
        s_r: copy_fixed(s_r.get_signature().as_bytes()),
    }
}

const ENCRYPTED_DATA_TAG: &[u8] = b"TARI_AAD_VALUE_AND_MASK_EXTEND_NONCE_VARIANT";

pub(crate) fn encrypt_data(
    encryption_key: &RistrettoSecretKey,
    commitment: &PedersenCommitment,
    value: u64,
    mask: &RistrettoSecretKey,
) -> Result<EncryptedData, aead::Error> {
    fn payload_slice_mut(bytes: &mut [u8]) -> &mut [u8] {
        &mut bytes[EncryptedData::payload_offset()..]
    }

    fn tag_slice_mut(bytes: &mut [u8]) -> &mut [u8] {
        &mut bytes[..EncryptedData::SIZE_TAG]
    }

    fn nonce_slice_mut(bytes: &mut [u8]) -> &mut [u8] {
        &mut bytes[EncryptedData::SIZE_TAG..EncryptedData::SIZE_TAG + EncryptedData::SIZE_NONCE]
    }

    // Produce a secure random nonce and the AEAD
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let aead_key = inner_encrypted_data_kdf_aead(encryption_key, commitment);
    let cipher = XChaCha20Poly1305::new(GenericArray::from_slice(aead_key.reveal()));

    // Encode the value and mask
    let mut bytes = vec![0; EncryptedData::min_size()];
    let payload_mut = payload_slice_mut(&mut bytes);
    payload_mut[..EncryptedData::SIZE_VALUE].copy_from_slice(value.to_le_bytes().as_ref());
    payload_mut[EncryptedData::SIZE_VALUE..EncryptedData::SIZE_VALUE + EncryptedData::SIZE_MASK]
        .copy_from_slice(mask.as_bytes());
    // Encrypt in place
    match cipher.encrypt_in_place_detached(&nonce, ENCRYPTED_DATA_TAG, payload_mut) {
        Ok(tag) => {
            tag_slice_mut(&mut bytes).copy_from_slice(&tag);
            nonce_slice_mut(&mut bytes).copy_from_slice(&nonce);

            Ok(EncryptedData::try_from(bytes).expect("bytes length == EncryptedData::min_size()"))
        },
        Err(err) => {
            bytes.zeroize();
            Err(err)
        },
    }
}

pub fn decrypt_data_and_mask(
    encryption_key: &RistrettoSecretKey,
    commitment: &PedersenCommitment,
    encrypted_data: &EncryptedData,
) -> Result<(u64, RistrettoSecretKey), aead::Error> {
    // Extract the tag, nonce, and ciphertext
    let tag = Tag::from_slice(encrypted_data.tag_slice());
    let nonce = XNonce::from_slice(encrypted_data.nonce_slice());
    let mut bytes = Zeroizing::new(encrypted_data.payload_slice().to_vec());

    // Set up the AEAD
    let aead_key = inner_encrypted_data_kdf_aead(encryption_key, commitment);
    let cipher = XChaCha20Poly1305::new(GenericArray::from_slice(aead_key.reveal()));

    // Decrypt in place
    cipher.decrypt_in_place_detached(nonce, ENCRYPTED_DATA_TAG, bytes.as_mut_slice(), tag)?;

    // Decode the value and mask
    let mut value_bytes = [0u8; EncryptedData::SIZE_VALUE];
    value_bytes.copy_from_slice(&bytes[..EncryptedData::SIZE_VALUE]);
    Ok((
        u64::from_le_bytes(value_bytes),
        RistrettoSecretKey::from_canonical_bytes(
            &bytes[EncryptedData::SIZE_VALUE..EncryptedData::SIZE_VALUE + EncryptedData::SIZE_MASK],
        )
        .expect("The length of bytes is exactly SIZE_MASK"),
    ))
}

fn generate_extended_bullet_proof(
    output_statement: Option<&ConfidentialProofStatement>,
    change_statement: Option<&ConfidentialProofStatement>,
) -> Result<Vec<u8>, RangeProofError> {
    if output_statement.is_none() && change_statement.is_none() {
        // We're only outputting revealed funds, so no need to generate a range proof (i.e. zero length is valid)
        return Ok(vec![]);
    }

    let mut extended_witnesses = vec![];

    let mut agg_factor = 0;
    if let Some(stmt) = output_statement {
        let extended_mask =
            RistrettoExtendedMask::assign(ExtensionDegree::DefaultPedersen, vec![stmt.mask.clone()]).unwrap();
        extended_witnesses.push(RistrettoExtendedWitness {
            mask: extended_mask,
            value: stmt.amount.value() as u64,
            minimum_value_promise: stmt.minimum_value_promise,
        });
        agg_factor += 1;
    }
    if let Some(stmt) = change_statement {
        let extended_mask =
            RistrettoExtendedMask::assign(ExtensionDegree::DefaultPedersen, vec![stmt.mask.clone()]).unwrap();
        extended_witnesses.push(RistrettoExtendedWitness {
            mask: extended_mask,
            value: stmt.amount.value() as u64,
            minimum_value_promise: stmt.minimum_value_promise,
        });
        agg_factor += 1;
    }

    let output_range_proof = get_range_proof_service(agg_factor).construct_extended_proof(extended_witnesses, None)?;
    Ok(output_range_proof)
}

#[cfg(test)]
mod tests {
    use rand::rngs::OsRng;
    use tari_crypto::{keys::SecretKey, ristretto::RistrettoSecretKey};
    use tari_engine_types::confidential::validate_confidential_proof;
    use tari_template_lib::models::Amount;

    use super::*;

    mod confidential_proof {

        use super::*;

        fn create_valid_proof(amount: Amount, minimum_value_promise: u64) -> ConfidentialOutputStatement {
            let mask = RistrettoSecretKey::random(&mut OsRng);
            create_confidential_output_statement(
                Some(&ConfidentialProofStatement {
                    amount,
                    minimum_value_promise,
                    mask,
                    sender_public_nonce: Default::default(),
                    encrypted_data: EncryptedData::try_from(vec![0; EncryptedData::min_size()]).unwrap(),
                    resource_view_key: None,
                }),
                Default::default(),
                None,
                Default::default(),
            )
            .unwrap()
        }

        #[test]
        fn it_is_valid_if_proof_is_valid() {
            let proof = create_valid_proof(100.into(), 0);
            validate_confidential_proof(&proof, None).unwrap();
        }

        #[test]
        fn it_is_invalid_if_minimum_value_changed() {
            let mut proof = create_valid_proof(100.into(), 100);
            proof.output_statement.as_mut().unwrap().minimum_value_promise = 99;
            validate_confidential_proof(&proof, None).unwrap_err();
            proof.output_statement.as_mut().unwrap().minimum_value_promise = 1000;
            validate_confidential_proof(&proof, None).unwrap_err();
        }
    }

    mod encrypt_decrypt {
        use tari_crypto::ristretto::RistrettoSecretKey;

        use super::*;

        #[test]
        fn it_encrypts_and_decrypts() {
            let key = RistrettoSecretKey::random(&mut OsRng);
            let amount = 100;
            let commitment = get_commitment_factory().commit_value(&key, amount);
            let mask = RistrettoSecretKey::random(&mut OsRng);
            let encrypted = encrypt_data(&key, &commitment, amount, &mask).unwrap();

            let val = decrypt_data_and_mask(&key, &commitment, &encrypted).unwrap();
            assert_eq!(val.0, 100);
        }
    }
}
