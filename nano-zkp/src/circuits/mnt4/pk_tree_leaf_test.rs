use ark_mnt6_753::{G1Projective, G2Projective};

#[derive(Clone)]
pub struct PKTreeLeafCircuitTest {
    // Witnesses (private)
    pks: G2Projective,
    pk_tree_nodes: G1Projective,
}

impl PKTreeLeafCircuitTest {
    pub fn new(pks: G2Projective, pk_tree_nodes: G1Projective) -> Self {
        Self { pks, pk_tree_nodes }
    }
}

#[cfg(test)]
mod tests {
    use crate::gadgets::mnt4::SerializeGadget;
    use ark_ec::ProjectiveCurve;
    use ark_mnt4_753::Fr as MNT4Fr;
    use ark_mnt6_753::constraints::{G1Var, G2Var};
    use ark_mnt6_753::{Fr, G1Projective, G2Projective};
    use ark_r1cs_std::prelude::AllocVar;
    use ark_relations::r1cs::ConstraintSystem;
    use ark_std::ops::MulAssign;
    use ark_std::{test_rng, UniformRand};

    #[test]
    fn constraints_pk_leaf() {
        // Initialize the constraint system.
        let cs = ConstraintSystem::<MNT4Fr>::new_ref();

        // Create random number generator.
        let rng = &mut test_rng();

        // Create random points.
        let mut g1_vec = vec![];
        let mut g2_vec = vec![];

        for _ in 0..10 {
            let r = Fr::rand(rng);

            let mut g1 = G1Projective::prime_subgroup_generator();
            g1.mul_assign(r);
            g1_vec.push(g1);

            let mut g2 = G2Projective::prime_subgroup_generator();
            g2.mul_assign(r);
            g2_vec.push(g2);
        }

        // Allocate.
        let mut constraints_prev = 0;

        let g1_var = Vec::<G1Var>::new_witness(cs.clone(), || Ok(&g1_vec[..])).unwrap();
        println!("{}", cs.num_constraints() - constraints_prev);
        constraints_prev = cs.num_constraints();

        let g2_var = Vec::<G2Var>::new_witness(cs.clone(), || Ok(&g2_vec[..])).unwrap();
        println!("{}", cs.num_constraints() - constraints_prev);
        constraints_prev = cs.num_constraints();

        let _bits = SerializeGadget::serialize_g1(cs.clone(), &g1_var[0]).unwrap();
        println!("{}", cs.num_constraints() - constraints_prev);
        constraints_prev = cs.num_constraints();

        let _bits = SerializeGadget::serialize_g2(cs.clone(), &g2_var[0]).unwrap();
        println!("{}", cs.num_constraints() - constraints_prev);

        panic!()
    }
}
