//! Tools for KZG commitment scheme

#[cfg(test)]
mod tests;

extern crate alloc;

use alloc::vec::Vec;
use dusk_bls12_381::{G1Affine, G2Affine, G2Prepared};
pub use dusk_bytes;
use dusk_bytes::{DeserializableSlice, Serializable};
pub use dusk_plonk::commitment_scheme::kzg10::key::{CommitKey, OpeningKey};
pub use dusk_plonk::commitment_scheme::PublicParameters;
pub use dusk_plonk::error::Error;
use dusk_plonk::fft::domain::EvaluationDomain;
use dusk_plonk::fft::evaluations::Evaluations;
pub use dusk_plonk::fft::polynomial::Polynomial;
pub use dusk_plonk::prelude::BlsScalar;

const TEST_PUBLIC_PARAMETERS: &[u8] = include_bytes!("kzg/test-public-parameters.bin");

/// TODO: Test public parameters, must be replaced with proper public parameters later
pub fn test_public_parameters() -> PublicParameters {
    // SAFETY: Bytes were produced with below `test_public_parameters_generate()` and are guaranteed
    // to be correct
    unsafe { PublicParameters::from_slice_unchecked(TEST_PUBLIC_PARAMETERS) }
}

#[cfg(test)]
fn test_public_parameters_generate() -> PublicParameters {
    use rand_core::SeedableRng;

    let mut rng = rand_chacha::ChaChaRng::seed_from_u64(1969897683899915189);
    PublicParameters::setup(256, &mut rng).expect("Static value, doesn't error")
}

#[test]
fn test_public_parameters_correct() {
    assert_eq!(
        test_public_parameters_generate().to_raw_var_bytes(),
        test_public_parameters().to_raw_var_bytes()
    );
}

/// Commitment to polynomial
#[derive(Debug, Copy, Clone)]
pub struct Commitment(G1Affine);

/// Witness for polynomial evaluation
#[derive(Debug, Copy, Clone)]
pub struct Witness(G1Affine);

/// Wrapper data structure for working with KZG commitment scheme
#[derive(Debug, Clone)]
pub struct Kzg {
    public_parameters: PublicParameters,
}

// Most of below implementation and comments are basically taken following code samples (and
// adapted):
// https://github.com/subspace/plonk/blob/65db5f0da6edef54048ddbf4495c6c5b4a664dff/src/commitment_scheme/kzg10/key.rs
// https://github.com/maticnetwork/avail/blob/76e2b45d13975ba87b632f62f29497f279986cbc/kate/src/com.rs
// https://github.com/maticnetwork/avail/blob/76e2b45d13975ba87b632f62f29497f279986cbc/kate/proof/src/lib.rs
impl Kzg {
    /// Create new instance with given public parameters
    pub fn new(public_parameters: PublicParameters) -> Self {
        Self { public_parameters }
    }

    #[cfg(feature = "std")]
    /// For testing purposes only.
    ///
    /// Returns an error if the configured degree is less than one.
    pub fn random(max_degree: u32) -> Result<Self, Error> {
        let public_parameters =
            PublicParameters::setup(max_degree as usize, &mut rand::thread_rng())?;
        Ok(Self { public_parameters })
    }

    // /// Runs a one-time trusted setup of the universal reference values `KZG_PARAMETERS`. The
    // /// initial `seed` for value generation can be provided by a multi-party computation at genesis.
    // fn setup(seed: &[u8]) -> PublicParameters {
    //     todo!()
    // }

    /// Represents data as a `polynomial` needed for the rest of the scheme. The degree of the
    /// polynomial $d$ is equal to the length of data.
    ///
    /// An ordered data set is treated as a set of values as `(x,y) = (w^i, data[i])`, where
    /// `data[i]` are `DATA_CHUNK_SIZE`-byte (currently 31 bytes) chunks, and `w` is a root of unity
    /// of degree $d$, of from which a polynomial that satisfies $p(x)=y$  for all these points is
    /// interpolated. This may be done every time needed using a saved root of unity (one field
    /// element).
    ///
    /// The resulting polynomial is in coefficient form.
    pub fn poly(&self, data: &[u8]) -> Result<Polynomial, Error> {
        let evals = data
            .chunks(BlsScalar::SIZE)
            .map(BlsScalar::from_slice)
            .collect::<Result<Vec<BlsScalar>, dusk_bytes::Error>>()?;
        let domain = EvaluationDomain::new(evals.len())?;
        let evaluations = Evaluations::from_vec_and_domain(evals, domain);
        Ok(evaluations.interpolate())
    }

    /// Computes a `Commitment` to `polynomial`
    pub fn commit(&self, polynomial: &Polynomial) -> Result<Commitment, Error> {
        self.public_parameters
            .commit_key
            .commit(polynomial)
            .map(|commitment| Commitment(commitment.0))
    }

    /// Computes a `Witness` of evaluation of `polynomial` at `index`
    pub fn create_witness(&self, polynomial: &Polynomial, index: u32) -> Result<Witness, Error> {
        let polynomial_degree = polynomial.0.degree();
        assert!((index as usize) <= polynomial_degree);
        // For a given polynomial `p` and a point `z`, compute the witness
        // for p(z) using Ruffini's method for simplicity.
        // The Witness is the quotient of f(x) - f(z) / x-z.
        // However we note that the quotient polynomial is invariant under the value
        // f(z) ie. only the remainder changes. We can therefore compute the
        // witness as f(x) / x - z and only use the remainder term f(z) during
        // verification.

        // Generate all the x-axis points of the domain on which all the row polynomials reside
        let eval_domain = EvaluationDomain::new(polynomial_degree)?;
        let point = eval_domain
            .elements()
            .nth(
                index
                    .try_into()
                    .expect("Always fits into usize on 32-bit+ platforms; qed"),
            )
            .ok_or(Error::MismatchedPolyLen)?;

        // Computes `f(x) / x-z`, returning it as the witness poly
        let witness_poly = polynomial.ruffini(point);

        self.public_parameters
            .commit_key
            .commit(&witness_poly)
            .map(|commitment| Witness(commitment.0))
    }

    /// Verifies that `value` is the evaluation at `index` of the polynomial of `polynomial_degree`
    /// matching the `commitment`
    pub fn verify(
        &self,
        commitment: &Commitment,
        num_values: u32,
        index: u32,
        value: &[u8],
        witness: &Witness,
    ) -> bool {
        // Generate all the x-axis points of the domain on which all the row polynomials reside
        let eval_domain = match EvaluationDomain::new(
            num_values
                .try_into()
                .expect("Always fits into usize on 32-bit+ platforms; qed"),
        ) {
            Ok(eval_domain) => eval_domain,
            Err(_error) => {
                return false;
            }
        };
        let point = eval_domain
            .elements()
            .nth(
                index
                    .try_into()
                    .expect("Always fits into usize on 32-bit+ platforms; qed"),
            )
            // TODO: Remove unwrap
            .unwrap();
        let value = match BlsScalar::from_slice(value) {
            Ok(value) => value,
            Err(_error) => {
                return false;
            }
        };

        // Checks that a polynomial `p` was evaluated at a point `z` and returned
        // the value specified `v`. ie. v = p(z).
        let inner_a: G1Affine =
            (commitment.0 - (self.public_parameters.opening_key.g * value)).into();

        let inner_b: G2Affine = (self.public_parameters.opening_key.beta_h
            - (self.public_parameters.opening_key.h * point))
            .into();
        let prepared_inner_b = G2Prepared::from(-inner_b);

        let pairing = dusk_bls12_381::multi_miller_loop(&[
            (&inner_a, &self.public_parameters.opening_key.prepared_h),
            (&witness.0, &prepared_inner_b),
        ])
        .final_exponentiation();

        pairing == dusk_bls12_381::Gt::identity()
    }
}
