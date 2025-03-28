// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    compiler::{Compiler, ScriptOrModule},
    config::{global::Config as GlobalConfig, transaction::Config as TransactionConfig},
    errors::*,
    executor::FakeExecutor,
};
use executor::account::AccountData;
use mirai_annotations::checked_verify;
use once_cell::sync::Lazy;
use starcoin_account_api::AccountPrivateKey;
use starcoin_config::DEFAULT_GAS_CONSTANTS;
use starcoin_types::{
    access_path::AccessPath,
    account_address::AccountAddress,
    block_metadata::BlockMetadata,
    transaction::{
        Module as TransactionModule, RawUserTransaction, Script as TransactionScript,
        SignedUserTransaction, Transaction as StarcoinTransaction, TransactionOutput,
        TransactionStatus,
    },
};
use starcoin_vm_types::genesis_config::ChainId;
use starcoin_vm_types::token::stc::STC_TOKEN_CODE_STR;
use starcoin_vm_types::transaction_argument::convert_txn_args;
use starcoin_vm_types::vm_status::{KeptVMStatus, VMStatus};
use starcoin_vm_types::{
    bytecode_verifier::{self, dependencies},
    errors::{Location, VMError},
    file_format::{CompiledModule, CompiledScript},
    gas_schedule::GasAlgebra,
    language_storage::ModuleId,
    state_view::StateView,
    views::ModuleView,
};
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

pub type TransactionId = usize;

//TODO remove this
static PRECOMPILED_TXN_SCRIPTS: Lazy<HashMap<String, CompiledScript>> = Lazy::new(HashMap::new);

/// A transaction to be evaluated by the testing infra.
/// Contains code and a transaction config.
#[derive(Debug)]
pub struct Transaction<'a> {
    pub config: TransactionConfig<'a>,
    pub input: String,
}

/// Commands that drives the operation of DiemVM. Such as:
/// 1. Execute user transaction
/// 2. Publish a new block metadata
///
/// In the future we will add more commands to mimic the full public API of DiemVM,
/// including reloading the on-chain configuration that will affect the code path for DiemVM,
/// cleaning the cache in the DiemVM, etc.
#[derive(Debug)]
pub enum Command<'a> {
    Transaction(Transaction<'a>),
    BlockMetadata(BlockMetadata),
}

/// Indicates one step in the pipeline the given move module/program goes through.
//  Ord is derived as we need to be able to determine if one stage is before another.
#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq)]
pub enum Stage {
    Compiler,
    Verifier,
    Serializer,
    Runtime,
}

impl FromStr for Stage {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "compiler" => Ok(Stage::Compiler),
            "verifier" => Ok(Stage::Verifier),
            "serializer" => Ok(Stage::Serializer),
            "runtime" => Ok(Stage::Runtime),
            _ => Err(ErrorKind::Other(format!("unrecognized stage '{:?}'", s)).into()),
        }
    }
}

/// Evaluation status: success or failure.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Status {
    Success,
    Failure,
}

#[derive(Debug, Clone)]
pub enum OutputType {
    CompiledModule(Box<CompiledModule>),
    CompiledScript(Box<CompiledScript>),
    CompilerLog(String),
    TransactionOutput(Box<TransactionOutput>),
}

impl OutputType {
    pub fn to_check_string(&self) -> String {
        format!("{:?}", self)
    }
}

/// An entry in the `EvaluationLog`.
#[derive(Debug)]
pub enum EvaluationOutput {
    Transaction(TransactionId),
    Stage(Stage),
    Output(OutputType),
    Error(Box<Error>),
    Status(Status),
}

impl EvaluationOutput {
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }
}

/// A log consisting of outputs from all stages and the final status.
/// This is checked against the directives.
#[derive(Debug, Default)]
pub struct EvaluationLog {
    pub outputs: Vec<EvaluationOutput>,
}

impl EvaluationLog {
    pub fn new() -> Self {
        Self { outputs: vec![] }
    }

    pub fn get_failed_transactions(&self) -> Vec<(usize, Stage)> {
        let mut res = vec![];
        let mut last_txn = None;
        let mut last_stage = None;

        for output in &self.outputs {
            match output {
                EvaluationOutput::Transaction(idx) => last_txn = Some(idx),
                EvaluationOutput::Stage(stage) => last_stage = Some(stage),
                EvaluationOutput::Status(Status::Failure) => match (last_txn, last_stage) {
                    (Some(idx), Some(stage)) => res.push((*idx, *stage)),
                    _ => unreachable!(),
                },
                _ => (),
            }
        }

        res
    }

    pub fn append(&mut self, output: EvaluationOutput) {
        self.outputs.push(output);
    }
}

impl fmt::Display for OutputType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use OutputType::*;
        match self {
            CompiledModule(cm) => write!(f, "{:#?}", cm),
            CompiledScript(cs) => write!(f, "{:#?}", cs),
            CompilerLog(s) => write!(f, "{}", s),
            TransactionOutput(output) => write!(f, "{:#?}", output),
        }
    }
}

impl fmt::Display for EvaluationOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use EvaluationOutput::*;
        match self {
            Transaction(idx) => write!(f, "Transaction {}", idx),
            Stage(stage) => write!(f, "Stage: {:?}", stage),
            Output(output) => write!(f, "{}", output),
            Error(error) => write!(f, "Error: {:#?}", error),
            Status(status) => write!(f, "Status: {:?}", status),
        }
    }
}

impl fmt::Display for EvaluationLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, output) in self.outputs.iter().enumerate() {
            writeln!(f, "[{}] {}", i, output)?;
        }
        Ok(())
    }
}

fn fetch_script_dependencies(
    exec: &mut FakeExecutor,
    script: &CompiledScript,
) -> Vec<CompiledModule> {
    let inner = script.as_inner();
    let idents = inner.module_handles.iter().map(|handle| {
        ModuleId::new(
            inner.address_identifiers[handle.address.0 as usize],
            inner.identifiers[handle.name.0 as usize].clone(),
        )
    });
    fetch_dependencies(exec, idents)
}

fn fetch_module_dependencies(
    exec: &mut FakeExecutor,
    module: &CompiledModule,
) -> Vec<CompiledModule> {
    let idents = ModuleView::new(module)
        .module_handles()
        .map(|handle_view| handle_view.module_id());
    fetch_dependencies(exec, idents)
}

fn fetch_dependencies(
    exec: &mut FakeExecutor,
    idents: impl Iterator<Item = ModuleId>,
) -> Vec<CompiledModule> {
    idents
        .flat_map(|ident| fetch_dependency(exec, ident))
        .collect()
}

fn fetch_dependency(exec: &mut FakeExecutor, ident: ModuleId) -> Option<CompiledModule> {
    let ap = AccessPath::from(&ident);
    let blob: Vec<u8> = exec.get_state_view().get(&ap).ok().flatten()?;
    let compiled: CompiledModule = CompiledModule::deserialize(&blob).ok()?;
    match bytecode_verifier::verify_module(&compiled) {
        Ok(_) => Some(compiled),
        Err(_) => None,
    }
}

/// Verify a script with its dependencies.
pub fn verify_script(
    script: CompiledScript,
    deps: &[CompiledModule],
) -> std::result::Result<CompiledScript, VMError> {
    bytecode_verifier::verify_script(&script)?;
    dependencies::verify_script(&script, deps)?;
    Ok(script)
}

/// Verify a module with its dependencies.
pub fn verify_module(
    module: CompiledModule,
    deps: &[CompiledModule],
) -> std::result::Result<CompiledModule, VMError> {
    bytecode_verifier::verify_module(&module)?;
    dependencies::verify_module(&module, deps)?;
    Ok(module)
}

/// A set of common parameters required to create transactions.
struct TransactionParameters<'a> {
    pub sender_addr: AccountAddress,
    pub privkey: &'a AccountPrivateKey,
    pub sequence_number: u64,
    pub max_gas_amount: u64,
    pub gas_unit_price: u64,
    pub expiration_timestamp_seconds: u64,
}

/// Gets the transaction parameters from the current execution environment and the config.
fn get_transaction_parameters<'a>(
    exec: &'a FakeExecutor,
    config: &'a TransactionConfig,
) -> TransactionParameters<'a> {
    let account_resource = exec
        .read_account_resource(config.sender)
        .expect("read_account_resource fail");
    let account_balance = exec
        .read_balance_resource(config.sender)
        .expect("read_balance_resource fail");
    let gas_unit_price = config.gas_price.unwrap_or(0);
    let max_number_of_gas_units = DEFAULT_GAS_CONSTANTS.clone().maximum_number_of_gas_units;
    let max_gas_amount = config.max_gas.unwrap_or_else(|| {
        if gas_unit_price == 0 {
            max_number_of_gas_units.get()
        } else {
            std::cmp::min(
                max_number_of_gas_units.get(),
                (account_balance.token() / gas_unit_price as u128) as u64,
            )
        }
    });

    TransactionParameters {
        sender_addr: *config.sender.address(),
        privkey: &config.sender.private_key(),
        sequence_number: config
            .sequence_number
            .unwrap_or_else(|| account_resource.sequence_number()),
        max_gas_amount,
        gas_unit_price,
        expiration_timestamp_seconds: exec.read_timestamp()
            + config.expiration_time.unwrap_or(3600),
    }
}

/// Creates and signs a script transaction.
fn make_script_transaction(
    exec: &FakeExecutor,
    config: &TransactionConfig,
    script: CompiledScript,
) -> Result<SignedUserTransaction> {
    let mut blob = vec![];
    script.serialize(&mut blob)?;
    let script =
        TransactionScript::new(blob, config.ty_args.clone(), convert_txn_args(&config.args));

    let params = get_transaction_parameters(exec, config);
    let raw_txn = RawUserTransaction::new_script(
        params.sender_addr,
        params.sequence_number,
        script,
        params.max_gas_amount,
        params.gas_unit_price,
        params.expiration_timestamp_seconds,
        ChainId::test(),
    );
    let signature = params.privkey.sign(&raw_txn);
    Ok(SignedUserTransaction::new(raw_txn, signature))
}

/// Creates and signs a module transaction.
fn make_module_transaction(
    exec: &FakeExecutor,
    config: &TransactionConfig,
    module: CompiledModule,
) -> Result<SignedUserTransaction> {
    let mut blob = vec![];
    module.serialize(&mut blob)?;
    let module = TransactionModule::new(blob);

    let params = get_transaction_parameters(exec, config);
    let raw_txn = RawUserTransaction::new_module(
        params.sender_addr,
        params.sequence_number,
        module,
        params.max_gas_amount,
        params.gas_unit_price,
        params.expiration_timestamp_seconds,
        ChainId::test(),
    );
    let signature = params.privkey.sign(&raw_txn);
    Ok(SignedUserTransaction::new(raw_txn, signature))
}

/// Runs a single transaction using the fake executor.
fn run_transaction(
    exec: &mut FakeExecutor,
    transaction: SignedUserTransaction,
) -> Result<TransactionOutput> {
    let mut outputs = exec.execute_block(vec![transaction]).unwrap();
    if outputs.len() == 1 {
        let (vm_status, output) = outputs.pop().unwrap();
        match output.status() {
            TransactionStatus::Keep(status) => {
                exec.apply_write_set(output.write_set());
                if status == &KeptVMStatus::Executed {
                    Ok(output)
                } else {
                    Err(ErrorKind::VMExecutionFailure(vm_status, output).into())
                }
            }
            TransactionStatus::Discard(_status) => {
                checked_verify!(output.write_set().is_empty());
                Err(ErrorKind::DiscardedTransaction(output).into())
            }
        }
    } else {
        unreachable!("transaction outputs size mismatch")
    }
}

/// Serializes the script then deserializes it.
fn serialize_and_deserialize_script(script: &CompiledScript) -> Result<()> {
    let mut script_blob = vec![];
    script.serialize(&mut script_blob)?;
    let deserialized_script = CompiledScript::deserialize(&script_blob)
        .map_err(|e| e.finish(Location::Undefined).into_vm_status())?;

    if *script != deserialized_script {
        return Err(ErrorKind::Other(
            "deserialized script different from original one".to_string(),
        )
        .into());
    }

    Ok(())
}

/// Serializes the module then deserializes it.
fn serialize_and_deserialize_module(module: &CompiledModule) -> Result<()> {
    let mut module_blob = vec![];
    module.serialize(&mut module_blob)?;
    let deserialized_module = CompiledModule::deserialize(&module_blob)
        .map_err(|e| e.finish(Location::Undefined).into_vm_status())?;

    if *module != deserialized_module {
        return Err(ErrorKind::Other(
            "deserialized module different from original one".to_string(),
        )
        .into());
    }

    Ok(())
}

fn is_precompiled_script(input_str: &str) -> Option<CompiledScript> {
    if let Some(script_name) = input_str.strip_prefix("stdlib_script::") {
        return PRECOMPILED_TXN_SCRIPTS.get(script_name).cloned();
    }
    None
}

fn eval_transaction<TComp: Compiler>(
    compiler: &mut TComp,
    exec: &mut FakeExecutor,
    idx: usize,
    transaction: &Transaction,
    log: &mut EvaluationLog,
) -> Result<Status> {
    /// Unwrap the given results. Upon failure, logs the error and aborts.
    macro_rules! unwrap_or_abort {
        ($res: expr) => {{
            match $res {
                Ok(r) => r,
                Err(e) => {
                    log.append(EvaluationOutput::Error(Box::new(e)));
                    return Ok(Status::Failure);
                }
            }
        }};
    }

    let sender_addr = *transaction.config.sender.address();

    // Start processing a new transaction.
    log.append(EvaluationOutput::Transaction(idx));

    // stage 1: Compile the script/module
    if transaction.config.is_stage_disabled(Stage::Compiler) {
        return Ok(Status::Success);
    }
    log.append(EvaluationOutput::Stage(Stage::Compiler));
    let compiler_log = |s| log.append(EvaluationOutput::Output(OutputType::CompilerLog(s)));

    //TODO support Call ScriptFunction
    let parsed_script_or_module =
        if let Some(compiled_script) = is_precompiled_script(&transaction.input) {
            ScriptOrModule::Script(compiled_script)
        } else {
            unwrap_or_abort!(compiler.compile(compiler_log, sender_addr, &transaction.input))
        };

    match parsed_script_or_module {
        ScriptOrModule::Script(compiled_script) => {
            log.append(EvaluationOutput::Output(OutputType::CompiledScript(
                Box::new(compiled_script.clone()),
            )));

            // stage 2: verify the script
            if transaction.config.is_stage_disabled(Stage::Verifier) {
                return Ok(Status::Success);
            }
            log.append(EvaluationOutput::Stage(Stage::Verifier));
            let deps = fetch_script_dependencies(exec, &compiled_script);
            let compiled_script = match verify_script(compiled_script, &deps) {
                Ok(script) => script,
                Err(err) => {
                    let err: Error = ErrorKind::VerificationError(err.into_vm_status()).into();
                    log.append(EvaluationOutput::Error(Box::new(err)));
                    return Ok(Status::Failure);
                }
            };

            // stage 3: serializer round trip
            if !transaction.config.is_stage_disabled(Stage::Serializer) {
                log.append(EvaluationOutput::Stage(Stage::Serializer));
                unwrap_or_abort!(serialize_and_deserialize_script(&compiled_script));
            }

            // stage 4: execute the script
            if transaction.config.is_stage_disabled(Stage::Runtime) {
                return Ok(Status::Success);
            }
            log.append(EvaluationOutput::Stage(Stage::Runtime));
            let script_transaction =
                make_script_transaction(&exec, &transaction.config, compiled_script)?;
            let txn_output = unwrap_or_abort!(run_transaction(exec, script_transaction));
            log.append(EvaluationOutput::Output(OutputType::TransactionOutput(
                Box::new(txn_output),
            )));
        }
        ScriptOrModule::Module(compiled_module) => {
            log.append(EvaluationOutput::Output(OutputType::CompiledModule(
                Box::new(compiled_module.clone()),
            )));

            // stage 2: verify the module
            if transaction.config.is_stage_disabled(Stage::Verifier) {
                return Ok(Status::Success);
            }
            log.append(EvaluationOutput::Stage(Stage::Verifier));
            let deps = fetch_module_dependencies(exec, &compiled_module);
            let compiled_module = match verify_module(compiled_module, &deps) {
                Ok(module) => module,
                Err(err) => {
                    let err: Error = ErrorKind::VerificationError(err.into_vm_status()).into();
                    log.append(EvaluationOutput::Error(Box::new(err)));
                    return Ok(Status::Failure);
                }
            };

            // stage 3: serializer round trip
            if !transaction.config.is_stage_disabled(Stage::Serializer) {
                log.append(EvaluationOutput::Stage(Stage::Serializer));
                unwrap_or_abort!(serialize_and_deserialize_module(&compiled_module));
            }

            // stage 4: publish the module
            if transaction.config.is_stage_disabled(Stage::Runtime) {
                return Ok(Status::Success);
            }
            log.append(EvaluationOutput::Stage(Stage::Runtime));
            let module_transaction =
                make_module_transaction(&exec, &transaction.config, compiled_module)?;
            let txn_output = unwrap_or_abort!(run_transaction(exec, module_transaction));
            log.append(EvaluationOutput::Output(OutputType::TransactionOutput(
                Box::new(txn_output),
            )));
        }
    }
    Ok(Status::Success)
}

pub fn eval_block_metadata(
    executor: &mut FakeExecutor,
    block_metadata: BlockMetadata,
    log: &mut EvaluationLog,
) -> Result<Status> {
    let outputs = executor
        .execute_transaction_block(vec![StarcoinTransaction::BlockMetadata(block_metadata)]);

    match outputs {
        Ok(mut outputs) => {
            let (_vm_status, output) = outputs
                .pop()
                .expect("There should be one output in the result");
            match output.status() {
                TransactionStatus::Keep(_status) => {
                    executor.apply_write_set(output.write_set());
                    log.append(EvaluationOutput::Output(OutputType::TransactionOutput(
                        Box::new(output),
                    )));
                    Ok(Status::Success)
                }
                TransactionStatus::Discard(status) => {
                    let err: Error = ErrorKind::VerificationError(VMStatus::Error(*status)).into();
                    log.append(EvaluationOutput::Error(Box::new(err)));
                    Ok(Status::Failure)
                }
            }
        }
        Err(err) => {
            let err: Error = ErrorKind::Other(err.to_string()).into();
            log.append(EvaluationOutput::Error(Box::new(err)));
            Ok(Status::Failure)
        }
    }
}

/// Feeds all given transactions through the pipeline and produces an EvaluationLog.
pub fn eval<TComp: Compiler>(
    config: &GlobalConfig,
    compiler: TComp,
    commands: &[Command],
) -> Result<EvaluationLog> {
    // Set up a fake executor with the genesis block and create the accounts.
    let mut exec = FakeExecutor::new();
    eval_with_executor(config, compiler, &mut exec, commands)
}

/// Feeds all given transactions through the pipeline and produces an EvaluationLog.
pub fn eval_with_executor<TComp: Compiler>(
    config: &GlobalConfig,
    mut compiler: TComp,
    exec: &mut FakeExecutor,
    commands: &[Command],
) -> Result<EvaluationLog> {
    for data in config.accounts.values() {
        exec.add_account_data(&data);
    }
    for genesis in config.genesis_accounts.values() {
        let genesis_account = exec.read_account_resource(genesis);
        println!(
            "addr: {}, account: {:?}",
            genesis.address(),
            genesis_account
        );
        if let Some(genesis_account) = genesis_account {
            let balance = exec.read_balance_resource(genesis);
            let genesis_account_data = AccountData::with_account_and_event_counts(
                genesis.clone(),
                balance.map(|b| b.token()).unwrap_or_default(),
                STC_TOKEN_CODE_STR,
                genesis_account.sequence_number(),
                genesis_account.withdraw_events().count(),
                genesis_account.deposit_events().count(),
                genesis_account.accept_token_events().count(),
                genesis_account.has_delegated_key_rotation_capability(),
                genesis_account.has_delegated_withdrawal_capability(),
            );
            exec.add_account_data(&genesis_account_data);
        }
    }

    let mut log = EvaluationLog { outputs: vec![] };

    for (idx, command) in commands.iter().enumerate() {
        match command {
            Command::Transaction(transaction) => {
                let status = eval_transaction(&mut compiler, exec, idx, transaction, &mut log)?;
                log.append(EvaluationOutput::Status(status));
            }
            Command::BlockMetadata(block_metadata) => {
                let status = eval_block_metadata(exec, block_metadata.clone(), &mut log)?;
                log.append(EvaluationOutput::Status(status));
            }
        }
    }

    Ok(log)
}
