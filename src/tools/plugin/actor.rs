use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc::{self, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle, Thread},
    time::{Duration, Instant},
};

use thiserror::Error;
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    model::{JsonValue, ToolSchema},
    tools::process::{
        PluginCleanup, PluginCleanupReport, PluginEmergencyHandle, PluginIo, PluginLeaderState,
        PluginProcess, PluginProcessError, ProcessRunner,
    },
};

use super::{
    MAX_PLUGIN_TOOLS,
    config::{PluginConfig, PluginProgram},
    protocol::{
        PluginCallId, PluginHello, PluginMessage, PluginResultPayload, PluginTool, encode_call,
        encode_cancel, encode_hello, parse_plugin_line,
    },
};

const CALL_QUEUE_CAPACITY: usize = 2;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const AGGREGATE_STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const PROTOCOL_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
const CANCEL_GRACE: Duration = Duration::from_millis(500);
const ACTOR_POLL_INTERVAL: Duration = Duration::from_millis(10);
const IO_CHUNK_BYTES: usize = 8 * 1024;
const MAX_PROTOCOL_LINE_BYTES: usize = 128 * 1024;

const STATE_STARTING: u8 = 0;
const STATE_READY: u8 = 1;
const STATE_FAULTED: u8 = 2;
const STATE_STOPPING: u8 = 3;
const STATE_EXITED: u8 = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PluginStop {
    CallerCancelled,
    TurnTimeout,
    ActionTimeout,
}

#[derive(Clone, Debug)]
pub(crate) struct PluginCallControl {
    cancellation: CancellationToken,
    turn_deadline: Instant,
    action_deadline: Instant,
}

impl PluginCallControl {
    pub(crate) fn new(
        cancellation: CancellationToken,
        turn_deadline: Instant,
        action_deadline: Instant,
    ) -> Self {
        Self {
            cancellation,
            turn_deadline,
            action_deadline,
        }
    }

    fn stop(&self, shutdown: &AtomicBool) -> Option<PluginStop> {
        if shutdown.load(Ordering::Acquire) || self.cancellation.is_cancelled() {
            Some(PluginStop::CallerCancelled)
        } else if Instant::now() >= self.turn_deadline {
            Some(PluginStop::TurnTimeout)
        } else if Instant::now() >= self.action_deadline {
            Some(PluginStop::ActionTimeout)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum PluginCallOutcome {
    Settled(PluginResultPayload),
    InvalidArguments,
    InvalidOutput,
    Busy,
    Unavailable,
    StoppedBeforeDispatch {
        stop: PluginStop,
    },
    StoppedAfterSettlement {
        stop: PluginStop,
    },
    OutcomeUnknown {
        stop: Option<PluginStop>,
    },
    OwnershipLost {
        stop: Option<PluginStop>,
        dispatched: bool,
    },
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(crate) enum PluginHostError {
    #[error("plugin {plugin_id} could not start safely")]
    Startup { plugin_id: String },
    #[error("plugin tool declaration collides with another tool")]
    ToolCollision,
    #[error("configured plugins declare too many tools")]
    TooManyTools,
    #[error("one or more plugin actors could not be shut down safely")]
    Shutdown,
}

#[derive(Clone)]
struct PluginBinding {
    actor: Arc<PluginActor>,
    plugin_id: String,
    tool: PluginTool,
}

pub(crate) struct PluginHost {
    actors: Box<[Arc<PluginActor>]>,
    bindings: BTreeMap<String, PluginBinding>,
    schemas: Box<[ToolSchema]>,
}

impl std::fmt::Debug for PluginHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginHost")
            .field("actor_count", &self.actors.len())
            .field("tool_count", &self.schemas.len())
            .finish()
    }
}

impl PluginHost {
    pub(crate) async fn start(
        config: PluginConfig,
        built_in_schemas: &[ToolSchema],
        cancellation: CancellationToken,
    ) -> Result<Self, PluginHostError> {
        let runner = ProcessRunner::open().map_err(|_| PluginHostError::Startup {
            plugin_id: "process-observer".to_owned(),
        })?;
        let aggregate_deadline = Instant::now() + AGGREGATE_STARTUP_TIMEOUT;
        let mut actors = Vec::new();
        let mut bindings = BTreeMap::new();
        let mut schemas = Vec::new();
        let mut names = built_in_schemas
            .iter()
            .map(|schema| schema.name().to_owned())
            .collect::<BTreeSet<_>>();

        for program in Vec::from(config.into_plugins()) {
            let plugin_id = program.id().to_owned();
            if cancellation.is_cancelled() || Instant::now() >= aggregate_deadline {
                if shutdown_actors(&actors).await.is_err() {
                    return Err(PluginHostError::Shutdown);
                }
                return Err(PluginHostError::Startup { plugin_id });
            }
            let deadline = (Instant::now() + STARTUP_TIMEOUT).min(aggregate_deadline);
            let (actor, hello) =
                match PluginActor::start(program, runner.clone(), cancellation.clone(), deadline)
                    .await
                {
                    Ok(started) => started,
                    Err(start_error) => {
                        let rollback_failed = shutdown_actors(&actors).await.is_err();
                        if start_error == ActorStartError::OwnershipLost || rollback_failed {
                            return Err(PluginHostError::Shutdown);
                        }
                        return Err(PluginHostError::Startup { plugin_id });
                    }
                };
            if hello.plugin_id() != plugin_id {
                actor.request_shutdown();
                let current = actor.shutdown().await;
                let rollback = shutdown_actors(&actors).await;
                if current == ActorExit::OwnershipLost || rollback.is_err() {
                    return Err(PluginHostError::Shutdown);
                }
                return Err(PluginHostError::Startup { plugin_id });
            }
            for tool in hello.tools() {
                if !names.insert(tool.model_schema().name().to_owned()) {
                    actor.request_shutdown();
                    let current = actor.shutdown().await;
                    let rollback = shutdown_actors(&actors).await;
                    if current == ActorExit::OwnershipLost || rollback.is_err() {
                        return Err(PluginHostError::Shutdown);
                    }
                    return Err(PluginHostError::ToolCollision);
                }
                if schemas.len() == MAX_PLUGIN_TOOLS {
                    actor.request_shutdown();
                    let current = actor.shutdown().await;
                    let rollback = shutdown_actors(&actors).await;
                    if current == ActorExit::OwnershipLost || rollback.is_err() {
                        return Err(PluginHostError::Shutdown);
                    }
                    return Err(PluginHostError::TooManyTools);
                }
                schemas.push(tool.model_schema().clone());
                bindings.insert(
                    tool.model_schema().name().to_owned(),
                    PluginBinding {
                        actor: Arc::clone(&actor),
                        plugin_id: plugin_id.clone(),
                        tool: tool.clone(),
                    },
                );
            }
            actors.push(actor);
        }

        Ok(Self {
            actors: actors.into_boxed_slice(),
            bindings,
            schemas: schemas.into_boxed_slice(),
        })
    }

    pub(crate) fn schemas(&self) -> &[ToolSchema] {
        &self.schemas
    }

    pub(crate) fn contains(&self, tool_name: &str) -> bool {
        self.bindings.contains_key(tool_name)
    }

    pub(crate) fn plugin_id(&self, tool_name: &str) -> Option<&str> {
        self.bindings
            .get(tool_name)
            .map(|binding| binding.plugin_id.as_str())
    }

    pub(crate) fn description(&self, tool_name: &str) -> Option<&str> {
        self.bindings
            .get(tool_name)
            .map(|binding| binding.tool.model_schema().description())
    }

    pub(crate) fn validate_arguments(
        &self,
        tool_name: &str,
        arguments: &JsonValue,
    ) -> Result<(), ()> {
        let binding = self.bindings.get(tool_name).ok_or(())?;
        binding
            .tool
            .parameter_schema()
            .validate(arguments)
            .map_err(|_| ())
    }

    pub(crate) fn is_available(&self, tool_name: &str) -> bool {
        self.bindings
            .get(tool_name)
            .is_some_and(|binding| binding.actor.is_ready())
    }

    pub(crate) async fn invoke(
        &self,
        tool_name: &str,
        arguments: JsonValue,
        control: PluginCallControl,
    ) -> PluginCallOutcome {
        let Some(binding) = self.bindings.get(tool_name) else {
            return PluginCallOutcome::Unavailable;
        };
        if binding
            .tool
            .parameter_schema()
            .validate(&arguments)
            .is_err()
        {
            return PluginCallOutcome::InvalidArguments;
        }
        binding
            .actor
            .invoke(tool_name.to_owned(), arguments, control)
            .await
    }

    pub(crate) async fn shutdown(&self) -> Result<(), PluginHostError> {
        for actor in &self.actors {
            actor.request_shutdown();
        }
        let mut failed = false;
        for actor in &self.actors {
            // A protocol/runtime fault already made that plugin unavailable
            // and may have produced a truthful tool error. Shutdown itself
            // fails only when process-group ownership could not be proven.
            failed |= actor.shutdown().await == ActorExit::OwnershipLost;
        }
        if failed {
            Err(PluginHostError::Shutdown)
        } else {
            Ok(())
        }
    }
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        for actor in &self.actors {
            actor.emergency_shutdown();
        }
    }
}

async fn shutdown_actors(actors: &[Arc<PluginActor>]) -> Result<(), ()> {
    let mut failed = false;
    for actor in actors {
        actor.request_shutdown();
    }
    for actor in actors {
        failed |= actor.shutdown().await == ActorExit::OwnershipLost;
    }
    if failed { Err(()) } else { Ok(()) }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActorStartError {
    Startup,
    OwnershipLost,
}

struct PluginActor {
    sender: SyncSender<ActorCommand>,
    shutdown: Arc<AtomicBool>,
    state: Arc<AtomicU8>,
    actor_thread: Thread,
    join: Mutex<Option<JoinHandle<ActorExit>>>,
    completion: watch::Receiver<Option<ActorExit>>,
    emergency: Arc<Mutex<Option<PluginEmergencyHandle>>>,
}

impl std::fmt::Debug for PluginActor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginActor")
            .field("state", &self.state.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl PluginActor {
    async fn start(
        program: PluginProgram,
        runner: ProcessRunner,
        cancellation: CancellationToken,
        deadline: Instant,
    ) -> Result<(Arc<Self>, PluginHello), ActorStartError> {
        let plugin_id = program.id().to_owned();
        let (sender, receiver) = mpsc::sync_channel(CALL_QUEUE_CAPACITY);
        let shutdown = Arc::new(AtomicBool::new(false));
        let state = Arc::new(AtomicU8::new(STATE_STARTING));
        let emergency = Arc::new(Mutex::new(None));
        let (startup_sender, startup_receiver) = oneshot::channel();
        let (completion_sender, completion) = watch::channel(None);
        let thread_shutdown = Arc::clone(&shutdown);
        let thread_state = Arc::clone(&state);
        let thread_emergency = Arc::clone(&emergency);
        let join = thread::Builder::new()
            .name(format!("dsh-plugin-{plugin_id}"))
            .spawn(move || {
                let exit = run_actor(
                    program,
                    runner,
                    receiver,
                    thread_shutdown,
                    thread_state,
                    thread_emergency,
                    startup_sender,
                    deadline,
                );
                completion_sender.send_replace(Some(exit));
                exit
            })
            .map_err(|_| ActorStartError::Startup)?;
        let actor_thread = join.thread().clone();
        let actor = Arc::new(Self {
            sender,
            shutdown,
            state,
            actor_thread,
            join: Mutex::new(Some(join)),
            completion,
            emergency,
        });

        let deadline = tokio::time::Instant::from_std(deadline);
        let startup = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(()),
            _ = tokio::time::sleep_until(deadline) => Err(()),
            result = startup_receiver => match result {
                Ok(result) => result,
                Err(_) => Err(()),
            },
        };
        match startup {
            Ok(hello) => Ok((actor, hello)),
            Err(()) => {
                actor.request_shutdown();
                if actor.shutdown().await == ActorExit::OwnershipLost {
                    Err(ActorStartError::OwnershipLost)
                } else {
                    Err(ActorStartError::Startup)
                }
            }
        }
    }

    async fn invoke(
        &self,
        tool: String,
        arguments: JsonValue,
        control: PluginCallControl,
    ) -> PluginCallOutcome {
        if self.state.load(Ordering::Acquire) != STATE_READY {
            return PluginCallOutcome::Unavailable;
        }
        let (response, receive_response) = oneshot::channel();
        let command = ActorCommand {
            tool,
            arguments,
            control,
            response,
        };
        match self.sender.try_send(command) {
            Ok(()) => self.actor_thread.unpark(),
            Err(TrySendError::Full(_)) => return PluginCallOutcome::Busy,
            Err(TrySendError::Disconnected(_)) => return PluginCallOutcome::Unavailable,
        }
        receive_response
            .await
            .unwrap_or(PluginCallOutcome::OwnershipLost {
                stop: None,
                dispatched: true,
            })
    }

    fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self
            .state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                matches!(state, STATE_STARTING | STATE_READY).then_some(STATE_STOPPING)
            });
        self.actor_thread.unpark();
    }

    fn is_ready(&self) -> bool {
        self.state.load(Ordering::Acquire) == STATE_READY
    }

    async fn shutdown(&self) -> ActorExit {
        self.request_shutdown();
        let mut completion = self.completion.clone();
        loop {
            if let Some(exit) = *completion.borrow() {
                let join = self
                    .join
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                return match join {
                    Some(join) => join.join().unwrap_or(ActorExit::OwnershipLost),
                    None => exit,
                };
            }
            if completion.changed().await.is_err() {
                return ActorExit::OwnershipLost;
            }
        }
    }

    fn emergency_shutdown(&self) {
        self.request_shutdown();
        if let Some(handle) = self
            .emergency
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            handle.kill_group();
        }
    }
}

impl Drop for PluginActor {
    fn drop(&mut self) {
        self.emergency_shutdown();
    }
}

struct ActorCommand {
    tool: String,
    arguments: JsonValue,
    control: PluginCallControl,
    response: oneshot::Sender<PluginCallOutcome>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActorExit {
    Clean,
    Fault,
    OwnershipLost,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DispatchEvidence {
    NotDispatched,
    MayHaveBeenDispatched,
    Dispatched,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IoFault {
    Protocol,
    OutputLimit,
    Process,
    OwnershipLost,
    WriteTimeout,
}

enum CallDrive {
    Continue(PluginCallOutcome),
    Fatal {
        evidence: DispatchEvidence,
        stop: Option<PluginStop>,
        fault: IoFault,
    },
}

#[allow(clippy::too_many_arguments)]
fn run_actor(
    program: PluginProgram,
    runner: ProcessRunner,
    receiver: mpsc::Receiver<ActorCommand>,
    shutdown: Arc<AtomicBool>,
    state: Arc<AtomicU8>,
    emergency: Arc<Mutex<Option<PluginEmergencyHandle>>>,
    startup_sender: oneshot::Sender<Result<PluginHello, ()>>,
    startup_deadline: Instant,
) -> ActorExit {
    let startup = start_process(&program, &runner, &shutdown);
    let mut process = match startup {
        Ok(process) => process,
        Err(error) => {
            let _ = startup_sender.send(Err(()));
            return match error {
                ActorStartError::Startup => {
                    state.store(STATE_EXITED, Ordering::Release);
                    ActorExit::Clean
                }
                ActorStartError::OwnershipLost => {
                    state.store(STATE_FAULTED, Ordering::Release);
                    ActorExit::OwnershipLost
                }
            };
        }
    };
    *emergency.lock().unwrap_or_else(|error| error.into_inner()) = Some(process.emergency_handle());

    let mut wire = WireBuffer::default();
    let hello = startup_handshake(
        &mut process,
        &mut wire,
        program.id(),
        &shutdown,
        startup_deadline,
    );
    let hello = match hello {
        Ok(hello) => hello,
        Err(fault) => {
            let _ = startup_sender.send(Err(()));
            state.store(STATE_FAULTED, Ordering::Release);
            let report = cleanup_after_fault(process, fault);
            return finish_actor(report, &state, &emergency);
        }
    };
    let tools = hello
        .tools()
        .iter()
        .map(|tool| (tool.model_schema().name().to_owned(), tool.clone()))
        .collect::<BTreeMap<_, _>>();
    state.store(
        if shutdown.load(Ordering::Acquire) {
            STATE_STOPPING
        } else {
            STATE_READY
        },
        Ordering::Release,
    );
    if startup_sender.send(Ok(hello)).is_err() {
        shutdown.store(true, Ordering::Release);
    }
    let mut next_id = 1_u64;

    loop {
        if shutdown.load(Ordering::Acquire) {
            reject_queued_for_shutdown(&receiver);
            let report = process.cleanup();
            reject_queued_for_shutdown(&receiver);
            return finish_actor(report, &state, &emergency);
        }
        let pump = match pump_io(&mut process, &mut wire) {
            Ok(pump) => pump,
            Err(fault) => {
                state.store(STATE_FAULTED, Ordering::Release);
                reject_queued_for_fault(&receiver);
                let report = cleanup_after_fault(process, fault);
                return finish_actor(report, &state, &emergency);
            }
        };
        if pump.stdout_received || wire.has_pending() {
            state.store(STATE_FAULTED, Ordering::Release);
            reject_queued_for_fault(&receiver);
            let report = process.terminate();
            return finish_actor(report, &state, &emergency);
        }
        if let Some(fault) = pump.fault {
            state.store(STATE_FAULTED, Ordering::Release);
            reject_queued_for_fault(&receiver);
            let report = cleanup_after_fault(process, fault);
            return finish_actor(report, &state, &emergency);
        }

        match receiver.try_recv() {
            Ok(command) => {
                let Some(id) = PluginCallId::new(next_id).ok() else {
                    let _ = command.response.send(PluginCallOutcome::Unavailable);
                    state.store(STATE_FAULTED, Ordering::Release);
                    reject_queued_for_fault(&receiver);
                    let report = process.terminate();
                    return finish_actor(report, &state, &emergency);
                };
                next_id = next_id.checked_add(1).unwrap_or(u64::MAX);
                let drive = drive_call(&mut process, &mut wire, id, &tools, &shutdown, &command);
                match drive {
                    CallDrive::Continue(outcome) => {
                        let stopped = matches!(
                            outcome,
                            PluginCallOutcome::StoppedBeforeDispatch { .. }
                                | PluginCallOutcome::StoppedAfterSettlement { .. }
                        );
                        let _ = command.response.send(outcome);
                        if stopped && shutdown.load(Ordering::Acquire) {
                            reject_queued_for_shutdown(&receiver);
                            let report = process.cleanup();
                            reject_queued_for_shutdown(&receiver);
                            return finish_actor(report, &state, &emergency);
                        }
                    }
                    CallDrive::Fatal {
                        evidence,
                        stop,
                        fault,
                    } => {
                        state.store(STATE_FAULTED, Ordering::Release);
                        let report = cleanup_after_fault(process, fault);
                        let outcome = fatal_outcome(evidence, stop, fault, report.state());
                        let _ = command.response.send(outcome);
                        reject_queued_for_fault(&receiver);
                        return finish_actor(report, &state, &emergency);
                    }
                }
            }
            Err(TryRecvError::Empty) => thread::park_timeout(ACTOR_POLL_INTERVAL),
            Err(TryRecvError::Disconnected) => {
                let report = process.cleanup();
                return finish_actor(report, &state, &emergency);
            }
        }
    }
}

fn start_process(
    program: &PluginProgram,
    runner: &ProcessRunner,
    shutdown: &AtomicBool,
) -> Result<PluginProcess, ActorStartError> {
    if shutdown.load(Ordering::Acquire) {
        return Err(ActorStartError::Startup);
    }
    program.revalidate().map_err(|_| ActorStartError::Startup)?;
    let workdir = program
        .open_working_directory()
        .map_err(|_| ActorStartError::Startup)?;
    let cancellation = CancellationToken::new();
    if shutdown.load(Ordering::Acquire) {
        cancellation.cancel();
    }
    let environment = plugin_environment(program.id());
    if shutdown.load(Ordering::Acquire) {
        cancellation.cancel();
    }
    PluginProcess::spawn(
        runner,
        program.path(),
        program.arguments(),
        workdir,
        &environment,
        &cancellation,
    )
    .map_err(map_plugin_process_start_error)
}

fn map_plugin_process_start_error(error: PluginProcessError) -> ActorStartError {
    match error {
        PluginProcessError::OwnershipLost => ActorStartError::OwnershipLost,
        PluginProcessError::ObserverUnavailable
        | PluginProcessError::Cancelled
        | PluginProcessError::Spawn
        | PluginProcessError::Pipes => ActorStartError::Startup,
    }
}

fn plugin_environment(plugin_id: &str) -> Vec<(OsString, OsString)> {
    vec![
        (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        (OsString::from("LANG"), OsString::from("C")),
        (OsString::from("LC_ALL"), OsString::from("C")),
        (OsString::from("DSH_PLUGIN_PROTOCOL"), OsString::from("1")),
        (OsString::from("DSH_PLUGIN_ID"), OsString::from(plugin_id)),
    ]
}

fn startup_handshake(
    process: &mut PluginProcess,
    wire: &mut WireBuffer,
    plugin_id: &str,
    shutdown: &AtomicBool,
    deadline: Instant,
) -> Result<PluginHello, IoFault> {
    let record = encode_hello(plugin_id).map_err(|_| IoFault::Protocol)?;
    write_record(process, wire, &record, shutdown, deadline, true)?;
    loop {
        if let Some(line) = wire.take_line()? {
            return finish_startup_hello(process, wire, plugin_id, &line);
        }
        if shutdown.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(IoFault::Process);
        }
        let pump = pump_io(process, wire)?;
        if let Some(line) = wire.take_line()? {
            return finish_startup_hello(process, wire, plugin_id, &line);
        }
        if let Some(fault) = pump.fault {
            return Err(fault);
        }
        if !pump.progress {
            thread::park_timeout(ACTOR_POLL_INTERVAL);
        }
    }
}

fn finish_startup_hello(
    process: &mut PluginProcess,
    wire: &mut WireBuffer,
    plugin_id: &str,
    line: &[u8],
) -> Result<PluginHello, IoFault> {
    if wire.has_pending() {
        return Err(IoFault::Protocol);
    }
    let hello = match parse_plugin_line(line).map_err(|_| IoFault::Protocol)? {
        PluginMessage::Hello(hello) if hello.plugin_id() == plugin_id => hello,
        PluginMessage::Hello(_) | PluginMessage::Result(_) => return Err(IoFault::Protocol),
    };
    // The schema handshake is not useful if the protocol pipe or owned
    // process is already gone. One non-blocking observation distinguishes a
    // live peer from the common "hello then exit" startup failure.
    let probe = pump_io(process, wire)?;
    if probe.stdout_received || wire.has_pending() {
        return Err(IoFault::Protocol);
    }
    if let Some(fault) = probe.fault {
        return Err(fault);
    }
    Ok(hello)
}

fn drive_call(
    process: &mut PluginProcess,
    wire: &mut WireBuffer,
    id: PluginCallId,
    tools: &BTreeMap<String, PluginTool>,
    shutdown: &AtomicBool,
    command: &ActorCommand,
) -> CallDrive {
    let Some(tool) = tools.get(&command.tool) else {
        return CallDrive::Continue(PluginCallOutcome::Unavailable);
    };
    if tool
        .parameter_schema()
        .validate(&command.arguments)
        .is_err()
    {
        return CallDrive::Continue(PluginCallOutcome::InvalidArguments);
    }
    let record = match encode_call(id, &command.tool, &command.arguments) {
        Ok(record) => record,
        Err(_) => return CallDrive::Continue(PluginCallOutcome::InvalidArguments),
    };
    let write_deadline = (Instant::now() + PROTOCOL_WRITE_TIMEOUT)
        .min(command.control.turn_deadline)
        .min(command.control.action_deadline);
    let mut offset = 0_usize;
    let mut evidence = DispatchEvidence::NotDispatched;
    let mut stop = None;
    while offset < record.len() {
        stop = stop.or_else(|| command.control.stop(shutdown));
        if let Some(stop) = stop.filter(|_| evidence == DispatchEvidence::NotDispatched) {
            return CallDrive::Continue(PluginCallOutcome::StoppedBeforeDispatch { stop });
        }
        if Instant::now() >= write_deadline {
            return CallDrive::Fatal {
                evidence,
                stop,
                fault: IoFault::WriteTimeout,
            };
        }
        match process.try_write(&record[offset..]) {
            Ok(PluginIo::Bytes(count)) => {
                offset += count;
                evidence = if offset == record.len() {
                    DispatchEvidence::Dispatched
                } else {
                    DispatchEvidence::MayHaveBeenDispatched
                };
            }
            Ok(PluginIo::WouldBlock) => {}
            Ok(PluginIo::Eof | PluginIo::LimitExceeded) | Err(_) => {
                return CallDrive::Fatal {
                    evidence,
                    stop,
                    fault: IoFault::Process,
                };
            }
        }
        let pump = match pump_io(process, wire) {
            Ok(pump) => pump,
            Err(fault) => {
                return CallDrive::Fatal {
                    evidence,
                    stop,
                    fault,
                };
            }
        };
        if wire.complete_line_available() {
            if evidence == DispatchEvidence::Dispatched {
                break;
            }
            return CallDrive::Fatal {
                evidence,
                stop,
                fault: IoFault::Protocol,
            };
        }
        if let Some(fault) = pump.fault {
            return CallDrive::Fatal {
                evidence,
                stop,
                fault,
            };
        }
        if !pump.progress {
            thread::park_timeout(ACTOR_POLL_INTERVAL);
        }
    }

    loop {
        stop = stop.or_else(|| command.control.stop(shutdown));
        if let Some(result) = match take_result(wire, id, tool) {
            Ok(result) => result,
            Err(fault) => {
                return CallDrive::Fatal {
                    evidence,
                    stop,
                    fault,
                };
            }
        } {
            return CallDrive::Continue(if let Some(stop) = stop {
                PluginCallOutcome::StoppedAfterSettlement { stop }
            } else {
                result
            });
        }

        if let Some(latched) = stop {
            let cancel = match encode_cancel(id) {
                Ok(cancel) => cancel,
                Err(_) => {
                    return CallDrive::Fatal {
                        evidence,
                        stop,
                        fault: IoFault::Protocol,
                    };
                }
            };
            let cancel_deadline = Instant::now() + CANCEL_GRACE;
            if write_record(process, wire, &cancel, shutdown, cancel_deadline, false).is_err() {
                return CallDrive::Fatal {
                    evidence,
                    stop,
                    fault: IoFault::Process,
                };
            }
            loop {
                match take_result(wire, id, tool) {
                    Ok(Some(_)) => {
                        return CallDrive::Continue(PluginCallOutcome::StoppedAfterSettlement {
                            stop: latched,
                        });
                    }
                    Ok(None) => {}
                    Err(fault) => {
                        return CallDrive::Fatal {
                            evidence,
                            stop,
                            fault,
                        };
                    }
                }
                if Instant::now() >= cancel_deadline {
                    return CallDrive::Fatal {
                        evidence,
                        stop,
                        fault: IoFault::Process,
                    };
                }
                match pump_io(process, wire) {
                    Ok(pump) => {
                        match take_result(wire, id, tool) {
                            Ok(Some(_)) => {
                                return CallDrive::Continue(
                                    PluginCallOutcome::StoppedAfterSettlement { stop: latched },
                                );
                            }
                            Ok(None) => {}
                            Err(fault) => {
                                return CallDrive::Fatal {
                                    evidence,
                                    stop,
                                    fault,
                                };
                            }
                        }
                        if let Some(fault) = pump.fault {
                            return CallDrive::Fatal {
                                evidence,
                                stop,
                                fault,
                            };
                        }
                        if !pump.progress {
                            thread::park_timeout(ACTOR_POLL_INTERVAL);
                        }
                    }
                    Err(fault) => {
                        return CallDrive::Fatal {
                            evidence,
                            stop,
                            fault,
                        };
                    }
                }
            }
        }

        match pump_io(process, wire) {
            Ok(pump) => {
                match take_result(wire, id, tool) {
                    Ok(Some(result)) => {
                        return CallDrive::Continue(if let Some(stop) = stop {
                            PluginCallOutcome::StoppedAfterSettlement { stop }
                        } else {
                            result
                        });
                    }
                    Ok(None) => {}
                    Err(fault) => {
                        return CallDrive::Fatal {
                            evidence,
                            stop,
                            fault,
                        };
                    }
                }
                if let Some(fault) = pump.fault {
                    return CallDrive::Fatal {
                        evidence,
                        stop,
                        fault,
                    };
                }
                if !pump.progress {
                    thread::park_timeout(ACTOR_POLL_INTERVAL);
                }
            }
            Err(fault) => {
                return CallDrive::Fatal {
                    evidence,
                    stop,
                    fault,
                };
            }
        }
    }
}

fn take_result(
    wire: &mut WireBuffer,
    id: PluginCallId,
    tool: &PluginTool,
) -> Result<Option<PluginCallOutcome>, IoFault> {
    let Some(line) = wire.take_line()? else {
        return Ok(None);
    };
    let PluginMessage::Result(result) = parse_plugin_line(&line).map_err(|_| IoFault::Protocol)?
    else {
        return Err(IoFault::Protocol);
    };
    if result.id() != id {
        return Err(IoFault::Protocol);
    }
    let outcome = match result.payload() {
        PluginResultPayload::Success(value) => {
            if tool.output_schema().validate(value).is_err() {
                PluginCallOutcome::InvalidOutput
            } else {
                PluginCallOutcome::Settled(result.payload().clone())
            }
        }
        PluginResultPayload::Failure(_) => PluginCallOutcome::Settled(result.payload().clone()),
    };
    Ok(Some(outcome))
}

fn write_record(
    process: &mut PluginProcess,
    wire: &mut WireBuffer,
    record: &[u8],
    shutdown: &AtomicBool,
    deadline: Instant,
    honor_shutdown: bool,
) -> Result<(), IoFault> {
    let mut offset = 0_usize;
    while offset < record.len() {
        if (honor_shutdown && shutdown.load(Ordering::Acquire)) || Instant::now() >= deadline {
            return Err(IoFault::Process);
        }
        let mut progress = false;
        match process.try_write(&record[offset..]) {
            Ok(PluginIo::Bytes(count)) => {
                offset += count;
                progress = true;
            }
            Ok(PluginIo::WouldBlock) => {}
            Ok(PluginIo::Eof | PluginIo::LimitExceeded) | Err(_) => {
                return Err(IoFault::Process);
            }
        }
        let pump = pump_io(process, wire)?;
        progress |= pump.progress;
        if offset < record.len() {
            if let Some(fault) = pump.fault {
                return Err(fault);
            }
        }
        if !progress {
            thread::park_timeout(ACTOR_POLL_INTERVAL);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
struct PumpResult {
    progress: bool,
    stdout_received: bool,
    fault: Option<IoFault>,
}

fn pump_io(process: &mut PluginProcess, wire: &mut WireBuffer) -> Result<PumpResult, IoFault> {
    let mut result = PumpResult::default();
    let mut buffer = [0_u8; IO_CHUNK_BYTES];
    match process.try_read_stdout(&mut buffer) {
        Ok(PluginIo::Bytes(count)) => {
            wire.push(&buffer[..count])?;
            result.progress = true;
            result.stdout_received = true;
        }
        Ok(PluginIo::WouldBlock) => {}
        Ok(PluginIo::Eof) => result.fault = Some(IoFault::Process),
        Ok(PluginIo::LimitExceeded) => result.fault = Some(IoFault::OutputLimit),
        Err(_) => result.fault = Some(IoFault::Process),
    }
    match process.try_read_stderr(&mut buffer) {
        Ok(PluginIo::Bytes(_)) => result.progress = true,
        Ok(PluginIo::WouldBlock | PluginIo::Eof) => {}
        Ok(PluginIo::LimitExceeded) => latch_pump_fault(&mut result, IoFault::OutputLimit),
        Err(_) => latch_pump_fault(&mut result, IoFault::Process),
    }
    finish_pump_for_leader(result, process.leader_state())
}

fn finish_pump_for_leader(
    mut result: PumpResult,
    leader: PluginLeaderState,
) -> Result<PumpResult, IoFault> {
    match leader {
        PluginLeaderState::Running => Ok(result),
        PluginLeaderState::Exited(_) => {
            // A short-lived plugin can exit immediately after flushing a
            // result. Drain any bytes already available from its pipe before
            // treating the exit as a fault, so a multi-chunk matching record
            // remains authoritative for the call that produced it.
            if !result.stdout_received {
                latch_pump_fault(&mut result, IoFault::Process);
            }
            Ok(result)
        }
        // Unlike a normal exit/EOF, lost process ownership cannot be delayed
        // behind a matching protocol record: the Agent may only publish a
        // settled result after it can still prove cleanup ownership.
        PluginLeaderState::OwnershipLost => Err(IoFault::OwnershipLost),
    }
}

fn latch_pump_fault(result: &mut PumpResult, observed: IoFault) {
    let rank = |fault| match fault {
        IoFault::OwnershipLost => 4,
        IoFault::OutputLimit => 3,
        IoFault::Protocol => 2,
        IoFault::Process | IoFault::WriteTimeout => 1,
    };
    if result
        .fault
        .is_none_or(|current| rank(observed) > rank(current))
    {
        result.fault = Some(observed);
    }
}

#[derive(Default)]
struct WireBuffer {
    bytes: Vec<u8>,
}

impl WireBuffer {
    fn push(&mut self, bytes: &[u8]) -> Result<(), IoFault> {
        let next = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or(IoFault::Protocol)?;
        if next > MAX_PROTOCOL_LINE_BYTES {
            return Err(IoFault::Protocol);
        }
        self.bytes
            .try_reserve_exact(bytes.len())
            .map_err(|_| IoFault::Protocol)?;
        self.bytes.extend_from_slice(bytes);
        if self
            .bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .is_some_and(|index| index + 1 > MAX_PROTOCOL_LINE_BYTES)
        {
            return Err(IoFault::Protocol);
        }
        Ok(())
    }

    fn complete_line_available(&self) -> bool {
        self.bytes.contains(&b'\n')
    }

    fn take_line(&mut self) -> Result<Option<Vec<u8>>, IoFault> {
        let Some(end) = self.bytes.iter().position(|byte| *byte == b'\n') else {
            return Ok(None);
        };
        let end = end.checked_add(1).ok_or(IoFault::Protocol)?;
        if end > MAX_PROTOCOL_LINE_BYTES {
            return Err(IoFault::Protocol);
        }
        Ok(Some(self.bytes.drain(..end).collect()))
    }

    fn has_pending(&self) -> bool {
        !self.bytes.is_empty()
    }
}

fn cleanup_after_fault(process: PluginProcess, fault: IoFault) -> PluginCleanupReport {
    match fault {
        IoFault::OutputLimit => process.kill(),
        IoFault::Protocol | IoFault::Process | IoFault::OwnershipLost | IoFault::WriteTimeout => {
            process.terminate()
        }
    }
}

fn fatal_outcome(
    evidence: DispatchEvidence,
    stop: Option<PluginStop>,
    fault: IoFault,
    cleanup: PluginCleanup,
) -> PluginCallOutcome {
    let dispatched = evidence != DispatchEvidence::NotDispatched;
    match cleanup {
        PluginCleanup::OwnershipLost => PluginCallOutcome::OwnershipLost { stop, dispatched },
        PluginCleanup::Quiescent(_) if dispatched => PluginCallOutcome::OutcomeUnknown { stop },
        PluginCleanup::Quiescent(_) => match (stop, fault) {
            (Some(stop), _) => PluginCallOutcome::StoppedBeforeDispatch { stop },
            (None, IoFault::WriteTimeout) => PluginCallOutcome::StoppedBeforeDispatch {
                stop: PluginStop::ActionTimeout,
            },
            (None, _) => PluginCallOutcome::Unavailable,
        },
    }
}

fn reject_queued_for_shutdown(receiver: &mpsc::Receiver<ActorCommand>) {
    while let Ok(command) = receiver.try_recv() {
        let stop = command
            .control
            .stop(&AtomicBool::new(true))
            .unwrap_or(PluginStop::CallerCancelled);
        let _ = command
            .response
            .send(PluginCallOutcome::StoppedBeforeDispatch { stop });
    }
}

fn reject_queued_for_fault(receiver: &mpsc::Receiver<ActorCommand>) {
    let running = AtomicBool::new(false);
    while let Ok(command) = receiver.try_recv() {
        let outcome = command
            .control
            .stop(&running)
            .map_or(PluginCallOutcome::Unavailable, |stop| {
                PluginCallOutcome::StoppedBeforeDispatch { stop }
            });
        let _ = command.response.send(outcome);
    }
}

fn finish_actor(
    report: PluginCleanupReport,
    state: &AtomicU8,
    emergency: &Mutex<Option<PluginEmergencyHandle>>,
) -> ActorExit {
    let exit = if report.state() == PluginCleanup::OwnershipLost {
        ActorExit::OwnershipLost
    } else if state.load(Ordering::Acquire) == STATE_FAULTED
        || report.stdout_limit_exceeded()
        || report.stderr_limit_exceeded()
        || report.pipe_failed()
    {
        ActorExit::Fault
    } else {
        ActorExit::Clean
    };
    state.store(STATE_EXITED, Ordering::Release);
    if exit != ActorExit::OwnershipLost {
        *emergency.lock().unwrap_or_else(|error| error.into_inner()) = None;
    }
    exit
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, OpenOptions},
        io::Write as _,
        os::unix::fs::PermissionsExt as _,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::{Duration, Instant},
    };

    use tokio_util::sync::CancellationToken;

    use rustix::{io::Errno, process::Pid};

    use crate::model::{JsonValue, ToolSchema};

    use super::{
        PluginCallControl, PluginCallOutcome, PluginConfig, PluginHost, PluginResultPayload,
    };

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new() -> Self {
            for _ in 0..100 {
                let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir()
                    .join(format!("dsh-plugin-actor-{}-{serial}", std::process::id()));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("could not create plugin fixture: {error}"),
                }
            }
            panic!("could not allocate a unique plugin fixture")
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_file(path: &Path, bytes: &[u8], mode: u32) {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
        file.set_permissions(fs::Permissions::from_mode(mode))
            .unwrap();
    }

    fn plugin_fixture(wait_for_cancel: bool) -> (TempDirectory, PluginConfig) {
        let root = TempDirectory::new();
        let wait = if wait_for_cancel {
            "IFS= read -r cancel || exit 2\n"
        } else {
            ""
        };
        let script = format!(
            r#"#!/bin/sh
IFS= read -r hello || exit 2
printf '%s\n' '{{"version":1,"type":"hello","plugin_id":"text-tools","tools":[{{"name":"text_stats","description":"Count text","parameters":{{"type":"object","properties":{{"text":{{"type":"string"}}}},"required":["text"],"additionalProperties":false}},"output":{{"type":"object","properties":{{"words":{{"type":"integer"}}}},"required":["words"],"additionalProperties":false}}}}]}}'
id=1
while IFS= read -r call; do
{wait}  printf '{{"version":1,"type":"result","id":%s,"ok":true,"value":{{"words":2}}}}\n' "$id"
  id=$((id + 1))
done
"#
        );
        let config = config_from_script(&root, script.as_bytes());
        (root, config)
    }

    fn config_from_script(root: &TempDirectory, script: &[u8]) -> PluginConfig {
        let program = root.path().join("plugin.sh");
        write_file(&program, script, 0o700);
        let program = fs::canonicalize(program).unwrap();
        let config_path = root.path().join("plugins.json");
        let config = serde_json::json!({
            "version":1,
            "plugins":[{"id":"text-tools","program":program,"args":[]}]
        });
        write_file(
            &config_path,
            serde_json::to_string(&config).unwrap().as_bytes(),
            0o600,
        );
        PluginConfig::load(root.path(), Path::new("plugins.json")).unwrap()
    }

    fn control(cancellation: CancellationToken) -> PluginCallControl {
        PluginCallControl::new(
            cancellation,
            Instant::now() + Duration::from_secs(3),
            Instant::now() + Duration::from_secs(2),
        )
    }

    #[tokio::test]
    async fn actor_handshakes_validates_and_returns_one_correlated_result() {
        let (_root, config) = plugin_fixture(false);
        let host = PluginHost::start(config, &[], CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(host.schemas().len(), 1);
        assert!(host.contains("text_stats"));

        let invalid = host
            .invoke(
                "text_stats",
                JsonValue::new(serde_json::json!({"extra":true})).unwrap(),
                control(CancellationToken::new()),
            )
            .await;
        assert!(matches!(invalid, PluginCallOutcome::InvalidArguments));

        let outcome = host
            .invoke(
                "text_stats",
                JsonValue::new(serde_json::json!({"text":"two words"})).unwrap(),
                control(CancellationToken::new()),
            )
            .await;
        assert!(matches!(
            outcome,
            PluginCallOutcome::Settled(PluginResultPayload::Success(value))
                if value.as_value() == &serde_json::json!({"words":2})
        ));
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn partial_startup_failure_awaits_cleanup_of_every_started_plugin_group() {
        let root = TempDirectory::new();
        let marker = root.path().join("first-child.pid");
        let first = root.path().join("first-plugin.sh");
        let first_script = format!(
            r#"#!/bin/sh
/bin/sleep 10 &
child=$!
printf '%s\n' "$child" > '{}'
IFS= read -r hello || exit 2
printf '%s\n' '{{"version":1,"type":"hello","plugin_id":"first-tools","tools":[{{"name":"first_probe","description":"First probe","parameters":{{"type":"object","properties":{{}},"required":[],"additionalProperties":false}},"output":{{"type":"string"}}}}]}}'
while IFS= read -r call; do :; done
"#,
            marker.display()
        );
        write_file(&first, first_script.as_bytes(), 0o700);
        let second = root.path().join("second-plugin.sh");
        write_file(
            &second,
            br#"#!/bin/sh
IFS= read -r hello || exit 2
printf '%s\n' '{"version":1,"type":"hello","plugin_id":"wrong-id","tools":[]}'
"#,
            0o700,
        );
        let config_path = root.path().join("plugins.json");
        let config = serde_json::json!({
            "version":1,
            "plugins":[
                {"id":"first-tools","program":fs::canonicalize(first).unwrap(),"args":[]},
                {"id":"second-tools","program":fs::canonicalize(second).unwrap(),"args":[]}
            ]
        });
        write_file(
            &config_path,
            serde_json::to_string(&config).unwrap().as_bytes(),
            0o600,
        );
        let config = PluginConfig::load(root.path(), Path::new("plugins.json")).unwrap();
        assert!(
            PluginHost::start(config, &[], CancellationToken::new())
                .await
                .is_err()
        );
        let child = fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        let child = Pid::from_raw(child).unwrap();
        assert_eq!(rustix::process::test_kill_process(child), Err(Errno::SRCH));
    }

    #[tokio::test]
    async fn host_rejects_builtin_name_collisions_and_more_than_thirty_two_tools() {
        let (collision_root, collision_config) = plugin_fixture(false);
        let built_in = ToolSchema::new(
            "text_stats",
            "Built-in collision",
            JsonValue::new(serde_json::json!({
                "type":"object",
                "properties":{},
                "required":[],
                "additionalProperties":false
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            PluginHost::start(collision_config, &[built_in], CancellationToken::new(),).await,
            Err(super::PluginHostError::ToolCollision)
        ));
        drop(collision_root);

        let root = TempDirectory::new();
        let mut entries = Vec::new();
        for plugin_index in 0..5 {
            let plugin_id = format!("plugin-{plugin_index}");
            let tools = (0..8)
                .map(|tool_index| {
                    serde_json::json!({
                        "name":format!("probe_{plugin_index}_{tool_index}"),
                        "description":"Bounded aggregate tool",
                        "parameters":{
                            "type":"object",
                            "properties":{},
                            "required":[],
                            "additionalProperties":false
                        },
                        "output":{"type":"string"}
                    })
                })
                .collect::<Vec<_>>();
            let hello = serde_json::to_string(&serde_json::json!({
                "version":1,
                "type":"hello",
                "plugin_id":plugin_id,
                "tools":tools
            }))
            .unwrap();
            let program = root.path().join(format!("plugin-{plugin_index}.sh"));
            let script = format!(
                "#!/bin/sh\nIFS= read -r hello || exit 2\nprintf '%s\\n' '{hello}'\nwhile IFS= read -r call; do :; done\n"
            );
            write_file(&program, script.as_bytes(), 0o700);
            entries.push(serde_json::json!({
                "id":format!("plugin-{plugin_index}"),
                "program":fs::canonicalize(program).unwrap(),
                "args":[]
            }));
        }
        let config_path = root.path().join("plugins.json");
        write_file(
            &config_path,
            serde_json::to_string(&serde_json::json!({
                "version":1,
                "plugins":entries
            }))
            .unwrap()
            .as_bytes(),
            0o600,
        );
        let config = PluginConfig::load(root.path(), Path::new("plugins.json")).unwrap();
        assert!(matches!(
            PluginHost::start(config, &[], CancellationToken::new()).await,
            Err(super::PluginHostError::TooManyTools)
        ));
    }

    #[tokio::test]
    async fn hello_followed_by_immediate_exit_is_a_startup_failure() {
        let root = TempDirectory::new();
        let script = br#"#!/bin/sh
IFS= read -r hello || exit 2
printf '%s\n' '{"version":1,"type":"hello","plugin_id":"text-tools","tools":[{"name":"text_stats","description":"Count text","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false},"output":{"type":"object","properties":{"words":{"type":"integer"}},"required":["words"],"additionalProperties":false}}]}'
exit 0
"#;
        let config = config_from_script(&root, script);
        assert!(matches!(
            PluginHost::start(config, &[], CancellationToken::new()).await,
            Err(super::PluginHostError::Startup { .. })
        ));
    }

    #[tokio::test]
    async fn stalled_handshake_deadline_cleans_up_without_waiting_for_the_plugin() {
        let root = TempDirectory::new();
        let script = br#"#!/bin/sh
IFS= read -r hello || exit 2
/bin/sleep 10
"#;
        let config = config_from_script(&root, script);
        let mut programs = Vec::from(config.into_plugins());
        let program = programs.remove(0);
        let runner = crate::tools::process::ProcessRunner::open().unwrap();
        let started = Instant::now();
        assert!(matches!(
            super::PluginActor::start(
                program,
                runner,
                CancellationToken::new(),
                Instant::now() + Duration::from_millis(100),
            )
            .await,
            Err(super::ActorStartError::Startup)
        ));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn cancellation_is_latched_but_a_matching_result_keeps_the_actor_usable() {
        let (_root, config) = plugin_fixture(true);
        let host = PluginHost::start(config, &[], CancellationToken::new())
            .await
            .unwrap();
        let cancellation = CancellationToken::new();
        let trigger = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            trigger.cancel();
        });
        let outcome = host
            .invoke(
                "text_stats",
                JsonValue::new(serde_json::json!({"text":"cancel me"})).unwrap(),
                control(cancellation),
            )
            .await;
        assert!(matches!(
            outcome,
            PluginCallOutcome::StoppedAfterSettlement {
                stop: super::PluginStop::CallerCancelled
            }
        ));
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_multichunk_matching_result_wins_before_an_immediate_plugin_exit() {
        let root = TempDirectory::new();
        let padding = "x".repeat(20_000);
        let script = format!(
            r#"#!/bin/sh
IFS= read -r hello || exit 2
printf '%s\n' '{{"version":1,"type":"hello","plugin_id":"text-tools","tools":[{{"name":"text_stats","description":"Count text","parameters":{{"type":"object","properties":{{"text":{{"type":"string"}}}},"required":["text"],"additionalProperties":false}},"output":{{"type":"object","properties":{{"words":{{"type":"integer"}},"padding":{{"type":"string"}}}},"required":["words","padding"],"additionalProperties":false}}}}]}}'
IFS= read -r call || exit 2
printf '%s\n' '{{"version":1,"type":"result","id":1,"ok":true,"value":{{"words":2,"padding":"{padding}"}}}}'
exit 0
"#
        );
        let config = config_from_script(&root, script.as_bytes());
        let host = PluginHost::start(config, &[], CancellationToken::new())
            .await
            .unwrap();
        let outcome = host
            .invoke(
                "text_stats",
                JsonValue::new(serde_json::json!({"text":"two words"})).unwrap(),
                control(CancellationToken::new()),
            )
            .await;
        assert!(matches!(
            outcome,
            PluginCallOutcome::Settled(PluginResultPayload::Success(value))
                if value.as_value()["padding"].as_str().is_some_and(|value| value.len() == 20_000)
        ));
        let next = host
            .invoke(
                "text_stats",
                JsonValue::new(serde_json::json!({"text":"future"})).unwrap(),
                control(CancellationToken::new()),
            )
            .await;
        assert!(matches!(next, PluginCallOutcome::Unavailable));
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn bounded_call_queue_reports_busy_instead_of_growing() {
        let root = TempDirectory::new();
        let script = br#"#!/bin/sh
IFS= read -r hello || exit 2
printf '%s\n' '{"version":1,"type":"hello","plugin_id":"text-tools","tools":[{"name":"text_stats","description":"Count text","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false},"output":{"type":"object","properties":{"words":{"type":"integer"}},"required":["words"],"additionalProperties":false}}]}'
id=1
while IFS= read -r call; do
  /bin/sleep 0.4
  printf '{"version":1,"type":"result","id":%s,"ok":true,"value":{"words":2}}\n' "$id"
  id=$((id + 1))
done
"#;
        let config = config_from_script(&root, script);
        let host = Arc::new(
            PluginHost::start(config, &[], CancellationToken::new())
                .await
                .unwrap(),
        );
        let spawn_call = |host: Arc<PluginHost>, text: &'static str| {
            tokio::spawn(async move {
                host.invoke(
                    "text_stats",
                    JsonValue::new(serde_json::json!({"text":text})).unwrap(),
                    control(CancellationToken::new()),
                )
                .await
            })
        };
        let first = spawn_call(Arc::clone(&host), "first");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let second = spawn_call(Arc::clone(&host), "second");
        tokio::task::yield_now().await;
        let third = spawn_call(Arc::clone(&host), "third");
        tokio::task::yield_now().await;
        let busy = host
            .invoke(
                "text_stats",
                JsonValue::new(serde_json::json!({"text":"fourth"})).unwrap(),
                control(CancellationToken::new()),
            )
            .await;
        assert!(matches!(busy, PluginCallOutcome::Busy));
        let _ = first.await.unwrap();
        let _ = second.await.unwrap();
        let _ = third.await.unwrap();
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_publishes_unavailability_before_waiting_for_process_cleanup() {
        let root = TempDirectory::new();
        let script = br#"#!/bin/sh
IFS= read -r hello || exit 2
printf '%s\n' '{"version":1,"type":"hello","plugin_id":"text-tools","tools":[{"name":"text_stats","description":"Count text","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false},"output":{"type":"object","properties":{"words":{"type":"integer"}},"required":["words"],"additionalProperties":false}}]}'
/bin/sleep 10
"#;
        let config = config_from_script(&root, script);
        let host = Arc::new(
            PluginHost::start(config, &[], CancellationToken::new())
                .await
                .unwrap(),
        );
        let closing_host = Arc::clone(&host);
        let closing = tokio::spawn(async move { closing_host.shutdown().await });
        let deadline = Instant::now() + Duration::from_secs(1);
        while host.is_available("text_stats") {
            assert!(
                Instant::now() < deadline,
                "shutdown state was not published"
            );
            tokio::task::yield_now().await;
        }

        let outcome = host
            .invoke(
                "text_stats",
                JsonValue::new(serde_json::json!({"text":"must not dispatch"})).unwrap(),
                control(CancellationToken::new()),
            )
            .await;
        assert!(matches!(outcome, PluginCallOutcome::Unavailable));
        closing.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn action_timeout_after_dispatch_is_unknown_and_forces_bounded_cleanup() {
        let root = TempDirectory::new();
        let script = br#"#!/bin/sh
IFS= read -r hello || exit 2
printf '%s\n' '{"version":1,"type":"hello","plugin_id":"text-tools","tools":[{"name":"text_stats","description":"Count text","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false},"output":{"type":"object","properties":{"words":{"type":"integer"}},"required":["words"],"additionalProperties":false}}]}'
IFS= read -r call || exit 2
/bin/sleep 10
"#;
        let config = config_from_script(&root, script);
        let host = PluginHost::start(config, &[], CancellationToken::new())
            .await
            .unwrap();
        let started = Instant::now();
        let outcome = host
            .invoke(
                "text_stats",
                JsonValue::new(serde_json::json!({"text":"timeout"})).unwrap(),
                PluginCallControl::new(
                    CancellationToken::new(),
                    Instant::now() + Duration::from_secs(3),
                    Instant::now() + Duration::from_millis(100),
                ),
            )
            .await;
        assert!(
            matches!(
                outcome,
                PluginCallOutcome::OutcomeUnknown {
                    stop: Some(super::PluginStop::ActionTimeout)
                }
            ),
            "{outcome:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn stalled_call_writer_preserves_partial_dispatch_evidence_at_its_deadline() {
        let root = TempDirectory::new();
        let script = br#"#!/bin/sh
IFS= read -r hello || exit 2
printf '%s\n' '{"version":1,"type":"hello","plugin_id":"text-tools","tools":[{"name":"text_stats","description":"Count text","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false},"output":{"type":"object","properties":{"words":{"type":"integer"}},"required":["words"],"additionalProperties":false}}]}'
/bin/sleep 10
"#;
        let config = config_from_script(&root, script);
        let host = PluginHost::start(config, &[], CancellationToken::new())
            .await
            .unwrap();
        let text = "x".repeat(64 * 1024 - r#"{"text":""}"#.len());
        let started = Instant::now();
        let outcome = host
            .invoke(
                "text_stats",
                JsonValue::new(serde_json::json!({"text":text})).unwrap(),
                PluginCallControl::new(
                    CancellationToken::new(),
                    Instant::now() + Duration::from_secs(5),
                    Instant::now() + Duration::from_secs(5),
                ),
            )
            .await;
        assert!(
            matches!(outcome, PluginCallOutcome::OutcomeUnknown { stop: None }),
            "{outcome:?}"
        );
        assert!(started.elapsed() >= super::PROTOCOL_WRITE_TIMEOUT);
        assert!(started.elapsed() < Duration::from_secs(4));
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn term_ignoring_plugin_reaches_kill_after_the_fixed_cancel_grace() {
        let root = TempDirectory::new();
        let script = br#"#!/bin/sh
trap '' TERM
IFS= read -r hello || exit 2
printf '%s\n' '{"version":1,"type":"hello","plugin_id":"text-tools","tools":[{"name":"text_stats","description":"Count text","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false},"output":{"type":"object","properties":{"words":{"type":"integer"}},"required":["words"],"additionalProperties":false}}]}'
IFS= read -r call || exit 2
IFS= read -r cancel || exit 2
/bin/sleep 10
"#;
        let config = config_from_script(&root, script);
        let host = PluginHost::start(config, &[], CancellationToken::new())
            .await
            .unwrap();
        let cancellation = CancellationToken::new();
        let trigger = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            trigger.cancel();
        });
        let started = Instant::now();
        let outcome = host
            .invoke(
                "text_stats",
                JsonValue::new(serde_json::json!({"text":"cancel and escalate"})).unwrap(),
                PluginCallControl::new(
                    cancellation,
                    Instant::now() + Duration::from_secs(8),
                    Instant::now() + Duration::from_secs(8),
                ),
            )
            .await;
        assert!(matches!(
            outcome,
            PluginCallOutcome::OutcomeUnknown {
                stop: Some(super::PluginStop::CallerCancelled)
            }
        ));
        assert!(started.elapsed() >= Duration::from_secs(3));
        assert!(started.elapsed() < Duration::from_secs(6));
        host.shutdown().await.unwrap();
    }

    #[test]
    fn dispatch_evidence_fixes_timeout_and_unknown_outcome_priority() {
        assert!(matches!(
            super::fatal_outcome(
                super::DispatchEvidence::NotDispatched,
                None,
                super::IoFault::WriteTimeout,
                crate::tools::process::PluginCleanup::Quiescent(
                    crate::tools::process::ProcessTermination::ExitCode(0)
                ),
            ),
            PluginCallOutcome::StoppedBeforeDispatch {
                stop: super::PluginStop::ActionTimeout
            }
        ));
        assert!(matches!(
            super::fatal_outcome(
                super::DispatchEvidence::MayHaveBeenDispatched,
                None,
                super::IoFault::WriteTimeout,
                crate::tools::process::PluginCleanup::Quiescent(
                    crate::tools::process::ProcessTermination::ExitCode(0)
                ),
            ),
            PluginCallOutcome::OutcomeUnknown { stop: None }
        ));
    }

    #[test]
    fn startup_cleanup_ownership_loss_is_never_downgraded_to_an_ordinary_failure() {
        assert_eq!(
            super::map_plugin_process_start_error(
                crate::tools::process::PluginProcessError::OwnershipLost,
            ),
            super::ActorStartError::OwnershipLost
        );
        assert_eq!(
            super::map_plugin_process_start_error(crate::tools::process::PluginProcessError::Spawn),
            super::ActorStartError::Startup
        );
    }

    #[test]
    fn plugin_process_environment_is_exactly_the_five_documented_variables() {
        let environment = super::plugin_environment("text-tools")
            .into_iter()
            .map(|(name, value)| (name.into_string().unwrap(), value.into_string().unwrap()))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            environment,
            std::collections::BTreeMap::from([
                ("DSH_PLUGIN_ID".to_owned(), "text-tools".to_owned()),
                ("DSH_PLUGIN_PROTOCOL".to_owned(), "1".to_owned()),
                ("LANG".to_owned(), "C".to_owned()),
                ("LC_ALL".to_owned(), "C".to_owned()),
                ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ])
        );
    }

    #[test]
    fn lost_process_ownership_beats_a_buffered_result_but_normal_exit_does_not() {
        let buffered = super::PumpResult {
            progress: true,
            stdout_received: true,
            fault: None,
        };
        assert_eq!(
            super::finish_pump_for_leader(
                buffered,
                crate::tools::process::PluginLeaderState::OwnershipLost,
            )
            .unwrap_err(),
            super::IoFault::OwnershipLost
        );
        let exited = super::finish_pump_for_leader(
            buffered,
            crate::tools::process::PluginLeaderState::Exited(
                crate::tools::process::ProcessTermination::ExitCode(0),
            ),
        )
        .unwrap();
        assert!(exited.fault.is_none());
    }
}
