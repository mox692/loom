#![allow(deprecated)]

use crate::rt::Execution;

use generator::{self, Generator, Gn};
use scoped_tls::scoped_thread_local;
use std::cell::RefCell;
use std::collections::VecDeque;

pub(crate) struct Scheduler {
    max_threads: usize,
}

type Thread = Generator<'static, Option<Box<dyn FnOnce()>>, ()>;

scoped_thread_local! {
    static STATE: RefCell<State<'_>>
}

struct QueuedSpawn {
    f: Box<dyn FnOnce()>,
    stack_size: Option<usize>,
}

struct State<'a> {
    execution: &'a mut Execution,
    queued_spawn: &'a mut VecDeque<QueuedSpawn>,
}

impl Scheduler {
    /// Create an execution
    pub(crate) fn new(capacity: usize) -> Scheduler {
        Scheduler {
            max_threads: capacity,
        }
    }

    /// Access the execution
    pub(crate) fn with_execution<F, R>(f: F) -> R
    where
        F: FnOnce(&mut Execution) -> R,
    {
        Self::with_state(|state| f(state.execution))
    }

    /// Perform a context switch
    pub(crate) fn switch() {
        use std::future::Future;
        use std::pin::Pin;
        use std::ptr;
        use std::task::{Context, RawWaker, RawWakerVTable, Waker};

        unsafe fn noop_clone(_: *const ()) -> RawWaker {
            unreachable!()
        }
        unsafe fn noop(_: *const ()) {}

        // Wrapping with an async block deals with the thread-local context
        // `std` uses to manage async blocks
        let mut switch = async { generator::yield_with(()) };
        let switch = unsafe { Pin::new_unchecked(&mut switch) };

        let raw_waker = RawWaker::new(
            ptr::null(),
            &RawWakerVTable::new(noop_clone, noop, noop, noop),
        );
        let waker = unsafe { Waker::from_raw(raw_waker) };
        let mut cx = Context::from_waker(&waker);

        assert!(switch.poll(&mut cx).is_ready());
    }

    pub(crate) fn spawn(stack_size: Option<usize>, f: Box<dyn FnOnce()>) {
        Self::with_state(|state| state.queued_spawn.push_back(QueuedSpawn { stack_size, f }));
    }

    // プログラム1回の実行に相当.
    // プログラム中のthread生成を扱うために, キューが使われていたり, 実行にはgeneratorが使われている
    pub(crate) fn run<F>(&mut self, execution: &mut Execution, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let mut threads = Vec::new();
        // 1つ目のthreadを起動する
        threads.push(spawn_thread(Box::new(f), None));
        threads[0].resume();

        loop {
            // 全てのthreadが終了しているかどうかをcheck
            if execution.threads.is_complete() {
                // もう一回resumeしてreturnする
                for thread in &mut threads {
                    thread.resume();
                    assert!(thread.is_done());
                }
                return;
            }

            // 今実行している thread_id, execution_id の取得
            let active = execution.threads.active_id();

            // 現在実行しているthreadをtick (具体的にはresume) する
            let mut queued_spawn = Self::tick(&mut threads[active.as_usize()], execution);

            // spawnされてキューに載せられたスレッドを, 処理していく?
            while let Some(th) = queued_spawn.pop_front() {
                assert!(threads.len() < self.max_threads);

                let thread_id = threads.len();
                let QueuedSpawn { f, stack_size } = th;

                threads.push(spawn_thread(f, stack_size));
                threads[thread_id].resume();
            }
        }
    }

    // 新しいstateを TLS に入れて, resumeを実施して, queuedSpawnを返す
    fn tick(thread: &mut Thread, execution: &mut Execution) -> VecDeque<QueuedSpawn> {
        let mut queued_spawn = VecDeque::new();
        let state = RefCell::new(State {
            execution,
            queued_spawn: &mut queued_spawn,
        });

        // transmute_lt(&state) を tls に入れて 実行
        STATE.set(unsafe { transmute_lt(&state) }, || {
            // 多分, thread.resume()がtlsを参照するんだと思う.
            // ぱっと思いついたのは, thread.resumeの中で loom::thread::spawn を呼んだ場合とか？(確かspawnがtlsをいじったはず)
            thread.resume();
        });
        queued_spawn
    }

    fn with_state<F, R>(f: F) -> R
    where
        F: FnOnce(&mut State<'_>) -> R,
    {
        if !STATE.is_set() {
            panic!("cannot access Loom execution state from outside a Loom model. \
            are you accessing a Loom synchronization primitive from outside a Loom test (a call to `model` or `check`)?")
        }
        STATE.with(|state| f(&mut state.borrow_mut()))
    }
}

// generatorを返す.
fn spawn_thread(f: Box<dyn FnOnce()>, stack_size: Option<usize>) -> Thread {
    // generatorに実行させる closure
    let body = move || {
        loop {
            let f: Option<Option<Box<dyn FnOnce()>>> = generator::yield_(());

            if let Some(f) = f {
                generator::yield_with(());
                f.unwrap()();
            } else {
                break;
            }
        }

        generator::done!();
    };
    let mut g = match stack_size {
        Some(stack_size) => Gn::new_opt(stack_size, body),
        None => Gn::new(body),
    };
    g.resume();
    g.set_para(Some(f));
    g
}

unsafe fn transmute_lt<'a, 'b>(state: &'a RefCell<State<'b>>) -> &'a RefCell<State<'static>> {
    ::std::mem::transmute(state)
}
