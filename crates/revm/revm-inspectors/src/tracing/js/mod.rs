//! Javascript inspector

use crate::tracing::{
    js::{
        bindings::{
            CallFrame, Contract, EvmContext, EvmDb, FrameResult, MemoryObj, OpObj, StackObj,
            StepLog,
        },
        builtins::register_builtins,
    },
    types::CallKind,
    utils::get_create_address,
};
use boa_engine::{Context, JsError, JsObject, JsResult, JsValue, Source};
use reth_primitives::{abi::decode_revert_reason, bytes::Bytes, Account, Address, H256, U256};
use revm::{
    inspectors::GasInspector,
    interpreter::{
        CallInputs, CallScheme, CreateInputs, Gas, InstructionResult, Interpreter, OpCode,
    },
    primitives::{Env, ExecutionResult, Output, ResultAndState, TransactTo, B160, B256},
    Database, EVMData, Inspector,
};
use tokio::sync::mpsc;

pub(crate) mod bindings;
pub(crate) mod builtins;

/// A javascript inspector that will delegate inspector functions to javascript functions
///
/// See also <https://geth.ethereum.org/docs/developers/evm-tracing/custom-tracer#custom-javascript-tracing>
#[derive(Debug)]
pub struct JsInspector {
    ctx: Context<'static>,
    /// The javascript config provided to the inspector.
    config: JsValue,
    /// The evaluated object that contains the inspector functions.
    obj: JsObject,

    /// The javascript function that will be called when the result is requested.
    result_fn: JsObject,
    fault_fn: JsObject,

    /// EVM inspector hook functions
    enter_fn: Option<JsObject>,
    exit_fn: Option<JsObject>,
    /// Executed before each instruction is executed.
    step_fn: Option<JsObject>,
    /// Keeps track of the current call stack.
    call_stack: Vec<CallStackItem>,
    /// The gas inspector used to track remaining gas.
    gas_inspector: GasInspector,
    /// sender half of a channel to communicate with the database service.
    to_db_service: mpsc::Sender<JsDbRequest>,
}

impl JsInspector {
    /// Creates a new inspector from a javascript code snipped that evaluates to an object with the
    /// expected fields and a config object.
    ///
    /// The object must have the following fields:
    ///  - `result`: a function that will be called when the result is requested.
    ///  - `fault`: a function that will be called when the transaction fails.
    ///
    /// Optional functions are invoked during inspection:
    /// - `enter`: a function that will be called when the execution enters a new call.
    /// - `exit`: a function that will be called when the execution exits a call.
    /// - `step`: a function that will be called when the execution steps to the next instruction.
    ///
    /// This also accepts a sender half of a channel to communicate with the database service so the
    /// DB can be queried from inside the inspector.
    pub fn new(
        code: String,
        config: serde_json::Value,
        to_db_service: mpsc::Sender<JsDbRequest>,
    ) -> Result<Self, JsInspectorError> {
        // Instantiate the execution context
        let mut ctx = Context::default();
        register_builtins(&mut ctx)?;

        // evaluate the code
        let code = format!("({})", code);
        let obj =
            ctx.eval(Source::from_bytes(code.as_bytes())).map_err(JsInspectorError::EvalCode)?;

        let obj = obj.as_object().cloned().ok_or(JsInspectorError::ExpectedJsObject)?;

        // ensure all the fields are callables, if present

        let result_fn = obj
            .get("result", &mut ctx)?
            .as_object()
            .cloned()
            .ok_or(JsInspectorError::ResultFunctionMissing)?;
        if !result_fn.is_callable() {
            return Err(JsInspectorError::ResultFunctionMissing)
        }

        let fault_fn = obj
            .get("fault", &mut ctx)?
            .as_object()
            .cloned()
            .ok_or(JsInspectorError::ResultFunctionMissing)?;
        if !result_fn.is_callable() {
            return Err(JsInspectorError::ResultFunctionMissing)
        }

        let enter_fn = obj.get("enter", &mut ctx)?.as_object().cloned().filter(|o| o.is_callable());
        let exit_fn = obj.get("exit", &mut ctx)?.as_object().cloned().filter(|o| o.is_callable());
        let step_fn = obj.get("step", &mut ctx)?.as_object().cloned().filter(|o| o.is_callable());

        let config =
            JsValue::from_json(&config, &mut ctx).map_err(JsInspectorError::InvalidJsonConfig)?;

        if let Some(setup_fn) = obj.get("setup", &mut ctx)?.as_object() {
            if !setup_fn.is_callable() {
                return Err(JsInspectorError::SetupFunctionNotCallable)
            }

            // call setup()
            setup_fn
                .call(&(obj.clone().into()), &[config.clone()], &mut ctx)
                .map_err(JsInspectorError::SetupCallFailed)?;
        }

        Ok(Self {
            ctx,
            config,
            obj,
            result_fn,
            fault_fn,
            enter_fn,
            exit_fn,
            step_fn,
            call_stack: Default::default(),
            gas_inspector: Default::default(),
            to_db_service,
        })
    }

    /// Calls the result function and returns the result as [serde_json::Value].
    ///
    /// Note: This is supposed to be called after the inspection has finished.
    pub fn json_result(&mut self, res: ResultAndState, env: &Env) -> JsResult<serde_json::Value> {
        self.result(res, env)?.to_json(&mut self.ctx)
    }

    /// Calls the result function and returns the result.
    pub fn result(&mut self, res: ResultAndState, env: &Env) -> JsResult<JsValue> {
        let ResultAndState { result, state } = res;
        let db = EvmDb::new(state, self.to_db_service.clone());

        let gas_used = result.gas_used();
        let mut to = None;
        let mut output_bytes = None;
        let mut error = None;
        match result {
            ExecutionResult::Success { output, .. } => match output {
                Output::Call(out) => {
                    output_bytes = Some(out);
                }
                Output::Create(out, addr) => {
                    to = addr;
                    output_bytes = Some(out);
                }
            },
            ExecutionResult::Revert { output, .. } => {
                error = Some(
                    decode_revert_reason(&output)
                        .ok_or_else(|| String::from_utf8_lossy(&output).to_string()),
                );
            }
            ExecutionResult::Halt { .. } => {}
        };

        let ctx = EvmContext {
            r#type: match env.tx.transact_to {
                TransactTo::Call(target) => {
                    to = Some(target);
                    "CALL"
                }
                TransactTo::Create(_) => "CREATE",
            }
            .to_string(),
            from: env.tx.caller,
            to,
            input: env.tx.data.clone(),
            gas: env.tx.gas_limit,
            gas_used,
            gas_price: env.tx.gas_price.try_into().unwrap_or(u64::MAX),
            value: env.tx.value,
            block: env.block.number.try_into().unwrap_or(u64::MAX),
            output: output_bytes.unwrap_or_default(),
            time: env.block.timestamp.to_string(),
            // TODO: fill in the following fields
            intrinsic_gas: 0,
            block_hash: None,
            tx_index: None,
            tx_hash: None,
        };
        let ctx = ctx.into_js_object(&mut self.ctx)?;
        let db = db.into_js_object(&mut self.ctx)?;
        self.result_fn.call(&(self.obj.clone().into()), &[ctx.into(), db.into()], &mut self.ctx)
    }

    fn try_step(&mut self, step: StepLog, db: EvmDb) -> JsResult<()> {
        if let Some(step_fn) = &self.step_fn {
            let step = step.into_js_object(&mut self.ctx)?;
            let db = db.into_js_object(&mut self.ctx)?;
            step_fn.call(&(self.obj.clone().into()), &[step.into(), db.into()], &mut self.ctx)?;
        }
        Ok(())
    }

    fn try_enter(&mut self, frame: CallFrame, db: EvmDb) -> JsResult<()> {
        if let Some(enter_fn) = &self.enter_fn {
            let frame = frame.into_js_object(&mut self.ctx)?;
            let db = db.into_js_object(&mut self.ctx)?;
            enter_fn.call(&(self.obj.clone().into()), &[frame.into(), db.into()], &mut self.ctx)?;
        }
        Ok(())
    }

    fn try_exit(&mut self, frame: FrameResult, db: EvmDb) -> JsResult<()> {
        if let Some(exit_fn) = &self.exit_fn {
            let frame = frame.into_js_object(&mut self.ctx)?;
            let db = db.into_js_object(&mut self.ctx)?;
            exit_fn.call(&(self.obj.clone().into()), &[frame.into(), db.into()], &mut self.ctx)?;
        }
        Ok(())
    }

    /// Returns the currently active call
    ///
    /// Panics: if there's no call yet
    #[track_caller]
    fn active_call(&self) -> &CallStackItem {
        self.call_stack.last().expect("call stack is empty")
    }

    /// Pushes a new call to the stack
    fn push_call(
        &mut self,
        address: Address,
        data: Bytes,
        value: U256,
        kind: CallKind,
        caller: Address,
        gas_limit: u64,
    ) -> &CallStackItem {
        let call = CallStackItem {
            contract: Contract { caller, contract: address, value, input: data },
            kind,
            gas_limit,
        };
        self.call_stack.push(call);
        self.active_call()
    }

    fn pop_call(&mut self) {
        self.call_stack.pop();
    }
}

impl<DB> Inspector<DB> for JsInspector
where
    DB: Database,
{
    fn initialize_interp(
        &mut self,
        interp: &mut Interpreter,
        data: &mut EVMData<'_, DB>,
        is_static: bool,
    ) -> InstructionResult {
        self.gas_inspector.initialize_interp(interp, data, is_static)
    }

    fn step(
        &mut self,
        interp: &mut Interpreter,
        data: &mut EVMData<'_, DB>,
        is_static: bool,
    ) -> InstructionResult {
        if self.step_fn.is_none() {
            return InstructionResult::Continue
        }

        self.gas_inspector.step(interp, data, is_static);

        let db = EvmDb::new(data.journaled_state.state.clone(), self.to_db_service.clone());

        let pc = interp.program_counter();
        let step = StepLog {
            stack: StackObj(interp.stack.clone()),
            op: OpObj(
                OpCode::try_from_u8(interp.contract.bytecode.bytecode()[pc])
                    .expect("is valid opcode;"),
            ),
            memory: MemoryObj(interp.memory.clone()),
            pc: pc as u64,
            gas_remaining: interp.gas.remaining(),
            cost: interp.gas.spend(),
            depth: data.journaled_state.depth(),
            refund: interp.gas.refunded() as u64,
            error: None,
            contract: self.active_call().contract.clone(),
        };

        if self.try_step(step, db).is_err() {
            return InstructionResult::Revert
        }
        InstructionResult::Continue
    }

    fn log(
        &mut self,
        evm_data: &mut EVMData<'_, DB>,
        address: &B160,
        topics: &[B256],
        data: &Bytes,
    ) {
        self.gas_inspector.log(evm_data, address, topics, data);
    }

    fn step_end(
        &mut self,
        interp: &mut Interpreter,
        data: &mut EVMData<'_, DB>,
        is_static: bool,
        eval: InstructionResult,
    ) -> InstructionResult {
        if self.step_fn.is_none() {
            return InstructionResult::Continue
        }

        self.gas_inspector.step_end(interp, data, is_static, eval);

        InstructionResult::Continue
    }

    fn call(
        &mut self,
        data: &mut EVMData<'_, DB>,
        inputs: &mut CallInputs,
        is_static: bool,
    ) -> (InstructionResult, Gas, Bytes) {
        self.gas_inspector.call(data, inputs, is_static);

        // determine correct `from` and `to` based on the call scheme
        let (from, to) = match inputs.context.scheme {
            CallScheme::DelegateCall | CallScheme::CallCode => {
                (inputs.context.address, inputs.context.code_address)
            }
            _ => (inputs.context.caller, inputs.context.address),
        };

        let value = inputs.transfer.value;
        self.push_call(
            to,
            inputs.input.clone(),
            value,
            inputs.context.scheme.into(),
            from,
            inputs.gas_limit,
        );

        if self.enter_fn.is_some() {
            let call = self.active_call();
            let frame = CallFrame {
                contract: call.contract.clone(),
                kind: call.kind,
                gas: inputs.gas_limit,
            };
            let db = EvmDb::new(data.journaled_state.state.clone(), self.to_db_service.clone());
            if let Err(err) = self.try_enter(frame, db) {
                return (InstructionResult::Revert, Gas::new(0), err.to_string().into())
            }
        }

        (InstructionResult::Continue, Gas::new(0), Bytes::new())
    }

    fn call_end(
        &mut self,
        data: &mut EVMData<'_, DB>,
        inputs: &CallInputs,
        remaining_gas: Gas,
        ret: InstructionResult,
        out: Bytes,
        is_static: bool,
    ) -> (InstructionResult, Gas, Bytes) {
        self.gas_inspector.call_end(data, inputs, remaining_gas, ret, out.clone(), is_static);

        if self.exit_fn.is_some() {
            let frame_result =
                FrameResult { gas_used: remaining_gas.spend(), output: out.clone(), error: None };
            let db = EvmDb::new(data.journaled_state.state.clone(), self.to_db_service.clone());
            if let Err(err) = self.try_exit(frame_result, db) {
                return (InstructionResult::Revert, Gas::new(0), err.to_string().into())
            }
        }

        self.pop_call();

        (ret, remaining_gas, out)
    }

    fn create(
        &mut self,
        data: &mut EVMData<'_, DB>,
        inputs: &mut CreateInputs,
    ) -> (InstructionResult, Option<B160>, Gas, Bytes) {
        self.gas_inspector.create(data, inputs);
        let _ = data.journaled_state.load_account(inputs.caller, data.db);
        let nonce = data.journaled_state.account(inputs.caller).info.nonce;
        let address = get_create_address(inputs, nonce);
        self.push_call(
            address,
            inputs.init_code.clone(),
            inputs.value,
            inputs.scheme.into(),
            inputs.caller,
            inputs.gas_limit,
        );

        if self.enter_fn.is_some() {
            let call = self.active_call();
            let frame =
                CallFrame { contract: call.contract.clone(), kind: call.kind, gas: call.gas_limit };
            let db = EvmDb::new(data.journaled_state.state.clone(), self.to_db_service.clone());
            if let Err(err) = self.try_enter(frame, db) {
                return (InstructionResult::Revert, None, Gas::new(0), err.to_string().into())
            }
        }

        (InstructionResult::Continue, None, Gas::new(inputs.gas_limit), Bytes::default())
    }

    fn create_end(
        &mut self,
        data: &mut EVMData<'_, DB>,
        inputs: &CreateInputs,
        ret: InstructionResult,
        address: Option<B160>,
        remaining_gas: Gas,
        out: Bytes,
    ) -> (InstructionResult, Option<B160>, Gas, Bytes) {
        self.gas_inspector.create_end(data, inputs, ret, address, remaining_gas, out.clone());

        self.pop_call();

        (ret, address, remaining_gas, out)
    }

    fn selfdestruct(&mut self, _contract: B160, _target: B160) {
        if self.enter_fn.is_some() {
            let call = self.active_call();
            let frame =
                CallFrame { contract: call.contract.clone(), kind: call.kind, gas: call.gas_limit };
            let db = EvmDb::new(Default::default(), self.to_db_service.clone());
            let _ = self.try_enter(frame, db);
        }
    }
}

/// Request variants to be sent from the inspector to the database
#[derive(Debug, Clone)]
pub enum JsDbRequest {
    /// Bindings for [Database::basic]
    Basic {
        /// The address of the account to be loaded
        address: Address,
        /// The response channel
        resp: std::sync::mpsc::Sender<Result<Option<Account>, String>>,
    },
    /// Bindings for [Database::code_by_hash]
    Code {
        /// The code hash of the code to be loaded
        code_hash: H256,
        /// The response channel
        resp: std::sync::mpsc::Sender<Result<Bytes, String>>,
    },
    /// Bindings for [Database::storage]
    StorageAt {
        /// The address of the account
        address: Address,
        /// Index of the storage slot
        index: H256,
        /// The response channel
        resp: std::sync::mpsc::Sender<Result<U256, String>>,
    },
}

/// Represents an active call
#[derive(Debug)]
struct CallStackItem {
    contract: Contract,
    kind: CallKind,
    gas_limit: u64,
}

#[derive(Debug, thiserror::Error)]
#[allow(missing_docs)]
pub enum JsInspectorError {
    #[error(transparent)]
    JsError(#[from] JsError),
    #[error("Failed to eval js code: {0}")]
    EvalCode(JsError),
    #[error("The evaluated code is not a JS object")]
    ExpectedJsObject,
    #[error("trace object must expose a function result()")]
    ResultFunctionMissing,
    #[error("trace object must expose a function fault()")]
    FaultFunctionMissing,
    #[error("setup object must be a function")]
    SetupFunctionNotCallable,
    #[error("Failed to call setup(): {0}")]
    SetupCallFailed(JsError),
    #[error("Invalid JSON config: {0}")]
    InvalidJsonConfig(JsError),
}
