use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bevy_app::{App, Plugin, Update};
use bevy_ecs::schedule::{InternedScheduleLabel, ScheduleLabel};
use bevy_ecs::{prelude::World, resource::Resource};

use tokio::{runtime::Runtime, task::JoinHandle};

/// A re-export of the tokio version used by this crate.
pub use tokio;

/// An internal struct keeping track of how many ticks have elapsed since the start of the program.
#[derive(Resource)]
struct UpdateTicks {
    ticks: Arc<AtomicUsize>,
    update_watch_tx: tokio::sync::watch::Sender<()>,
}

impl UpdateTicks {
    fn increment_ticks(&self) -> usize {
        let new_ticks = self.ticks.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        self.update_watch_tx
            .send(())
            .expect("Failed to send update_watch channel message");
        new_ticks
    }
}

/// The Bevy [`Plugin`] which sets up the [`TokioTasksRuntime`] Bevy resource and registers
/// the [`tick_runtime_update`] exclusive system.
pub struct TokioTasksPlugin {
    /// Callback which is used to create a Tokio runtime when the plugin is installed. The
    /// default value for this field configures a multi-threaded [`Runtime`] with IO and timer
    /// functionality enabled if building for non-wasm32 architectures. On wasm32 the current-thread
    /// scheduler is used instead.
    pub make_runtime: Box<dyn Fn() -> Runtime + Send + Sync + 'static>,
    /// The [`ScheduleLabel`] during which the [`tick_runtime_update`] function will be executed.
    /// The default value for this field is [`Update`].
    pub schedule_label: InternedScheduleLabel,
}

impl Default for TokioTasksPlugin {
    /// Configures the plugin to build a new Tokio [`Runtime`] with both IO and timer functionality
    /// enabled. On the wasm32 architecture, the [`Runtime`] will be the current-thread runtime, on all other
    /// architectures the [`Runtime`] will be the multi-thread runtime.
    /// 
    /// The default schedule label is [`Update`].
    fn default() -> Self {
        Self {
            make_runtime: Box::new(|| {
                #[cfg(not(target_arch = "wasm32"))]
                let mut runtime = tokio::runtime::Builder::new_multi_thread();
                #[cfg(target_arch = "wasm32")]
                let mut runtime = tokio::runtime::Builder::new_current_thread();
                runtime.enable_all();
                runtime
                    .build()
                    .expect("Failed to create Tokio runtime for background tasks")
            }),
            schedule_label: Update.intern()
        }
    }
}

impl Plugin for TokioTasksPlugin {
    fn build(&self, app: &mut App) {
        let ticks = Arc::new(AtomicUsize::new(0));
        let (update_watch_tx, update_watch_rx) = tokio::sync::watch::channel(());
        let runtime = (self.make_runtime)();
        app.insert_resource(UpdateTicks {
            ticks: ticks.clone(),
            update_watch_tx,
        });
        app.insert_resource(TokioTasksRuntime::new(ticks, runtime, update_watch_rx));
        app.add_systems(self.schedule_label, tick_runtime_update);
    }
}

/// The Bevy exclusive system which executes the main thread callbacks that background
/// tasks requested using [`run_on_main_thread`](TaskContext::run_on_main_thread). You
/// can control which Bevy schedule stage this system executes in by specifying a custom
/// [`schedule_label`](TokioTasksPlugin::schedule_label) value.
pub fn tick_runtime_update(world: &mut World) {
    let current_tick = {
        let tick_counter = match world.get_resource::<UpdateTicks>() {
            Some(counter) => counter,
            None => return,
        };

        // Increment update ticks and notify watchers of update tick.
        tick_counter.increment_ticks()
    };

    if let Some(mut runtime) = world.remove_resource::<TokioTasksRuntime>() {
        runtime.execute_main_thread_work(world, current_tick);
        world.insert_resource(runtime);
    }
}

type MainThreadCallback = Box<dyn FnOnce(MainThreadContext) + Send + 'static>;

/// The Bevy [`Resource`] which stores the Tokio [`Runtime`] and allows for spawning new
/// background tasks.
#[derive(Resource)]
pub struct TokioTasksRuntime(Box<TokioTasksRuntimeInner>);

/// The inner fields are boxed to reduce the cost of the every-frame move out of and back into
/// the world in [`tick_runtime_update`].
struct TokioTasksRuntimeInner {
    runtime: Runtime,
    ticks: Arc<AtomicUsize>,
    update_watch_rx: tokio::sync::watch::Receiver<()>,
    update_run_tx: tokio::sync::mpsc::UnboundedSender<MainThreadCallback>,
    update_run_rx: tokio::sync::mpsc::UnboundedReceiver<MainThreadCallback>,
}

impl TokioTasksRuntime {
    fn new(
        ticks: Arc<AtomicUsize>,
        runtime: Runtime,
        update_watch_rx: tokio::sync::watch::Receiver<()>,
    ) -> Self {
        let (update_run_tx, update_run_rx) = tokio::sync::mpsc::unbounded_channel();

        Self(Box::new(TokioTasksRuntimeInner {
            runtime,
            ticks,
            update_watch_rx,
            update_run_tx,
            update_run_rx,
        }))
    }

    /// Returns the Tokio [`Runtime`] on which background tasks are executed. You can specify
    /// how this is created by providing a custom [`make_runtime`](TokioTasksPlugin::make_runtime).
    pub fn runtime(&self) -> &Runtime {
        &self.0.runtime
    }

    /// Spawn a task which will run on the background Tokio [`Runtime`] managed by this [`TokioTasksRuntime`]. The
    /// background task is provided a [`TaskContext`] which allows it to do things like
    /// [sleep for a given number of main thread updates](TaskContext::sleep_updates) or
    /// [invoke callbacks on the main Bevy thread](TaskContext::run_on_main_thread).
    pub fn spawn_background_task<Task, Output, Spawnable>(
        &self,
        spawnable_task: Spawnable,
    ) -> JoinHandle<Output>
    where
        Task: Future<Output = Output> + Send + 'static,
        Output: Send + 'static,
        Spawnable: FnOnce(TaskContext) -> Task + Send + 'static,
    {
        let inner = &self.0;
        let context = TaskContext {
            update_watch_rx: inner.update_watch_rx.clone(),
            ticks: inner.ticks.clone(),
            update_run_tx: inner.update_run_tx.clone(),
        };
        let future = spawnable_task(context);
        inner.runtime.spawn(future)
    }

    /// Execute all of the requested runnables on the main thread.
    pub(crate) fn execute_main_thread_work(&mut self, world: &mut World, current_tick: usize) {
        // Running this single future which yields once allows the runtime to process tasks
        // if the runtime is a current_thread runtime. If its a multi-thread runtime then
        // this isn't necessary but is harmless.
        self.0.runtime.block_on(async {
            tokio::task::yield_now().await;
        });
        while let Ok(runnable) = self.0.update_run_rx.try_recv() {
            let context = MainThreadContext {
                world,
                current_tick,
            };
            runnable(context);
        }
    }
}

/// The context arguments which are available to main thread callbacks requested using
/// [`run_on_main_thread`](TaskContext::run_on_main_thread).
pub struct MainThreadContext<'a> {
    /// A mutable reference to the main Bevy [World].
    pub world: &'a mut World,
    /// The current update tick in which the current main thread callback is executing.
    pub current_tick: usize,
}

/// The context arguments which are available to background tasks spawned onto the
/// [`TokioTasksRuntime`].
#[derive(Clone)]
pub struct TaskContext {
    update_watch_rx: tokio::sync::watch::Receiver<()>,
    update_run_tx: tokio::sync::mpsc::UnboundedSender<MainThreadCallback>,
    ticks: Arc<AtomicUsize>,
}

impl TaskContext {
    /// Returns the current value of the ticket count from the main thread - how many updates
    /// have occurred since the start of the program. Because the tick count is updated from the
    /// main thread, the tick count may change any time after this function call returns.
    pub fn current_tick(&self) -> usize {
        self.ticks.load(Ordering::SeqCst)
    }

    /// Sleeps the background task until a given number of main thread updates have occurred. If
    /// you instead want to sleep for a given length of wall-clock time, call the normal Tokio sleep
    /// function.
    pub async fn sleep_updates(&mut self, updates_to_sleep: usize) {
        let target_tick = self
            .ticks
            .load(Ordering::SeqCst)
            .wrapping_add(updates_to_sleep);
        while self.ticks.load(Ordering::SeqCst) < target_tick {
            if self.update_watch_rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Invokes a synchronous callback on the main Bevy thread. The callback will have mutable access to the
    /// main Bevy [`World`], allowing it to update any resources or entities that it wants. The callback can
    /// report results back to the background thread by returning an output value, which will then be returned from
    /// this async function once the callback runs.
    pub async fn run_on_main_thread<Runnable, Output>(&mut self, runnable: Runnable) -> Output
    where
        Runnable: FnOnce(MainThreadContext) -> Output + Send + 'static,
        Output: Send + 'static,
    {
        let (output_tx, output_rx) = tokio::sync::oneshot::channel();
        if self.update_run_tx.send(Box::new(move |ctx| {
            if output_tx.send(runnable(ctx)).is_err() {
                panic!("Failed to sent output from operation run on main thread back to waiting task");
            }
        })).is_err() {
            panic!("Failed to send operation to be run on main thread");
        }
        output_rx
            .await
            .expect("Failed to receive output from operation on main thread")
    }
}
