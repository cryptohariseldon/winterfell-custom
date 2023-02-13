// Copyright (c) Facebook, Inc. and its affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

//! This crate contains Winterfell STARK verifier.
//!
//! This verifier can be used to verify STARK proofs generated by the Winterfell STARK prover.
//!
//! # Usage
//! To verify a proof that a computation was executed correctly, you'll need to do the following:
//!
//! 1. Define an *algebraic intermediate representation* (AIR) for you computation. This can be
//!    done by implementing [Air] trait.
//! 2. Execute [verify()] function and supply the AIR of your computation together with the
//!    [StarkProof] and related public inputs as parameters.
//!
//! # Performance
//! Proof verification is extremely fast and is nearly independent of the complexity of the
//! computation being verified. In vast majority of cases proofs can be verified in 3 - 5 ms
//! on a modern mid-range laptop CPU (using a single core).
//!
//! There is one exception, however: if a computation requires a lot of `sequence` assertions
//! (see [Assertion] for more info), the verification time will grow linearly in the number of
//! asserted values. But for the impact to be noticeable, the number of asserted values would
//! need to be in tens of thousands. And even for hundreds of thousands of asserted values, the
//! verification time should not exceed 50 ms.

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
#[macro_use]
extern crate alloc;

pub use air::{
    proof::StarkProof, Air, AirContext, Assertion, AuxTraceRandElements, BoundaryConstraint,
    BoundaryConstraintGroup, ConstraintCompositionCoefficients, ConstraintDivisor,
    DeepCompositionCoefficients, EvaluationFrame, FieldExtension, ProofOptions, TraceInfo,
    TransitionConstraintDegree, TransitionConstraintGroup,
};

pub use math;
use math::{
    fields::{CubeExtension, QuadExtension},
    FieldElement,
};

use utils::collections::Vec;
pub use utils::{
    ByteReader, ByteWriter, Deserializable, DeserializationError, Serializable, SliceReader,
};

pub use crypto;
use crypto::{ElementHasher, RandomCoin};

use fri::FriVerifier;

mod channel;
use channel::VerifierChannel;

mod evaluator;
use evaluator::evaluate_constraints;

mod composer;
use composer::DeepComposer;

mod errors;
pub use errors::VerifierError;

// VERIFIER
// ================================================================================================
/// Verifies that the specified computation was executed correctly against the specified inputs.
///
/// Specifically, for a computation specified by `AIR` and `HashFn` type parameter, verifies that the provided
/// `proof` attests to the correct execution of the computation against public inputs specified
/// by `pub_inputs`. If the verification is successful, `Ok(())` is returned.
///
/// # Errors
/// Returns an error if combination of the provided proof and public inputs does not attest to
/// a correct execution of the computation. This could happen for many various reasons, including:
/// - The specified proof was generated for a different computation.
/// - The specified proof was generated for this computation but for different public inputs.
#[rustfmt::skip]
pub fn verify<AIR: Air, HashFn: ElementHasher<BaseField = AIR::BaseField>>(
    proof: StarkProof,
    pub_inputs: AIR::PublicInputs,
) -> Result<(), VerifierError> {
    // build a seed for the public coin; the initial seed is the hash of public inputs and proof
    // context, but as the protocol progresses, the coin will be reseeded with the info received
    // from the prover
    let mut public_coin_seed = Vec::new();
    pub_inputs.write_into(&mut public_coin_seed);
    proof.context.write_into(&mut public_coin_seed);

    // create AIR instance for the computation specified in the proof
    let air = AIR::new(proof.get_trace_info(), pub_inputs, proof.options().clone());

    // figure out which version of the generic proof verification procedure to run. this is a sort
    // of static dispatch for selecting two generic parameter: extension field and hash function.
    match air.options().field_extension() {
        FieldExtension::None => {
            let public_coin = RandomCoin::new(&public_coin_seed);
            let channel = VerifierChannel::new(&air, proof)?;
            perform_verification::<AIR, AIR::BaseField, HashFn>(air, channel, public_coin)
        },
        FieldExtension::Quadratic => {
            if !<QuadExtension<AIR::BaseField>>::is_supported() {
                return Err(VerifierError::UnsupportedFieldExtension(2));
            }
            let public_coin = RandomCoin::new(&public_coin_seed);
            let channel = VerifierChannel::new(&air, proof)?;
            perform_verification::<AIR, QuadExtension<AIR::BaseField>, HashFn>(air, channel, public_coin)
        },
        FieldExtension::Cubic => {
            if !<CubeExtension<AIR::BaseField>>::is_supported() {
                return Err(VerifierError::UnsupportedFieldExtension(3));
            }
            let public_coin = RandomCoin::new(&public_coin_seed);
            let channel = VerifierChannel::new(&air, proof)?;
            perform_verification::<AIR, CubeExtension<AIR::BaseField>, HashFn>(air, channel, public_coin)
        },
    }
}

// VERIFICATION PROCEDURE
// ================================================================================================
/// Performs the actual verification by reading the data from the `channel` and making sure it
/// attests to a correct execution of the computation specified by the provided `air`.
fn perform_verification<A, E, H>(
    air: A,
    mut channel: VerifierChannel<E, H>,
    mut public_coin: RandomCoin<A::BaseField, H>,
) -> Result<(), VerifierError>
where
    A: Air,
    E: FieldElement<BaseField = A::BaseField>,
    H: ElementHasher<BaseField = A::BaseField>,
{
    // 1 ----- trace commitment -------------------------------------------------------------------
    // Read the commitments to evaluations of the trace polynomials over the LDE domain sent by the
    // prover. The commitments are used to update the public coin, and draw sets of random elements
    // from the coin (in the interactive version of the protocol the verifier sends these random
    // elements to the prover after each commitment is made). When there are multiple trace
    // commitments (i.e., the trace consists of more than one segment), each previous commitment is
    // used to draw random elements needed to construct the next trace segment. The last trace
    // commitment is used to draw a set of random coefficients which the prover uses to compute
    // constraint composition polynomial.
    let trace_commitments = channel.read_trace_commitments();

    // reseed the coin with the commitment to the main trace segment
    public_coin.reseed(trace_commitments[0]);

    // process auxiliary trace segments (if any), to build a set of random elements for each segment
    let mut aux_trace_rand_elements = AuxTraceRandElements::<E>::new();
    for (i, commitment) in trace_commitments.iter().skip(1).enumerate() {
        let rand_elements = air
            .get_aux_trace_segment_random_elements(i, &mut public_coin)
            .map_err(|_| VerifierError::RandomCoinError)?;
        aux_trace_rand_elements.add_segment_elements(rand_elements);
        public_coin.reseed(*commitment);
    }

    // build random coefficients for the composition polynomial
    let constraint_coeffs = air
        .get_constraint_composition_coefficients(&mut public_coin)
        .map_err(|_| VerifierError::RandomCoinError)?;

    // 2 ----- constraint commitment --------------------------------------------------------------
    // read the commitment to evaluations of the constraint composition polynomial over the LDE
    // domain sent by the prover, use it to update the public coin, and draw an out-of-domain point
    // z from the coin; in the interactive version of the protocol, the verifier sends this point z
    // to the prover, and the prover evaluates trace and constraint composition polynomials at z,
    // and sends the results back to the verifier.
    let constraint_commitment = channel.read_constraint_commitment();
    public_coin.reseed(constraint_commitment);
    let z = public_coin
        .draw::<E>()
        .map_err(|_| VerifierError::RandomCoinError)?;

    // 3 ----- OOD consistency check --------------------------------------------------------------
    // make sure that evaluations obtained by evaluating constraints over the out-of-domain frame
    // are consistent with the evaluations of composition polynomial columns sent by the prover

    // read the out-of-domain trace frames (the main trace frame and auxiliary trace frame, if
    // provided) sent by the prover and evaluate constraints over them; also, reseed the public
    // coin with the OOD frames received from the prover.
    let (ood_main_trace_frame, ood_aux_trace_frame) = channel.read_ood_trace_frame();
    let ood_constraint_evaluation_1 = evaluate_constraints(
        &air,
        constraint_coeffs,
        &ood_main_trace_frame,
        &ood_aux_trace_frame,
        aux_trace_rand_elements,
        z,
    );

    if let Some(ref aux_trace_frame) = ood_aux_trace_frame {
        // when the trace contains auxiliary segments, append auxiliary trace elements at the
        // end of main trace elements for both current and next rows in the frame. this is
        // needed to be consistent with how the prover writes OOD frame into the channel.

        let mut current = ood_main_trace_frame.current().to_vec();
        current.extend_from_slice(aux_trace_frame.current());
        public_coin.reseed(H::hash_elements(&current));

        let mut next = ood_main_trace_frame.next().to_vec();
        next.extend_from_slice(aux_trace_frame.next());
        public_coin.reseed(H::hash_elements(&next));
    } else {
        public_coin.reseed(H::hash_elements(ood_main_trace_frame.current()));
        public_coin.reseed(H::hash_elements(ood_main_trace_frame.next()));
    }

    // read evaluations of composition polynomial columns sent by the prover, and reduce them into
    // a single value by computing sum(z^i * value_i), where value_i is the evaluation of the ith
    // column polynomial at z^m, where m is the total number of column polynomials; also, reseed
    // the public coin with the OOD constraint evaluations received from the prover.
    let ood_constraint_evaluations = channel.read_ood_constraint_evaluations();
    let ood_constraint_evaluation_2 = ood_constraint_evaluations
        .iter()
        .enumerate()
        .fold(E::ZERO, |result, (i, &value)| {
            result + z.exp_vartime((i as u32).into()) * value
        });
    public_coin.reseed(H::hash_elements(&ood_constraint_evaluations));

    // finally, make sure the values are the same
    if ood_constraint_evaluation_1 != ood_constraint_evaluation_2 {
        return Err(VerifierError::InconsistentOodConstraintEvaluations);
    }

    // 4 ----- FRI commitments --------------------------------------------------------------------
    // draw coefficients for computing DEEP composition polynomial from the public coin; in the
    // interactive version of the protocol, the verifier sends these coefficients to the prover
    // and the prover uses them to compute the DEEP composition polynomial. the prover, then
    // applies FRI protocol to the evaluations of the DEEP composition polynomial.
    let deep_coefficients = air
        .get_deep_composition_coefficients::<E, H>(&mut public_coin)
        .map_err(|_| VerifierError::RandomCoinError)?;

    // instantiates a FRI verifier with the FRI layer commitments read from the channel. From the
    // verifier's perspective, this is equivalent to executing the commit phase of the FRI protocol.
    // The verifier uses these commitments to update the public coin and draw random points alpha
    // from them; in the interactive version of the protocol, the verifier sends these alphas to
    // the prover, and the prover uses them to compute and commit to the subsequent FRI layers.

    // READ : REPLACE WITH BYTEMUCK
    let fri_verifier = FriVerifier::new(
        &mut channel,
        &mut public_coin,
        air.options().to_fri_options(),
        air.trace_poly_degree(),
    )
    .map_err(VerifierError::FriVerificationFailed)?;
    // TODO: make sure air.lde_domain_size() == fri_verifier.domain_size()

    // 5 ----- trace and constraint queries -------------------------------------------------------
    // read proof-of-work nonce sent by the prover and update the public coin with it
    let pow_nonce = channel.read_pow_nonce();
    public_coin.reseed_with_int(pow_nonce);

    // make sure the proof-of-work specified by the grinding factor is satisfied
    if public_coin.leading_zeros() < air.options().grinding_factor() {
        return Err(VerifierError::QuerySeedProofOfWorkVerificationFailed);
    }

    // draw pseudo-random query positions for the LDE domain from the public coin; in the
    // interactive version of the protocol, the verifier sends these query positions to the prover,
    // and the prover responds with decommitments against these positions for trace and constraint
    // composition polynomial evaluations.
    let query_positions = public_coin
        .draw_integers(air.options().num_queries(), air.lde_domain_size())
        .map_err(|_| VerifierError::RandomCoinError)?;

    // read evaluations of trace and constraint composition polynomials at the queried positions;
    // this also checks that the read values are valid against trace and constraint commitments
    let (queried_main_trace_states, queried_aux_trace_states) =
        channel.read_queried_trace_states(&query_positions)?;
    let queried_constraint_evaluations = channel.read_constraint_evaluations(&query_positions)?;

    // 6 ----- DEEP composition -------------------------------------------------------------------
    // compute evaluations of the DEEP composition polynomial at the queried positions
    let composer = DeepComposer::new(&air, &query_positions, z, deep_coefficients);
    let t_composition = composer.compose_trace_columns(
        queried_main_trace_states,
        queried_aux_trace_states,
        ood_main_trace_frame,
        ood_aux_trace_frame,
    );
    let c_composition = composer
        .compose_constraint_evaluations(queried_constraint_evaluations, ood_constraint_evaluations);
    let deep_evaluations = composer.combine_compositions(t_composition, c_composition);

    // 7 ----- Verify low-degree proof -------------------------------------------------------------
    // make sure that evaluations of the DEEP composition polynomial we computed in the previous
    // step are in fact evaluations of a polynomial of degree equal to trace polynomial degree
    fri_verifier
        .verify(&mut channel, &deep_evaluations, &query_positions)
        .map_err(VerifierError::FriVerificationFailed)
}
