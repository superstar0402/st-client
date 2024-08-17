use std::collections::HashMap;
use std::convert::{Infallible, TryInto};
use std::ffi::OsString;
use std::num::ParseIntError;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::{fmt::Debug, fs, io};

use clap::{arg, command, value_parser, Parser};
use ed25519_dalek::SigningKey;
use heck::ToKebabCase;

use soroban_env_host::{
    xdr::{
        self, Hash, HostFunction, InvokeContractArgs, InvokeHostFunctionOp, LedgerEntryData,
        LedgerFootprint, Limits, Memo, MuxedAccount, Operation, OperationBody, Preconditions,
        PublicKey, ScAddress, ScSpecEntry, ScSpecFunctionV0, ScSpecTypeDef, ScVal, ScVec,
        SequenceNumber, SorobanAuthorizationEntry, SorobanResources, String32, StringM,
        Transaction, TransactionExt, Uint256, VecM, WriteXdr,
    },
    HostError,
};

use soroban_env_host::xdr::{
    AccountEntry, AccountEntryExt, AccountId, ContractDataEntry, DiagnosticEvent, Thresholds,
};
use soroban_spec::read::FromWasmError;
use stellar_strkey::DecodeError;

use super::super::{
    config::{self, locator},
    events,
};
use crate::commands::txn_result::{TxnEnvelopeResult, TxnResult};
use crate::commands::NetworkRunnable;
use crate::{
    commands::{config::data, global, network},
    rpc, Pwd,
};
use soroban_spec_tools::{contract, Spec};

#[derive(Parser, Debug, Default, Clone)]
#[allow(clippy::struct_excessive_bools)]
#[group(skip)]
pub struct Cmd {
    /// Contract ID to invoke
    #[arg(long = "id", env = "STELLAR_CONTRACT_ID")]
    pub contract_id: String,
    // For testing only
    #[arg(skip)]
    pub wasm: Option<std::path::PathBuf>,
    /// View the result simulating and do not sign and submit transaction
    #[arg(long, env = "STELLAR_INVOKE_VIEW")]
    pub is_view: bool,
    /// Function name as subcommand, then arguments for that function as `--arg-name value`
    #[arg(last = true, id = "CONTRACT_FN_AND_ARGS")]
    pub slop: Vec<OsString>,
    #[command(flatten)]
    pub config: config::Args,
    #[command(flatten)]
    pub fee: crate::fee::Args,
}

impl FromStr for Cmd {
    type Err = clap::error::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        use clap::{CommandFactory, FromArgMatches};
        Self::from_arg_matches_mut(&mut Self::command().get_matches_from(s.split_whitespace()))
    }
}

impl Pwd for Cmd {
    fn set_pwd(&mut self, pwd: &Path) {
        self.config.set_pwd(pwd);
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("parsing argument {arg}: {error}")]
    CannotParseArg {
        arg: String,
        error: soroban_spec_tools::Error,
    },
    #[error("cannot add contract to ledger entries: {0}")]
    CannotAddContractToLedgerEntries(xdr::Error),
    #[error(transparent)]
    // TODO: the Display impl of host errors is pretty user-unfriendly
    //       (it just calls Debug). I think we can do better than that
    Host(#[from] HostError),
    #[error("reading file {0:?}: {1}")]
    CannotReadContractFile(PathBuf, io::Error),
    #[error("committing file {filepath}: {error}")]
    CannotCommitEventsFile {
        filepath: std::path::PathBuf,
        error: events::Error,
    },
    #[error("cannot parse contract ID {0}: {1}")]
    CannotParseContractId(String, DecodeError),
    #[error("function {0} was not found in the contract")]
    FunctionNotFoundInContractSpec(String),
    #[error("parsing contract spec: {0}")]
    CannotParseContractSpec(FromWasmError),
    // },
    #[error("function name {0} is too long")]
    FunctionNameTooLong(String),
    #[error("argument count ({current}) surpasses maximum allowed count ({maximum})")]
    MaxNumberOfArgumentsReached { current: usize, maximum: usize },
    #[error("cannot print result {result:?}: {error}")]
    CannotPrintResult {
        result: ScVal,
        error: soroban_spec_tools::Error,
    },
    #[error(transparent)]
    Xdr(#[from] xdr::Error),
    #[error("error parsing int: {0}")]
    ParseIntError(#[from] ParseIntError),
    #[error(transparent)]
    Rpc(#[from] rpc::Error),
    #[error("unexpected contract code data type: {0:?}")]
    UnexpectedContractCodeDataType(LedgerEntryData),
    #[error("missing operation result")]
    MissingOperationResult,
    #[error("missing result")]
    MissingResult,
    #[error(transparent)]
    StrVal(#[from] soroban_spec_tools::Error),
    #[error("error loading signing key: {0}")]
    SignatureError(#[from] ed25519_dalek::SignatureError),
    #[error(transparent)]
    Config(#[from] config::Error),
    #[error("unexpected ({length}) simulate transaction result length")]
    UnexpectedSimulateTransactionResultSize { length: usize },
    #[error("Missing argument {0}")]
    MissingArgument(String),
    #[error(transparent)]
    Clap(#[from] clap::Error),
    #[error(transparent)]
    Locator(#[from] locator::Error),
    #[error("Contract Error\n{0}: {1}")]
    ContractInvoke(String, String),
    #[error(transparent)]
    StrKey(#[from] stellar_strkey::DecodeError),
    #[error(transparent)]
    ContractSpec(#[from] contract::Error),
    #[error("")]
    MissingFileArg(PathBuf),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Data(#[from] data::Error),
    #[error(transparent)]
    Network(#[from] network::Error),
}

impl From<Infallible> for Error {
    fn from(_: Infallible) -> Self {
        unreachable!()
    }
}

impl Cmd {
    fn is_view(&self) -> bool {
        self.is_view ||
            // TODO: Remove at next major release. Was added to retain backwards
            // compatibility when this env var used to be used for the --is-view
            // option.
            std::env::var("SYSTEM_TEST_VERBOSE_OUTPUT").as_deref() == Ok("true")
    }

    fn build_host_function_parameters(
        &self,
        contract_id: [u8; 32],
        spec_entries: &[ScSpecEntry],
        config: &config::Args,
    ) -> Result<(String, Spec, InvokeContractArgs, Vec<SigningKey>), Error> {
        let spec = Spec(Some(spec_entries.to_vec()));
        let mut cmd = clap::Command::new(self.contract_id.clone())
            .no_binary_name(true)
            .term_width(300)
            .max_term_width(300);

        for ScSpecFunctionV0 { name, .. } in spec.find_functions()? {
            cmd = cmd.subcommand(build_custom_cmd(&name.to_utf8_string_lossy(), &spec)?);
        }
        cmd.build();
        let long_help = cmd.render_long_help();
        let mut matches_ = cmd.get_matches_from(&self.slop);
        let Some((function, matches_)) = &matches_.remove_subcommand() else {
            println!("{long_help}");
            std::process::exit(1);
        };

        let func = spec.find_function(function)?;
        // create parsed_args in same order as the inputs to func
        let mut signers: Vec<SigningKey> = vec![];
        let parsed_args = func
            .inputs
            .iter()
            .map(|i| {
                let name = i.name.to_utf8_string()?;
                if let Some(mut val) = matches_.get_raw(&name) {
                    let mut s = val.next().unwrap().to_string_lossy().to_string();
                    if matches!(i.type_, ScSpecTypeDef::Address) {
                        let cmd = crate::commands::keys::address::Cmd {
                            name: s.clone(),
                            hd_path: Some(0),
                            locator: config.locator.clone(),
                        };
                        if let Ok(address) = cmd.public_key() {
                            s = address.to_string();
                        }
                        if let Ok(key) = cmd.private_key() {
                            signers.push(key);
                        }
                    }
                    spec.from_string(&s, &i.type_)
                        .map_err(|error| Error::CannotParseArg { arg: name, error })
                } else if matches!(i.type_, ScSpecTypeDef::Option(_)) {
                    Ok(ScVal::Void)
                } else if let Some(arg_path) =
                    matches_.get_one::<PathBuf>(&fmt_arg_file_name(&name))
                {
                    if matches!(i.type_, ScSpecTypeDef::Bytes | ScSpecTypeDef::BytesN(_)) {
                        Ok(ScVal::try_from(
                            &std::fs::read(arg_path)
                                .map_err(|_| Error::MissingFileArg(arg_path.clone()))?,
                        )
                        .map_err(|()| Error::CannotParseArg {
                            arg: name.clone(),
                            error: soroban_spec_tools::Error::Unknown,
                        })?)
                    } else {
                        let file_contents = std::fs::read_to_string(arg_path)
                            .map_err(|_| Error::MissingFileArg(arg_path.clone()))?;
                        tracing::debug!(
                            "file {arg_path:?}, has contents:\n{file_contents}\nAnd type {:#?}\n{}",
                            i.type_,
                            file_contents.len()
                        );
                        spec.from_string(&file_contents, &i.type_)
                            .map_err(|error| Error::CannotParseArg { arg: name, error })
                    }
                } else {
                    Err(Error::MissingArgument(name))
                }
            })
            .collect::<Result<Vec<_>, Error>>()?;

        let contract_address_arg = ScAddress::Contract(Hash(contract_id));
        let function_symbol_arg = function
            .try_into()
            .map_err(|()| Error::FunctionNameTooLong(function.clone()))?;

        let final_args =
            parsed_args
                .clone()
                .try_into()
                .map_err(|_| Error::MaxNumberOfArgumentsReached {
                    current: parsed_args.len(),
                    maximum: ScVec::default().max_len(),
                })?;

        let invoke_args = InvokeContractArgs {
            contract_address: contract_address_arg,
            function_name: function_symbol_arg,
            args: final_args,
        };

        Ok((function.clone(), spec, invoke_args, signers))
    }

    pub async fn run(&self, global_args: &global::Args) -> Result<(), Error> {
        let res = self.invoke(global_args).await?.to_envelope();
        match res {
            TxnEnvelopeResult::TxnEnvelope(tx) => println!("{}", tx.to_xdr_base64(Limits::none())?),
            TxnEnvelopeResult::Res(output) => {
                println!("{output}");
            }
        }
        Ok(())
    }

    pub async fn invoke(&self, global_args: &global::Args) -> Result<TxnResult<String>, Error> {
        self.run_against_rpc_server(Some(global_args), None).await
    }

    pub fn read_wasm(&self) -> Result<Option<Vec<u8>>, Error> {
        Ok(if let Some(wasm) = self.wasm.as_ref() {
            Some(fs::read(wasm).map_err(|e| Error::CannotReadContractFile(wasm.clone(), e))?)
        } else {
            None
        })
    }

    pub fn spec_entries(&self) -> Result<Option<Vec<ScSpecEntry>>, Error> {
        self.read_wasm()?
            .map(|wasm| {
                soroban_spec::read::from_wasm(&wasm).map_err(Error::CannotParseContractSpec)
            })
            .transpose()
    }
}

impl Cmd {
    fn contract_id(&self) -> Result<[u8; 32], Error> {
        soroban_spec_tools::utils::contract_id_from_str(&self.contract_id)
            .map_err(|e| Error::CannotParseContractId(self.contract_id.clone(), e))
    }
}

#[async_trait::async_trait]
impl NetworkRunnable for Cmd {
    type Error = Error;
    type Result = TxnResult<String>;

    async fn run_against_rpc_server(
        &self,
        global_args: Option<&global::Args>,
        config: Option<&config::Args>,
    ) -> Result<TxnResult<String>, Error> {
        let config = config.unwrap_or(&self.config);
        let network = config.get_network()?;
        tracing::trace!(?network);
        let contract_id = self.contract_id()?;
        let spec_entries = self.spec_entries()?;
        if let Some(spec_entries) = &spec_entries {
            // For testing wasm arg parsing
            let _ = self.build_host_function_parameters(contract_id, spec_entries, config)?;
        }
        let client = rpc::Client::new(&network.rpc_url)?;
        let account_details = if self.is_view {
            default_account_entry()
        } else {
            client
                .verify_network_passphrase(Some(&network.network_passphrase))
                .await?;
            let key = config.key_pair()?;

            // Get the account sequence number
            let public_strkey =
                stellar_strkey::ed25519::PublicKey(key.verifying_key().to_bytes()).to_string();
            client.get_account(&public_strkey).await?
        };
        let sequence: i64 = account_details.seq_num.into();
        let AccountId(PublicKey::PublicKeyTypeEd25519(account_id)) = account_details.account_id;

        let r = client.get_contract_data(&contract_id).await?;
        tracing::trace!("{r:?}");
        let ContractDataEntry {
            val: xdr::ScVal::ContractInstance(xdr::ScContractInstance { executable, .. }),
            ..
        } = r
        else {
            return Err(Error::MissingResult);
        };
        // Get the contract
        let spec_entries = match executable {
            xdr::ContractExecutable::Wasm(hash) => {
                let hash = hash.to_string();
                if let Ok(entries) = data::read_spec(&hash) {
                    entries
                } else {
                    let res = client.get_remote_contract_spec(&contract_id).await?;
                    if global_args.map_or(true, |a| !a.no_cache) {
                        data::write_spec(&hash, &res)?;
                    }
                    res
                }
            }
            xdr::ContractExecutable::StellarAsset => {
                soroban_spec::read::parse_raw(&soroban_sdk::token::StellarAssetSpec::spec_xdr())?
            }
        };

        // Get the ledger footprint
        let (function, spec, host_function_params, signers) =
            self.build_host_function_parameters(contract_id, &spec_entries, config)?;
        let tx = build_invoke_contract_tx(
            host_function_params.clone(),
            sequence + 1,
            self.fee.fee,
            account_id,
        )?;
        if self.fee.build_only {
            return Ok(TxnResult::Txn(tx));
        }
        let txn = client.create_assembled_transaction(&tx).await?;
        let txn = self.fee.apply_to_assembled_txn(txn);
        if self.fee.sim_only {
            return Ok(TxnResult::Txn(txn.transaction().clone()));
        }
        let sim_res = txn.sim_response();
        if global_args.map_or(true, |a| !a.no_cache) {
            data::write(sim_res.clone().into(), &network.rpc_uri()?)?;
        }
        let (return_value, events) = if self.is_view() {
            (sim_res.results()?[0].xdr.clone(), sim_res.events()?)
        } else {
            let global::Args {
                verbose,
                very_verbose,
                no_cache,
                ..
            } = global_args.map(Clone::clone).unwrap_or_default();
            let res = client
                .send_assembled_transaction(
                    txn,
                    &config.key_pair()?,
                    &signers,
                    &network.network_passphrase,
                    Some(log_events),
                    (verbose || very_verbose || self.fee.cost).then_some(log_resources),
                )
                .await?;
            if !no_cache {
                data::write(res.clone().try_into()?, &network.rpc_uri()?)?;
            }
            (res.return_value()?, res.contract_events()?)
        };

        crate::log::diagnostic_events(&events, tracing::Level::INFO);
        output_to_string(&spec, &return_value, &function)
    }
}

const DEFAULT_ACCOUNT_ID: AccountId = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0; 32])));

fn default_account_entry() -> AccountEntry {
    AccountEntry {
        account_id: DEFAULT_ACCOUNT_ID,
        balance: 0,
        seq_num: SequenceNumber(0),
        num_sub_entries: 0,
        inflation_dest: None,
        flags: 0,
        home_domain: String32::from(unsafe { StringM::<32>::from_str("TEST").unwrap_unchecked() }),
        thresholds: Thresholds([0; 4]),
        signers: unsafe { [].try_into().unwrap_unchecked() },
        ext: AccountEntryExt::V0,
    }
}

fn log_events(
    footprint: &LedgerFootprint,
    auth: &[VecM<SorobanAuthorizationEntry>],
    events: &[DiagnosticEvent],
) {
    crate::log::auth(auth);
    crate::log::diagnostic_events(events, tracing::Level::TRACE);
    crate::log::footprint(footprint);
}

fn log_resources(resources: &SorobanResources) {
    crate::log::cost(resources);
}

pub fn output_to_string(
    spec: &Spec,
    res: &ScVal,
    function: &str,
) -> Result<TxnResult<String>, Error> {
    let mut res_str = String::new();
    if let Some(output) = spec.find_function(function)?.outputs.first() {
        res_str = spec
            .xdr_to_json(res, output)
            .map_err(|e| Error::CannotPrintResult {
                result: res.clone(),
                error: e,
            })?
            .to_string();
    }
    Ok(TxnResult::Res(res_str))
}

fn build_invoke_contract_tx(
    parameters: InvokeContractArgs,
    sequence: i64,
    fee: u32,
    source_account_id: Uint256,
) -> Result<Transaction, Error> {
    let op = Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: HostFunction::InvokeContract(parameters),
            auth: VecM::default(),
        }),
    };
    Ok(Transaction {
        source_account: MuxedAccount::Ed25519(source_account_id),
        fee,
        seq_num: SequenceNumber(sequence),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![op].try_into()?,
        ext: TransactionExt::V0,
    })
}

fn build_custom_cmd(name: &str, spec: &Spec) -> Result<clap::Command, Error> {
    let func = spec
        .find_function(name)
        .map_err(|_| Error::FunctionNotFoundInContractSpec(name.to_string()))?;

    // Parse the function arguments
    let inputs_map = &func
        .inputs
        .iter()
        .map(|i| (i.name.to_utf8_string().unwrap(), i.type_.clone()))
        .collect::<HashMap<String, ScSpecTypeDef>>();
    let name: &'static str = Box::leak(name.to_string().into_boxed_str());
    let mut cmd = clap::Command::new(name)
        .no_binary_name(true)
        .term_width(300)
        .max_term_width(300);
    let kebab_name = name.to_kebab_case();
    if kebab_name != name {
        cmd = cmd.alias(kebab_name);
    }
    let doc: &'static str = Box::leak(func.doc.to_utf8_string_lossy().into_boxed_str());
    let long_doc: &'static str = Box::leak(arg_file_help(doc).into_boxed_str());

    cmd = cmd.about(Some(doc)).long_about(long_doc);
    for (name, type_) in inputs_map {
        let mut arg = clap::Arg::new(name);
        let file_arg_name = fmt_arg_file_name(name);
        let mut file_arg = clap::Arg::new(&file_arg_name);
        arg = arg
            .long(name)
            .alias(name.to_kebab_case())
            .num_args(1)
            .value_parser(clap::builder::NonEmptyStringValueParser::new())
            .long_help(spec.doc(name, type_)?);

        file_arg = file_arg
            .long(&file_arg_name)
            .alias(file_arg_name.to_kebab_case())
            .num_args(1)
            .hide(true)
            .value_parser(value_parser!(PathBuf))
            .conflicts_with(name);

        if let Some(value_name) = spec.arg_value_name(type_, 0) {
            let value_name: &'static str = Box::leak(value_name.into_boxed_str());
            arg = arg.value_name(value_name);
        }

        // Set up special-case arg rules
        arg = match type_ {
            ScSpecTypeDef::Bool => arg
                .num_args(0..1)
                .default_missing_value("true")
                .default_value("false")
                .num_args(0..=1),
            ScSpecTypeDef::Option(_val) => arg.required(false),
            ScSpecTypeDef::I256 | ScSpecTypeDef::I128 | ScSpecTypeDef::I64 | ScSpecTypeDef::I32 => {
                arg.allow_hyphen_values(true)
            }
            _ => arg,
        };

        cmd = cmd.arg(arg);
        cmd = cmd.arg(file_arg);
    }
    Ok(cmd)
}

fn fmt_arg_file_name(name: &str) -> String {
    format!("{name}-file-path")
}

fn arg_file_help(docs: &str) -> String {
    format!(
        r#"{docs}
Usage Notes:
Each arg has a corresponding --<arg_name>-file-path which is a path to a file containing the corresponding JSON argument.
Note: The only types which aren't JSON are Bytes and Bytes which are raw bytes"#
    )
}
