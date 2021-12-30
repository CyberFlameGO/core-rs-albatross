use ark_crypto_primitives::{CircuitSpecificSetupSNARK, SNARK};
use ark_ec::ProjectiveCurve;
use ark_groth16::Groth16;
use ark_mnt4_753::{Fr as MNT4Fr, MNT4_753};
use ark_mnt6_753::constraints::{G1Var, G2Var};
use ark_mnt6_753::{Fr, G1Projective, G2Projective};
use ark_r1cs_std::prelude::AllocVar;
use ark_relations::r1cs::{ConstraintSynthesizer, ConstraintSystemRef, SynthesisError};
use ark_std::{test_rng, UniformRand};
use std::ops::MulAssign;
use std::time::Instant;

const NUMBER_OF_KEYS: usize = 1;

#[derive(Clone)]
pub struct G1Circuit {
    // Witnesses (private)
    witnesses: Vec<G1Projective>,
}

impl ConstraintSynthesizer<MNT4Fr> for G1Circuit {
    /// This function generates the constraints for the circuit.
    fn generate_constraints(self, cs: ConstraintSystemRef<MNT4Fr>) -> Result<(), SynthesisError> {
        // Allocate all the witnesses.
        let _var = Vec::<G1Var>::new_witness(cs.clone(), || Ok(&self.witnesses[..]))?;

        Ok(())
    }
}

#[derive(Clone)]
pub struct G2Circuit {
    // Witnesses (private)
    witnesses: Vec<G2Projective>,
}

impl ConstraintSynthesizer<MNT4Fr> for G2Circuit {
    /// This function generates the constraints for the circuit.
    fn generate_constraints(self, cs: ConstraintSystemRef<MNT4Fr>) -> Result<(), SynthesisError> {
        // Allocate all the witnesses.
        let _var = Vec::<G2Var>::new_witness(cs.clone(), || Ok(&self.witnesses[..]))?;

        Ok(())
    }
}

#[test]
fn thingy() {
    // Create random number generator.
    let rng = &mut test_rng();

    // Create random points.
    let mut g1_vec = vec![];
    let mut g2_vec = vec![];

    for _ in 0..NUMBER_OF_KEYS {
        let r = Fr::rand(rng);

        let mut g1 = G1Projective::prime_subgroup_generator();
        g1.mul_assign(r);
        g1_vec.push(g1);

        let mut g2 = G2Projective::prime_subgroup_generator();
        g2.mul_assign(r);
        g2_vec.push(g2);
    }

    // Circuit 1.
    let circuit_1 = G1Circuit {
        witnesses: g1_vec.clone(),
    };

    let (pk_1, _) = Groth16::<MNT4_753>::setup(circuit_1.clone(), rng).unwrap();

    let start = Instant::now();
    Groth16::<MNT4_753>::prove(&pk_1, circuit_1, rng).unwrap();
    println!("G1 circuit: {:?}", start.elapsed());

    // Circuit 2
    let circuit_2 = G2Circuit {
        witnesses: g2_vec.clone(),
    };

    let (pk_2, _) = Groth16::<MNT4_753>::setup(circuit_2.clone(), rng).unwrap();

    let start = Instant::now();
    Groth16::<MNT4_753>::prove(&pk_2, circuit_2, rng).unwrap();
    println!("G2 circuit: {:?}", start.elapsed());

    panic!()
}
