use std::sync::Arc;

use anyhow::bail;
use fedimint_client::transaction::{ClientInput, ClientOutput, TransactionBuilder};
use fedimint_core::config::ClientModuleConfig;
use fedimint_core::core::{IntoDynInstance, ModuleKind, OperationId};
use fedimint_core::module::ModuleConsensusVersion;
use fedimint_core::{sats, Amount, OutPoint};
use fedimint_dummy_client::states::DummyStateMachine;
use fedimint_dummy_client::{DummyClientInit, DummyClientModule};
use fedimint_dummy_common::config::{DummyClientConfig, DummyGenParams};
use fedimint_dummy_common::{broken_fed_key_pair, DummyInput, DummyOutput, KIND};
use fedimint_dummy_server::DummyInit;
use fedimint_testing::fixtures::Fixtures;
use secp256k1::Secp256k1;

fn fixtures() -> Fixtures {
    Fixtures::new_primary(DummyClientInit, DummyInit, DummyGenParams::default())
}

#[tokio::test(flavor = "multi_thread")]
async fn can_print_and_send_money() -> anyhow::Result<()> {
    let fed = fixtures().new_fed().await;
    let (client1, client2) = fed.two_clients().await;

    let client1_dummy_module = client1.get_first_module::<DummyClientModule>();
    let client2_dummy_module = client2.get_first_module::<DummyClientModule>();
    let (_, outpoint) = client1_dummy_module.print_money(sats(1000)).await?;
    client1_dummy_module.receive_money(outpoint).await?;
    assert_eq!(client1.get_balance().await, sats(1000));

    let outpoint = client1_dummy_module
        .send_money(client2_dummy_module.account(), sats(250))
        .await?;
    client2_dummy_module.receive_money(outpoint).await?;
    assert_eq!(client1.get_balance().await, sats(750));
    assert_eq!(client2.get_balance().await, sats(250));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn client_ignores_unknown_module() {
    let fed = fixtures().new_fed().await;
    let client = fed.new_client().await;

    let mut cfg = client.get_config().clone();
    let module_id = 2142;
    let extra_mod = ClientModuleConfig::from_typed(
        module_id,
        ModuleKind::from_static_str("unknown_module"),
        ModuleConsensusVersion::new(0, 0),
        DummyClientConfig {
            tx_fee: Amount::from_sats(1),
        },
    )
    .unwrap();
    cfg.modules.insert(2142, extra_mod);

    // Test that building the client worked
    let _client = fed.new_client_with_config(cfg).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn federation_should_abort_if_balance_sheet_is_negative() -> anyhow::Result<()> {
    let fed = fixtures().new_fed().await;
    let client = fed.new_client().await;

    let (panic_sender, panic_receiver) = std::sync::mpsc::channel::<()>();
    let prev_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let panic_str = info.to_string();
        if panic_str
            .contains("Balance sheet of the fed has gone negative, this should never happen!")
        {
            // The first panic may lead to the receiver being dropped, so we have to swallow
            // the error here
            let _ = panic_sender.send(());
        }

        prev_panic_hook(info);
    }));

    let dummy = client.get_first_module::<DummyClientModule>();
    let op_id = OperationId(rand::random());
    let account_kp = broken_fed_key_pair();
    let input = ClientInput {
        input: DummyInput {
            amount: sats(1000),
            account: account_kp.public_key(),
        },
        keys: vec![account_kp],
        state_machines: Arc::new(move |_, _| Vec::<DummyStateMachine>::new()),
    };

    let tx = TransactionBuilder::new().with_input(input.into_dyn(dummy.id));
    let outpoint = |txid, _| OutPoint { txid, out_idx: 0 };
    client
        .finalize_and_submit_transaction(op_id, KIND.as_str(), outpoint, tx)
        .await?;

    // Make sure we panicked with the right message
    panic_receiver.recv().expect("Sender not dropped");

    Ok(())
}

/// A proper transaction is balanced, which means the sum of its inputs and
/// outputs are the same.
/// In this case we create a transaction with zero inputs and one output, which
/// the federation should reject because it's unbalanced.
#[tokio::test(flavor = "multi_thread")]
async fn unbalanced_transactions_get_rejected() -> anyhow::Result<()> {
    let fed = fixtures().new_fed().await;
    let client = fed.new_client().await;

    let dummy_module = client.get_first_module::<DummyClientModule>();
    let output = ClientOutput {
        output: DummyOutput {
            amount: sats(1000),
            account: dummy_module.account(),
        },
        state_machines: Arc::new(move |_, _| Vec::<DummyStateMachine>::new()),
    };
    let tx = TransactionBuilder::new().with_output(output.into_dyn(dummy_module.id));
    let (tx, _) = tx.build(&Secp256k1::new(), rand::thread_rng());
    let result = client.api().submit_transaction(tx).await;
    match result {
        Ok(submission_result) => {
            if submission_result
                .try_into_inner(client.decoders())
                .unwrap()
                .is_ok()
            {
                bail!("Should have been rejected")
            }
        }
        Err(e) => bail!("Submission unsuccessful: {}", e),
    }

    Ok(())
}

mod fedimint_migration_tests {
    use anyhow::ensure;
    use fedimint_core::db::{
        Database, DatabaseVersion, DatabaseVersionKeyV0, IDatabaseTransactionOpsCoreTyped,
    };
    use fedimint_core::module::DynCommonModuleInit;
    use fedimint_core::{Amount, BitcoinHash, OutPoint, TransactionId};
    use fedimint_dummy_client::db::{
        DummyClientFundsKeyV0, DummyClientFundsKeyV1, DummyClientNameKey,
    };
    use fedimint_dummy_client::DummyClientInit;
    use fedimint_dummy_common::DummyOutputOutcome;
    use fedimint_dummy_server::db::{
        DbKeyPrefix, DummyFundsKeyV0, DummyFundsPrefixV1, DummyOutcomeKey, DummyOutcomePrefix,
    };
    use fedimint_dummy_server::DummyInit;
    use fedimint_logging::TracingSetup;
    use fedimint_testing::db::{snapshot_db_migrations, validate_migrations_module, BYTE_32};
    use futures::StreamExt;
    use rand::rngs::OsRng;
    use strum::IntoEnumIterator;
    use tracing::info;

    /// Create a database with version 0 data. The database produced is not
    /// intended to be real data or semantically correct. It is only
    /// intended to provide coverage when reading the database
    /// in future code versions. This function should not be updated when
    /// database keys/values change - instead a new function should be added
    /// that creates a new database backup that can be tested.
    async fn create_server_db_with_v0_data(db: Database) {
        let mut dbtx = db.begin_transaction().await;

        // Will be migrated to `DatabaseVersionKey` during `apply_migrations`
        dbtx.insert_new_entry(&DatabaseVersionKeyV0, &DatabaseVersion(0))
            .await;

        // Write example v0 funds record to the database
        let (_, pk) = secp256k1::generate_keypair(&mut OsRng);
        dbtx.insert_new_entry(&DummyFundsKeyV0(pk), &()).await;

        // Write example v0 outcome record to the database
        let txid = TransactionId::from_slice(&BYTE_32).unwrap();
        dbtx.insert_new_entry(
            &DummyOutcomeKey(OutPoint { txid, out_idx: 0 }),
            &DummyOutputOutcome(Amount::from_sats(1000), pk),
        )
        .await;

        dbtx.commit_tx().await;
    }

    async fn create_client_db_with_v0_data(db: Database) {
        let mut dbtx = db.begin_transaction().await;

        // Will be migrated to `DatabaseVersionKey` during `apply_migrations`
        dbtx.insert_new_entry(&DatabaseVersionKeyV0, &DatabaseVersion(0))
            .await;

        // Write example v0 `ClientFunds`
        dbtx.insert_new_entry(&DummyClientFundsKeyV0, &()).await;

        dbtx.commit_tx().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn snapshot_server_db_migrations() -> anyhow::Result<()> {
        let module = DynCommonModuleInit::from(DummyInit);
        snapshot_db_migrations(module, "dummy-server-v0", |db| {
            Box::pin(async move {
                create_server_db_with_v0_data(db).await;
            })
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_server_db_migrations() -> anyhow::Result<()> {
        let _ = TracingSetup::default().init();

        let module = DynCommonModuleInit::from(DummyInit);
        validate_migrations_module(module, "dummy-server", |db| async move {
            let mut dbtx = db.begin_transaction_nc().await;
            for prefix in DbKeyPrefix::iter() {
                match prefix {
                    DbKeyPrefix::Funds => {
                        let funds = dbtx
                            .find_by_prefix(&DummyFundsPrefixV1)
                            .await
                            .collect::<Vec<_>>()
                            .await;
                        let num_funds = funds.len();
                        ensure!(
                            num_funds > 0,
                            "validate_migrations was not able to read any funds for version 0"
                        );
                        info!("Validated Funds");
                    }
                    DbKeyPrefix::Outcome => {
                        let outcomes = dbtx
                            .find_by_prefix(&DummyOutcomePrefix)
                            .await
                            .collect::<Vec<_>>()
                            .await;
                        let num_outcomes = outcomes.len();
                        ensure!(
                            num_outcomes > 0,
                            "validate_migration was not able to read any outcomes for version 0"
                        );
                        info!("Validated Outcome");
                    }
                }
            }

            Ok(())
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn snapshot_client_db_migrations() -> anyhow::Result<()> {
        let module = DynCommonModuleInit::from(DummyClientInit);
        snapshot_db_migrations(module, "dummy-client-v0", |db| {
            Box::pin(async move { create_client_db_with_v0_data(db).await })
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_client_db_migrations() -> anyhow::Result<()> {
        TracingSetup::default().init()?;

        let module = DynCommonModuleInit::from(DummyClientInit);

        validate_migrations_module(module, "dummy-client", |db| async move {
            let mut dbtx = db.begin_transaction_nc().await;

            // After applying migrations, validate that `ClientName` cannot currently be
            // read
            ensure!(
                dbtx.get_value(&DummyClientNameKey).await.is_none(),
                "DatabaseVersion was not migrated successfully, since ClientName is not none"
            );

            for prefix in fedimint_dummy_client::db::DbKeyPrefix::iter() {
                match prefix {
                    fedimint_dummy_client::db::DbKeyPrefix::ClientFunds => {
                        let funds = dbtx.get_value(&DummyClientFundsKeyV1).await;
                        ensure!(
                            funds.is_some(),
                            "validate_migrations was not able to read any client funds"
                        );
                        info!("Validated client funds");
                    }
                    fedimint_dummy_client::db::DbKeyPrefix::ClientName => {
                        // No need to validate re-reading of ClientName, it
                        // is only used to validate that the
                        // `DatabaseVersion` key
                        // was migrated successfully.
                    }
                }
            }

            Ok(())
        })
        .await
    }
}
