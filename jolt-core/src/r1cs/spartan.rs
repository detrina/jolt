use ark_ec::CurveGroup;
use ark_ff::PrimeField;
use rayon::prelude::*;
use thiserror::Error;

use super::r1cs_shape::R1CSShape;
use crate::{
    poly::{
        dense_mlpoly::DensePolynomial,
        eq_poly::EqPolynomial,
        hyrax::{HyraxCommitment, HyraxGenerators},
    },
    subprotocols::sumcheck::SumcheckInstanceProof,
};

pub struct UniformSpartanKey<F: PrimeField> {
    shape_single_step: R1CSShape<F>, // Single step shape
    shape_full: R1CSShape<F>,        // Single step shape
    num_cons_total: usize,           // Number of constraints
    num_vars_total: usize,           // Number of variables
    num_steps: usize,                // Number of steps
    vk_digest: F,                    // digest of the verifier's key
}

/// A succinct proof of knowledge of a witness to a relaxed R1CS instance
/// The proof is produced using Spartan's combination of the sum-check and
/// the commitment to a vector viewed as a polynomial commitment
pub struct UniformSpartanProof<F: PrimeField, G: CurveGroup<ScalarField = F>> {
    witness_segment_commitments: Vec<HyraxCommitment<1, G>>,
    outer_sumcheck_proof: SumcheckInstanceProof<F>,
    outer_sumcheck_claims: (F, F, F),
    inner_sumcheck_proof: SumcheckInstanceProof<F>,
    eval_W: Vec<F>,   // TODO(arasuarun): better name (claimed_eval_witness_segments?)
    eval_arg: Vec<F>, // TODO(arasuarun): better name
}

pub struct PrecommittedR1CSInstance<F: PrimeField, G: CurveGroup<ScalarField = F>> {
    comm_W: Vec<HyraxCommitment<1, G>>,
    X: Vec<F>,
}

// Trait which will kick out a small and big R1CS shape
pub trait UniformShapeBuilder<F: PrimeField> {
    fn single_step_shape(&self) -> R1CSShape<F>;
    fn full_shape(&self, N: usize, single_step_shape: &R1CSShape<F>) -> R1CSShape<F>;
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum SpartanError {
    /// returned if the supplied row or col in (row,col,val) tuple is out of range
    #[error("InvalidIndex")]
    InvalidIndex,
    /// returned when an invalid sum-check proof is provided
    #[error("InvalidSumcheckProof")]
    InvalidSumcheckProof,
    /// returned if the supplied witness is not of the right length
    #[error("InvalidWitnessLength")]
    InvalidWitnessLength,
}

impl<F: PrimeField, G: CurveGroup<ScalarField = F>> UniformSpartanProof<F, G> {
    #[tracing::instrument(skip_all, name = "SNARK::setup_precommitted")]
    fn setup_precommitted<C: UniformShapeBuilder<F>>(
        circuit: C,
        num_steps: usize,
        generators: HyraxGenerators<1, G>,
    ) -> Result<UniformSpartanKey<F>, SpartanError> {
        let shape_single_step = circuit.single_step_shape();
        let shape_full = circuit.full_shape(num_steps, &shape_single_step);

        let num_constraints_total = shape_single_step.num_cons * num_steps;
        let num_aux_total = shape_single_step.num_vars * num_steps;

        let pad_num_constraints = num_constraints_total.next_power_of_two();
        let pad_num_aux = num_aux_total.next_power_of_two();

        // TODO(sragss / arasuarun): Verifier key digest
        let vk_digest = F::one();

        let key = UniformSpartanKey {
            shape_single_step,
            shape_full,
            num_cons_total: pad_num_constraints,
            num_vars_total: pad_num_aux,
            num_steps,
            vk_digest,
        };
        Ok(key)
    }

    /// produces a succinct proof of satisfiability of a `RelaxedR1CS` instance
    #[tracing::instrument(skip_all, name = "Spartan2::UPSnark::prove")]
    fn prove_precommitted(
        prover_generators: HyraxGenerators<1, G>,
        key: &UniformSpartanKey<F>,
        w_segments: Vec<Vec<F>>,
        witness_commitments: Vec<HyraxCommitment<1, G>>,
    ) -> Result<Self, SpartanError> {
        let witness_segments = w_segments;

        let mut transcript = G::TE::new(b"R1CSSNARK");

        // append the digest of vk (which includes R1CS matrices) and the RelaxedR1CSInstance to the transcript
        transcript.absorb(b"vk", &key.vk_digest);

        let span_u = tracing::span!(tracing::Level::INFO, "absorb_u");
        let _guard_u = span_u.enter();
        transcript.absorb(b"U", &witness_commitments);
        drop(_guard_u);

        // TODO(sragss/arasuarun/moodlezoup): We can do this by reference in prove_quad_batched_unrolled.
        let span = tracing::span!(tracing::Level::INFO, "witness_batching");
        let _guard = span.enter();
        let mut witness = Vec::with_capacity(witness_segments.len() * witness_segments[0].len());
        witness_segments.iter().for_each(|segment| {
            witness.par_extend(segment);
        });
        drop(_guard);

        let span = tracing::span!(tracing::Level::INFO, "witness_resizing");
        let _guard = span.enter();
        witness.resize(key.num_vars_total, F::zero());
        drop(_guard);

        let (num_rounds_x, num_rounds_y) = (
            usize::try_from(key.num_cons_total.ilog2()).unwrap(),
            (usize::try_from(key.num_vars_total.ilog2()).unwrap() + 1),
        );

        // outer sum-check
        let tau = (0..num_rounds_x)
            .map(|_i| transcript.squeeze(b"t"))
            .collect::<Result<Vec<F>, SpartanError>>()?;

        let mut poly_tau = DensePolynomial::new(EqPolynomial::new(tau).evals());
        // poly_Az is the polynomial extended from the vector Az
        let (mut poly_Az, mut poly_Bz, mut poly_Cz) = {
            let (poly_Az, poly_Bz, poly_Cz) =
                key.S
                    .multiply_vec_uniform(&witness, &witness_commitments.X, key.num_steps)?;
            (
                DensePolynomial::new(poly_Az),
                DensePolynomial::new(poly_Bz),
                DensePolynomial::new(poly_Cz),
            )
        };

        let comb_func_outer =
            |poly_A_comp: &F, poly_B_comp: &F, poly_C_comp: &F, poly_D_comp: &F| -> F {
                // Below is an optimized form of: *poly_A_comp * (*poly_B_comp * *poly_C_comp - *poly_D_comp)
                if *poly_B_comp == F::zero() || *poly_C_comp == F::zero() {
                    if *poly_D_comp == F::zero() {
                        F::zero()
                    } else {
                        *poly_A_comp * (-(*poly_D_comp))
                    }
                } else {
                    *poly_A_comp * (*poly_B_comp * *poly_C_comp - *poly_D_comp)
                }
            };

        let (sc_proof_outer, r_x, claims_outer) =
            SumcheckInstanceProof::prove_cubic_with_additive_term(
                &F::zero(), // claim is zero
                num_rounds_x,
                &mut poly_tau,
                &mut poly_Az,
                &mut poly_Bz,
                &mut poly_Cz,
                comb_func_outer,
                &mut transcript,
            )?;
        std::thread::spawn(|| drop(poly_Az));
        std::thread::spawn(|| drop(poly_Bz));
        std::thread::spawn(|| drop(poly_Cz));
        std::thread::spawn(|| drop(poly_tau));

        // claims from the end of sum-check
        // claim_Az is the (scalar) value v_A = \sum_y A(r_x, y) * z(r_x) where r_x is the sumcheck randomness
        let (claim_Az, claim_Bz, claim_Cz): (F, F, F) =
            (claims_outer[1], claims_outer[2], claims_outer[3]);
        transcript.absorb(b"claims_outer", &[claim_Az, claim_Bz, claim_Cz].as_slice());

        // inner sum-check
        let r = transcript.squeeze(b"r")?;
        let claim_inner_joint = claim_Az + r * claim_Bz + r * r * claim_Cz;

        let span = tracing::span!(tracing::Level::TRACE, "poly_ABC");
        let _enter = span.enter();

        // this is the polynomial extended from the vector r_A * A(r_x, y) + r_B * B(r_x, y) + r_C * C(r_x, y) for all y
        let poly_ABC = {
            let num_steps_bits = key.num_steps.trailing_zeros();
            let (rx_con, rx_ts) = r_x.split_at(r_x.len() - num_steps_bits as usize);
            let (eq_rx_con, eq_rx_ts) = rayon::join(
                || EqPolynomial::new(rx_con.to_vec()).evals(),
                || EqPolynomial::new(rx_ts.to_vec()).evals(),
            );

            let n_steps = key.num_steps;

            // With uniformity, each entry of the RLC of A, B, C can be expressed using
            // the RLC of the small_A, small_B, small_C matrices.

            // 1. Evaluate \tilde smallM(r_x, y) for all y
            let compute_eval_table_sparse_single = |small_M: &Vec<(usize, usize, F)>| -> Vec<F> {
                let mut small_M_evals = vec![F::zero(); key.shape_full.num_vars + 1];
                for (row, col, val) in small_M.iter() {
                    small_M_evals[*col] += eq_rx_con[*row] * val;
                }
                small_M_evals
            };

            let (small_A_evals, (small_B_evals, small_C_evals)) = rayon::join(
                || compute_eval_table_sparse_single(&key.S.A),
                || {
                    rayon::join(
                        || compute_eval_table_sparse_single(&key.S.B),
                        || compute_eval_table_sparse_single(&key.S.C),
                    )
                },
            );

            let r_sq = r * r;
            let small_RLC_evals = (0..small_A_evals.len())
                .into_par_iter()
                .map(|i| small_A_evals[i] + small_B_evals[i] * r + small_C_evals[i] * r_sq)
                .collect::<Vec<F>>();

            // 2. Handles all entries but the last one with the constant 1 variable
            let mut RLC_evals: Vec<F> = (0..key.num_vars_total)
                .into_par_iter()
                .map(|col| eq_rx_ts[col % n_steps] * small_RLC_evals[col / n_steps])
                .collect();
            let next_pow_2 = 2 * key.num_vars_total;
            RLC_evals.resize(next_pow_2, F::zero());

            // 3. Handles the constant 1 variable
            let compute_eval_constant_column = |small_M: &Vec<(usize, usize, F)>| -> F {
                let constant_sum: F = small_M.iter()
              .filter(|(_, col, _)| *col == key.S.num_vars)   // expecting ~1
              .map(|(row, _, val)| {
                  let eq_sum = (0..n_steps).into_par_iter().map(|t| eq_rx_ts[t]).sum::<F>();
                  *val * eq_rx_con[*row] * eq_sum
              }).sum();

                constant_sum
            };

            let (constant_term_A, (constant_term_B, constant_term_C)) = rayon::join(
                || compute_eval_constant_column(&key.S.A),
                || {
                    rayon::join(
                        || compute_eval_constant_column(&key.S.B),
                        || compute_eval_constant_column(&key.S.C),
                    )
                },
            );

            RLC_evals[key.num_vars_total] =
                constant_term_A + r * constant_term_B + r * r * constant_term_C;

            RLC_evals
        };
        drop(_enter);
        drop(span);

        let comb_func = |poly_A_comp: &F, poly_B_comp: &F| -> F {
            if *poly_A_comp == F::zero() || *poly_B_comp == F::zero() {
                F::zero()
            } else {
                *poly_A_comp * *poly_B_comp
            }
        };
        let mut poly_ABC = DensePolynomial::new(poly_ABC);
        let (sc_proof_inner, r_y, _claims_inner) = SumcheckInstanceProof::prove_quad_unrolled(
            &claim_inner_joint, // r_A * v_A + r_B * v_B + r_C * v_C
            num_rounds_y,
            &mut poly_ABC, // r_A * A(r_x, y) + r_B * B(r_x, y) + r_C * C(r_x, y) for all y
            &witness,
            &witness_commitments, // TODO(sragss): This is wrong
            comb_func,
            &mut transcript,
        )?;
        std::thread::spawn(|| drop(poly_ABC));

        // The number of prefix bits needed to identify a segment within the witness vector
        // assuming that num_vars_total is a power of 2 and each segment has length num_steps, which is also a power of 2.
        // The +1 is the first element used to separate the inputs and the witness.
        let n_prefix = (key.num_vars_total.trailing_zeros() as usize
            - key.num_steps.trailing_zeros() as usize)
            + 1;
        let r_y_point = &r_y[n_prefix..];

        // Evaluate each segment on r_y_point
        let span = tracing::span!(tracing::Level::TRACE, "evaluate_segments");
        let _enter = span.enter();
        let witness_evals = DensePolynomial::batch_evaluate(&witness_segments, &r_y_point);
        drop(_enter);
        let comm_vec = witness_commitments;

        // now batch these together
        let c = transcript.squeeze(b"c")?;
        todo!("change batching strategy");
        // let w: PolyEvalWitness<G> = PolyEvalWitness::batch(&w.W.as_slice().iter().map(|v| v.as_ref()).collect::<Vec<_>>(), &c);
        // let u: PolyEvalInstance<G> = PolyEvalInstance::batch(&comm_vec, &r_y_point, &witness_evals, &c);

        // TODO(sragss/arasuarun): switch to hyrax
        // let eval_arg = EE::prove(
        //   &pk.ck,
        //   &pk.pk_ee,
        //   &mut transcript,
        //   &u.c,
        //   &w.p,
        //   &r_y_point,
        //   &mut Some(u.e),
        // )?;

        // let compressed_commitments = comm_vec.par_iter().map(|elem| elem.compress()).collect::<Vec<_>>();

        // Ok(UniformSpartanProof{
        //   comm_W: compressed_commitments,
        //   sc_proof_outer,
        //   claims_outer: (claim_Az, claim_Bz, claim_Cz),
        //   sc_proof_inner,
        //   eval_W: witness_evals,
        //   eval_arg,
        // })
    }

    /// verifies a proof of satisfiability of a `RelaxedR1CS` instance
    #[tracing::instrument(skip_all, name = "SNARK::verify")]
    fn verify_precommitted(
        &self,
        key: &UniformSpartanKey<F>,
        io: &[F],
    ) -> Result<(), SpartanError> {
        let N_SEGMENTS = self.witness_segment_commitments.len();

        // construct an instance using the provided commitment to the witness and IO
        // let comm_W_vec = self.witness_segment_commitments.iter()
        //   .map(|c| Commitment::<G>::decompress(c).unwrap())
        //   .collect::<Vec::<<<G as Group>::CE as CommitmentEngineTrait<G>>::Commitment>>();

        assert_eq!(io.len(), 0);
        // let witness_segment_commitments = PrecommittedR1CSInstance::new(&hollow_S, comm_W_vec.clone(), io)?;

        let mut transcript = G::TE::new(b"R1CSSNARK");

        // append the digest of R1CS matrices and the RelaxedR1CSInstance to the transcript
        transcript.absorb(b"vk", &key.digest());
        transcript.absorb(b"U", &self.witness_segment_commitments);

        let (num_rounds_x, num_rounds_y) = (
            usize::try_from(key.num_cons_total.ilog2()).unwrap(),
            (usize::try_from(key.num_vars_total.ilog2()).unwrap() + 1),
        );

        // outer sum-check
        let tau = (0..num_rounds_x)
            .map(|_i| transcript.squeeze(b"t"))
            .collect::<Result<Vec<F>, SpartanError>>()?;

        let (claim_outer_final, r_x) =
            self.outer_sumcheck_proof
                .verify(F::zero(), num_rounds_x, 3, &mut transcript)?;

        // verify claim_outer_final
        let (claim_Az, claim_Bz, claim_Cz) = self.outer_sumcheck_claims;
        let taus_bound_rx = EqPolynomial::new(tau).evaluate(&r_x);
        let claim_outer_final_expected = taus_bound_rx * (claim_Az * claim_Bz - claim_Cz);
        if claim_outer_final != claim_outer_final_expected {
            return Err(SpartanError::InvalidSumcheckProof);
        }

        transcript.absorb(
            b"claims_outer",
            &[
                self.outer_sumcheck_claims.0,
                self.outer_sumcheck_claims.1,
                self.outer_sumcheck_claims.2,
            ]
            .as_slice(),
        );

        // inner sum-check
        let r = transcript.squeeze(b"r")?;
        let claim_inner_joint = self.outer_sumcheck_claims.0
            + r * self.outer_sumcheck_claims.1
            + r * r * self.outer_sumcheck_claims.2;

        let (claim_inner_final, r_y) = self.inner_sumcheck_proof.verify(
            claim_inner_joint,
            num_rounds_y,
            2,
            &mut transcript,
        )?;

        // verify claim_inner_final
        // this should be log (num segments)
        let n_prefix = (key.num_vars_total.trailing_zeros() as usize
            - key.num_steps.trailing_zeros() as usize)
            + 1;

        let eval_Z = {
            let eval_X = {
                // constant term
                let mut poly_X = vec![(0, 1.into())];
                //remaining inputs
                poly_X.extend(
                    (0..self.witness_segment_commitments.len())
                        .map(|i| (i + 1, self.witness_segment_commitments[i]))
                        .collect::<Vec<(usize, F)>>(),
                );
                SparsePolynomial::new(usize::try_from(key.num_vars_total.ilog2()).unwrap(), poly_X)
                    .evaluate(&r_y[1..])
            };

            // evaluate the segments of W
            let r_y_witness = &r_y[1..n_prefix]; // skip the first as it's used to separate the inputs and the witness
            let eval_W = (0..N_SEGMENTS)
                .map(|i| {
                    let bin = format!("{:0width$b}", i, width = n_prefix - 1); // write i in binary using N_PREFIX bits

                    let product = bin.chars().enumerate().fold(F::one(), |acc, (j, bit)| {
                        acc * if bit == '0' {
                            F::one() - r_y_witness[j]
                        } else {
                            r_y_witness[j]
                        }
                    });

                    product * self.eval_W[i]
                })
                .sum::<F>();

            (F::one() - r_y[0]) * eval_W + r_y[0] * eval_X
        };

        // compute evaluations of R1CS matrices
        let multi_evaluate_uniform =
            |M_vec: &[&[(usize, usize, F)]], r_x: &[F], r_y: &[F], num_steps: usize| -> Vec<F> {
                let evaluate_with_table_uniform =
                    |M: &[(usize, usize, F)], T_x: &[F], T_y: &[F], num_steps: usize| -> F {
                        (0..M.len())
                            .into_par_iter()
                            .map(|i| {
                                let (row, col, val) = M[i];
                                (0..num_steps)
                                    .into_par_iter()
                                    .map(|j| {
                                        let row = row * num_steps + j;
                                        let col = if col != key.shape_single_step.num_vars {
                                            col * num_steps + j
                                        } else {
                                            key.num_vars_total
                                        };
                                        let val = val * T_x[row] * T_y[col];
                                        val
                                    })
                                    .sum::<F>()
                            })
                            .sum()
                    };

                let (T_x, T_y) = rayon::join(
                    || EqPolynomial::new(r_x.to_vec()).evals(),
                    || EqPolynomial::new(r_y.to_vec()).evals(),
                );

                (0..M_vec.len())
                    .into_par_iter()
                    .map(|i| evaluate_with_table_uniform(M_vec[i], &T_x, &T_y, num_steps))
                    .collect()
            };

        let evals = multi_evaluate_uniform(
            &[
                &key.shape_single_step.A,
                &key.shape_single_step.B,
                &key.shape_single_step.C,
            ],
            &r_x,
            &r_y,
            key.num_steps,
        );

        let claim_inner_final_expected = (evals[0] + r * evals[1] + r * r * evals[2]) * eval_Z;
        if claim_inner_final != claim_inner_final_expected {
            // DEDUPE(arasuarun): add
            return Err(SpartanError::InvalidSumcheckProof);
        }

        // we now combine evaluation claims at the same point rz into one
        let comm_vec = self.witness_segment_commitments;
        let eval_vec = &self.eval_W;

        let r_y_point = &r_y[n_prefix..];
        let c = transcript.squeeze(b"c")?;
        todo!("Fix batching strategy")
        // let u: PolyEvalInstance<G> = PolyEvalInstance::batch(&comm_vec, &r_y_point, &eval_vec, &c);

        // verify
        // EE::verify(
        //   &vk.vk_ee,
        //   &mut transcript,
        //   &u.c,
        //   &r_y_point,
        //   &u.e,
        //   &self.eval_arg,
        // )?;

        // Ok(())
    }
}
