//! Test ChainStore implementation of Store, in particular, how
//! the chain head pointer gets updated in various situations

use futures::future::IntoFuture;
use std::fmt::Debug;
use std::sync::Arc;

use graph::prelude::anyhow;
use graph::{
    components::store::BlockStore as _,
    prelude::{anyhow::Error, Future01CompatExt, SubgraphDeploymentId},
};
use graph::{components::store::ChainStore as _, prelude::QueryStoreManager};
use graph_store_postgres::Store as DieselStore;
use graph_store_postgres::{layout_for_tests::FAKE_NETWORK_SHARED, ChainStore as DieselChainStore};

use test_store::block_store::{
    Chain, FakeBlock, BLOCK_FIVE, BLOCK_FOUR, BLOCK_ONE, BLOCK_ONE_NO_PARENT, BLOCK_ONE_SIBLING,
    BLOCK_THREE, BLOCK_THREE_NO_PARENT, BLOCK_TWO, BLOCK_TWO_NO_PARENT, GENESIS_BLOCK, NO_PARENT,
};
use test_store::*;

// The ancestor count we use for chain head updates. We keep this very small
// to make setting up the tests easier
const ANCESTOR_COUNT: u64 = 3;

/// Test harness for running database integration tests.
fn run_test<R, F>(chain: Chain, test: F)
where
    F: Fn(Arc<DieselChainStore>, Arc<DieselStore>) -> R + Send + 'static,
    R: IntoFuture<Item = ()> + Send + 'static,
    R::Error: Send + Debug,
    R::Future: Send,
{
    run_test_sequentially(
        || (),
        |store, ()| async move {
            for name in vec![NETWORK_NAME, FAKE_NETWORK_SHARED] {
                block_store::set_chain(chain.clone(), name);

                let chain_store = store.block_store().chain_store(name).expect("chain store");

                // Run test
                let result = test(chain_store, store.clone());
                result
                    .into_future()
                    .compat()
                    .await
                    .expect(&format!("test finishes successfully on network {}", name));
            }
        },
    );
}

/// Check that `attempt_chain_head_update` works as expected on the given
/// chain. After writing the blocks in `chain` to the store, call
/// `attempt_chain_head_update` and check its result. Check that the new head
/// is the one indicated in `head_exp`. If `missing` is not `None`, check that
/// `attempt_chain_head_update` reports that block as missing
fn check_chain_head_update(
    chain: Chain,
    head_exp: Option<&'static FakeBlock>,
    missing: Option<&'static str>,
) {
    run_test(chain, move |store, _| -> Result<(), ()> {
        let missing_act: Vec<_> = store
            .attempt_chain_head_update(ANCESTOR_COUNT)
            .expect("attempt_chain_head_update failed")
            .iter()
            .map(|h| format!("{:x}", h))
            .collect();
        let missing_exp: Vec<_> = missing.into_iter().collect();
        assert_eq!(missing_exp, missing_act);

        let head_hash_exp = head_exp.map(|block| block.hash.clone());
        let head_hash_act = store
            .chain_head_ptr()
            .expect("chain_head_ptr failed")
            .map(|ebp| format!("{:x}", ebp.hash));
        assert_eq!(head_hash_exp, head_hash_act);
        Ok(())
    })
}

#[test]
fn genesis_only() {
    check_chain_head_update(vec![&*GENESIS_BLOCK], Some(&GENESIS_BLOCK), None);
}

#[test]
fn genesis_plus_one() {
    check_chain_head_update(vec![&*GENESIS_BLOCK, &*BLOCK_ONE], Some(&BLOCK_ONE), None);
}

#[test]
fn genesis_plus_two() {
    check_chain_head_update(
        vec![&*GENESIS_BLOCK, &*BLOCK_ONE, &*BLOCK_TWO],
        Some(&*BLOCK_TWO),
        None,
    );
}

#[test]
fn genesis_plus_one_with_sibling() {
    // Two valid blocks at the same height should give an error, but
    // we currently get one of them at random
    let chain = vec![&*GENESIS_BLOCK, &*BLOCK_ONE, &*BLOCK_ONE_SIBLING];
    check_chain_head_update(chain, Some(&*BLOCK_ONE), None);
}

#[test]
fn short_chain_missing_parent() {
    let chain = vec![&*BLOCK_ONE_NO_PARENT];
    check_chain_head_update(chain, None, Some(&NO_PARENT));
}

#[test]
fn long_chain() {
    let chain = vec![
        &*BLOCK_ONE,
        &*BLOCK_TWO,
        &*BLOCK_THREE,
        &*BLOCK_FOUR,
        &*BLOCK_FIVE,
    ];
    check_chain_head_update(chain, Some(&*BLOCK_FIVE), None);
}

#[test]
fn long_chain_missing_blocks_within_ancestor_count() {
    // BLOCK_THREE does not have a parent in the store
    let chain = vec![&*BLOCK_THREE, &*BLOCK_FOUR, &*BLOCK_FIVE];
    check_chain_head_update(chain, None, Some(&BLOCK_THREE.parent_hash));
}

#[test]
fn long_chain_missing_blocks_beyond_ancestor_count() {
    // We don't mind missing blocks ANCESTOR_COUNT many blocks out, in
    // this case BLOCK_ONE
    let chain = vec![&*BLOCK_TWO, &*BLOCK_THREE, &*BLOCK_FOUR, &*BLOCK_FIVE];
    check_chain_head_update(chain, Some(&*BLOCK_FIVE), None);
}

#[test]
fn long_chain_with_uncles() {
    let chain = vec![
        &*BLOCK_ONE,
        &*BLOCK_TWO,
        &*BLOCK_TWO_NO_PARENT,
        &*BLOCK_THREE,
        &*BLOCK_THREE_NO_PARENT,
        &*BLOCK_FOUR,
    ];
    check_chain_head_update(chain, Some(&*BLOCK_FOUR), None);
}

#[test]
fn block_number() {
    let chain = vec![&*GENESIS_BLOCK, &*BLOCK_ONE, &*BLOCK_TWO];
    let subgraph = SubgraphDeploymentId::new("nonExistentSubgraph").unwrap();

    run_test(chain, move |_, subgraph_store| -> Result<(), ()> {
        create_test_subgraph(&subgraph, "type Dummy @entity { id: ID! }");

        let query_store = subgraph_store
            .query_store(subgraph.clone().into(), false)
            .unwrap();
        let block = query_store
            .block_number(GENESIS_BLOCK.block_hash())
            .expect("Found genesis block");
        assert_eq!(Some(0), block);

        let block = query_store
            .block_number(BLOCK_ONE.block_hash())
            .expect("Found block 1");
        assert_eq!(Some(1), block);

        let block = query_store
            .block_number(BLOCK_THREE.block_hash())
            .expect("Looked for block 3");
        assert!(block.is_none());

        Ok(())
    })
}

#[test]
fn block_hashes_by_number() {
    let chain = vec![
        &*GENESIS_BLOCK,
        &*BLOCK_ONE,
        &*BLOCK_TWO,
        &*BLOCK_TWO_NO_PARENT,
    ];
    run_test(chain, move |store, _| -> Result<(), ()> {
        let hashes = store.block_hashes_by_block_number(1).unwrap();
        assert_eq!(vec![BLOCK_ONE.block_hash()], hashes);

        let hashes = store.block_hashes_by_block_number(2).unwrap();
        assert_eq!(2, hashes.len());
        assert!(hashes.contains(&BLOCK_TWO.block_hash()));
        assert!(hashes.contains(&BLOCK_TWO_NO_PARENT.block_hash()));

        let hashes = store.block_hashes_by_block_number(127).unwrap();
        assert_eq!(0, hashes.len());

        let deleted = store
            .confirm_block_hash(1, &BLOCK_ONE.block_hash())
            .unwrap();
        assert_eq!(0, deleted);

        let deleted = store
            .confirm_block_hash(2, &BLOCK_TWO.block_hash())
            .unwrap();
        assert_eq!(1, deleted);

        // Make sure that we do not delete anything for a nonexistent block
        let deleted = store
            .confirm_block_hash(127, &GENESIS_BLOCK.block_hash())
            .unwrap();
        assert_eq!(0, deleted);

        let hashes = store.block_hashes_by_block_number(1).unwrap();
        assert_eq!(vec![BLOCK_ONE.block_hash()], hashes);

        let hashes = store.block_hashes_by_block_number(2).unwrap();
        assert_eq!(vec![BLOCK_TWO.block_hash()], hashes);
        Ok(())
    })
}

#[track_caller]
fn check_ancestor(
    store: &Arc<DieselChainStore>,
    child: &FakeBlock,
    offset: u64,
    exp: &FakeBlock,
) -> Result<(), Error> {
    let act = store
        .ancestor_block(child.block_ptr(), offset)?
        .ok_or_else(|| anyhow!("block {} has no ancestor at offset {}", child.hash, offset))?;
    let act_hash = format!("{:x}", act.block.hash.unwrap());
    let exp_hash = &exp.hash;

    if &act_hash != exp_hash {
        Err(anyhow!(
            "expected hash `{}` but got `{}`",
            exp_hash,
            act_hash
        ))
    } else {
        Ok(())
    }
}

#[test]
fn ancestor_block_simple() {
    let chain = vec![
        &*GENESIS_BLOCK,
        &*BLOCK_ONE,
        &*BLOCK_TWO,
        &*BLOCK_THREE,
        &*BLOCK_FOUR,
        &*BLOCK_FIVE,
    ];

    run_test(chain, move |store, _| -> Result<(), Error> {
        check_ancestor(&store, &*BLOCK_FIVE, 1, &*BLOCK_FOUR)?;
        check_ancestor(&store, &*BLOCK_FIVE, 2, &*BLOCK_THREE)?;
        check_ancestor(&store, &*BLOCK_FIVE, 3, &*BLOCK_TWO)?;
        check_ancestor(&store, &*BLOCK_FIVE, 4, &*BLOCK_ONE)?;
        check_ancestor(&store, &*BLOCK_FIVE, 5, &*GENESIS_BLOCK)?;
        check_ancestor(&store, &*BLOCK_THREE, 2, &*BLOCK_ONE)?;

        for offset in [6, 7, 8, 50].iter() {
            let offset = offset.clone() as u64;
            let res = store.ancestor_block(BLOCK_FIVE.block_ptr(), offset);
            assert!(res.is_err());
        }

        let block = store.ancestor_block(BLOCK_TWO_NO_PARENT.block_ptr(), 1)?;
        assert!(block.is_none());
        Ok(())
    });
}

#[test]
fn ancestor_block_ommers() {
    let chain = vec![
        &*GENESIS_BLOCK,
        &*BLOCK_ONE,
        &*BLOCK_ONE_SIBLING,
        &*BLOCK_TWO,
    ];

    run_test(chain, move |store, _| -> Result<(), Error> {
        check_ancestor(&store, &*BLOCK_ONE, 1, &*GENESIS_BLOCK)?;
        check_ancestor(&store, &*BLOCK_ONE_SIBLING, 1, &*GENESIS_BLOCK)?;
        check_ancestor(&store, &*BLOCK_TWO, 1, &*BLOCK_ONE)?;
        check_ancestor(&store, &*BLOCK_TWO, 2, &*GENESIS_BLOCK)?;
        Ok(())
    });
}
