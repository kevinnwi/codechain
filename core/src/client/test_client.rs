// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

// Copyright 2018-2019 Kodebox, Inc.
// This file is part of CodeChain.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use std::collections::HashMap;
use std::mem;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrder};
use std::sync::Arc;

use ckey::{public_to_address, Address, Generator, NetworkId, PlatformAddress, Random};
use cmerkle::skewed_merkle_root;
use cnetwork::NodeId;
use cstate::{FindActionHandler, StateDB};
use ctimer::{TimeoutHandler, TimerToken};
use ctypes::transaction::{Action, Transaction};
use ctypes::BlockNumber;
use cvm::ChainTimeInfo;
use journaldb;
use kvdb::KeyValueDB;
use kvdb_memorydb;
use parking_lot::RwLock;
use primitives::{Bytes, H256, U256};
use rlp::*;

use crate::block::{ClosedBlock, OpenBlock, SealedBlock};
use crate::blockchain_info::BlockChainInfo;
use crate::client::ImportResult;
use crate::client::{
    AccountData, Balance, BlockChain, BlockChainClient, BlockInfo, BlockProducer, BlockStatus, ChainInfo, ImportBlock,
    ImportSealedBlock, MiningBlockChainClient, PrepareOpenBlock, RegularKeyOwner, ReopenBlock, ResealTimer, Seq,
    StateOrBlock, TransactionInfo,
};
use crate::db::{COL_STATE, NUM_COLUMNS};
use crate::encoded;
use crate::error::BlockImportError;
use crate::header::Header as BlockHeader;
use crate::miner::{Miner, MinerService, TransactionImportResult};
use crate::scheme::Scheme;
use crate::transaction::{LocalizedTransaction, PendingSignedTransactions, SignedTransaction};
use crate::types::{BlockId, TransactionId, VerificationQueueInfo as QueueInfo};

/// Test client.
pub struct TestBlockChainClient {
    /// Blocks.
    pub blocks: RwLock<HashMap<H256, Bytes>>,
    /// Mapping of numbers to hashes.
    pub numbers: RwLock<HashMap<usize, H256>>,
    /// Genesis block hash.
    pub genesis_hash: H256,
    /// Last block hash.
    pub last_hash: RwLock<H256>,
    /// Last transactions_root
    pub last_transactions_root: RwLock<H256>,
    /// Extra data do set for each block
    pub extra_data: Bytes,
    /// Score.
    pub score: RwLock<U256>,
    /// Balances.
    pub balances: RwLock<HashMap<Address, u64>>,
    /// Seqs.
    pub seqs: RwLock<HashMap<Address, u64>>,
    /// Storage.
    pub storage: RwLock<HashMap<(Address, H256), H256>>,
    /// Block queue size.
    pub queue_size: AtomicUsize,
    /// Miner
    pub miner: Arc<Miner>,
    /// Scheme
    pub scheme: Scheme,
    /// Timestamp assigned to latest sealed block
    pub latest_block_timestamp: RwLock<u64>,
    /// Pruning history size to report.
    pub history: RwLock<Option<u64>>,
}

impl Default for TestBlockChainClient {
    fn default() -> Self {
        TestBlockChainClient::new()
    }
}

impl TestBlockChainClient {
    /// Creates new test client.
    pub fn new() -> Self {
        Self::new_with_extra_data(Bytes::new())
    }

    /// Creates new test client with specified extra data for each block
    pub fn new_with_extra_data(extra_data: Bytes) -> Self {
        let db = Arc::new(kvdb_memorydb::create(NUM_COLUMNS.unwrap()));
        let scheme = Scheme::new_test();
        TestBlockChainClient::new_with_scheme_and_extra(scheme, extra_data, db)
    }

    /// Create test client with custom scheme.
    pub fn new_with_scheme(scheme: Scheme) -> Self {
        let db = Arc::new(kvdb_memorydb::create(NUM_COLUMNS.unwrap()));
        TestBlockChainClient::new_with_scheme_and_extra(scheme, Bytes::new(), db)
    }

    /// Create test client with custom scheme and extra data.
    pub fn new_with_scheme_and_extra(scheme: Scheme, extra_data: Bytes, db: Arc<KeyValueDB>) -> Self {
        let genesis_block = scheme.genesis_block();
        let genesis_header = scheme.genesis_header();
        let genesis_hash = genesis_header.hash();
        let genesis_transactions_root = *genesis_header.transactions_root();
        let genesis_score = *genesis_header.score();

        let mut client = TestBlockChainClient {
            blocks: RwLock::new(HashMap::new()),
            numbers: RwLock::new(HashMap::new()),
            genesis_hash,
            extra_data,
            last_hash: RwLock::new(genesis_hash),
            last_transactions_root: RwLock::new(genesis_transactions_root),
            score: RwLock::new(genesis_score),
            balances: RwLock::new(HashMap::new()),
            seqs: RwLock::new(HashMap::new()),
            storage: RwLock::new(HashMap::new()),
            queue_size: AtomicUsize::new(0),
            miner: Arc::new(Miner::with_scheme(&scheme, db)),
            scheme,
            latest_block_timestamp: RwLock::new(10_000_000),
            history: RwLock::new(None),
        };

        // insert genesis hash.
        client.blocks.get_mut().insert(genesis_hash, genesis_block);
        client.numbers.get_mut().insert(0, genesis_hash);
        client
    }

    /// Set the balance of account `address` to `balance`.
    pub fn set_balance(&self, address: Address, balance: u64) {
        self.balances.write().insert(address, balance);
    }

    /// Set seq of account `address` to `seq`.
    pub fn set_seq(&self, address: Address, seq: u64) {
        self.seqs.write().insert(address, seq);
    }

    /// Set storage `position` to `value` for account `address`.
    pub fn set_storage(&self, address: Address, position: H256, value: H256) {
        self.storage.write().insert((address, position), value);
    }

    /// Set block queue size for testing
    pub fn set_queue_size(&self, size: usize) {
        self.queue_size.store(size, AtomicOrder::Relaxed);
    }

    /// Set timestamp assigned to latest sealed block
    pub fn set_latest_block_timestamp(&self, ts: u64) {
        *self.latest_block_timestamp.write() = ts;
    }

    /// Add blocks to test client.
    pub fn add_blocks(&self, count: usize, transaction_length: usize) {
        let len = self.numbers.read().len();
        for n in len..(len + count) {
            self.add_block_with_author(None, n, transaction_length);
        }
    }
    /// Add a block to test client with designated author.
    pub fn add_block_with_author(&self, author: Option<Address>, n: usize, transaction_length: usize) -> H256 {
        let mut header = BlockHeader::new();
        header.set_score(From::from(n));
        header.set_parent_hash(*self.last_hash.read());
        header.set_number(n as BlockNumber);
        header.set_extra_data(self.extra_data.clone());
        if let Some(addr) = author {
            header.set_author(addr);
        }
        let mut transactions = Vec::with_capacity(transaction_length);
        for _ in 0..transaction_length {
            let keypair = Random.generate().unwrap();
            // Update seqs value
            self.seqs.write().insert(keypair.address(), 0);
            let tx = Transaction {
                seq: 0,
                fee: 10,
                network_id: NetworkId::default(),
                action: Action::Pay {
                    receiver: Address::random(),
                    quantity: 0,
                },
            };
            let signed = SignedTransaction::new_with_sign(tx, keypair.private());
            transactions.push(signed);
        }
        header.set_transactions_root(skewed_merkle_root(
            *self.last_transactions_root.read(),
            transactions.iter().map(Encodable::rlp_bytes),
        ));
        let mut rlp = RlpStream::new_list(2);
        rlp.append(&header);
        rlp.append_list(&transactions);
        self.import_block(rlp.as_raw().to_vec()).unwrap()
    }

    /// Make a bad block by setting invalid extra data.
    pub fn corrupt_block(&self, n: BlockNumber) {
        let block_id = n.into();
        let hash = self.block_hash(&block_id).unwrap();
        let mut header: BlockHeader = self.block_header(&block_id).unwrap().decode();
        header.set_extra_data(b"This extra data is way too long to be considered valid".to_vec());
        let mut rlp = RlpStream::new_list(3);
        rlp.append(&header);
        rlp.append_raw(&::rlp::NULL_RLP, 1);
        rlp.append_raw(&::rlp::NULL_RLP, 1);
        self.blocks.write().insert(hash, rlp.out());
    }

    /// Make a bad block by setting invalid parent hash.
    pub fn corrupt_block_parent(&self, n: BlockNumber) {
        let block_id = n.into();
        let hash = self.block_hash(&block_id).unwrap();
        let mut header: BlockHeader = self.block_header(&block_id).unwrap().decode();
        header.set_parent_hash(H256::from(42));
        let mut rlp = RlpStream::new_list(3);
        rlp.append(&header);
        rlp.append_raw(&::rlp::NULL_RLP, 1);
        rlp.append_raw(&::rlp::NULL_RLP, 1);
        self.blocks.write().insert(hash, rlp.out());
    }

    /// TODO:
    pub fn block_hash_delta_minus(&mut self, delta: usize) -> H256 {
        let blocks_read = self.numbers.read();
        let index = blocks_read.len() - delta;
        blocks_read[&index]
    }

    fn block_hash(&self, id: &BlockId) -> Option<H256> {
        match id {
            BlockId::Hash(hash) => Some(*hash),
            BlockId::Number(n) => self.numbers.read().get(&(*n as usize)).cloned(),
            BlockId::Earliest => self.numbers.read().get(&0).cloned(),
            BlockId::Latest => self.numbers.read().get(&(self.numbers.read().len() - 1)).cloned(),
        }
    }

    /// Inserts a transaction to miners mem pool.
    pub fn insert_transaction_to_pool(&self) -> H256 {
        let keypair = Random.generate().unwrap();
        let tx = Transaction {
            seq: 0,
            fee: 10,
            network_id: NetworkId::default(),
            action: Action::Pay {
                receiver: Address::random(),
                quantity: 0,
            },
        };
        let signed = SignedTransaction::new_with_sign(tx, keypair.private());
        let sender_address = public_to_address(&signed.signer_public());
        self.set_balance(sender_address, 10_000_000_000_000_000_000);
        let hash = signed.hash();
        let res = self.miner.import_external_transactions(self, vec![signed.into()]);
        let res = res.into_iter().next().unwrap().expect("Successful import");
        assert_eq!(res, TransactionImportResult::Current);
        hash
    }

    /// Set reported history size.
    pub fn set_history(&self, h: Option<u64>) {
        *self.history.write() = h;
    }
}

pub fn get_temp_state_db() -> StateDB {
    let db = kvdb_memorydb::create(NUM_COLUMNS.unwrap_or(0));
    let journal_db = journaldb::new(Arc::new(db), journaldb::Algorithm::Archive, COL_STATE);
    StateDB::new(journal_db)
}

impl ReopenBlock for TestBlockChainClient {
    fn reopen_block(&self, block: ClosedBlock) -> OpenBlock {
        block.reopen(&*self.scheme.engine)
    }
}

impl PrepareOpenBlock for TestBlockChainClient {
    fn prepare_open_block(&self, _parent_block: BlockId, author: Address, extra_data: Bytes) -> OpenBlock {
        let engine = &*self.scheme.engine;
        let genesis_header = self.scheme.genesis_header();
        let db = get_temp_state_db();

        let mut open_block = OpenBlock::try_new(engine, db, &genesis_header, author, extra_data, false)
            .expect("Opening block for tests will not fail.");
        // TODO [todr] Override timestamp for predictability (set_timestamp_now kind of sucks)
        open_block.set_timestamp(*self.latest_block_timestamp.read());
        open_block
    }
}

impl ImportSealedBlock for TestBlockChainClient {
    fn import_sealed_block(&self, _block: &SealedBlock) -> ImportResult {
        Ok(H256::default())
    }
}

impl BlockProducer for TestBlockChainClient {}

impl MiningBlockChainClient for TestBlockChainClient {}

impl Seq for TestBlockChainClient {
    fn seq(&self, address: &Address, id: BlockId) -> Option<u64> {
        match id {
            BlockId::Latest => Some(self.seqs.read().get(address).cloned().unwrap_or(0)),
            _ => None,
        }
    }

    fn latest_seq(&self, address: &Address) -> u64 {
        self.seq(address, BlockId::Latest).unwrap()
    }
}

impl Balance for TestBlockChainClient {
    fn balance(&self, address: &Address, state: StateOrBlock) -> Option<u64> {
        match state {
            StateOrBlock::Block(BlockId::Latest) | StateOrBlock::State(_) => {
                Some(self.balances.read().get(address).cloned().unwrap_or(0))
            }
            _ => None,
        }
    }

    fn latest_balance(&self, address: &Address) -> u64 {
        self.balance(address, BlockId::Latest.into()).unwrap()
    }
}

impl AccountData for TestBlockChainClient {}

impl RegularKeyOwner for TestBlockChainClient {
    fn regular_key_owner(&self, _address: &Address, _state: StateOrBlock) -> Option<Address> {
        Some(Address::default())
    }
}

impl ChainInfo for TestBlockChainClient {
    fn chain_info(&self) -> BlockChainInfo {
        let number = self.blocks.read().len() as BlockNumber - 1;
        BlockChainInfo {
            best_score: *self.score.read(),
            best_proposal_score: *self.score.read(),
            pending_total_score: *self.score.read(),
            genesis_hash: self.genesis_hash,
            best_block_hash: *self.last_hash.read(),
            best_proposal_block_hash: *self.last_hash.read(),
            best_block_number: number,
            best_block_timestamp: number,
        }
    }

    fn genesis_accounts(&self) -> Vec<PlatformAddress> {
        unimplemented!()
    }
}

impl BlockInfo for TestBlockChainClient {
    fn block_header(&self, id: &BlockId) -> Option<encoded::Header> {
        self.block_hash(id)
            .and_then(|hash| self.blocks.read().get(&hash).map(|r| Rlp::new(r).at(0).as_raw().to_vec()))
            .map(encoded::Header::new)
    }

    fn best_block_header(&self) -> encoded::Header {
        self.block_header(&self.chain_info().best_block_hash.into()).expect("Best block always has header.")
    }

    fn best_header(&self) -> encoded::Header {
        unimplemented!()
    }

    fn best_proposal_header(&self) -> encoded::Header {
        unimplemented!()
    }

    fn block(&self, id: &BlockId) -> Option<encoded::Block> {
        self.block_hash(id).and_then(|hash| self.blocks.read().get(&hash).cloned()).map(encoded::Block::new)
    }
}

impl TransactionInfo for TestBlockChainClient {
    fn transaction_block(&self, _id: &TransactionId) -> Option<H256> {
        None // Simple default.
    }

    fn transaction_header(&self, _tracker: &H256) -> Option<::encoded::Header> {
        None
    }
}

impl BlockChain for TestBlockChainClient {}

impl ImportBlock for TestBlockChainClient {
    fn import_block(&self, b: Bytes) -> Result<H256, BlockImportError> {
        let header = Rlp::new(&b).val_at::<BlockHeader>(0);
        let h = header.hash();
        let number: usize = header.number() as usize;
        if number > self.blocks.read().len() {
            panic!("Unexpected block number. Expected {}, got {}", self.blocks.read().len(), number);
        }
        if number > 0 {
            let blocks = self.blocks.read();
            let parent = blocks
                .get(header.parent_hash())
                .unwrap_or_else(|| panic!("Unknown block parent {:?} for block {}", header.parent_hash(), number));
            let parent = Rlp::new(parent).val_at::<BlockHeader>(0);
            assert_eq!(parent.number(), header.number() - 1, "Unexpected block parent");
        }
        let len = self.numbers.read().len();
        if number == len {
            {
                let mut score = self.score.write();
                *score += *header.score();
            }
            mem::replace(&mut *self.last_hash.write(), h);
            mem::replace(&mut *self.last_transactions_root.write(), h);
            self.blocks.write().insert(h, b);
            self.numbers.write().insert(number, h);
            let mut parent_hash = *header.parent_hash();
            if number > 0 {
                let mut n = number - 1;
                while n > 0 && self.numbers.read()[&n] != parent_hash {
                    *self.numbers.write().get_mut(&n).unwrap() = parent_hash;
                    n -= 1;
                    parent_hash = *Rlp::new(&self.blocks.read()[&parent_hash]).val_at::<BlockHeader>(0).parent_hash();
                }
            }
        } else {
            self.blocks.write().insert(h, b.to_vec());
        }
        Ok(h)
    }

    fn import_header(&self, _bytes: Bytes) -> Result<H256, BlockImportError> {
        unimplemented!()
    }
}

impl BlockChainClient for TestBlockChainClient {
    fn queue_info(&self) -> QueueInfo {
        QueueInfo {
            verified_queue_size: self.queue_size.load(AtomicOrder::Relaxed),
            unverified_queue_size: 0,
            verifying_queue_size: 0,
            max_queue_size: 0,
            max_mem_use: 0,
            mem_used: 0,
        }
    }

    fn queue_transactions(&self, transactions: Vec<Bytes>, _peer_id: NodeId) {
        // import right here
        let transactions =
            transactions.into_iter().filter_map(|bytes| UntrustedRlp::new(&bytes).as_val().ok()).collect();
        self.miner.import_external_transactions(self, transactions);
    }

    fn ready_transactions(&self, range: Range<u64>) -> PendingSignedTransactions {
        self.miner.ready_transactions(range)
    }

    fn count_pending_transactions(&self, range: Range<u64>) -> usize {
        self.miner.count_pending_transactions(range)
    }


    fn is_pending_queue_empty(&self) -> bool {
        self.miner.status().transactions_in_pending_queue == 0
    }

    fn block_number(&self, _id: &BlockId) -> Option<BlockNumber> {
        unimplemented!()
    }

    fn block_body(&self, id: &BlockId) -> Option<encoded::Body> {
        self.block_hash(id).and_then(|hash| {
            self.blocks.read().get(&hash).map(|r| {
                let mut stream = RlpStream::new_list(1);
                stream.append_raw(Rlp::new(r).at(1).as_raw(), 1);
                encoded::Body::new(stream.out())
            })
        })
    }

    fn block_status(&self, id: &BlockId) -> BlockStatus {
        match id {
            BlockId::Number(number) if (*number as usize) < self.blocks.read().len() => BlockStatus::InChain,
            BlockId::Hash(ref hash) if self.blocks.read().get(hash).is_some() => BlockStatus::InChain,
            BlockId::Latest | BlockId::Earliest => BlockStatus::InChain,
            _ => BlockStatus::Unknown,
        }
    }

    fn block_total_score(&self, _id: &BlockId) -> Option<U256> {
        Some(U256::zero())
    }

    fn block_hash(&self, id: &BlockId) -> Option<H256> {
        Self::block_hash(self, id)
    }

    fn transaction(&self, _id: &TransactionId) -> Option<LocalizedTransaction> {
        unimplemented!();
    }

    fn error_hint(&self, _hash: &H256) -> Option<String> {
        unimplemented!();
    }

    fn transaction_by_tracker(&self, _: &H256) -> Option<LocalizedTransaction> {
        unimplemented!();
    }

    fn error_hints_by_tracker(&self, _: &H256) -> Vec<(H256, Option<String>)> {
        unimplemented!();
    }
}

impl TimeoutHandler for TestBlockChainClient {
    fn on_timeout(&self, _token: TimerToken) {}
}

impl ResealTimer for TestBlockChainClient {
    fn set_max_timer(&self) {}

    fn set_min_timer(&self) {}
}

impl ChainTimeInfo for TestBlockChainClient {
    fn transaction_block_age(&self, _: &H256, _parent_block_number: BlockNumber) -> Option<u64> {
        Some(0)
    }

    fn transaction_time_age(&self, _: &H256, _parent_timestamp: u64) -> Option<u64> {
        Some(0)
    }
}

impl FindActionHandler for TestBlockChainClient {}

impl super::EngineClient for TestBlockChainClient {
    fn update_sealing(&self, parent_block: BlockId, allow_empty_block: bool) {
        self.miner.update_sealing(self, parent_block, allow_empty_block)
    }

    fn submit_seal(&self, block_hash: H256, seal: Vec<Bytes>) {
        if self.miner.submit_seal(self, block_hash, seal).is_err() {
            cwarn!(CLIENT, "Wrong internal seal submission!")
        }
    }

    fn score_to_target(&self, _score: &U256) -> U256 {
        U256::zero()
    }

    fn update_best_as_committed(&self, _block_hash: H256) {}

    fn get_kvdb(&self) -> Arc<KeyValueDB> {
        let db = kvdb_memorydb::create(NUM_COLUMNS.unwrap_or(0));
        Arc::new(db)
    }
}
