use futures::{channel::mpsc, future::join_all, SinkExt};
use std::ops::Add;
use tokio::task::JoinHandle;
use zksync_eth_client::BoundEthInterface;
use zksync_types::REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE;

use zksync::ethereum::{PriorityOpHolder, DEFAULT_PRIORITY_FEE};
use zksync::utils::{
    get_approval_based_paymaster_input, get_approval_based_paymaster_input_for_estimation,
};
use zksync::web3::{contract::Options, types::TransactionReceipt};
use zksync::{EthereumProvider, ZksNamespaceClient};
use zksync_config::constants::MAX_L1_TRANSACTION_GAS_LIMIT;
use zksync_eth_client::EthInterface;
use zksync_eth_signer::PrivateKeySigner;
use zksync_types::api::{BlockNumber, U64};
use zksync_types::{tokens::ETHEREUM_ADDRESS, Address, Nonce, U256};

use crate::report::ReportBuilder;
use crate::{
    account::AccountLifespan,
    account_pool::AccountPool,
    config::{ExecutionConfig, LoadtestConfig},
    constants::*,
    report_collector::{LoadtestResult, ReportCollector},
};

pub const MAX_L1_TRANSACTIONS: u64 = 10;

/// Executor is the entity capable of running the loadtest flow.
///
/// It takes care of the following topics:
///
/// - Minting the tokens on L1 for the main account.
/// - Depositing tokens to the main account in L2 and unlocking it.
/// - Spawning the report collector.
/// - Distributing the funds among the test wallets.
/// - Spawning account lifespan futures.
/// - Awaiting for all the account futures to complete.
/// - Getting the final test resolution from the report collector.
pub struct Executor {
    config: LoadtestConfig,
    execution_config: ExecutionConfig,
    l2_main_token: Address,
    pool: AccountPool,
}

impl Executor {
    /// Creates a new Executor entity.
    pub async fn new(
        config: LoadtestConfig,
        execution_config: ExecutionConfig,
    ) -> anyhow::Result<Self> {
        let pool = AccountPool::new(&config).await?;

        // derive l2 main token address
        let l2_main_token = pool
            .master_wallet
            .ethereum(&config.l1_rpc_address)
            .await
            .expect("Can't get Ethereum client")
            .l2_token_address(config.main_token, None)
            .await
            .unwrap();

        Ok(Self {
            config,
            execution_config,
            pool,
            l2_main_token,
        })
    }

    /// Runs the loadtest until the completion.
    pub async fn start(&mut self) -> LoadtestResult {
        // If the error occurs during the main flow, we will consider it as a test failure.
        self.start_inner().await.unwrap_or_else(|err| {
            vlog::error!("Loadtest was interrupted by the following error: {}", err);
            LoadtestResult::TestFailed
        })
    }

    /// Inner representation of `start` function which returns a `Result`, so it can conveniently use `?`.
    async fn start_inner(&mut self) -> anyhow::Result<LoadtestResult> {
        vlog::info!("Initializing accounts");
        vlog::info!("Running for MASTER {:?}", self.pool.master_wallet.address());
        self.check_onchain_balance().await?;
        self.mint().await?;
        self.deposit_to_master().await?;

        // Top up paymaster for local env.
        if self.config.l2_rpc_address == crate::config::get_default_l2_rpc_address() {
            self.deposit_eth_to_paymaster().await?;
        }

        let (executor_future, account_futures) = self.send_initial_transfers().await?;
        self.wait_account_routines(account_futures).await;

        let final_resultion = executor_future.await.unwrap_or(LoadtestResult::TestFailed);

        Ok(final_resultion)
    }

    /// Verifies that onchain ETH balance for the main account is sufficient to run the loadtest.
    async fn check_onchain_balance(&mut self) -> anyhow::Result<()> {
        vlog::info!("Master Account: Checking onchain balance...");
        let master_wallet = &mut self.pool.master_wallet;
        let ethereum = master_wallet.ethereum(&self.config.l1_rpc_address).await?;
        let eth_balance = ethereum.balance().await?;
        if eth_balance < 2u64.pow(17).into() {
            anyhow::bail!(
                "ETH balance on {:x} is too low to safely perform the loadtest: {}",
                ethereum.client().sender_account(),
                eth_balance
            );
        }

        vlog::info!("Master Account: Onchain balance is OK");
        Ok(())
    }

    /// Mints the ERC-20 token on the main wallet.
    async fn mint(&mut self) -> anyhow::Result<()> {
        vlog::info!("Master Account: Minting ERC20 token...");
        let mint_amount = self.amount_to_deposit() + self.amount_for_l1_distribution();

        let master_wallet = &self.pool.master_wallet;
        let mut ethereum = master_wallet.ethereum(&self.config.l1_rpc_address).await?;
        ethereum.set_confirmation_timeout(ETH_CONFIRMATION_TIMEOUT);
        ethereum.set_polling_interval(ETH_POLLING_INTERVAL);

        let token = self.config.main_token;

        let eth_balance = ethereum
            .erc20_balance(master_wallet.address(), token)
            .await?;

        // Only send the mint transaction if it's necessary.
        if eth_balance > U256::from(mint_amount) {
            vlog::info!("There is already enough money on the master balance");
            return Ok(());
        }

        let mint_tx_hash = ethereum
            .mint_erc20(token, U256::from(u128::MAX), master_wallet.address())
            .await;

        let mint_tx_hash = match mint_tx_hash {
            Err(error) => {
                let balance = ethereum.balance().await;
                let gas_price = ethereum.client().get_gas_price("executor").await;

                anyhow::bail!(
                    "{:?}, Balance: {:?}, Gas Price: {:?}",
                    error,
                    balance,
                    gas_price
                );
            }
            Ok(value) => value,
        };

        vlog::info!("Mint tx with hash {:?}", mint_tx_hash);
        let receipt = ethereum.wait_for_tx(mint_tx_hash).await?;
        self.assert_eth_tx_success(&receipt).await;

        let erc20_balance = ethereum
            .erc20_balance(master_wallet.address(), token)
            .await?;
        assert!(
            erc20_balance >= mint_amount.into(),
            "Minting didn't result in tokens added to balance"
        );

        vlog::info!("Master Account: Minting is OK (balance: {})", erc20_balance);
        Ok(())
    }

    /// Deposits the ERC-20 token to main wallet in L2.
    async fn deposit_to_master(&mut self) -> anyhow::Result<()> {
        vlog::info!("Master Account: Performing an ERC-20 deposit to master");

        let balance = self
            .pool
            .master_wallet
            .get_balance(BlockNumber::Latest, self.l2_main_token)
            .await?;
        let necessary_balance =
            U256::from(self.erc20_transfer_amount() * self.config.accounts_amount as u128);

        if balance > necessary_balance {
            vlog::info!(
                "Master account has enough money on l2, nothing to deposit. Current balance {:?},\
             necessary balance for initial transfers {:?}",
                balance,
                necessary_balance
            );
            return Ok(());
        }

        let mut ethereum = self
            .pool
            .master_wallet
            .ethereum(&self.config.l1_rpc_address)
            .await?;
        ethereum.set_confirmation_timeout(ETH_CONFIRMATION_TIMEOUT);
        ethereum.set_polling_interval(ETH_POLLING_INTERVAL);

        let main_token = self.config.main_token;
        let deposits_allowed = ethereum.is_erc20_deposit_approved(main_token, None).await?;
        if !deposits_allowed {
            // Approve ERC20 deposits.
            let approve_tx_hash = ethereum
                .approve_erc20_token_deposits(main_token, None)
                .await?;
            let receipt = ethereum.wait_for_tx(approve_tx_hash).await?;
            self.assert_eth_tx_success(&receipt).await;
        }

        vlog::info!("Approved ERC20 deposits");
        let receipt = deposit_with_attempts(
            &ethereum,
            self.pool.master_wallet.address(),
            main_token,
            U256::from(self.amount_to_deposit()),
            3,
        )
        .await?;

        self.assert_eth_tx_success(&receipt).await;
        let mut priority_op_handle = receipt
            .priority_op_handle(&self.pool.master_wallet.provider)
            .unwrap_or_else(|| {
                panic!(
                    "Can't get the handle for the deposit operation: {:?}",
                    receipt
                );
            });

        priority_op_handle
            .polling_interval(POLLING_INTERVAL)
            .unwrap();
        priority_op_handle
            .commit_timeout(COMMIT_TIMEOUT)
            .wait_for_commit()
            .await?;

        vlog::info!("Master Account: ERC-20 deposit is OK");
        Ok(())
    }

    async fn deposit_eth_to_paymaster(&mut self) -> anyhow::Result<()> {
        vlog::info!("Master Account: Performing an ETH deposit to the paymaster");
        let deposit_amount = U256::from(10u32).pow(U256::from(20u32)); // 100 ETH
        let mut ethereum = self
            .pool
            .master_wallet
            .ethereum(&self.config.l1_rpc_address)
            .await?;
        ethereum.set_confirmation_timeout(ETH_CONFIRMATION_TIMEOUT);
        ethereum.set_polling_interval(ETH_POLLING_INTERVAL);

        let paymaster_address = self
            .pool
            .master_wallet
            .provider
            .get_testnet_paymaster()
            .await?
            .expect("No testnet paymaster is set");

        // Perform the deposit itself.
        let receipt = deposit_with_attempts(
            &ethereum,
            paymaster_address,
            ETHEREUM_ADDRESS,
            deposit_amount,
            3,
        )
        .await?;

        self.assert_eth_tx_success(&receipt).await;
        let mut priority_op_handle = receipt
            .priority_op_handle(&self.pool.master_wallet.provider)
            .unwrap_or_else(|| {
                panic!(
                    "Can't get the handle for the deposit operation: {:?}",
                    receipt
                );
            });

        priority_op_handle
            .polling_interval(POLLING_INTERVAL)
            .unwrap();
        priority_op_handle
            .commit_timeout(COMMIT_TIMEOUT)
            .wait_for_commit()
            .await?;

        vlog::info!("Master Account: ETH deposit to the paymaster is OK");
        Ok(())
    }

    async fn send_initial_transfers_inner(&self, accounts_to_process: usize) -> anyhow::Result<()> {
        let eth_to_distribute = self.eth_amount_to_distribute().await?;
        let master_wallet = &self.pool.master_wallet;

        let l1_transfer_amount =
            self.amount_for_l1_distribution() / self.config.accounts_amount as u128;
        let l2_transfer_amount = self.erc20_transfer_amount();

        let weight_of_l1_txs = self.execution_config.transaction_weights.l1_transactions
            + self.execution_config.transaction_weights.deposit;

        let paymaster_address = self
            .pool
            .master_wallet
            .provider
            .get_testnet_paymaster()
            .await?
            .expect("No testnet paymaster is set");

        let mut ethereum = master_wallet
            .ethereum(&self.config.l1_rpc_address)
            .await
            .expect("Can't get Ethereum client");
        ethereum.set_confirmation_timeout(ETH_CONFIRMATION_TIMEOUT);
        ethereum.set_polling_interval(ETH_POLLING_INTERVAL);

        // We request nonce each time, so that if one iteration was failed, it will be repeated on the next iteration.
        let mut nonce = Nonce(master_wallet.get_nonce().await?);

        let txs_amount = accounts_to_process * 2 + 1;
        let mut handles = Vec::with_capacity(accounts_to_process);

        // 2 txs per account (1 ERC-20 & 1 ETH transfer).
        let mut eth_txs = Vec::with_capacity(txs_amount * 2);
        let mut eth_nonce = ethereum.client().pending_nonce("loadnext").await?;

        for account in self.pool.accounts.iter().take(accounts_to_process) {
            let target_address = account.wallet.address();

            // Prior to sending funds in L2, we will send funds in L1 for accounts
            // to be able to perform priority operations.
            // We don't actually care whether transactions will be successful or not; at worst we will not use
            // priority operations in test.

            // If we don't need to send l1 txs we don't need to distribute the funds
            if weight_of_l1_txs != 0.0 {
                let balance = ethereum
                    .client()
                    .eth_balance(target_address, "loadnext")
                    .await?;
                let gas_price = ethereum.client().get_gas_price("loadnext").await?;

                if balance < eth_to_distribute {
                    let options = Options {
                        nonce: Some(eth_nonce),
                        max_fee_per_gas: Some(gas_price * 2),
                        max_priority_fee_per_gas: Some(gas_price * 2),
                        ..Default::default()
                    };
                    let res = ethereum
                        .transfer(
                            ETHEREUM_ADDRESS.to_owned(),
                            eth_to_distribute,
                            target_address,
                            Some(options),
                        )
                        .await
                        .unwrap();
                    eth_nonce = eth_nonce.add(U256::one());
                    eth_txs.push(res);
                }

                let ethereum_erc20_balance = ethereum
                    .erc20_balance(target_address, self.config.main_token)
                    .await?;

                if ethereum_erc20_balance < U256::from(l1_transfer_amount) {
                    let options = Options {
                        nonce: Some(eth_nonce),
                        max_fee_per_gas: Some(gas_price * 2),
                        max_priority_fee_per_gas: Some(gas_price * 2),
                        ..Default::default()
                    };
                    let res = ethereum
                        .transfer(
                            self.config.main_token,
                            U256::from(l1_transfer_amount),
                            target_address,
                            Some(options),
                        )
                        .await?;
                    eth_nonce = eth_nonce.add(U256::one());
                    eth_txs.push(res);
                }
            }

            // And then we will prepare an L2 transaction to send ERC20 token (for transfers and fees).
            let mut builder = master_wallet
                .start_transfer()
                .to(target_address)
                .amount(l2_transfer_amount.into())
                .token(self.l2_main_token)
                .nonce(nonce);

            let paymaster_params = get_approval_based_paymaster_input_for_estimation(
                paymaster_address,
                self.l2_main_token,
            );

            let fee = builder.estimate_fee(Some(paymaster_params)).await?;
            builder = builder.fee(fee.clone());

            let paymaster_params = get_approval_based_paymaster_input(
                paymaster_address,
                self.l2_main_token,
                fee.max_total_fee(),
                Vec::new(),
            );
            builder = builder.fee(fee);
            builder = builder.paymaster_params(paymaster_params);

            let handle_erc20 = builder.send().await?;
            handles.push(handle_erc20);

            *nonce += 1;
        }

        // Wait for transactions to be committed, if at least one of them fails,
        // return error.
        for mut handle in handles {
            handle.polling_interval(POLLING_INTERVAL).unwrap();

            let result = handle
                .commit_timeout(COMMIT_TIMEOUT)
                .wait_for_commit()
                .await?;
            if result.status == Some(U64::zero()) {
                return Err(anyhow::format_err!("Transfer failed"));
            }
        }

        vlog::info!(
            "Master account: Wait for ethereum txs confirmations, {:?}",
            &eth_txs
        );
        for eth_tx in eth_txs {
            ethereum.wait_for_tx(eth_tx).await?;
        }

        Ok(())
    }

    /// Returns the amount sufficient for wallets to perform many operations.
    fn erc20_transfer_amount(&self) -> u128 {
        let accounts_amount = self.config.accounts_amount;
        let account_balance = self.amount_to_deposit();
        let for_fees = u64::MAX; // Leave some spare funds on the master account for fees.
        let funds_to_distribute = account_balance - u128::from(for_fees);
        funds_to_distribute / accounts_amount as u128
    }

    /// Initializes the loadtest by doing the following:
    ///
    /// - Spawning the `ReportCollector`.
    /// - Distributing ERC-20 token in L2 among test wallets via `Transfer` operation.
    /// - Distributing ETH in L1 among test wallets in order to make them able to perform priority operations.
    /// - Spawning test account routine futures.
    /// - Collecting all the spawned tasks and returning them to the caller.
    async fn send_initial_transfers(
        &mut self,
    ) -> anyhow::Result<(JoinHandle<LoadtestResult>, Vec<JoinHandle<()>>)> {
        vlog::info!("Master Account: Sending initial transfers");
        // How many times we will resend a batch.
        const MAX_RETRIES: usize = 3;

        // Prepare channels for the report collector.
        let (mut report_sender, report_receiver) = mpsc::channel(256);

        let report_collector = ReportCollector::new(
            report_receiver,
            self.config.expected_tx_count,
            self.config.duration(),
            self.config.prometheus_label.clone(),
        );
        let report_collector_future = tokio::spawn(report_collector.run());

        let config = &self.config;
        let accounts_amount = config.accounts_amount;
        let addresses = self.pool.addresses.clone();
        let paymaster_address = self
            .pool
            .master_wallet
            .provider
            .get_testnet_paymaster()
            .await?
            .expect("No testnet paymaster is set");

        let mut retry_counter = 0;
        let mut accounts_processed = 0;

        let mut account_futures = Vec::new();
        while accounts_processed != accounts_amount {
            if retry_counter > MAX_RETRIES {
                anyhow::bail!("Reached max amount of retries when sending initial transfers");
            }

            let accounts_left = accounts_amount - accounts_processed;
            let max_accounts_per_iter = MAX_OUTSTANDING_NONCE;
            let accounts_to_process = std::cmp::min(accounts_left, max_accounts_per_iter);

            if let Err(err) = self.send_initial_transfers_inner(accounts_to_process).await {
                vlog::warn!(
                    "Iteration of the initial funds distribution failed: {}",
                    err
                );
                retry_counter += 1;
                continue;
            }

            accounts_processed += accounts_to_process;
            vlog::info!(
                "[{}/{}] Accounts processed",
                accounts_processed,
                accounts_amount
            );

            retry_counter = 0;

            let contract_execution_params = self.execution_config.contract_execution_params.clone();
            // Spawn each account lifespan.
            let main_token = self.l2_main_token;
            report_sender
                .send(ReportBuilder::build_init_complete_report())
                .await?;
            let new_account_futures =
                self.pool
                    .accounts
                    .drain(..accounts_to_process)
                    .map(|wallet| {
                        let account = AccountLifespan::new(
                            config,
                            contract_execution_params.clone(),
                            addresses.clone(),
                            wallet,
                            report_sender.clone(),
                            main_token,
                            paymaster_address,
                        );
                        tokio::spawn(account.run())
                    });

            account_futures.extend(new_account_futures);
        }

        assert!(
            self.pool.accounts.is_empty(),
            "Some accounts were not drained"
        );
        vlog::info!("All the initial transfers are completed");

        Ok((report_collector_future, account_futures))
    }

    /// Calculates amount of ETH to be distributed per account in order to make them
    /// able to perform priority operations.
    async fn eth_amount_to_distribute(&self) -> anyhow::Result<U256> {
        let ethereum = self
            .pool
            .master_wallet
            .ethereum(&self.config.l1_rpc_address)
            .await
            .expect("Can't get Ethereum client");

        // Assuming that gas prices on testnets are somewhat stable, we will consider it a constant.
        let average_gas_price = ethereum.client().get_gas_price("executor").await?;

        let gas_price_with_priority = average_gas_price + U256::from(DEFAULT_PRIORITY_FEE);

        let average_l1_to_l2_gas_limit = 5_000_000u32;
        let average_price_for_l1_to_l2_execute = ethereum
            .base_cost(
                average_l1_to_l2_gas_limit.into(),
                REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE as u32,
                Some(gas_price_with_priority),
            )
            .await?;

        Ok(
            gas_price_with_priority * MAX_L1_TRANSACTION_GAS_LIMIT * MAX_L1_TRANSACTIONS
                + average_price_for_l1_to_l2_execute * MAX_L1_TRANSACTIONS,
        )
    }

    /// Waits for all the test account futures to be completed.
    async fn wait_account_routines(&self, account_futures: Vec<JoinHandle<()>>) {
        vlog::info!("Waiting for the account futures to be completed...");
        join_all(account_futures).await;
        vlog::info!("All the spawned tasks are completed");
    }

    /// Returns the amount of funds to be deposited on the main account in L2.
    /// Amount is chosen to be big enough to not worry about precisely calculating the remaining balances on accounts,
    /// but also to not be close to the supported limits in zkSync.
    fn amount_to_deposit(&self) -> u128 {
        u128::MAX >> 32
    }

    /// Returns the amount of funds to be distributed between accounts on l1.
    fn amount_for_l1_distribution(&self) -> u128 {
        u128::MAX >> 29
    }

    /// Ensures that Ethereum transaction was successfully executed.
    async fn assert_eth_tx_success(&self, receipt: &TransactionReceipt) {
        if receipt.status != Some(1u64.into()) {
            let master_wallet = &self.pool.master_wallet;
            let ethereum = master_wallet
                .ethereum(&self.config.l1_rpc_address)
                .await
                .expect("Can't get Ethereum client");
            let failure_reason = ethereum
                .client()
                .failure_reason(receipt.transaction_hash)
                .await
                .expect("Can't connect to the Ethereum node");
            panic!(
                "Ethereum transaction unexpectedly failed.\nReceipt: {:#?}\nFailure reason: {:#?}",
                receipt, failure_reason
            );
        }
    }
}

async fn deposit_with_attempts(
    ethereum: &EthereumProvider<PrivateKeySigner>,
    to: Address,
    token: Address,
    deposit_amount: U256,
    max_attempts: usize,
) -> anyhow::Result<TransactionReceipt> {
    let nonce = ethereum.client().current_nonce("loadtest").await.unwrap();
    for attempt in 1..=max_attempts {
        let pending_block_base_fee_per_gas = ethereum
            .client()
            .get_pending_block_base_fee_per_gas("loadtest")
            .await
            .unwrap();

        let max_priority_fee_per_gas = U256::from(DEFAULT_PRIORITY_FEE * 10 * attempt as u64);
        let max_fee_per_gas = U256::from(
            (pending_block_base_fee_per_gas.as_u64() as f64 * (1.0 + 0.1 * attempt as f64)) as u64,
        ) + max_priority_fee_per_gas;

        let options = Options {
            max_fee_per_gas: Some(max_fee_per_gas),
            max_priority_fee_per_gas: Some(max_priority_fee_per_gas),
            nonce: Some(nonce),
            ..Default::default()
        };
        let deposit_tx_hash = ethereum
            .deposit(token, deposit_amount, to, None, None, Some(options))
            .await?;

        vlog::info!("Deposit with tx_hash {:?}", deposit_tx_hash);

        // Wait for the corresponding priority operation to be committed in zkSync.
        match ethereum.wait_for_tx(deposit_tx_hash).await {
            Ok(eth_receipt) => {
                return Ok(eth_receipt);
            }
            Err(err) => {
                vlog::error!("Deposit error: {:?}", err);
            }
        };
    }
    anyhow::bail!("Max attempts limits reached");
}
